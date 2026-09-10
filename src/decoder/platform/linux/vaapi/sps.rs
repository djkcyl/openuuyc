//! H.264 SPS fields needed to fill `VAPictureParameterBufferH264` (ITU-T H.264 § 7.3.2.1.1).
//!
//! Deliberately narrower than a general-purpose SPS parser: this crate only decodes baseline
//! (`profile_idc == 66`) / main (`profile_idc == 77`) profile, progressive
//! (`frame_mbs_only_flag == 1`), 4:2:0 (implied by profile), `pic_order_cnt_type == 0`
//! bitstreams this session — see `adr/0001-vaapi-h264-cpu-out.md` § Scope. Anything outside
//! that returns [`DecodeError::Unsupported`] rather than guessing.
//!
//! Reuses [`mediaway_sw::h264::BitReader`] (Exp-Golomb / fixed-width bit reads) instead of
//! re-implementing it — see the ADR's "Alternatives Considered" for why the rest of
//! `mediaway_sw::h264` (its own `Sps`/`Pps`) is not reused: that crate's structs intentionally
//! discard several raw syntax elements (`log2_max_frame_num_minus4`, `pic_order_cnt_type`, …)
//! this crate's VA-API parameter buffers require.

use crate::decoder::platform::DecodeError;
use crate::decoder::platform::bitstream::BitReader;

/// Parsed SPS fields needed for VA-API H.264 decode parameter buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Sps {
    /// `profile_idc` — always `66` or `77` (this parser rejects anything else).
    pub(super) profile_idc: u8,
    /// `log2_max_frame_num_minus4`.
    pub(super) log2_max_frame_num_minus4: u32,
    /// `pic_order_cnt_type` — always `0` (this parser rejects `1`/`2`).
    pub(super) pic_order_cnt_type: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4` (only meaningful when `pic_order_cnt_type == 0`).
    pub(super) log2_max_pic_order_cnt_lsb_minus4: u32,
    /// `max_num_ref_frames` — sizes this session's DPB and surface pool (see `dpb.rs`,
    /// `h264.rs::ensure_pipeline`) and fills `VAPictureParameterBufferH264::num_ref_frames`.
    pub(super) max_num_ref_frames: u32,
    /// `gaps_in_frame_num_value_allowed_flag`.
    pub(super) gaps_in_frame_num_value_allowed_flag: bool,
    /// `pic_width_in_mbs_minus1`.
    pub(super) pic_width_in_mbs_minus1: u32,
    /// `pic_height_in_map_units_minus1` (frame height in MBs since `frame_mbs_only_flag == 1`).
    pub(super) pic_height_in_map_units_minus1: u32,
    /// `direct_8x8_inference_flag`.
    pub(super) direct_8x8_inference_flag: bool,
}

impl Sps {
    /// Parse an SPS RBSP (NAL header byte and emulation-prevention bytes already removed,
    /// e.g. [`mediaway_sw::h264::NalUnit::rbsp`] for a
    /// [`mediaway_sw::h264::NalUnitType::Sps`] unit).
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidInput`] on truncated/malformed data, or
    /// [`DecodeError::Unsupported`] for `profile_idc` outside `{66, 77}`,
    /// `pic_order_cnt_type != 0`, or `frame_mbs_only_flag == 0` (interlace) — all out of scope
    /// this session.
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self, DecodeError> {
        let profile_idc = *rbsp.first().ok_or(DecodeError::InvalidInput)?;
        if !matches!(profile_idc, 66 | 77) {
            return Err(DecodeError::Unsupported);
        }
        // rbsp[1] = constraint_set flags (unused), rbsp[2] = level_idc (unused: driver derives
        // its own capability check from profile + resolution via `vaQueryConfigProfiles`).
        let mut r = BitReader::new(rbsp.get(3..).ok_or(DecodeError::InvalidInput)?);
        let map_err = |_| DecodeError::InvalidInput;

        let _seq_parameter_set_id = r.read_ue().map_err(map_err)?;
        // profile_idc in {66, 77} never carries the chroma_format_idc/bit-depth/scaling-matrix
        // block (ITU-T H.264 § 7.3.2.1.1's `if (profile_idc == 100 || …)`), so
        // chroma_format_idc is implied 4:2:0 and bit depth implied 8-bit.
        let log2_max_frame_num_minus4 = r.read_ue().map_err(map_err)?;
        let pic_order_cnt_type = r.read_ue().map_err(map_err)?;
        if pic_order_cnt_type != 0 {
            return Err(DecodeError::Unsupported);
        }
        let log2_max_pic_order_cnt_lsb_minus4 = r.read_ue().map_err(map_err)?;
        let max_num_ref_frames = r.read_ue().map_err(map_err)?;
        let gaps_in_frame_num_value_allowed_flag = r.read_bit().map_err(map_err)? != 0;
        let pic_width_in_mbs_minus1 = r.read_ue().map_err(map_err)?;
        let pic_height_in_map_units_minus1 = r.read_ue().map_err(map_err)?;
        let frame_mbs_only_flag = r.read_bit().map_err(map_err)? != 0;
        if !frame_mbs_only_flag {
            return Err(DecodeError::Unsupported);
        }
        let direct_8x8_inference_flag = r.read_bit().map_err(map_err)? != 0;
        // frame_cropping_rect() / vui_parameters() not needed for the parameter buffers this
        // crate builds — parsing stops here (cropping is not applied to output dimensions this
        // session, see ADR-0001 § Scope).

        Ok(Self {
            profile_idc,
            log2_max_frame_num_minus4,
            pic_order_cnt_type,
            log2_max_pic_order_cnt_lsb_minus4,
            max_num_ref_frames,
            gaps_in_frame_num_value_allowed_flag,
            pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1,
            direct_8x8_inference_flag,
        })
    }

    /// Coded picture width in luma samples (`(pic_width_in_mbs_minus1 + 1) * 16`).
    pub(super) const fn width(&self) -> u32 {
        (self.pic_width_in_mbs_minus1 + 1).saturating_mul(16)
    }

    /// Coded picture height in luma samples. `frame_mbs_only_flag == 1` is enforced by
    /// [`Self::parse`], so this is simply `(pic_height_in_map_units_minus1 + 1) * 16`.
    pub(super) const fn height(&self) -> u32 {
        (self.pic_height_in_map_units_minus1 + 1).saturating_mul(16)
    }
}
