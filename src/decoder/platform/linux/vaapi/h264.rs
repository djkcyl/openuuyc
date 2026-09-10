//! VA-API H.264 decode session — I and single-forward-reference P slices, single slice per
//! picture, progressive, `pic_order_cnt_type == 0`, CPU NV12 output or Zero-Copy DMA-BUF export.
//!
//! See [ADR-0001](../../adr/0001-vaapi-h264-cpu-out.md) for the original IDR-only baseline,
//! [ADR-0002](../../adr/linux/0002-vaapi-h264-p-slice-dpb.md) for the sliding-window DPB /
//! single-forward-reference P-slice extension, and
//! [ADR-0006](../../adr/linux/0006-vaapi-dmabuf-zero-copy-output.md) for the
//! `VideoOutputPreference::ZeroCopyGpu` DMA-BUF export path this file also implements — all
//! three name the zero-hardware-verification caveat for this crate as authored.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::decoder::platform::bitstream::{NalUnit, NalUnitType, split_annex_b};
use crate::decoder::platform::{
    DecodeError, VideoDecoder, VideoDecoderConfig, VideoOutputPreference,
};
use mediaway_common::{Bytes, Packet, PixelFormat, VideoFrame, VideoFrameStorage};

use cros_libva::{
    BufferType, Config, Context, Display, H264PicFields, H264SeqFields, IQMatrix,
    IQMatrixBufferH264, Picture, PictureH264, PictureParameter, PictureParameterBufferH264,
    SliceParameter, SliceParameterBufferH264, Surface, VA_FOURCC_NV12, VA_INVALID_SURFACE,
    VA_PICTURE_H264_INVALID, VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RT_FORMAT_YUV420,
    VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAImageFormat,
};

use super::codec::h264_profile_candidates;
use super::dmabuf::{self, DmaBufFds};
use super::dpb::{
    Dpb, DpbSlot, H264_MAX_DPB_SLOTS, default_ref_pic_list0, derive_pic_order_cnt_msb,
};
use super::nv12::copy_nv12_from_planes;
use super::pps::Pps;
use super::slice::SliceHeader;
use super::sps::Sps;

/// A decode pipeline bound to one negotiated profile + coded resolution, created lazily the
/// first time an SPS is available — `open()` cannot know the profile/coded resolution before
/// that (`VideoDecoderConfig::extra_data` "may be empty until first keyframe").
struct Pipeline {
    /// Kept alive for the context's lifetime (VA-API does not document that the config may be
    /// destroyed immediately after `vaCreateContext` — mirrors `mediaway-encoder-linux`'s
    /// identical `_config` field).
    _config: Config,
    context: Rc<Context>,
    /// DPB-slot-indexed: `surfaces[i]` is the physical surface backing `dpb`'s slot `i`.
    surfaces: Vec<Option<Surface<()>>>,
    dpb: Dpb,
    /// DPB-slot-indexed, parallel to `surfaces`/`dpb`: the `OwnedFd`(s) backing any slot
    /// currently exported as a Zero-Copy [`mediaway_common::GpuBufferHandle::DmaBuf`] — only
    /// ever populated when `VaapiH264Decoder::output` is `ZeroCopyGpu` (see ADR-0003 § Fd
    /// lifetime contract). Dropping an entry closes the fd(s).
    exported_fds: Vec<Option<DmaBufFds>>,
    coded_width: u32,
    coded_height: u32,
    nv12_format: VAImageFormat,
    /// From `Sps::max_num_ref_frames` — `VAPictureParameterBufferH264::num_ref_frames` is filled
    /// from this static per-stream value (`FFmpeg`'s `fill_vaapi_pic` convention), not a
    /// per-picture "currently occupied" count. See adr/linux/0002 § VA-API-specific plumbing.
    max_num_ref_frames: u32,
}

/// VA-API H.264 decode session. See module docs for scope.
pub(crate) struct VaapiH264Decoder {
    display: Rc<Display>,
    pipeline: Option<Pipeline>,
    sps: Option<Sps>,
    pps: Option<Pps>,
    declared_width: u32,
    declared_height: u32,
    /// Caller's requested output mode (`open()`-time, fixed for the session's lifetime — see
    /// ADR-0003). Each pending frame's `Option<usize>` is `Some(dpb_slot_index)` when the frame
    /// carries a Zero-Copy handle into that slot, `None` for CPU frames.
    output: VideoOutputPreference,
    pending: VecDeque<(VideoFrame, Option<usize>)>,
    /// The DPB slot index of the most recent Zero-Copy frame handed to the caller via
    /// [`VideoDecoder::poll_frame`], if any — released (outstanding mark cleared, fd closed) at
    /// the top of the next `push_packet`/`poll_frame`/`flush` call, matching
    /// [`VideoDecoder::poll_frame`]'s documented handle-lifetime contract (see ADR-0003 § Fd
    /// lifetime contract).
    last_delivered_gpu_slot: Option<usize>,
    /// Carried across pictures for `derive_pic_order_cnt_msb` (ITU-T H.264 § 8.2.1.1); reset to
    /// `0` on every IDR. Mirrors `vulkan::decoder::H264Session`'s identical pair.
    prev_poc_msb: i32,
    prev_poc_lsb: u32,
}

