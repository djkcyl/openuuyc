//! Video decode config and [`VideoDecoder`] trait.

#![forbid(unsafe_code)]

use crate::decoder::platform::error::DecodeError;
use mediaway_common::{CodecKind, GpuDeviceHandle, Packet, PixelFormat, Rational, VideoFrame};

/// How the caller prefers to receive decoded frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum VideoOutputPreference {
    /// Prefer GPU handles ([`mediaway_common::VideoFrameStorage::Gpu`]).
    #[default]
    ZeroCopyGpu,
    /// Accept CPU frames (may imply copy/readback — backends must document cost).
    CpuFramesOk,
}

/// Parameters for opening a video decoder session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoDecoderConfig {
    /// Input codec (H.264 or HEVC).
    pub codec: CodecKind,
    /// Expected width (may be refined from bitstream).
    pub width: u32,
    /// Expected height (may be refined from bitstream).
    pub height: u32,
    /// Timestamp timebase for input packets and output frames.
    pub time_base: Rational,
    /// Preferred output pixel format when the backend converts.
    pub pixel_format: PixelFormat,
    /// Output path preference (Zero-Copy vs CPU).
    pub output: VideoOutputPreference,
    /// GPU device handle when [`VideoOutputPreference::ZeroCopyGpu`].
    ///
    /// `None` means unset (Zero-Copy open fails). `Some(GpuDeviceHandle::DirectX11(handle))`
    /// specifies the device that owns returned textures; other variants select other backends
    /// (see [`GpuDeviceHandle`](mediaway_common::GpuDeviceHandle) for platform options).
    pub gpu_device: Option<GpuDeviceHandle>,
    /// Codec configuration bytes (AVCC / extradata); may be empty until first keyframe.
    pub extra_data: mediaway_common::Bytes,
}

/// Streaming hardware (or backend) video decoder.
///
/// Push packets, then [`poll_frame`](VideoDecoder::poll_frame) until `Ok(None)`,
/// then continue receiving complete access units.
pub trait VideoDecoder {
    /// Notify a dedicated consumer thread when asynchronous output or an input
    /// credit is available. Synchronous backends need no notification source.
    fn set_notification(&mut self, notification: DecoderNotification) {
        let _ = notification;
    }

    /// Submit one compressed packet. May produce zero or more frames (drain via poll).
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] when the packet is rejected or the session failed.
    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError>;

    /// Pull the next decoded frame, if any.
    ///
    /// For GPU frames, the texture remains valid until the next
    /// [`push_packet`](Self::push_packet) / [`poll_frame`](Self::poll_frame) that recycles the surface (see platform ADR).
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] on backend failure.
    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError>;

    /// Asynchronous adapters preserve per-input drops/errors as output events;
    /// callers can reject events from a retired decode generation.
    fn poll_output(&mut self) -> Result<Option<VideoDecoderOutput>, DecodeError> {
        self.poll_frame().map(|frame| {
            frame.map(|frame| VideoDecoderOutput::Frame {
                frame,
                ready_at: std::time::Instant::now(),
            })
        })
    }
}

pub enum VideoDecoderOutput {
    Frame {
        frame: VideoFrame,
        ready_at: std::time::Instant,
    },
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Dropped { pts: i64 },
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Failed {
        pts: Option<i64>,
        error: DecodeError,
    },
}

/// Owned decoder-consumer wakeup and cancellation, independent of an executor.
#[derive(Clone)]
pub struct DecoderNotification {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    consumer: std::thread::Thread,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DecoderNotification {
    pub fn new(
        _consumer: std::thread::Thread,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            consumer: _consumer,
            cancelled,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub fn notify(&self) {
        self.consumer.unpark();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(windows)]
    pub(crate) fn cancellation(&self) -> &std::sync::atomic::AtomicBool {
        &self.cancelled
    }
}

impl Default for DecoderNotification {
    fn default() -> Self {
        Self::new(
            std::thread::current(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }
}
