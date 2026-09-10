//! H.264/HEVC VideoToolbox session with asynchronous output and reset.
#![allow(unsafe_code)] // real `objc2-*` FFI calls — see this crate's `apple/mod.rs` doc comment

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use crate::decoder::platform::{
    DecodeError, DecoderNotification, VideoDecoder, VideoDecoderConfig, VideoDecoderOutput,
    VideoOutputPreference,
};
use mediaway_common::{
    Bytes, CodecKind, GpuBufferHandle, NativeHandle, Packet, PixelFormat, Rational, VideoFrame,
    VideoFrameStorage,
};

use iso_bmff::bitstream::avc::parse_avc_decoder_config;
use iso_bmff::bitstream::avc::to_avcc;

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMBlockBufferCustomBlockSource, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMTimeFlags, CMVideoFormatDescription, kCMBlockBufferCustomBlockSourceVersion,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession,
};

use super::codec::{
    cmtime_value_from_ticks, copy_nv12_planes, duration_ticks_from_cmtime_value,
    is_supported_video_codec, ticks_from_cmtime_value, validate_hevc_parameter_sets,
    validate_parameter_sets,
};
use super::format_desc;
use super::hevc_config::{parse_hevc_decoder_config, to_hvcc};

/// `OSStatus`/`CVReturn` "no error" value (both use the plain C convention `0 == success`) —
/// same reuse-for-both convention `mediaway-encoder::apple` already established.
const NO_ERROR: i32 = 0;

/// One decoded frame plus, for [`VideoOutputPreference::ZeroCopyGpu`], the real owned
/// `CVPixelBuffer` reference backing its [`VideoFrameStorage::Gpu`] raw `NativeHandle` bits —
/// `None` for [`VideoOutputPreference::CpuFramesOk`] frames, which own no backend resource. See
/// [ADR-0003](../../../adr/apple/0003-videotoolbox-metal-zero-copy-decode.md) § Handle lifetime.
struct PendingFrame {
    frame: VideoFrame,
    zero_copy_retain: Option<CFRetained<CVPixelBuffer>>,
}

enum PendingOutput {
    Frame(PendingFrame, std::time::Instant),
    Dropped(i64),
    Failed(Option<i64>, DecodeError),
}

/// State shared between [`VideoToolboxVideoDecoder`] and the `VTDecompressionOutputCallback`,
/// which fires asynchronously on a VideoToolbox-internal thread — see ADR-0001 § Callback /
/// output-collection design. Bundles `time_base`/`output` alongside the output queue (needed by
/// the callback to convert `CMTime`s back to this crate's tick convention and to choose the
/// CPU-copy vs. Zero-Copy path) the same way the encoder ADR's own `SharedState` bundles fields
/// beyond a bare queue.
struct SharedState {
    pending: Mutex<VecDeque<PendingOutput>>,
    notification: Mutex<DecoderNotification>,
    time_base: Rational,
    output: VideoOutputPreference,
}

impl SharedState {
    fn publish(&self, event: PendingOutput) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(event);
        self.notification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .notify();
    }
}

/// H.264/HEVC decode session with VideoToolbox-managed references.
pub(crate) struct VideoToolboxVideoDecoder {
    /// Created from the first parameter sets or nonempty configuration data.
    session: Option<CFRetained<VTDecompressionSession>>,
    /// The format description backing `session` — also `CMSampleBuffer::new`'s per-packet
    /// `format_description` argument (via `Deref` to `&CMFormatDescription`).
    video_format_desc: Option<CFRetained<CMVideoFormatDescription>>,
    shared: Arc<SharedState>,
    /// The extra `Arc::into_raw` strong count passed as `decompressionOutputRefCon` — `None`
    /// until a session exists (no callback can fire before then), reclaimed exactly once in
    /// `Drop`.
    refcon_ptr: Option<*const SharedState>,
    codec: CodecKind,
    /// The `CVPixelBuffer` reference backing the most recently [`Self::poll_frame`]-returned
    /// [`VideoOutputPreference::ZeroCopyGpu`] handle — `None` for `CpuFramesOk` sessions.
    /// Replaced (dropping, i.e. releasing, the previous value) at the start of every
    /// `push_packet`/`poll_frame`/`flush` call, matching [`VideoDecoder::poll_frame`]'s
    /// documented "valid until the next call" GPU-handle contract. See
    /// [ADR-0003](../../../adr/apple/0003-videotoolbox-metal-zero-copy-decode.md).
    last_zero_copy_retain: Option<CFRetained<CVPixelBuffer>>,
}