impl VaapiH264Decoder {
    /// Open per [`VideoDecoderConfig::output`].
    pub(crate) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        validate(config)?;
        // Both `CpuFramesOk` and `ZeroCopyGpu` are supported — see ADR-0003 for the DMA-BUF
        // export path `ZeroCopyGpu` takes (ADR-0001's original unconditional rejection of
        // anything but `CpuFramesOk` is narrowed here).

        // `Display::open()` tries `/dev/dri/renderD128..` in order (cros_libva's
        // `DrmDeviceIterator`), wrapping `vaGetDisplayDRM` + `vaInitialize`. In this session's
        // environment (no real VA-API device) this honestly returns `None` — see ADR-0001's
        // hardware caveat.
        let display: Rc<Display> = Display::open().ok_or(DecodeError::Unsupported)?;

        let (sps, pps) = seed_params(&config.extra_data);

        Ok(Self {
            display,
            pipeline: None,
            sps,
            pps,
            declared_width: config.width,
            declared_height: config.height,
            output: config.output,
            pending: VecDeque::new(),
            last_delivered_gpu_slot: None,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
        })
    }

    /// Create the `Config`/`Surface`/`Context`/`Dpb` pipeline on first use. A no-op once created
    /// — dynamic resolution/profile renegotiation mid-session is unsupported this session.
    fn ensure_pipeline(&mut self, sps: Sps) -> Result<(), DecodeError> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        let coded_width = sps.width();
        let coded_height = sps.height();
        if coded_width == 0 || coded_height == 0 {
            return Err(DecodeError::InvalidInput);
        }
        if self.declared_width != 0
            && self.declared_height != 0
            && (coded_width > round_up_16(self.declared_width)
                || coded_height > round_up_16(self.declared_height))
        {
            // Stream reports a picture larger than the caller declared at `open()`; dynamic
            // resolution renegotiation is unsupported this session.
            return Err(DecodeError::InvalidInput);
        }

        let candidates = h264_profile_candidates(sps.profile_idc)?;
        let supported = self
            .display
            .query_config_profiles()
            .map_err(|_| DecodeError::Backend)?;
        let mut chosen = None;
        for profile in candidates {
            if !supported.contains(&profile) {
                continue;
            }
            let entrypoints = self
                .display
                .query_config_entrypoints(profile)
                .map_err(|_| DecodeError::Backend)?;
            if entrypoints.contains(&VAEntrypoint::VAEntrypointVLD) {
                chosen = Some(profile);
                break;
            }
        }
        let profile = chosen.ok_or(DecodeError::Unsupported)?;

        let attrs = vec![VAConfigAttrib {
            type_: VAConfigAttribType::VAConfigAttribRTFormat,
            value: VA_RT_FORMAT_YUV420,
        }];
        let config = self
            .display
            .create_config(attrs, profile, VAEntrypoint::VAEntrypointVLD)
            .map_err(|_| DecodeError::Backend)?;

        // +1: room for the picture currently being decoded alongside every active short-term
        // reference — verbatim port of vulkan/decoder.rs's own sizing comment (see
        // adr/linux/0002 § VaapiH264Decoder struct shape).
        let pool_size = usize::try_from(sps.max_num_ref_frames)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
            .clamp(1, H264_MAX_DPB_SLOTS);

        let surfaces = self
            .display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(VA_FOURCC_NV12),
                coded_width,
                coded_height,
                None,
                vec![(); pool_size],
            )
            .map_err(|_| DecodeError::Backend)?;

        let context = self
            .display
            .create_context(&config, coded_width, coded_height, Some(&surfaces), true)
            .map_err(|_| DecodeError::Backend)?;

        let nv12_format = self
            .display
            .query_image_formats()
            .map_err(|_| DecodeError::Backend)?
            .into_iter()
            .find(|f| f.fourcc == VA_FOURCC_NV12)
            .ok_or(DecodeError::Unsupported)?;

        self.pipeline = Some(Pipeline {
            _config: config,
            context,
            surfaces: surfaces.into_iter().map(Some).collect(),
            dpb: Dpb::new(pool_size),
            // `DmaBufFds` (via its `OwnedFd` fields) is not `Clone`, so `vec![None; pool_size]`
            // (which requires `Clone` to duplicate the initial element) does not typecheck here
            // — build the `None`-filled vec by iteration instead, matching `surfaces`' own
            // `Surface<()>: !Clone` handling just above.
            exported_fds: (0..pool_size).map(|_| None).collect(),
            coded_width,
            coded_height,
            nv12_format,
            max_num_ref_frames: sps.max_num_ref_frames,
        });
        Ok(())
    }

    /// Allocate the destination slot for a new picture via `Dpb::allocate_slot` (sliding-window
    /// eviction if the DPB is full) and take its physical surface.
    fn take_surface_slot(&mut self) -> Result<(usize, Surface<()>), DecodeError> {
        let pipeline = self.pipeline.as_mut().ok_or(DecodeError::Backend)?;
        let index = pipeline
            .dpb
            .allocate_slot()
            .map_err(|_| DecodeError::Backend)?;
        let surface = pipeline.surfaces[index]
            .take()
            .ok_or(DecodeError::Backend)?;
        Ok((index, surface))
    }

    /// Releases the Zero-Copy DMA-BUF export for the DPB slot most recently delivered to the
    /// caller via [`VideoDecoder::poll_frame`] (if any): clears the DPB's outstanding mark and
    /// drops (closes) the internally-held fd(s). Matches [`VideoDecoder::poll_frame`]'s
    /// documented handle-lifetime contract ("the texture remains valid until the next
    /// `push_packet`/`poll_frame`/`flush` that recycles the surface") — called at the top of all
    /// three of those methods, so the *very next* one after a `ZeroCopyGpu` frame was delivered
    /// always releases it, independent of whether that same slot happens to be picked for reuse
    /// (see ADR-0003 § Fd lifetime contract). A no-op in `CpuFramesOk` mode, where
    /// `last_delivered_gpu_slot` is always `None`.
    fn release_previous_gpu_export(&mut self) {
        let Some(index) = self.last_delivered_gpu_slot.take() else {
            return;
        };
        let Some(pipeline) = self.pipeline.as_mut() else {
            return;
        };
        // Best-effort: an out-of-range index cannot happen (it was produced by this same
        // session's own DPB), but this cleanup path has nothing further to do even if it did.
        let _ = pipeline.dpb.clear_outstanding(index);
        pipeline.exported_fds[index] = None; // drop closes the fd(s)
    }

    /// Decode one I or P picture, following the per-picture ordering ported from
    /// `vulkan/decoder.rs::decode_slice_h264` (see adr/linux/0002 § Per-picture decode
    /// ordering's 8 numbered steps — this function's block comments mark each step).
    #[allow(
        clippy::too_many_lines,
        reason = "linear per-picture decode sequence (slice header already parsed by caller -> \
                  DPB update -> ref-list build -> slot allocate -> upload -> GPU submit -> \
                  output) — splitting further would just move consecutive steps of the same \
                  picture's decode into a same-file helper, mirroring vulkan/decoder.rs's \
                  identical allow/reasoning for decode_slice_h264"
    )]
    fn decode_picture(
        &mut self,
        packet: &Packet,
        original_nal: &[u8],
        unit: &NalUnit,
        sps: &Sps,
        pps: &Pps,
        header: &SliceHeader,
    ) -> Result<(VideoFrame, Option<usize>), DecodeError> {
        let is_reference = unit.ref_idc != 0;

        // Step 2: an IDR picture clears the whole DPB and resets cross-picture POC state. May
        // fail loudly (`DpbError::SlotOutstanding`) if a still-undelivered `ZeroCopyGpu` frame
        // from an earlier `push_packet` call in this same drain cycle occupies a slot — the
        // caller must drain pending frames via `poll_frame` before pushing more packets in that
        // case (see ADR-0003 § Fd lifetime contract).
        if header.is_idr {
            self.pipeline
                .as_mut()
                .ok_or(DecodeError::Backend)?
                .dpb
                .clear_all()
                .map_err(|_| DecodeError::Backend)?;
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
        }

        // Step 3: FrameNumWrap is defined relative to *this* picture — refresh every occupied
        // slot before either sliding-window eviction (step 6) or RefPicList0 construction
        // (step 5) can use it.
        let max_frame_num = 1u32
            .checked_shl(sps.log2_max_frame_num_minus4 + 4)
            .ok_or(DecodeError::InvalidInput)?;
        self.pipeline
            .as_mut()
            .ok_or(DecodeError::Backend)?
            .dpb
            .refresh_frame_num_wraps(header.frame_num, max_frame_num);

        // Step 4: derive PicOrderCnt; only reference pictures perpetuate prev_poc state.
        let max_pic_order_cnt_lsb = 1u32
            .checked_shl(sps.log2_max_pic_order_cnt_lsb_minus4 + 4)
            .ok_or(DecodeError::InvalidInput)?;
        let poc_msb = derive_pic_order_cnt_msb(
            header.pic_order_cnt_lsb,
            self.prev_poc_msb,
            self.prev_poc_lsb,
            max_pic_order_cnt_lsb,
        );
        let pic_order_cnt = poc_msb
            .checked_add_unsigned(header.pic_order_cnt_lsb)
            .ok_or(DecodeError::InvalidInput)?;
        if is_reference {
            self.prev_poc_msb = poc_msb;
            self.prev_poc_lsb = header.pic_order_cnt_lsb;
        }

        // Step 5: resolve reference(s) from the DPB's current state *before* allocating the
        // destination slot — allocate_slot (step 6) may sliding-window-evict a reference slot as
        // a side effect, so the reference(s) this picture uses must be captured first. Safe
        // because the DPB is sized max_num_ref_frames + 1 (ensure_pipeline), guaranteeing a
        // genuinely free slot whenever every active reference is still occupied.
        let is_p_slice = header.slice_type == 0;
        let (reference_frames, ref_pic0, max_num_ref_frames) = {
            let pipeline = self.pipeline.as_ref().ok_or(DecodeError::Backend)?;
            // VA-API wants every occupied DPB slot here (not just the active RefPicList0),
            // matching FFmpeg's fill_vaapi_ReferenceFrames convention — see adr/linux/0002
            // § VA-API-specific plumbing.
            let reference_frames: Vec<(cros_libva::VASurfaceID, DpbSlot)> = pipeline
                .dpb
                .occupied_slots()
                .filter_map(|(index, slot)| {
                    pipeline.surfaces[index]
                        .as_ref()
                        .map(|surface| (surface.id(), *slot))
                })
                .collect();
            let ref_pic0 = if is_p_slice {
                let default_list = default_ref_pic_list0(&pipeline.dpb);
                let index = *default_list.first().ok_or(DecodeError::InvalidInput)?;
                let slot = *pipeline.dpb.slot(index).ok_or(DecodeError::Backend)?;
                let surface_id = pipeline.surfaces[index]
                    .as_ref()
                    .ok_or(DecodeError::Backend)?
                    .id();
                Some((surface_id, slot))
            } else {
                None
            };
            (reference_frames, ref_pic0, pipeline.max_num_ref_frames)
        };

        // Step 6: allocate the destination slot now that references are captured, take its
        // physical surface.
        let (dst_slot_index, surface) = self.take_surface_slot()?;

        // Step 7: build parameter buffers from the resolved reference(s) + destination surface
        // as CurrPic; VA-API call order is unchanged from the IDR-only path — only parameter
        // buffer *contents* change.
        let (returned_surface, result) = self.decode_one(
            surface,
            packet,
            original_nal,
            unit,
            sps,
            pps,
            header,
            pic_order_cnt,
            &reference_frames,
            ref_pic0,
            max_num_ref_frames,
        );
        if let Some(pipeline) = self.pipeline.as_mut() {
            pipeline.surfaces[dst_slot_index] = Some(returned_surface);
        }
        let output = result?;

        // Step 8: on success, register this slot as a reference if applicable. Must run before
        // step 9's `mark_outstanding` below — `Dpb::insert` itself refuses an outstanding slot,
        // and this slot cannot be outstanding yet (it was just freshly allocated).
        if is_reference {
            let frame_num_wrap = i32::try_from(header.frame_num).unwrap_or(0);
            self.pipeline
                .as_mut()
                .ok_or(DecodeError::Backend)?
                .dpb
                .insert(
                    dst_slot_index,
                    DpbSlot::new_reference(header.frame_num, frame_num_wrap, pic_order_cnt),
                )
                .map_err(|_| DecodeError::Backend)?;
        }

        // Step 9 (ADR-0003): a `ZeroCopyGpu` frame exports this slot's surface — keep the fd(s)
        // alive and mark the slot outstanding so it cannot be recycled (evicted or re-inserted
        // into) until this decoder itself releases it (see `release_previous_gpu_export`).
        let gpu_slot = if let Some(fds) = output.exported {
            let pipeline = self.pipeline.as_mut().ok_or(DecodeError::Backend)?;
            pipeline.exported_fds[dst_slot_index] = Some(fds);
            pipeline
                .dpb
                .mark_outstanding(dst_slot_index)
                .map_err(|_| DecodeError::Backend)?;
            Some(dst_slot_index)
        } else {
            None
        };

        Ok((output.frame, gpu_slot))
    }

    /// Build parameter buffers and run the VA-API decode sequence for one already-allocated
    /// surface. Takes `&self` (not `&mut self`) so the caller can hand the returned surface back
    /// into `self.pipeline` afterward without a double mutable borrow — mirrors
    /// `mediaway-encoder-linux`'s `encode_one` shape.
    #[allow(clippy::too_many_arguments)]
    fn decode_one(
        &self,
        surface: Surface<()>,
        packet: &Packet,
        original_nal: &[u8],
        unit: &NalUnit,
        sps: &Sps,
        pps: &Pps,
        header: &SliceHeader,
        pic_order_cnt: i32,
        reference_frames: &[(cros_libva::VASurfaceID, DpbSlot)],
        ref_pic0: Option<(cros_libva::VASurfaceID, DpbSlot)>,
        max_num_ref_frames: u32,
    ) -> (Surface<()>, Result<DecodedPicture, DecodeError>) {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return (surface, Err(DecodeError::Backend));
        };
        let context = Rc::clone(&pipeline.context);
        let coded_width = pipeline.coded_width;
        let coded_height = pipeline.coded_height;
        let nv12_format = pipeline.nv12_format;
        let surface_id = surface.id();
        let output_pref = self.output;

        let outcome =
            (|| -> Result<(VideoFrameStorage, Option<DmaBufFds>, Surface<()>), DecodeError> {
                let pic_param = build_pic_param(
                    sps,
                    pps,
                    unit,
                    header,
                    surface_id,
                    pic_order_cnt,
                    reference_frames,
                    max_num_ref_frames,
                )?;
                let iq_matrix = IQMatrixBufferH264::new([[16u8; 16]; 6], [[16u8; 64]; 2]);
                let slice_param = build_slice_param(header, original_nal.len(), ref_pic0)?;
                let slice_data_bytes = original_nal.to_vec();

                let pic_param_buf = context
                    .create_buffer(BufferType::PictureParameter(PictureParameter::H264(
                        pic_param,
                    )))
                    .map_err(|_| DecodeError::Backend)?;
                let iq_buf = context
                    .create_buffer(BufferType::IQMatrix(IQMatrix::H264(iq_matrix)))
                    .map_err(|_| DecodeError::Backend)?;
                let slice_param_buf = context
                    .create_buffer(BufferType::SliceParameter(SliceParameter::H264(
                        slice_param,
                    )))
                    .map_err(|_| DecodeError::Backend)?;
                let slice_data_buf = context
                    .create_buffer(BufferType::SliceData(slice_data_bytes))
                    .map_err(|_| DecodeError::Backend)?;

                let timestamp = u64::try_from(packet.pts).unwrap_or(0);
                let mut picture = Picture::new(timestamp, Rc::clone(&context), surface);
                picture.add_buffer(pic_param_buf);
                picture.add_buffer(iq_buf);
                picture.add_buffer(slice_param_buf);
                picture.add_buffer(slice_data_buf);

                let picture = picture.begin::<()>().map_err(|_| DecodeError::Backend)?;
                let picture = picture.render().map_err(|_| DecodeError::Backend)?;
                let picture = picture.end().map_err(|_| DecodeError::Backend)?;
                let picture = picture
                    .sync::<()>()
                    .map_err(|(_e, _pic)| DecodeError::Backend)?;

                match output_pref {
                    VideoOutputPreference::CpuFramesOk => {
                        let image = picture
                            .create_image::<()>(
                                nv12_format,
                                (coded_width, coded_height),
                                (coded_width, coded_height),
                            )
                            .map_err(|_| DecodeError::Backend)?;
                        let va_image = *image.image();
                        let bytes = copy_nv12_from_planes(
                            image.as_ref(),
                            coded_width,
                            coded_height,
                            va_image.pitches[0],
                            va_image.offsets[0],
                            va_image.pitches[1],
                            va_image.offsets[1],
                        );
                        drop(image);

                        let surface = picture.take_surface().map_err(|_| DecodeError::Backend)?;
                        Ok((VideoFrameStorage::Cpu { data: bytes }, None, surface))
                    }
                    VideoOutputPreference::ZeroCopyGpu => {
                        // No CPU readback on this path (ADR-0003) — `export_prime()` runs on the
                        // surface directly; the driver's decoded pixels are never copied out.
                        let surface = picture.take_surface().map_err(|_| DecodeError::Backend)?;
                        let (handle, fds) = dmabuf::build_handle(&surface)?;
                        Ok((VideoFrameStorage::Gpu(handle), Some(fds), surface))
                    }
                }
            })();

        match outcome {
            Ok((storage, exported, returned_surface)) => {
                let frame = VideoFrame {
                    pts: packet.pts,
                    duration: packet.duration,
                    width: coded_width,
                    height: coded_height,
                    format: PixelFormat::Nv12,
                    storage,
                };
                (returned_surface, Ok(DecodedPicture { frame, exported }))
            }
            Err(e) => (
                fresh_surface_or_placeholder(&context, coded_width, coded_height),
                Err(e),
            ),
        }
    }
}

