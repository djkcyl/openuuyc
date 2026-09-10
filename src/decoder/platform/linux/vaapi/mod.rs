//! H.264/HEVC VA-API session dispatch.

mod codec;
mod dmabuf;
mod dpb;
mod h264;
mod hevc;
mod hevc_dpb;
mod hevc_nal;
mod hevc_pps;
mod hevc_slice;
mod hevc_sps;
mod nv12;
mod pps;
mod slice;
mod sps;

use crate::decoder::platform::{DecodeError, VideoDecoder, VideoDecoderConfig};
use mediaway_common::{CodecKind, Packet, VideoFrame};

use h264::VaapiH264Decoder;
use hevc::VaapiHevcDecoder;

/// Dispatches to the right per-codec VA-API decode session based on
/// [`VideoDecoderConfig::codec`] — a plain enum over this crate's concrete decoder types
/// rather than `Box<dyn VideoDecoder>` (see `docs/spec/zero-cost-abstractions.md`).
pub(crate) enum VaapiVideoDecoder {
    H264(VaapiH264Decoder),
    Hevc(VaapiHevcDecoder),
}

impl VaapiVideoDecoder {
    /// Open the per-codec VA-API decoder matching `config.codec`.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::Unsupported`] for any codec this vaapi backend does not decode at
    /// all, or the chosen decoder's own `open` errors otherwise.
    pub(crate) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        match config.codec {
            CodecKind::H264 => Ok(Self::H264(VaapiH264Decoder::open(config)?)),
            CodecKind::Hevc => Ok(Self::Hevc(VaapiHevcDecoder::open(config)?)),
            _ => Err(DecodeError::Unsupported),
        }
    }
}

impl VideoDecoder for VaapiVideoDecoder {
    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError> {
        match self {
            Self::H264(d) => d.push_packet(packet),
            Self::Hevc(d) => d.push_packet(packet),
        }
    }

    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        match self {
            Self::H264(d) => d.poll_frame(),
            Self::Hevc(d) => d.poll_frame(),
        }
    }
}
