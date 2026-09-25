//! Local controlled-device services; access policy and media sessions have separate owners.
mod access;
mod allocation;
mod burst;
mod congestion;
pub(crate) mod desktop;
pub(crate) mod displays;
mod encoding_settings;
mod fec;
pub(crate) mod format;
mod hevc;
mod keyframe;
pub(crate) mod network;
pub(crate) mod parameters;
pub(crate) mod peer;
mod protection;
mod settings;
mod track;
mod transport;

use std::sync::{Mutex, MutexGuard};

pub(crate) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) use access::{AccessRequest, ActiveEncoding, Handle, Lease, SessionLease};
pub(crate) use encoding_settings::{EncoderCodec, EncoderMode, EncodingSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoConfig {
    pub fps: u32,
    pub requested_fps: u32,
    pub fps_limit: u32,
    pub bitrate: u32,
    pub quality: i32,
    pub auto_quality: i32,
    pub revision: u64,
    pub reported_quality: i32,
    pub format: format::Format,
    // Runtime CaptureSetting scale bounds, separate from initial decoder caps.
    pub requested_maximum: Option<(u32, u32)>,
    pub maximum: (u32, u32),
    pub maximum_fps: u32,
    pub maximum_quality: i32,
    pub sending: bool,
    pub capturing: bool,
    pub cursor_capture: bool,
}
impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            requested_fps: 30,
            fps_limit: 144,
            bitrate: 8_000_000,
            quality: 2,
            auto_quality: 2,
            revision: 0,
            reported_quality: 2,
            format: format::Format::AVC,
            requested_maximum: None,
            maximum: (3840, 2160),
            maximum_fps: 144,
            maximum_quality: 4,
            sending: true,
            capturing: true,
            cursor_capture: false,
        }
    }
}

pub(crate) use crate::media::geometry::output_size;

use crate::platform::transfer;
pub(crate) use crate::platform::{capture, encoder};
