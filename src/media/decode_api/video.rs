//! Video decode config and [`VideoDecoder`] trait.

#![forbid(unsafe_code)]

use crate::media::decode_api::error::DecodeError;
use mediaway_common::{CodecKind, GpuDeviceHandle, Packet, PixelFormat, Rational};

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
    /// specifies the D3D11 device that owns returned textures.
    pub gpu_device: Option<GpuDeviceHandle>,
    /// Annex-B codec parameter sets; may be empty until the first keyframe.
    pub extra_data: mediaway_common::Bytes,
}

/// Streaming hardware (or backend) video decoder.
///
/// Submit packets; Windows owning output is drained via `poll_owned_frame`.
pub trait VideoDecoder {
    /// Install the shared cancellation state for Windows decode work.
    fn set_notification(&mut self, notification: DecoderNotification) {
        let _ = notification;
    }

    /// Submit one compressed packet. May produce zero or more frames (drain via poll).
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] when the packet is rejected or the session failed.
    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError>;
}

/// Shared decoder cancellation, independent of an executor.
#[derive(Clone)]
pub struct DecoderNotification {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DecoderNotification {
    pub fn new(cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { cancelled }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn cancellation(&self) -> &std::sync::atomic::AtomicBool {
        &self.cancelled
    }
}

impl Default for DecoderNotification {
    fn default() -> Self {
        Self::new(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )))
    }
}
