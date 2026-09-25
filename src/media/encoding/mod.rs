//! Sending formats are negotiated independently from the SDP codec family.
use crate::media::video_color::{HdrMetadata, VideoColorSpace};
use crate::protocol::capability::CodecCapability;

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

pub(crate) struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_100ns: i64,
    pub is_new: bool,
    pub timing: Option<FrameTiming>,
    pub format: Format,
    pub color: crate::media::video_color::VideoColorSpace,
}
#[derive(Clone, Copy)]
pub(crate) struct FrameTiming {
    pub captured: std::time::Instant,
    pub encode_started: std::time::Instant,
    pub encode_finished: std::time::Instant,
}

pub(crate) mod rate;
pub(crate) mod software;
