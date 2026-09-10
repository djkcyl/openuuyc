//! Map [`CodecKind`] / SPS `profile_idc` to VA-API profile candidates for decode.

use crate::decoder::platform::DecodeError;

/// VA-API profile candidates for a parsed SPS `profile_idc`, best match first.
///
/// This crate's [`super::sps::Sps::parse`] already rejects any `profile_idc` outside
/// `{66, 77}` (baseline / main — this session's scope, see
/// `adr/0001-vaapi-h264-cpu-out.md`), so the `_` arm here is unreachable in practice but kept
/// exhaustive and honest rather than assuming the caller always validates first.
///
/// # Errors
///
/// Returns [`DecodeError::Unsupported`] for any `profile_idc` this crate does not decode.
pub(super) fn h264_profile_candidates(
    profile_idc: u8,
) -> Result<Vec<cros_libva::VAProfile::Type>, DecodeError> {
    match profile_idc {
        // Baseline: most drivers only expose the Constrained Baseline VA-API profile.
        66 => Ok(vec![
            cros_libva::VAProfile::VAProfileH264ConstrainedBaseline,
        ]),
        // Main: prefer an exact match, fall back to Constrained Baseline (a strict decode
        // subset of Main for VLD purposes on drivers that do not enumerate Main separately).
        77 => Ok(vec![
            cros_libva::VAProfile::VAProfileH264Main,
            cros_libva::VAProfile::VAProfileH264ConstrainedBaseline,
        ]),
        _ => Err(DecodeError::Unsupported),
    }
}

/// VA-API profile candidates for a parsed HEVC SPS `general_profile_idc`, best match first
/// (ADR-0003, `adr/linux/0003-vaapi-hevc-p-slice-dpb.md`).
///
/// This crate's own [`super::hevc_sps::HevcSps::parse`] does not itself restrict
/// `general_profile_idc` (unlike H.264's [`super::sps::Sps::parse`], which already rejects
/// anything outside `{66, 77}`) — this function is the single point that narrows to Main
/// profile only (this ADR's scope), returning [`DecodeError::Unsupported`] for anything else.
///
/// # Errors
///
/// Returns [`DecodeError::Unsupported`] for any `general_profile_idc` this crate does not
/// decode.
pub(super) fn hevc_profile_candidates(
    general_profile_idc: u8,
) -> Result<Vec<cros_libva::VAProfile::Type>, DecodeError> {
    match general_profile_idc {
        // Main (ITU-T H.265 Table A.1): this ADR's only supported profile.
        1 => Ok(vec![cros_libva::VAProfile::VAProfileHEVCMain]),
        _ => Err(DecodeError::Unsupported),
    }
}
