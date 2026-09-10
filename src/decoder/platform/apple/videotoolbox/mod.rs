//! H.264/HEVC VideoToolbox implementation, compiled only on Apple targets.
mod codec;
mod format_desc;
mod hevc_config;
mod video;
pub(crate) use video::VideoToolboxVideoDecoder;