/// One successfully decoded picture's output, before `decode_picture` decides whether to mark
/// its DPB slot outstanding (see ADR-0003 § Fd lifetime contract).
struct DecodedPicture {
    frame: VideoFrame,
    /// `Some` only for a [`VideoOutputPreference::ZeroCopyGpu`] frame — the fd(s) `decode_picture`
    /// must keep alive in `Pipeline::exported_fds` for as long as the handle stays valid.
    exported: Option<DmaBufFds>,
}

impl VideoDecoder for VaapiH264Decoder {
    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError> {
        self.release_previous_gpu_export();
        if packet.is_discard {
            return Ok(());
        }
        let nals = split_annex_b(packet.payload.as_ref()).map_err(|_| DecodeError::InvalidInput)?;
        for nal in nals {
            let unit = NalUnit::parse(nal).map_err(|_| DecodeError::InvalidInput)?;
            match unit.unit_type {
                NalUnitType::Sps => {
                    self.sps = Some(Sps::parse(unit.rbsp.as_ref())?);
                }
                NalUnitType::Pps => {
                    self.pps = Some(Pps::parse(unit.rbsp.as_ref())?);
                }
                NalUnitType::IdrSlice | NalUnitType::NonIdrSlice => {
                    let is_idr = matches!(unit.unit_type, NalUnitType::IdrSlice);
                    let sps = self.sps.ok_or(DecodeError::InvalidInput)?;
                    let pps = self.pps.ok_or(DecodeError::InvalidInput)?;
                    let header =
                        SliceHeader::parse(unit.rbsp.as_ref(), unit.ref_idc, is_idr, &sps, &pps)?;
                    if header.pic_parameter_set_id != pps.pic_parameter_set_id {
                        return Err(DecodeError::InvalidInput);
                    }
                    self.ensure_pipeline(sps)?;
                    let frame = self.decode_picture(packet, nal, &unit, &sps, &pps, &header)?;
                    self.pending.push_back(frame);
                }
                // SEI/AUD/end-of-sequence/-stream/filler are ignored.
                _ => {}
            }
        }
        Ok(())
    }

    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        self.release_previous_gpu_export();
        let Some((frame, gpu_slot)) = self.pending.pop_front() else {
            return Ok(None);
        };
        self.last_delivered_gpu_slot = gpu_slot;
        Ok(Some(frame))
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::similar_names,
    reason = "pic_init_qp_minus26 / pic_init_qs_minus26 are the ITU-T H.264 spec's own names; VAPictureParameterBufferH264::new takes every field positionally"
)]
fn build_pic_param(
    sps: &Sps,
    pps: &Pps,
    unit: &NalUnit,
    header: &SliceHeader,
    surface_id: cros_libva::VASurfaceID,
    pic_order_cnt: i32,
    reference_frames: &[(cros_libva::VASurfaceID, DpbSlot)],
    max_num_ref_frames: u32,
) -> Result<PictureParameterBufferH264, DecodeError> {
    let curr_pic = PictureH264::new(
        surface_id,
        header.frame_num,
        0,
        pic_order_cnt,
        pic_order_cnt,
    );
    let mut reference_frames_array: [PictureH264; 16] = std::array::from_fn(|_| invalid_picture());
    // reference_frames.len() is bounded by H264_MAX_DPB_SLOTS (16), same as this array, so this
    // never silently truncates a real reference.
    for (slot_entry, (ref_surface_id, dpb_slot)) in reference_frames_array
        .iter_mut()
        .zip(reference_frames.iter())
    {
        *slot_entry = PictureH264::new(
            *ref_surface_id,
            dpb_slot.frame_num,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            dpb_slot.pic_order_cnt,
            dpb_slot.pic_order_cnt,
        );
    }

    let seq_fields = H264SeqFields::new(
        1, // chroma_format_idc: 4:2:0 (implied by baseline/main profile_idc)
        0, // residual_colour_transform_flag: not present for baseline/main SPS
        u32::from(sps.gaps_in_frame_num_value_allowed_flag),
        1, // frame_mbs_only_flag: progressive only (enforced by Sps::parse)
        0, // mb_adaptive_frame_field_flag: unused, frame_mbs_only_flag == 1
        u32::from(sps.direct_8x8_inference_flag),
        0, // min_luma_bi_pred_size8x8: unused (no B slices this scope)
        sps.log2_max_frame_num_minus4,
        sps.pic_order_cnt_type,
        sps.log2_max_pic_order_cnt_lsb_minus4,
        0, // delta_pic_order_always_zero_flag: unused, pic_order_cnt_type == 0
    );
    let pic_fields = H264PicFields::new(
        u32::from(pps.entropy_coding_mode_flag),
        0, // weighted_pred_flag: rejected on P slices by SliceHeader::parse, irrelevant for I
        0, // weighted_bipred_idc: unused (no B slices this scope)
        0, // transform_8x8_mode_flag: rejected by Pps::parse when the PPS sets it
        0, // field_pic_flag: unused, frame_mbs_only_flag == 1
        u32::from(pps.constrained_intra_pred_flag),
        u32::from(pps.pic_order_present_flag),
        u32::from(pps.deblocking_filter_control_present_flag),
        u32::from(pps.redundant_pic_cnt_present_flag),
        u32::from(unit.ref_idc != 0),
    );

    let pic_width_in_mbs_minus1 =
        u16::try_from(sps.pic_width_in_mbs_minus1).map_err(|_| DecodeError::InvalidInput)?;
    let pic_height_in_mbs_minus1 =
        u16::try_from(sps.pic_height_in_map_units_minus1).map_err(|_| DecodeError::InvalidInput)?;
    let pic_init_qp_minus26 =
        i8::try_from(pps.pic_init_qp_minus26).map_err(|_| DecodeError::InvalidInput)?;
    let pic_init_qs_minus26 =
        i8::try_from(pps.pic_init_qs_minus26).map_err(|_| DecodeError::InvalidInput)?;
    let chroma_qp_index_offset =
        i8::try_from(pps.chroma_qp_index_offset).map_err(|_| DecodeError::InvalidInput)?;
    let second_chroma_qp_index_offset =
        i8::try_from(pps.second_chroma_qp_index_offset).map_err(|_| DecodeError::InvalidInput)?;
    let frame_num = u16::try_from(header.frame_num).map_err(|_| DecodeError::InvalidInput)?;
    let num_ref_frames = u8::try_from(max_num_ref_frames).map_err(|_| DecodeError::InvalidInput)?;

    Ok(PictureParameterBufferH264::new(
        curr_pic,
        reference_frames_array,
        pic_width_in_mbs_minus1,
        pic_height_in_mbs_minus1,
        0, // bit_depth_luma_minus8: 8-bit only (baseline/main)
        0, // bit_depth_chroma_minus8: 8-bit only (baseline/main)
        num_ref_frames,
        &seq_fields,
        // num_slice_groups_minus1 / slice_group_map_type / slice_group_change_rate_minus1:
        // FMO unused — Pps::parse already rejected num_slice_groups_minus1 > 0.
        0,
        0,
        0,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        second_chroma_qp_index_offset,
        &pic_fields,
        frame_num,
    ))
}

