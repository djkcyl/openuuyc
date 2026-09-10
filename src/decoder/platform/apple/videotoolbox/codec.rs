//! VideoToolbox timestamp, NV12 packing and H.264/HEVC parameter-set helpers.

use iso_bmff::bitstream::avc::AvcDecoderConfig;
use mediaway_common::{Bytes, CodecKind, Rational};

use super::hevc_config::HevcDecoderConfig;
use crate::decoder::platform::DecodeError;

/// Input codecs negotiated by OpenUUYC.
pub(super) const fn is_supported_video_codec(codec: CodecKind) -> bool {
    matches!(codec, CodecKind::H264 | CodecKind::Hevc)
}

/// Validate `config` against this backend's H.264 scope: exactly one SPS + one PPS, 4-byte
/// AVCC length-prefix size only (ADR-0001 § Byte framing / § Session lifecycle). A stream that
/// doesn't fit is `DecodeError::Unsupported`, never silently truncated to the first SPS/PPS.
///
/// # Errors
///
/// Returns [`DecodeError::Unsupported`] when `config` has more than one SPS/PPS or a NAL length
/// size other than 4 bytes.
pub(super) const fn validate_parameter_sets(config: &AvcDecoderConfig) -> Result<(), DecodeError> {
    if config.nal_length_size != 4 {
        return Err(DecodeError::Unsupported);
    }
    if config.sps.len() != 1 || config.pps.len() != 1 {
        return Err(DecodeError::Unsupported);
    }
    Ok(())
}

/// Validate `config` against this backend's HEVC scope: exactly one VPS + one SPS + one PPS,
/// 4-byte `hvcC` length-prefix size only — the HEVC analog of [`validate_parameter_sets`].
///
/// # Errors
///
/// Returns [`DecodeError::Unsupported`] when `config` has more than one VPS/SPS/PPS or a NAL
/// length size other than 4 bytes.
pub(super) const fn validate_hevc_parameter_sets(
    config: &HevcDecoderConfig,
) -> Result<(), DecodeError> {
    if config.nal_length_size != 4 {
        return Err(DecodeError::Unsupported);
    }
    if config.vps.len() != 1 || config.sps.len() != 1 || config.pps.len() != 1 {
        return Err(DecodeError::Unsupported);
    }
    Ok(())
}

/// Build the `(value, timescale)` pair for a `CMTime` from `ticks` (in `time_base` units):
/// `value = ticks * time_base.num`, `timescale = time_base.den` — algebraically exact since
/// `CMTime`'s contract is `value / timescale == seconds` and `time_base` is ticks-to-seconds
/// (`num / den`), for an arbitrary (not just `num == 1`) rational timebase. See ADR-0001 §
/// Timestamps.
#[must_use]
pub(super) fn cmtime_value_from_ticks(ticks: i64, time_base: Rational) -> (i64, i32) {
    let num = i64::try_from(time_base.num).unwrap_or(i64::MAX);
    let value = ticks.saturating_mul(num);
    let timescale = i32::try_from(time_base.den).unwrap_or(i32::MAX);
    (value, timescale)
}

/// Inverse of [`cmtime_value_from_ticks`] — recovers a tick count in `time_base` units from a
/// `CMTime`'s `(value, timescale)` pair, used by the decompression output callback to convert
/// `VideoToolbox`'s returned `presentationTimeStamp`/`presentationDuration` back into this crate's
/// `VideoFrame::pts`/`duration` convention. Returns `0` on degenerate input (`timescale == 0` or
/// `time_base.num == 0`) rather than dividing by zero.
#[must_use]
pub(super) fn ticks_from_cmtime_value(value: i64, timescale: i32, time_base: Rational) -> i64 {
    if timescale == 0 || time_base.num == 0 {
        return 0;
    }
    // i128 intermediate: avoids overflow on `value * time_base.den` for pathological CMTime
    // values from the decoder, without needing a fallible/panicking path.
    let numerator = i128::from(value) * i128::from(time_base.den);
    let denominator = i128::from(timescale) * i128::from(time_base.num);
    if denominator == 0 {
        return 0;
    }
    let ticks = numerator / denominator;
    i64::try_from(ticks).unwrap_or_else(|_| {
        if ticks.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

/// Same conversion as [`ticks_from_cmtime_value`], clamped to `u64` for
/// [`VideoFrame::duration`](mediaway_common::VideoFrame::duration) (negative/degenerate results
/// become `0`, matching this crate's "0 == unknown" convention).
#[must_use]
pub(super) fn duration_ticks_from_cmtime_value(
    value: i64,
    timescale: i32,
    time_base: Rational,
) -> u64 {
    u64::try_from(ticks_from_cmtime_value(value, timescale, time_base)).unwrap_or(0)
}

/// Copy stride-padded NV12 Y/UV plane data — **two separate** base-address slices, matching
/// `CVPixelBuffer`'s per-plane layout (unlike VA-API's single contiguous mapped image the
/// `linux::vaapi::nv12` sibling reads from) — into one tightly packed [`Bytes`]. Output layout
/// matches that sibling's convention: `width * height` luma bytes followed by `width * height /
/// 2` interleaved chroma bytes, both tightly packed (stride removed).
///
/// Rows that would read past either plane slice's end are left zeroed (defensive: a
/// driver/OS reporting inconsistent strides should not panic this path).
#[must_use]
pub(super) fn copy_nv12_planes(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: u32,
    height: u32,
) -> Bytes {
    let width = width as usize;
    let height = height as usize;
    let uv_rows = height / 2;

    let mut out = vec![0u8; width * height + width * uv_rows];

    for row in 0..height {
        let src_start = row * y_stride;
        let src_end = src_start + width;
        if src_end > y_plane.len() {
            break;
        }
        let dst_start = row * width;
        out[dst_start..dst_start + width].copy_from_slice(&y_plane[src_start..src_end]);
    }

    let y_plane_bytes = width * height;
    for row in 0..uv_rows {
        let src_start = row * uv_stride;
        let src_end = src_start + width;
        if src_end > uv_plane.len() {
            break;
        }
        let dst_start = y_plane_bytes + row * width;
        out[dst_start..dst_start + width].copy_from_slice(&uv_plane[src_start..src_end]);
    }

    Bytes::from(out)
}
