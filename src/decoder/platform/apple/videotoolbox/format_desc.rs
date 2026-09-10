//! H.264/HEVC CoreMedia format descriptions created from parameter sets.
use crate::decoder::platform::DecodeError;
use mediaway_common::Bytes;
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMFormatDescription, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use std::ptr::NonNull;

const NO_ERROR: i32 = 0;

/// Wraps a just-populated `*const CMFormatDescription` Create-Rule out-parameter into an owned
/// `CFRetained` — shared by every constructor below. `raw` is null on any path that didn't
/// actually produce a format description (status was non-zero, or a conforming implementation
/// still left the out-param untouched); both are reported as [`DecodeError::Backend`] rather than
/// assumed impossible.
fn retained_from_create(
    raw: *const CMFormatDescription,
) -> Result<CFRetained<CMFormatDescription>, DecodeError> {
    let ptr = NonNull::new(raw.cast_mut()).ok_or(DecodeError::Backend)?;
    // SAFETY: `raw` came from a `CMVideoFormatDescriptionCreate*` Create-Rule out-parameter that
    // just reported success (checked by every caller before this is reached) — the Create Rule
    // guarantees a +1 retain count the caller now owns.
    Ok(unsafe { CFRetained::from_raw(ptr) })
}

/// SPS/PPS → `CMFormatDescription`, via `CMVideoFormatDescriptionCreateFromH264ParameterSets`.
/// `sps`/`pps` must be raw NAL payload with any emulation prevention bytes needed and no start
/// code / length prefix — exactly what `iso_bmff::bitstream::avc::AvcDecoderConfig::{sps, pps}`
/// already returns.
pub(super) fn create_h264(
    sps: &Bytes,
    pps: &Bytes,
) -> Result<CFRetained<CMVideoFormatDescription>, DecodeError> {
    if sps.is_empty() || pps.is_empty() {
        return Err(DecodeError::InvalidInput);
    }
    let Some(sps_ptr) = NonNull::new(sps.as_ptr().cast_mut()) else {
        return Err(DecodeError::InvalidInput);
    };
    let Some(pps_ptr) = NonNull::new(pps.as_ptr().cast_mut()) else {
        return Err(DecodeError::InvalidInput);
    };
    let mut pointers = [sps_ptr, pps_ptr];
    let mut sizes = [sps.len(), pps.len()];
    let Some(pointers_ptr) = NonNull::new(pointers.as_mut_ptr()) else {
        return Err(DecodeError::Backend);
    };
    let Some(sizes_ptr) = NonNull::new(sizes.as_mut_ptr()) else {
        return Err(DecodeError::Backend);
    };

    let mut format_desc_out: *const CMFormatDescription = std::ptr::null();
    // SAFETY: `pointers_ptr`/`sizes_ptr` point at 2-element stack arrays matching
    // `parameter_set_count = 2`; each pointer in `pointers` is valid for the corresponding
    // `sizes` entry's byte length for the duration of this call (borrowed from `sps`/`pps`,
    // both live for this whole function); `4` (`nal_unit_header_length`) matches this backend's
    // 4-byte AVCC length-prefix scope; `format_desc_out` is a valid stack out-pointer.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            pointers_ptr,
            sizes_ptr,
            4,
            NonNull::from(&mut format_desc_out),
        )
    };
    if status != NO_ERROR {
        return Err(DecodeError::Backend);
    }
    retained_from_create(format_desc_out)
}

/// VPS/SPS/PPS → `CMFormatDescription`, via
/// `CMVideoFormatDescriptionCreateFromHEVCParameterSets`. `vps`/`sps`/`pps` must be raw NAL
/// payload with any emulation prevention bytes needed and no start code / length prefix —
/// exactly what `super::hevc_config::HevcDecoderConfig::{vps, sps, pps}` already returns.
pub(super) fn create_hevc(
    vps: &Bytes,
    sps: &Bytes,
    pps: &Bytes,
) -> Result<CFRetained<CMVideoFormatDescription>, DecodeError> {
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        return Err(DecodeError::InvalidInput);
    }
    let Some(vps_ptr) = NonNull::new(vps.as_ptr().cast_mut()) else {
        return Err(DecodeError::InvalidInput);
    };
    let Some(sps_ptr) = NonNull::new(sps.as_ptr().cast_mut()) else {
        return Err(DecodeError::InvalidInput);
    };
    let Some(pps_ptr) = NonNull::new(pps.as_ptr().cast_mut()) else {
        return Err(DecodeError::InvalidInput);
    };
    let mut pointers = [vps_ptr, sps_ptr, pps_ptr];
    let mut sizes = [vps.len(), sps.len(), pps.len()];
    let Some(pointers_ptr) = NonNull::new(pointers.as_mut_ptr()) else {
        return Err(DecodeError::Backend);
    };
    let Some(sizes_ptr) = NonNull::new(sizes.as_mut_ptr()) else {
        return Err(DecodeError::Backend);
    };

    let mut format_desc_out: *const CMFormatDescription = std::ptr::null();
    // SAFETY: `pointers_ptr`/`sizes_ptr` point at 3-element stack arrays matching
    // `parameter_set_count = 3`; each pointer in `pointers` is valid for the corresponding
    // `sizes` entry's byte length for the duration of this call (borrowed from `vps`/`sps`/
    // `pps`, all live for this whole function); `4` (`nal_unit_header_length`) matches this
    // backend's 4-byte `hvcC` length-prefix scope; `extensions: None` (nothing to add beyond
    // the parameter sets themselves); `format_desc_out` is a valid stack out-pointer.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromHEVCParameterSets(
            None,
            3,
            pointers_ptr,
            sizes_ptr,
            4,
            None,
            NonNull::from(&mut format_desc_out),
        )
    };
    if status != NO_ERROR {
        return Err(DecodeError::Backend);
    }
    retained_from_create(format_desc_out)
}
