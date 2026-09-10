//! OpenUUYC platform decode contract and native backend adapters.
//! Windows uses our direct DXVA11 sessions and H264 software core; Apple and
//! Linux retain independent platform backends. Owning Windows output is
//! available through `windows::WindowsVideoDecoder::poll_owned_frame`.

#![allow(unsafe_code)]

#[cfg(target_os = "linux")]
mod bitstream;
mod error;
mod video;

pub use error::DecodeError;
pub use video::{
    DecoderNotification, VideoDecoder, VideoDecoderConfig, VideoDecoderOutput,
    VideoOutputPreference,
};

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod windows;