fn build_slice_param(
    header: &SliceHeader,
    nal_len: usize,
    ref_pic0: Option<(cros_libva::VASurfaceID, DpbSlot)>,
) -> Result<SliceParameterBufferH264, DecodeError> {
    let mut ref_pic_list_0: [PictureH264; 32] = std::array::from_fn(|_| invalid_picture());
    if let Some((surface_id, slot)) = ref_pic0 {
        ref_pic_list_0[0] = PictureH264::new(
            surface_id,
            slot.frame_num,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            slot.pic_order_cnt,
            slot.pic_order_cnt,
        );
    }
    let ref_pic_list_1: [PictureH264; 32] = std::array::from_fn(|_| invalid_picture());
    let slice_data_size = u32::try_from(nal_len).map_err(|_| DecodeError::InvalidInput)?;
    // "relative to and includes the NAL unit byte" (ITU-T H.264 VA-API buffer contract) — the
    // NAL header byte itself is 8 bits, then `header.bits_consumed` more (see
    // `SliceHeader::bits_consumed` doc comment).
    let slice_data_bit_offset =
        u16::try_from(8usize + header.bits_consumed).map_err(|_| DecodeError::InvalidInput)?;
    let first_mb_in_slice =
        u16::try_from(header.first_mb_in_slice).map_err(|_| DecodeError::InvalidInput)?;
    let slice_qp_delta =
        i8::try_from(header.slice_qp_delta).map_err(|_| DecodeError::InvalidInput)?;
    let slice_alpha_c0_offset_div2 =
        i8::try_from(header.slice_alpha_c0_offset_div2).map_err(|_| DecodeError::InvalidInput)?;
    let slice_beta_offset_div2 =
        i8::try_from(header.slice_beta_offset_div2).map_err(|_| DecodeError::InvalidInput)?;

    Ok(SliceParameterBufferH264::new(
        slice_data_size,
        0, // slice_data_offset: the SliceData buffer *is* this NAL, offset 0
        0, // slice_data_flag = VA_SLICE_DATA_FLAG_ALL: whole slice is in the buffer
        slice_data_bit_offset,
        first_mb_in_slice,
        header.slice_type,
        0, // direct_spatial_mv_pred_flag: unused (no B slices this scope)
        // num_ref_idx_l0_active_minus1: always 0 — num_ref_idx_l0_active is exactly 1 for P
        // slices (enforced by SliceHeader::parse), 0/unused for I slices.
        0,
        0, // num_ref_idx_l1_active_minus1: unused (no B slices this scope)
        0, // cabac_init_idc: unused (CABAC P-slices rejected by SliceHeader::parse)
        slice_qp_delta,
        header.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        ref_pic_list_0,
        ref_pic_list_1,
        0, // luma_log2_weight_denom: unused (weighted prediction rejected by SliceHeader::parse)
        0, // chroma_log2_weight_denom: unused
        0,
        [0i16; 32],
        [0i16; 32],
        0,
        [[0i16; 2]; 32],
        [[0i16; 2]; 32],
        0,
        [0i16; 32],
        [0i16; 32],
        0,
        [[0i16; 2]; 32],
        [[0i16; 2]; 32],
    ))
}