// SAFETY: `VTDecompressionSession`/`CFRetained` are Core Foundation objects, which Apple
// documents as safe to use from any thread as long as access is externally synchronized (this
// type's `&mut self` API surface already enforces that for the Rust side); the only shared
// mutable state reachable from another thread is `SharedState`, which uses `Mutex` internally.
#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "CFRetained<VTDecompressionSession> is a Core Foundation object — see the SAFETY comment above"
)]
unsafe impl Send for VideoToolboxVideoDecoder {}

impl VideoToolboxVideoDecoder {
    /// Open according to [`VideoDecoderConfig`].
    pub(crate) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        validate(config)?;

        let shared = Arc::new(SharedState {
            pending: Mutex::new(VecDeque::new()),
            notification: Mutex::new(DecoderNotification::default()),
            time_base: config.time_base,
            output: config.output,
        });

        let mut decoder = Self {
            session: None,
            video_format_desc: None,
            shared,
            refcon_ptr: None,
            codec: config.codec,
            last_zero_copy_retain: None,
        };

        if !config.extra_data.is_empty() {
            // `extra_data` non-empty at `open()` ⇒ build immediately (ADR-0001 § Session
            // lifecycle) — a malformed/unsupported record is a real error here, not a silent
            // fallback to in-band detection (unlike `linux::vaapi`'s best-effort `seed_params`,
            // which only ever softens genuine parse failures of otherwise-optional seed data).
            match config.codec {
                CodecKind::H264 => {
                    let avcc_config = parse_avc_decoder_config(&config.extra_data)
                        .ok_or(DecodeError::InvalidInput)?;
                    validate_parameter_sets(&avcc_config)?;
                    let fd = format_desc::create_h264(&avcc_config.sps[0], &avcc_config.pps[0])?;
                    decoder.ensure_session(fd)?;
                }
                CodecKind::Hevc => {
                    let hvcc_config = parse_hevc_decoder_config(&config.extra_data)
                        .ok_or(DecodeError::InvalidInput)?;
                    validate_hevc_parameter_sets(&hvcc_config)?;
                    let fd = format_desc::create_hevc(
                        &hvcc_config.vps[0],
                        &hvcc_config.sps[0],
                        &hvcc_config.pps[0],
                    )?;
                    decoder.ensure_session(fd)?;
                }
                _ => return Err(DecodeError::Unsupported),
            }
        }
        Ok(decoder)
    }

    /// Create a session from H.264/HEVC parameter sets.
    /// A no-op once a session exists — dynamic format
    /// renegotiation mid-session is unsupported this stage (ADR-0001 § Scope).
    fn ensure_session(
        &mut self,
        video_format_desc: CFRetained<CMVideoFormatDescription>,
    ) -> Result<(), DecodeError> {
        if self.session.is_some() {
            return Ok(());
        }
        let dest_attrs = destination_pixel_buffer_attributes();

        let refcon_ptr = Arc::into_raw(Arc::clone(&self.shared));
        let callback_record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decompression_output_callback),
            decompressionOutputRefCon: refcon_ptr.cast::<c_void>().cast_mut(),
        };

        let mut session_out: *mut VTDecompressionSession = std::ptr::null_mut();
        // SAFETY: `video_format_desc` is a valid, just-created video format description;
        // `dest_attrs` is a valid `CFDictionary`; `callback_record.decompressionOutputCallback`
        // is a real `extern "C-unwind" fn` matching `VTDecompressionOutputCallback`'s exact
        // signature; `decompressionOutputRefCon` is a valid, live pointer for at least the
        // session's lifetime (reclaimed only in `Drop`, after `invalidate()`); `callback_record`
        // outlives this call (stack value, borrowed only for the duration of `create`);
        // `session_out` is a valid stack out-pointer.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &video_format_desc,
                None,
                Some(&dest_attrs),
                std::ptr::from_ref(&callback_record),
                NonNull::from(&mut session_out),
            )
        };
        if status != NO_ERROR {
            // SAFETY: reclaims the extra strong count taken above; no callback can have fired
            // since the session was never successfully created.
            drop(unsafe { Arc::from_raw(refcon_ptr) });
            return Err(DecodeError::Backend);
        }
        let Some(session_ptr) = NonNull::new(session_out) else {
            // SAFETY: same reasoning as the failure branch above.
            drop(unsafe { Arc::from_raw(refcon_ptr) });
            return Err(DecodeError::Backend);
        };
        // SAFETY: `session_ptr` came from `VTDecompressionSessionCreate`, which just reported
        // success — the Create Rule guarantees a +1 retain count this call now owns.
        let session: CFRetained<VTDecompressionSession> =
            unsafe { CFRetained::from_raw(session_ptr) };

        self.video_format_desc = Some(video_format_desc);
        self.session = Some(session);
        self.refcon_ptr = Some(refcon_ptr);
        Ok(())
    }

    pub(crate) fn reset_for_keyframe(&mut self, hard: bool) -> Result<(), DecodeError> {
        if let Some(session) = self.session.as_ref() {
            let status = unsafe { session.wait_for_asynchronous_frames() };
            if status != NO_ERROR {
                return Err(DecodeError::Backend);
            }
        }
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.last_zero_copy_retain = None;
        if hard && self.session.is_some() {
            if let Some(session) = self.session.take() {
                unsafe { session.invalidate() };
            }
            if let Some(refcon) = self.refcon_ptr.take() {
                drop(unsafe { Arc::from_raw(refcon) });
            }
            if let Some(format) = self.video_format_desc.take() {
                self.ensure_session(format)?;
            }
        }
        Ok(())
    }
}

