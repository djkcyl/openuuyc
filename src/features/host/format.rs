//! Publisher negotiation and format selection policy.
pub(crate) use crate::media::encoding::{Backend, Capability, Codec, Format, Rate};
use crate::media::video_color::VideoColorSpace;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub capability: Capability,
    pub maximum: (u32, u32),
    format_maximum: (u32, u32),
    pub fps: u32,
}
impl Choice {
    pub fn maximum_for(&self, requested: Option<(u32, u32)>) -> (u32, u32) {
        // S543710: a complete RPC pair replaces the initial PB decoder size.
        // It cannot enlarge the real encoder/JSON format capability.
        requested.map_or(self.maximum, |size| {
            (
                size.0.min(self.format_maximum.0),
                size.1.min(self.format_maximum.1),
            )
        })
    }
}
#[derive(Debug)]
pub(crate) struct Negotiated {
    pub choices: Vec<Choice>,
    dual: crate::protocol::capability::DualCapability,
    codecs: AtomicU8,
    encoder_true_color: bool,
}
impl Negotiated {
    pub(crate) fn for_track(&self) -> Self {
        Self {
            choices: self.choices.clone(),
            dual: self.dual.clone(),
            codecs: AtomicU8::new(3),
            encoder_true_color: self.encoder_true_color,
        }
    }
    pub fn new(
        local: &[Capability],
        remote: &crate::protocol::capability::DeviceCapability,
        decoders: &[crate::features::stream_control::publisher::DecoderCapability],
    ) -> anyhow::Result<Self> {
        let local_wire = crate::protocol::capability::DeviceCapability {
            video_codec_capability: local.iter().map(Capability::wire).collect(),
            ..Default::default()
        };
        let dual =
            crate::protocol::capability::DualCapability::negotiate(&local_wire, remote.clone());
        let mut choices = Vec::new();
        for capability in local {
            let Some(row) = dual
                .exact(
                    capability.format.codec.wire(),
                    capability.format.chroma,
                    capability.format.hdr(),
                )
                .filter(|r| r.result == 0)
            else {
                continue;
            };
            for peer in &remote.video_codec_capability {
                if peer.video_codec != capability.format.codec.wire()
                    || peer.chroma_sampling != capability.format.chroma
                    || peer.bit_depth != capability.format.depth
                    || peer.width < 2
                    || peer.height < 2
                {
                    continue;
                }
                for decoder in decoders {
                    if decoder.codec != peer.video_codec
                        || decoder.chroma != i32::from(peer.chroma_sampling)
                        || decoder.fps <= 0
                        || decoder.width < 2
                        || decoder.height < 2
                    {
                        continue;
                    }
                    choices.push(Choice {
                        capability: capability.clone(),
                        format_maximum: (
                            capability
                                .maximum
                                .0
                                .min(peer.width as u32)
                                .min(row.max_width as u32),
                            capability
                                .maximum
                                .1
                                .min(peer.height as u32)
                                .min(row.max_height as u32),
                        ),
                        maximum: (
                            capability
                                .maximum
                                .0
                                .min(peer.width as u32)
                                .min(decoder.width as u32)
                                .min(row.max_width as u32),
                            capability
                                .maximum
                                .1
                                .min(peer.height as u32)
                                .min(decoder.height as u32)
                                .min(row.max_height as u32),
                        ),
                        fps: (decoder.fps as u32).min(if capability.backend == Backend::Software {
                            crate::media::encoding::software::MAX_FPS
                        } else {
                            144
                        }),
                    });
                }
            }
        }
        anyhow::ensure!(!choices.is_empty(), "没有共同的画面编码/解码能力");
        Ok(Self {
            choices,
            dual,
            codecs: AtomicU8::new(3),
            // S543170 computes this from local encoder caps before intersection.
            encoder_true_color: local.iter().any(|c| c.format.chroma == 3),
        })
    }
    pub fn encoder_true_color(&self) -> bool {
        self.encoder_true_color
    }
    pub fn bind_codecs(
        &self,
        codecs: &[webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters],
    ) {
        let bits = codecs.iter().fold(0, |bits, codec| {
            bits | match codec.capability.mime_type.to_ascii_lowercase().as_str() {
                "video/h264" => 1,
                "video/h265" => 2,
                _ => 0,
            }
        });
        self.codecs.store(bits, Ordering::Release);
    }
    pub fn permits_codec(&self, codec: Codec) -> bool {
        self.codecs.load(Ordering::Acquire) & if codec == Codec::H264 { 1 } else { 2 } != 0
    }
    pub fn maximum_quality(&self, format: Format, source: (u32, u32), maximum: (u32, u32)) -> i32 {
        let json_limit = self
            .dual
            .exact(format.codec.wire(), format.chroma, format.hdr())
            .filter(|r| r.result == 0)
            .map_or(1, |r| r.max_frame_quality);
        (1..=json_limit)
            .filter(|&q| {
                let tier = super::parameters::dimensions(q);
                source.0.min(tier.0) <= maximum.0 && source.1.min(tier.1) <= maximum.1
            })
            .max()
            .unwrap_or(1)
    }
    pub fn apply(
        &self,
        config: &mut super::VideoConfig,
        codec: Option<Codec>,
        chroma: u8,
        hdr: bool,
        source: (u32, u32),
    ) -> anyhow::Result<()> {
        let mut dual = self.dual.clone();
        dual.frame_quality_capability.retain(|row| {
            Codec::from_wire(row.video_codec)
                .is_some_and(|c| self.permits_codec(c) && codec.is_none_or(|wanted| wanted == c))
                && self.choices.iter().any(|choice| {
                    choice.capability.format.codec.wire() == row.video_codec
                        && choice.capability.format.chroma == row.chroma_sampling
                        && choice.capability.format.depth == row.bit_depth
                        && choice.fps >= 30
                })
        });
        let quality = match config.quality {
            5 => 0,
            6 => 5,
            q => q,
        };
        let mut formats = vec![(chroma, hdr)];
        if chroma == 3 {
            formats.push((1, hdr));
        }
        if hdr {
            formats.push((chroma, false));
            if chroma == 3 {
                formats.push((1, false));
            }
        }
        let mut selected = None;
        for (chroma, hdr) in formats {
            let row = dual.select(chroma, hdr, quality);
            if row.result != 0 {
                continue;
            }
            let Some(codec) = Codec::from_wire(row.video_codec) else {
                continue;
            };
            let format = Format {
                codec,
                chroma,
                depth: if hdr { 10 } else { 8 },
            };
            let mut fps = config.requested_fps;
            while fps >= 30 {
                if let Some(choice) = self
                    .choices
                    .iter()
                    .filter(|c| c.capability.format == format && c.fps >= fps)
                    .max_by_key(|c| {
                        let maximum = c.maximum_for(config.requested_maximum);
                        (
                            maximum.0.max(maximum.1),
                            c.capability.backend != Backend::Software,
                        )
                    })
                {
                    selected = Some((choice, fps));
                    break;
                }
                fps = match fps {
                    144.. => 90,
                    90..=143 => 60,
                    60..=89 => 30,
                    _ => 0,
                };
            }
            if selected.is_some() {
                break;
            }
        }
        let (choice, fps) =
            selected.ok_or_else(|| anyhow::anyhow!("请求的画面格式没有共同能力"))?;
        config.format = choice.capability.format;
        config.maximum = choice.maximum_for(config.requested_maximum);
        config.maximum_fps = fps;
        config.fps = fps.min(config.fps_limit);
        config.maximum_quality = self.maximum_quality(config.format, source, config.maximum);
        config.auto_quality = config.auto_quality.min(config.maximum_quality);
        if !matches!(config.quality, 5 | 6) {
            config.quality = config.quality.min(config.maximum_quality);
        }
        Ok(())
    }
    pub fn supports_codec(&self, codec: Codec) -> bool {
        self.choices
            .iter()
            .any(|c| c.capability.format.codec == codec)
    }
    pub fn apply_source(
        &self,
        config: &mut super::VideoConfig,
        source: (u32, u32),
        hdr_available: bool,
    ) -> anyhow::Result<()> {
        if config.format.hdr() && !hdr_available {
            let chroma = config.format.chroma;
            self.apply(config, None, chroma, false, source)?;
        }
        Ok(())
    }
}

pub(crate) fn color_extension(color: VideoColorSpace) -> Vec<u8> {
    let mut bytes = vec![
        color.primaries,
        color.transfer,
        color.matrix,
        color.range << 4,
    ];
    if let Some(metadata) = color.hdr_metadata {
        for value in [metadata.max_luminance, metadata.min_luminance]
            .into_iter()
            .chain(metadata.chromaticity)
            .chain([
                metadata.max_content_light_level,
                metadata.max_frame_average_light_level,
            ])
        {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
    }
    bytes
}