/// An unused `VAPictureH264` DPB / reference-list slot.
fn invalid_picture() -> PictureH264 {
    PictureH264::new(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
}

/// After a `Picture` consumes its surface, this crate cannot recover the exact same
/// [`Surface`] object on any of `decode_one`'s error paths (the typestate API only returns it
/// via [`Picture::take_surface`] on the success path) — mirrors
/// `mediaway-encoder-linux`'s identical `fresh_surface_or_placeholder` rationale.
fn fresh_surface_or_placeholder(
    context: &Rc<Context>,
    coded_width: u32,
    coded_height: u32,
) -> Surface<()> {
    let display = context.display();
    display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            coded_width,
            coded_height,
            None,
            vec![()],
        )
        .ok()
        .and_then(|mut v| v.pop())
        .unwrap_or_else(|| placeholder_surface(display, coded_width, coded_height))
}

/// Last-resort placeholder when even a minimal surface allocation fails (session is already
/// unusable at that point; subsequent `push_packet` calls will fail at `create_buffer`/
/// `begin` anyway). Keeping the pool slot `Some(_)` avoids special-casing an exhausted slot
/// everywhere else in this module.
fn placeholder_surface(display: &Rc<Display>, coded_width: u32, coded_height: u32) -> Surface<()> {
    #[allow(
        clippy::expect_used,
        reason = "session is already unrecoverable by this point; see caller doc comment"
    )]
    display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            coded_width,
            coded_height,
            None,
            vec![()],
        )
        .expect("VA-API display known-good at session open time")
        .pop()
        .expect("exactly one surface requested")
}