impl VideoDecoder for VideoToolboxVideoDecoder {
    fn set_notification(&mut self, notification: DecoderNotification) {
        *self
            .shared
            .notification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = notification;
        self.shared
            .notification
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .notify();
    }

    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError> {
        // Drop (release) the previously `poll_frame`-returned Zero-Copy handle — a no-op for
        // `CpuFramesOk` sessions, where this is always `None`. See `last_zero_copy_retain`'s doc.
        self.last_zero_copy_retain = None;
        if packet.is_discard {
            return Ok(());
        }

        // Per-codec framing (ADR-0002 § Byte framing): H.264/HEVC Annex-B input becomes
        // 4-byte-length-prefixed (and yields a fresh avcC/hvcC on the first parameter-set-
        // bearing packet, feeding lazy session creation below); already-framed input passes
        // through.
        let payload = match self.codec {
            CodecKind::H264 => {
                let avcc_out = to_avcc(&packet.payload);
                if self.session.is_none() {
                    let avcc_bytes = avcc_out.avcc.as_ref().ok_or(DecodeError::InvalidInput)?;
                    let avcc_config =
                        parse_avc_decoder_config(avcc_bytes).ok_or(DecodeError::InvalidInput)?;
                    validate_parameter_sets(&avcc_config)?;
                    let fd = format_desc::create_h264(&avcc_config.sps[0], &avcc_config.pps[0])?;
                    self.ensure_session(fd)?;
                }
                avcc_out.payload
            }
            CodecKind::Hevc => {
                let hvcc_out = to_hvcc(&packet.payload);
                if self.session.is_none() {
                    let hvcc_bytes = hvcc_out.hvcc.as_ref().ok_or(DecodeError::InvalidInput)?;
                    let hvcc_config =
                        parse_hevc_decoder_config(hvcc_bytes).ok_or(DecodeError::InvalidInput)?;
                    validate_hevc_parameter_sets(&hvcc_config)?;
                    let fd = format_desc::create_hevc(
                        &hvcc_config.vps[0],
                        &hvcc_config.sps[0],
                        &hvcc_config.pps[0],
                    )?;
                    self.ensure_session(fd)?;
                }
                hvcc_out.payload
            }
            _ => return Err(DecodeError::Unsupported),
        };

        let session = self.session.as_ref().ok_or(DecodeError::Backend)?;
        let format_desc = self
            .video_format_desc
            .as_ref()
            .ok_or(DecodeError::Backend)?;

        let block_buffer = create_block_buffer(&payload)?;
        let timing = build_timing_info(packet, self.shared.time_base);
        let sample_buffer = create_sample_buffer(&block_buffer, format_desc, &timing)?;

        let flags = VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression
            | VTDecodeFrameFlags::Frame_EnableTemporalProcessing;
        // SAFETY: `session` is a valid, open session (guarded by the `Option` check above);
        // `sample_buffer` is a valid, just-created `CMSampleBuffer`; `source_frame_ref_con`/
        // `info_flags_out` are intentionally unused (`null_mut`/`None`) — this backend recovers
        // timing from the output callback's own `CMTime` parameters, not a threaded-through
        // refcon (ADR-0001 § Callback design).
        let status = unsafe {
            session.decode_frame(
                &sample_buffer,
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != NO_ERROR {
            return Err(DecodeError::Backend);
        }
        Ok(())
    }

    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        loop {
            match self.poll_output()? {
                Some(VideoDecoderOutput::Frame { frame, .. }) => return Ok(Some(frame)),
                Some(VideoDecoderOutput::Dropped { .. }) => continue,
                Some(VideoDecoderOutput::Failed { error, .. }) => return Err(error),
                None => return Ok(None),
            }
        }
    }

    fn poll_output(&mut self) -> Result<Option<VideoDecoderOutput>, DecodeError> {
        // Same invalidation as `push_packet`/`flush` — see `last_zero_copy_retain`'s doc. Runs
        // before the pop below so a caller polling repeatedly without pushing new packets still
        // gets the documented one-call validity window, not an accidental extra grace period.
        self.last_zero_copy_retain = None;
        let popped = {
            let mut pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.pop_front()
        };
        let Some(popped) = popped else {
            return Ok(None);
        };
        // Keep the new frame's retain alive (superseding the one just dropped above) until the
        // next call — `None` for `CpuFramesOk` frames, which carry no backend resource.
        Ok(Some(match popped {
            PendingOutput::Frame(popped, ready_at) => {
                self.last_zero_copy_retain = popped.zero_copy_retain;
                VideoDecoderOutput::Frame {
                    frame: popped.frame,
                    ready_at,
                }
            }
            PendingOutput::Dropped(pts) => VideoDecoderOutput::Dropped { pts },
            PendingOutput::Failed(pts, error) => VideoDecoderOutput::Failed { pts, error },
        }))
    }
}

impl Drop for VideoToolboxVideoDecoder {
    fn drop(&mut self) {
        if let Some(session) = self.session.as_ref() {
            // SAFETY: draining before invalidate — mitigates the unconfirmed
            // invalidate/callback-cutoff ordering guarantee ADR-0001 § Callback design flags as
            // an open question, same defensive posture the encoder backend already uses for its
            // own session type.
            let _ = unsafe { session.wait_for_asynchronous_frames() };
            // SAFETY: `session` is a valid, owned `CFRetained<VTDecompressionSession>` about to
            // be dropped; `invalidate()` is Apple's documented deterministic-teardown call,
            // meant to be followed by releasing the last reference (the `CFRetained` drop glue
            // below does that).
            unsafe { session.invalidate() };
        }
        if let Some(refcon_ptr) = self.refcon_ptr {
            // SAFETY: `refcon_ptr` is exactly the `Arc::into_raw` pointer taken in
            // `ensure_session` and handed to VideoToolbox as the callback's
            // `decompressionOutputRefCon`; reclaimed here exactly once, after `invalidate()`
            // above — no other code path frees it.
            drop(unsafe { Arc::from_raw(refcon_ptr) });
        }
    }
}

/// `VTDecompressionOutputCallback` — fires on a VideoToolbox-internal thread (possibly
/// asynchronously, decoupled from the `decode_frame` call that produced it, since
/// `kVTDecodeFrame_EnableTemporalProcessing` is always set — see ADR-0001 § Output ordering).
unsafe extern "C-unwind" fn decompression_output_callback(
    decompression_output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    info_flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
) {
    let ready_at = std::time::Instant::now();
    // SAFETY: `decompression_output_ref_con` is exactly the `Arc::into_raw::<SharedState>`
    // pointer passed at session creation, reclaimed only in `Drop` after `invalidate()` — valid
    // for the whole lifetime any callback can fire in. Borrowed here, never re-owned
    // (`Arc::from_raw` is never called inside this function — see `Drop`'s own comment for the
    // single reclaim site).
    let shared = unsafe { &*(decompression_output_ref_con.cast::<SharedState>()) };
    let pts = (presentation_time_stamp.flags.contains(CMTimeFlags::Valid)
        && presentation_time_stamp.timescale != 0)
        .then(|| {
            ticks_from_cmtime_value(
                presentation_time_stamp.value,
                presentation_time_stamp.timescale,
                shared.time_base,
            )
        });
    if status != NO_ERROR {
        tracing::warn!(status, ?pts, "VideoToolbox asynchronous frame failure");
        shared.publish(PendingOutput::Failed(pts, DecodeError::Backend));
        return;
    }
    if image_buffer.is_null() {
        if info_flags.contains(VTDecodeInfoFlags::FrameDropped)
            && let Some(pts) = pts
        {
            shared.publish(PendingOutput::Dropped(pts));
        } else {
            shared.publish(PendingOutput::Failed(pts, DecodeError::Backend));
        }
        return;
    }
    // SAFETY: non-null (checked above); VideoToolbox guarantees a valid image buffer for the
    // duration of this callback invocation per `VTDecompressionOutputCallback`'s own contract.
    let image_buffer = unsafe { &*image_buffer };

    // `CVPixelBuffer` is a type alias for `CVImageBuffer` (`objc2-core-video`), not a distinct
    // downcast target — `image_buffer` already *is* a `&CVPixelBuffer` once VideoToolbox has
    // handed it back through this callback (the session was created with a pixel-buffer-typed
    // `destinationImageBufferAttributes`, so nothing else can appear here).
    let pixel_buffer: &CVPixelBuffer = image_buffer;
    if CVPixelBufferGetPixelFormatType(pixel_buffer)
        != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
    {
        // VideoToolbox declined the requested NV12/VideoRange destination format — do not
        // misinterpret the bytes (ADR-0001 § Callback design).
        shared.publish(PendingOutput::Failed(pts, DecodeError::Unsupported));
        return;
    }

    let pending_frame = if shared.output == VideoOutputPreference::ZeroCopyGpu {
        build_zero_copy_frame(
            pixel_buffer,
            presentation_time_stamp,
            presentation_duration,
            shared.time_base,
        )
    } else {
        // SAFETY: `pixel_buffer` is a valid, concrete `CVPixelBuffer`; this backend only ever
        // reads plane bytes below (never modifies the buffer), matching `ReadOnly`'s contract —
        // a symmetric lock/unlock pair follows (unlocked with the same flags before every
        // return path below).
        if unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly) }
            != NO_ERROR
        {
            shared.publish(PendingOutput::Failed(pts, DecodeError::Backend));
            return;
        }

