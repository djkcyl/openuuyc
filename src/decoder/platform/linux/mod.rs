//! Linux H.264/HEVC backend. Built only on its target platform.
//! Platform playback remains subject to the validation limits in docs/status.md.

#![forbid(unsafe_code)]

use crate::decoder::platform::{DecodeError, VideoDecoder, VideoDecoderConfig};
use mediaway_common::{Packet, VideoFrame};

mod vaapi;

pub struct LinuxVideoDecoder {
    inner: vaapi::VaapiVideoDecoder,
}

impl LinuxVideoDecoder {
    pub fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        Ok(Self {
            inner: vaapi::VaapiVideoDecoder::open(config)?,
        })
    }
}

impl VideoDecoder for LinuxVideoDecoder {
    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError> {
        self.inner.push_packet(packet)
    }
    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        self.inner.poll_frame()
    }
}