/// Best-effort seed of SPS/PPS from `extra_data` at `open()` time — parse
/// failures here are not fatal since in-band SPS/PPS from `push_packet` can seed them instead.
fn seed_params(extra_data: &Bytes) -> (Option<Sps>, Option<Pps>) {
    if extra_data.is_empty() {
        return (None, None);
    }
    let Ok(nals) = split_annex_b(extra_data.as_ref()) else {
        return (None, None);
    };
    let mut sps = None;
    let mut pps = None;
    for nal in nals {
        let Ok(unit) = NalUnit::parse(nal) else {
            continue;
        };
        match unit.unit_type {
            NalUnitType::Sps => {
                if let Ok(s) = Sps::parse(unit.rbsp.as_ref()) {
                    sps = Some(s);
                }
            }
            NalUnitType::Pps => {
                if let Ok(p) = Pps::parse(unit.rbsp.as_ref()) {
                    pps = Some(p);
                }
            }
            _ => {}
        }
    }
    (sps, pps)
}

fn validate(config: &VideoDecoderConfig) -> Result<(), DecodeError> {
    // Reject configurations intended for another codec before allocating resources.
    if config.codec != mediaway_common::CodecKind::H264 {
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

const fn round_up_16(value: u32) -> u32 {
    value.div_ceil(16) * 16
}