        let frame = build_frame(
            CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0),
            CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0),
            CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1),
            CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1),
            CVPixelBufferGetHeightOfPlane(pixel_buffer, 1),
            CVPixelBufferGetWidthOfPlane(pixel_buffer, 0),
            CVPixelBufferGetHeightOfPlane(pixel_buffer, 0),
            presentation_time_stamp,
            presentation_duration,
            shared.time_base,
        );

        // SAFETY: matches the lock above — always unlock with the same flags used to lock.
        let _ = unsafe {
            CVPixelBufferUnlockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly)
        };

        frame.map(|frame| PendingFrame {
            frame,
            zero_copy_retain: None,
        })
    };

    let Some(pending_frame) = pending_frame else {
        shared.publish(PendingOutput::Failed(pts, DecodeError::Backend));
        return;
    };
    shared.publish(PendingOutput::Frame(pending_frame, ready_at));
}

/// Build a Zero-Copy [`PendingFrame`] from a `CVPixelBuffer` VideoToolbox still owns (borrowed
/// for this callback's duration) — takes a **new**, independent owned reference via
/// `CFRetained::retain` (never `lock_base_address`/reads no bytes), so the buffer stays valid
/// past the callback's return. See
/// [ADR-0003](../../../adr/apple/0003-videotoolbox-metal-zero-copy-decode.md) § Handle lifetime
/// for who drops (releases) `zero_copy_retain` and when.
fn build_zero_copy_frame(
    pixel_buffer: &CVPixelBuffer,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
    time_base: Rational,
) -> Option<PendingFrame> {
    let width = u32::try_from(CVPixelBufferGetWidth(pixel_buffer)).ok()?;
    let height = u32::try_from(CVPixelBufferGetHeight(pixel_buffer)).ok()?;
    if width == 0 || height == 0 {
        return None;
    }

    // SAFETY: `pixel_buffer` is a valid, concrete `CVPixelBuffer` for the duration of this call
    // (VideoToolbox's own callback contract) — `CFRetained::retain` takes a brand new, fully
    // independent `+1` reference, unrelated to whatever lifetime VideoToolbox itself grants the
    // callback's borrowed `image_buffer` parameter.
    let retained = unsafe { CFRetained::retain(NonNull::from(pixel_buffer)) };
    let bits = CFRetained::as_ptr(&retained).as_ptr() as usize;
    let buffer = NativeHandle::new(bits)?;

    let pts = ticks_from_cmtime_value(
        presentation_time_stamp.value,
        presentation_time_stamp.timescale,
        time_base,
    );
    let duration = duration_ticks_from_cmtime_value(
        presentation_duration.value,
        presentation_duration.timescale,
        time_base,
    );

    Some(PendingFrame {
        frame: VideoFrame {
            pts,
            duration,
            width,
            height,
            format: PixelFormat::Nv12,
            storage: VideoFrameStorage::Gpu(GpuBufferHandle::Metal { buffer }),
        },
        zero_copy_retain: Some(retained),
    })
}

