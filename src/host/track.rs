//! One ordinary video SSRC, with only SDP-negotiated codec payload types.
use super::{format::Codec, lock};
use async_trait::async_trait;
use bytes::Bytes;
use std::{
    any::Any,
    sync::{Arc, Mutex},
};
use webrtc::{
    Error,
    rtp::packet::Packet,
    rtp_transceiver::rtp_codec::{RTCRtpCodecParameters, RTPCodecType},
    track::track_local::{TrackLocal, TrackLocalContext},
};

pub(crate) struct VideoTrack {
    preferred: Codec,
    negotiated: Arc<super::format::Negotiated>,
    binding: Mutex<Option<TrackLocalContext>>,
}
impl VideoTrack {
    pub fn new(preferred: Codec, negotiated: Arc<super::format::Negotiated>) -> Self {
        Self {
            preferred,
            negotiated,
            binding: Mutex::new(None),
        }
    }
    pub fn mid_len(&self) -> usize {
        lock(&self.binding)
            .as_ref()
            .and_then(|b| b.mid())
            .map_or(0, |v| v.len())
    }
    pub async fn write(&self, codec: Codec, packet: &Packet) -> webrtc::error::Result<usize> {
        let Some(binding) = lock(&self.binding).clone() else {
            return Ok(0);
        };
        let payload = binding
            .codec_parameters()
            .iter()
            .find(|p| p.capability.mime_type.eq_ignore_ascii_case(codec.mime()))
            .ok_or(Error::ErrUnsupportedCodec)?
            .payload_type;
        let mut packet = packet.clone();
        packet.header.ssrc = binding.ssrc();
        packet.header.payload_type = payload;
        if let Some(mid) = binding.mid() {
            if let Some(ext) = binding
                .header_extensions()
                .iter()
                .find(|e| e.uri == "urn:ietf:params:rtp-hdrext:sdes:mid")
            {
                packet
                    .header
                    .set_extension(ext.id as u8, Bytes::copy_from_slice(mid.as_bytes()))?;
            }
        }
        binding.write_stream().write_rtp(&packet).await
    }
}
#[async_trait]
impl TrackLocal for VideoTrack {
    async fn bind(
        &self,
        context: &TrackLocalContext,
    ) -> webrtc::error::Result<RTCRtpCodecParameters> {
        let selected = context
            .codec_parameters()
            .iter()
            .find(|c| {
                c.capability
                    .mime_type
                    .eq_ignore_ascii_case(self.preferred.mime())
            })
            .or_else(|| {
                context.codec_parameters().iter().find(|c| {
                    [Codec::H265, Codec::H264].iter().any(|codec| {
                        self.negotiated.supports_codec(*codec)
                            && c.capability.mime_type.eq_ignore_ascii_case(codec.mime())
                    })
                })
            })
            .cloned()
            .ok_or(Error::ErrUnsupportedCodec)?;
        self.negotiated.bind_codecs(context.codec_parameters());
        *lock(&self.binding) = Some(context.clone());
        Ok(selected)
    }
    async fn unbind(&self, context: &TrackLocalContext) -> webrtc::error::Result<()> {
        let mut binding = lock(&self.binding);
        if binding.as_ref().is_some_and(|b| b.id() == context.id()) {
            *binding = None;
            Ok(())
        } else {
            Err(Error::ErrUnbindFailed)
        }
    }
    fn id(&self) -> &str {
        "video_0"
    }
    fn rid(&self) -> Option<&str> {
        None
    }
    fn stream_id(&self) -> &str {
        "video_0"
    }
    fn kind(&self) -> RTPCodecType {
        RTPCodecType::Video
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
