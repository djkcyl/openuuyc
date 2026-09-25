//! Sending formats are negotiated independently from the SDP codec family.
use crate::{
    capability::CodecCapability,
    video_color::{HdrMetadata, VideoColorSpace},
};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Codec {
    H264,
    H265,
}
impl Codec {
    pub fn media(self) -> crate::media::VideoCodec {
        match self {
            Self::H264 => crate::media::VideoCodec::H264,
            Self::H265 => crate::media::VideoCodec::H265,
        }
    }
    pub fn wire(self) -> i32 {
        match self {
            Self::H264 => 1,
            Self::H265 => 2,
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            Self::H264 => "video/H264",
            Self::H265 => "video/H265",
        }
    }
    pub fn from_wire(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::H264),
            2 => Some(Self::H265),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Format {
    pub codec: Codec,
    pub chroma: u8,
    pub depth: u8,
}
impl Format {
    pub const AVC: Self = Self {
        codec: Codec::H264,
        chroma: 1,
        depth: 8,
    };
    pub fn valid(self) -> bool {
        matches!(self.chroma, 1 | 3) && matches!(self.depth, 8 | 10)
    }
    pub fn hdr(self) -> bool {
        self.depth == 10
    }
    pub fn color(self, metadata: Option<HdrMetadata>) -> VideoColorSpace {
        if self.hdr() {
            VideoColorSpace {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                range: 2,
                hdr_metadata: metadata,
            }
        } else {
            VideoColorSpace {
                primaries: 6,
                transfer: 4,
                matrix: 6,
                range: 1,
                hdr_metadata: None,
            }
        }
    }
    pub fn capability(self, implementation: i32, maximum: (u32, u32)) -> CodecCapability {
        CodecCapability {
            video_codec: self.codec.wire(),
            width: maximum.0 as i32,
            height: maximum.1 as i32,
            chroma_sampling: self.chroma,
            bit_depth: self.depth,
            codec_impl: implementation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Backend {
    Nvidia,
    Amd,
    Intel,
    Software,
}
impl Backend {
    pub fn implementation(self) -> i32 {
        match self {
            Self::Nvidia => 0,
            Self::Amd => 1,
            Self::Intel => 2,
            Self::Software => 5,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Nvidia => "NVIDIA NVENC",
            Self::Amd => "AMD AMF",
            Self::Intel => "INTEL QSV",
            Self::Software => "OpenUUYC H264",
        }
    }
    // ReportQosStats carries a classification, not sender_para_info's name.
    pub fn qos_type(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Nvidia | Self::Amd | Self::Intel => "hardware",
        }
    }
    pub fn accepts(self, format: Format) -> bool {
        if !format.valid() {
            return false;
        }
        match self {
            // T C4C300 rejects Y410, and the current AVC configuration has no
            // 10-bit producer. A generic D3D converter is not encoder support.
            Self::Nvidia => {
                !(format.depth == 10 && (format.chroma == 3 || format.codec == Codec::H264))
            }
            Self::Software => format == Format::AVC,
            Self::Amd | Self::Intel => true,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Capability {
    pub adapter: u64,
    pub backend: Backend,
    pub format: Format,
    pub maximum: (u32, u32),
}
impl Capability {
    pub fn wire(&self) -> CodecCapability {
        self.format
            .capability(self.backend.implementation(), self.maximum)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rate {
    pub target: u32,
    pub peak: u32,
    pub fps: u32,
    pub quality: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct Choice {
    pub capability: Capability,
    pub maximum: (u32, u32),
    pub fps: u32,
}
#[derive(Debug)]
pub(crate) struct Negotiated {
    pub choices: Vec<Choice>,
    dual: crate::capability::DualCapability,
    codecs: AtomicU8,
}
impl Negotiated {
    pub fn new(
        local: &[Capability],
        remote: &crate::capability::DeviceCapability,
        decoders: &[crate::stream_control::publisher::DecoderCapability],
    ) -> anyhow::Result<Self> {
        let local_wire = crate::capability::DeviceCapability {
            video_codec_capability: local.iter().map(Capability::wire).collect(),
            ..Default::default()
        };
        let dual = crate::capability::DualCapability::negotiate(&local_wire, remote.clone());
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
                            super::rust_h264::MAX_FPS
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
        })
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
                        (
                            c.maximum.0.max(c.maximum.1),
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
        config.maximum = choice.maximum;
        config.maximum_fps = fps;
        config.fps = fps.min(config.fps_limit);
        config.maximum_quality = self.maximum_quality(config.format, source, choice.maximum);
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
