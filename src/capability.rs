//! UU device_capability intersection and ordinary-viewer selection.
//!
//! streamer 9AEA90/9ABEF0 and GameViewer 16293C0/16297F0, 4.38.3.9325.
//! Capability quality numbers are NOT CaptureSetting protobuf quality numbers.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct CodecCapability {
    pub video_codec: i32,
    pub width: i32,
    pub height: i32,
    pub chroma_sampling: u8,
    pub bit_depth: u8,
    pub codec_impl: i32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct DisplayCapability {
    pub id: i32,
    pub fps: u32,
    #[serde(rename = "type")]
    pub kind: i32,
    pub hdr: i32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct DeviceCapability {
    pub ice_id: String,
    pub display_info: Vec<DisplayCapability>,
    pub video_codec_capability: Vec<CodecCapability>,
}

pub(crate) const QUALITY_DIMENSIONS: [(i32, i32); 4] =
    [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct FrameQualityCapability {
    pub video_codec: i32,
    pub chroma_sampling: u8,
    pub bit_depth: u8,
    pub max_width: i32,
    pub max_height: i32,
    pub max_frame_quality: i32,
    pub result: i32,
}

impl FrameQualityCapability {
    fn valid(self) -> bool {
        self.has_dimensions() && self.result == 0
    }

    fn has_dimensions(self) -> bool {
        self.max_width != 0 && self.max_height != 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct DualCapability {
    pub remote_display_info: Vec<DisplayCapability>,
    pub local_display_info: Vec<DisplayCapability>,
    pub frame_quality_capability: Vec<FrameQualityCapability>,
}

impl DualCapability {
    pub(crate) fn negotiate(local: &DeviceCapability, remote: DeviceCapability) -> Self {
        let mut rows = Vec::with_capacity(8);
        for video_codec in [1, 2] {
            for chroma_sampling in [1, 3] {
                for bit_depth in [8, 10] {
                    let maximum = |caps: &[CodecCapability]| {
                        caps.iter()
                            .filter(|cap| {
                                (cap.video_codec, cap.chroma_sampling, cap.bit_depth)
                                    == (video_codec, chroma_sampling, bit_depth)
                            })
                            .map(|cap| (cap.width, cap.height))
                            .max()
                    };
                    let encoder = maximum(&remote.video_codec_capability);
                    let decoder = maximum(&local.video_codec_capability);
                    let (quality, result) = match (encoder, decoder) {
                        (Some(encoder), Some(decoder)) => {
                            let limit = encoder.min(decoder);
                            match QUALITY_DIMENSIONS.iter().rposition(|size| *size <= limit) {
                                Some(index) => (index as i32 + 1, 0),
                                None => (-1, -3),
                            }
                        }
                        (Some(_), None) => (-1, -1),
                        (None, Some(_)) => (-1, -2),
                        (None, None) => (-1, -3),
                    };
                    // 9ABEF0 emits the nominal tier, not the raw intersection.
                    // Failed rows have dimensions too; result must be checked.
                    let (max_width, max_height) = if quality > 0 {
                        QUALITY_DIMENSIONS[(quality - 1) as usize]
                    } else {
                        QUALITY_DIMENSIONS[1]
                    };
                    rows.push(FrameQualityCapability {
                        video_codec,
                        chroma_sampling,
                        bit_depth,
                        max_width,
                        max_height,
                        max_frame_quality: quality,
                        result,
                    });
                }
            }
        }
        Self {
            remote_display_info: remote.display_info,
            local_display_info: local.display_info.clone(),
            frame_quality_capability: rows,
        }
    }

    pub(crate) fn exact(
        &self,
        codec: i32,
        chroma: u8,
        hdr: bool,
    ) -> Option<FrameQualityCapability> {
        self.frame_quality_capability
            .iter()
            .rev()
            .copied()
            .find(|row| {
                row.video_codec == codec
                    && row.chroma_sampling == chroma
                    && (row.bit_depth >= 10) == hdr
            })
    }

    pub(crate) fn select(&self, chroma: u8, hdr: bool, quality: i32) -> FrameQualityCapability {
        let minimum = if matches!(quality, 0 | 5) { 0 } else { quality };
        let h264 = self.exact(1, chroma, hdr);
        let h265 = self.exact(2, chroma, hdr);
        for row in [h265, h264].into_iter().flatten() {
            if row.valid() && row.max_frame_quality >= minimum {
                return row;
            }
        }
        match (
            h265.filter(|row| row.valid()),
            h264.filter(|row| row.valid()),
        ) {
            (Some(hevc), Some(avc)) => {
                if hevc.max_frame_quality >= avc.max_frame_quality {
                    hevc
                } else {
                    avc
                }
            }
            (Some(row), None) | (None, Some(row)) => row,
            (None, None) => [h265, h264]
                .into_iter()
                .flatten()
                .find(|row| row.has_dimensions())
                .unwrap_or(FrameQualityCapability {
                    video_codec: 2,
                    chroma_sampling: 1,
                    bit_depth: 8,
                    max_width: 0,
                    max_height: 0,
                    max_frame_quality: 0,
                    result: -3,
                }),
        }
    }
}
