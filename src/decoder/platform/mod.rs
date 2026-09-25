//! Platform video decode contract: Windows DXVA11 and Linux software decoding.

#![allow(unsafe_code)]

mod error;
mod video;

pub use error::DecodeError;
pub use video::{DecoderNotification, VideoDecoder, VideoDecoderConfig, VideoOutputPreference};

pub mod linux;
pub mod windows;

#[cfg(windows)]
pub use windows::{
    WindowsCpuFormat as CpuFormat, WindowsDecodedFrame as PlatformDecodedFrame,
    WindowsVideoDecoder as PlatformVideoDecoder,
};

#[cfg(not(windows))]
pub use linux::{CpuFormat, LinuxVideoDecoder as PlatformVideoDecoder, PlatformDecodedFrame};