/// Build a [`VideoFrame`] from a locked `CVPixelBuffer`'s NV12 plane pointers, or `None` on a
/// degenerate readback (zero dimensions / null plane base address) — never panics on bad
/// driver-reported geometry, mirroring `linux::vaapi`'s defensive NV12 readback discipline.
#[allow(
    clippy::too_many_arguments,
    reason = "raw CVPixelBuffer plane accessors read individually by the one call site above; grouping them into a struct would not simplify anything"
)]
fn build_frame(
    y_base: *mut c_void,
    y_stride: usize,
    uv_base: *mut c_void,
    uv_stride: usize,
    uv_height: usize,
    width: usize,
    height: usize,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
    time_base: Rational,
) -> Option<VideoFrame> {
    let width_u32 = u32::try_from(width).ok()?;
    let height_u32 = u32::try_from(height).ok()?;
    if width_u32 == 0 || height_u32 == 0 || y_base.is_null() || uv_base.is_null() {
        return None;
    }

    // SAFETY: `y_base` is a valid, locked `CVPixelBuffer` plane-0 base address (checked
    // non-null above), valid for `y_stride * height` bytes per
    // `CVPixelBufferGetBytesPerRowOfPlane`'s own contract, for as long as the buffer stays
    // locked (held by the caller for this whole scope).
    let y_plane = unsafe { std::slice::from_raw_parts(y_base.cast::<u8>(), y_stride * height) };
    // SAFETY: same reasoning as `y_plane` above, for plane 1 (UV).
    let uv_plane =
        unsafe { std::slice::from_raw_parts(uv_base.cast::<u8>(), uv_stride * uv_height) };

    let data = copy_nv12_planes(
        y_plane, y_stride, uv_plane, uv_stride, width_u32, height_u32,
    );

    let pts = ticks_from_cmtime_value(
        presentation_time_stamp.value,
        presentation_time_stamp.timescale,
        time_base,
    );
    let duration = duration_ticks_from_cmtime_value(
        presentation_duration.value,
        presentation_duration.timescale,
        time_base,
    );

    Some(VideoFrame {
        pts,
        duration,
        width: width_u32,
        height: height_u32,
        format: PixelFormat::Nv12,
        storage: VideoFrameStorage::Cpu { data },
    })
}

