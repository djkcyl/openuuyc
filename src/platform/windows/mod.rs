//! Windows OS and GPU drivers. No account/session ownership in this layer.
mod adapter_type;
pub(crate) mod amf;
pub(crate) mod capture;
pub(crate) mod cursor;
pub(crate) mod cursor_shape;
pub(crate) mod decoder;
pub(crate) mod display;
pub(crate) mod display_hdr;
pub(crate) mod encoder;
pub(crate) mod gdi;
pub(crate) mod gpu_conversion;
pub(crate) mod nvenc;
pub(crate) mod preprocess;
pub(crate) mod qsv;
pub(crate) mod qsv_allocator;
pub(crate) mod surface;
pub(crate) mod transfer;

use crate::media::encoding as format;
use crate::media::encoding::rate as encoder_rate;
use crate::media::encoding::software as rust_h264;
use crate::media::geometry::{fit_size, output_size};
fn lock<T>(value: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) mod graphics;
pub(crate) mod swapchain;
pub(crate) mod video_shader;
