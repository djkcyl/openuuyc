//! Apple H.264/HEVC backend. Built only on its target platform.
//! Platform playback remains subject to the validation limits in docs/status.md.

use crate::decoder::platform::{DecodeError, VideoDecoder, VideoDecoderConfig};
use mediaway_common::{Packet, VideoFrame};

mod videotoolbox;

pub struct AppleVideoDecoder {
    inner: videotoolbox::VideoToolboxVideoDecoder,
}

impl AppleVideoDecoder {
    pub fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        Ok(Self {
            inner: videotoolbox::VideoToolboxVideoDecoder::open(config)?,
        })
    }
    pub fn reset_for_keyframe(&mut self, hard: bool) -> Result<(), DecodeError> {
        self.inner.reset_for_keyframe(hard)
    }
}

impl VideoDecoder for AppleVideoDecoder {
    fn set_notification(&mut self, notification: crate::decoder::platform::DecoderNotification) {
        self.inner.set_notification(notification);
    }
    fn poll_output(
        &mut self,
    ) -> Result<Option<crate::decoder::platform::VideoDecoderOutput>, DecodeError> {
        self.inner.poll_output()
    }

    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError> {
        self.inner.push_packet(packet)
    }
    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        self.inner.poll_frame()
    }
}