/// `destinationImageBufferAttributes` for `VTDecompressionSession::create` — forces NV12
/// (`kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`) output (ADR-0001 § Session lifecycle):
/// **`VideoRange`, not `FullRange`** — decode consumes arbitrary third-party H.264 streams, for
/// which `VideoRange` is the more honest default absent VUI-based range detection (deferred,
/// ADR-0001 § Scope).
fn destination_pixel_buffer_attributes() -> CFRetained<CFDictionary> {
    // Bit-pattern reinterpret of the `FourCharCode`/`OSType` pixel-format-type constant as a
    // signed 32-bit `CFNumber` — the standard CoreFoundation convention for a FourCC-valued
    // property (same reinterpretation `mediaway-encoder::apple` already uses for its own
    // pixel-format-type properties), not a magnitude cast.
    let value = i32::from_ne_bytes(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange.to_ne_bytes());
    let number = CFNumber::new_i32(value);
    let number_ct: &CFType = &number;
    // SAFETY: `kCVPixelBufferPixelFormatTypeKey` is a real, always-initialized CoreVideo
    // framework constant (an `extern "C"` static, safe to read for the process's lifetime).
    let key = unsafe { kCVPixelBufferPixelFormatTypeKey };
    let dict = CFDictionary::<CFString, CFType>::from_slices(&[key], &[number_ct]);
    // SAFETY: `CFDictionary<CFString, CFType>` and the bare `CFDictionary` (==
    // `CFDictionary<Opaque, Opaque>`, `VTDecompressionSession::create`'s own
    // `destination_image_buffer_attributes` parameter type) are the identical Core Foundation
    // object at runtime — same toll-free-bridging cast `format_desc::create_raw` already makes.
    unsafe { CFRetained::cast_unchecked(dict) }
}

/// Per-packet `CMSampleTimingInfo` from `Packet::{pts, dts, duration}` (ADR-0001 § Timestamps).
fn build_timing_info(packet: &Packet, time_base: Rational) -> CMSampleTimingInfo {
    let (pts_value, timescale) = cmtime_value_from_ticks(packet.pts, time_base);
    let (dts_value, _) = cmtime_value_from_ticks(packet.dts, time_base);
    let duration_ticks = i64::try_from(packet.duration).unwrap_or(i64::MAX);
    let (duration_value, _) = cmtime_value_from_ticks(duration_ticks, time_base);

    let cmtime = |value: i64| CMTime {
        value,
        timescale,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    };

    CMSampleTimingInfo {
        duration: cmtime(duration_value),
        presentationTimeStamp: cmtime(pts_value),
        decodeTimeStamp: cmtime(dts_value),
    }
}

/// Wrap AVCC-framed packet bytes in a `CMBlockBuffer` for `CMSampleBuffer::new` (ADR-0001 §
/// Byte framing). The AVCC bytes are copied once into a heap-owned `Box<Vec<u8>>` — the one
/// real, named `memcpy` on this path (`to_avcc`'s own `Bytes` output is not directly
/// `CMBlockBuffer`-compatible, so a second copy is unavoidable here; flagged by the ADR as an
/// implementation-pass question, resolved by taking the straightforward owned-copy path rather
/// than an unproven raw-`Bytes`-pointer handoff) — freed exactly once by `free_avcc_block` when
/// VideoToolbox releases the block buffer.
fn create_block_buffer(payload: &Bytes) -> Result<CFRetained<CMBlockBuffer>, DecodeError> {
    // clone: `payload` is a shared `Bytes`; `CMBlockBuffer` needs a distinctly heap-owned,
    // exclusively-VideoToolbox-managed allocation it frees itself via `custom_block_source`'s
    // `FreeBlock` callback — see this function's own doc comment.
    let owned: Box<Vec<u8>> = Box::new(payload.to_vec());
    let len = owned.len();
    if len == 0 {
        return Err(DecodeError::InvalidInput);
    }
    let data_ptr = owned.as_ptr().cast_mut().cast::<c_void>();
    let refcon_ptr = Box::into_raw(owned).cast::<c_void>();

    let custom_block_source = CMBlockBufferCustomBlockSource {
        version: kCMBlockBufferCustomBlockSourceVersion,
        AllocateBlock: None,
        FreeBlock: Some(free_avcc_block),
        refCon: refcon_ptr,
    };

    let mut block_buffer_out: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: `data_ptr` points at `len` valid, exclusively-owned bytes for at least the
    // lifetime of the returned `CMBlockBuffer` (reclaimed exactly once by `free_avcc_block`,
    // which VideoToolbox calls when the block buffer's backing memory is freed);
    // `custom_block_source.FreeBlock` is a real `extern "C-unwind" fn` matching
    // `CMBlockBufferCustomBlockSource`'s exact `FreeBlock` signature; `block_buffer_out` is a
    // valid stack out-pointer.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            data_ptr,
            len,
            None,
            std::ptr::from_ref(&custom_block_source),
            0,
            len,
            0,
            NonNull::from(&mut block_buffer_out),
        )
    };
    if status != NO_ERROR {
        // SAFETY: creation failed before VideoToolbox could take ownership of the box (the
        // custom `FreeBlock` callback is never invoked on a `create_with_memory_block` failure)
        // — reclaim it here instead, the only path that would otherwise leak it.
        drop(unsafe { Box::from_raw(refcon_ptr.cast::<Vec<u8>>()) });
        return Err(DecodeError::Backend);
    }
    let Some(block_buffer_ptr) = NonNull::new(block_buffer_out) else {
        drop(unsafe { Box::from_raw(refcon_ptr.cast::<Vec<u8>>()) });
        return Err(DecodeError::Backend);
    };
    // SAFETY: `block_buffer_ptr` came from `CMBlockBufferCreateWithMemoryBlock`, which just
    // reported success — the Create Rule guarantees a +1 retain count this call now owns.
    Ok(unsafe { CFRetained::from_raw(block_buffer_ptr) })
}

/// SAFETY: `ref_con` is exactly the `Box::into_raw(Box::new(Vec<u8>))` pointer
/// `create_block_buffer` passed as `custom_block_source.refCon` — `CMBlockBuffer` calls this
/// callback exactly once, when the block's backing memory is freed, and never otherwise touches
/// `ref_con`.
unsafe extern "C-unwind" fn free_avcc_block(
    ref_con: *mut c_void,
    _mem: NonNull<c_void>,
    _size: usize,
) {
    drop(unsafe { Box::from_raw(ref_con.cast::<Vec<u8>>()) });
}

/// One `CMSampleBuffer` per packet — this backend never batches multiple frames into one
/// buffer, matching `timing`'s single-entry contract (ADR-0001 § Timestamps).
fn create_sample_buffer(
    block_buffer: &CMBlockBuffer,
    format_desc: &CMVideoFormatDescription,
    timing: &CMSampleTimingInfo,
) -> Result<CFRetained<CMSampleBuffer>, DecodeError> {
    let mut sample_buffer_out: *mut CMSampleBuffer = std::ptr::null_mut();
    // SAFETY: `block_buffer` is a valid, just-created `CMBlockBuffer`; `format_desc` is the
    // session's own retained format description (kept alive by `VideoToolboxVideoDecoder::
    // video_format_desc` for the whole session, `Deref`s to `&CMFormatDescription`); `timing`
    // is a valid stack value describing exactly the one sample in this buffer
    // (`num_sample_timing_entries: 1`, matching `CMSampleBufferCreate`'s "one entry applies to
    // all samples in this call" contract); `sample_buffer_out` is a valid stack out-pointer.
    let status = unsafe {
        CMSampleBuffer::create(
            None,
            Some(block_buffer),
            true,
            None,
            std::ptr::null_mut(),
            Some(format_desc),
            1,
            1,
            std::ptr::from_ref(timing),
            0,
            std::ptr::null(),
            NonNull::from(&mut sample_buffer_out),
        )
    };
    if status != NO_ERROR {
        return Err(DecodeError::Backend);
    }
    let Some(sample_buffer_ptr) = NonNull::new(sample_buffer_out) else {
        return Err(DecodeError::Backend);
    };
    // SAFETY: `sample_buffer_ptr` came from `CMSampleBufferCreate`, which just reported success
    // — the Create Rule guarantees a +1 retain count this call now owns.
    Ok(unsafe { CFRetained::from_raw(sample_buffer_ptr) })
}

fn validate(config: &VideoDecoderConfig) -> Result<(), DecodeError> {
    if !is_supported_video_codec(config.codec) {
        return Err(DecodeError::Unsupported);
    }
    // `VideoOutputPreference` is `#[non_exhaustive]` — an unmatched future variant is a real
    // "we don't know this output path" case, not reachable today.
    if !matches!(
        config.output,
        VideoOutputPreference::CpuFramesOk | VideoOutputPreference::ZeroCopyGpu
    ) {
        return Err(DecodeError::Unsupported);
    }
    if config.pixel_format != PixelFormat::Nv12 {
        return Err(DecodeError::Unsupported);
    }
    if config.time_base.den == 0 {
        return Err(DecodeError::InvalidInput);
    }
    Ok(())
}
