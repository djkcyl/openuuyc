//! H.264 PPS fields needed to fill `VAPictureParameterBufferH264` (ITU-T H.264 § 7.3.2.2).
//!
//! Scope matches [`super::sps::Sps`]: single slice group only (`num_slice_groups_minus1 == 0`
//! — multiple slice groups / FMO / ASO are rejected), and the `more_rbsp_data()`-gated
//! extension (`transform_8x8_mode_flag`, custom scaling lists, `second_chroma_qp_index_offset`)
//! is read only far enough to confirm it is absent or trivial; custom scaling lists are
//! unsupported. See `adr/0001-vaapi-h264-cpu-out.md` § Scope.

use crate::decoder::platform::DecodeError;
use crate::decoder::platform::bitstream::BitReader;

/// Parsed PPS fields needed for VA-API H.264 decode parameter buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each field names one ITU-T H.264 PPS syntax element (`entropy_coding_mode_flag`, …); a state machine would obscure the 1:1 spec mapping this crate relies on for review"
)]
pub(super) struct Pps {
    /// `pic_parameter_set_id`.
    pub(super) pic_parameter_set_id: u32,
    /// `entropy_coding_mode_flag` (`false` = CAVLC, `true` = CABAC).
    pub(super) entropy_coding_mode_flag: bool,
    /// `bottom_field_pic_order_in_frame_present_flag` (`pic_order_present_flag`).
    pub(super) pic_order_present_flag: bool,
    /// `num_ref_idx_l0_default_active_minus1 + 1` — the default active `RefPicList0` size when
    /// a P slice's `num_ref_idx_active_override_flag` is unset.
    pub(super) num_ref_idx_l0_default_active: u32,
    /// `weighted_pred_flag`. This crate does not parse `pred_weight_table()` (see
    /// `adr/linux/0002-vaapi-h264-p-slice-dpb.md` § Bitstream-parser changes), so
    /// `SliceHeader::parse` rejects any P slice referencing a PPS with this set, rather than
    /// silently misparsing every bit downstream.
    pub(super) weighted_pred_flag: bool,
    /// `pic_init_qp_minus26`.
    pub(super) pic_init_qp_minus26: i32,
    /// `pic_init_qs_minus26`.
    pub(super) pic_init_qs_minus26: i32,
    /// `chroma_qp_index_offset`.
    pub(super) chroma_qp_index_offset: i32,
    /// `second_chroma_qp_index_offset` (inferred equal to `chroma_qp_index_offset` when the
    /// PPS extension is absent, per ITU-T H.264 § 7.4.2.2).
    pub(super) second_chroma_qp_index_offset: i32,
    /// `deblocking_filter_control_present_flag`.
    pub(super) deblocking_filter_control_present_flag: bool,
    /// `constrained_intra_pred_flag`.
    pub(super) constrained_intra_pred_flag: bool,
    /// `redundant_pic_cnt_present_flag`.
    pub(super) redundant_pic_cnt_present_flag: bool,
}

impl Pps {
    /// Parse a PPS RBSP (NAL header byte and emulation-prevention bytes already removed).
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidInput`] on truncated/malformed data, or
    /// [`DecodeError::Unsupported`] when `num_slice_groups_minus1 > 0` (FMO/ASO), or when the
    /// PPS extension sets `pic_scaling_matrix_present_flag` (custom scaling lists) or
    /// `transform_8x8_mode_flag` (High-profile-only, out of scope with the baseline/main SPS
    /// this crate accepts).
    #[allow(
        clippy::similar_names,
        reason = "pic_init_qp_minus26 / pic_init_qs_minus26 are the ITU-T H.264 spec's own names"
    )]
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self, DecodeError> {
        let mut r = BitReader::new(rbsp);
        let map_err = |_| DecodeError::InvalidInput;

        let pic_parameter_set_id = r.read_ue().map_err(map_err)?;
        let _seq_parameter_set_id = r.read_ue().map_err(map_err)?;
        let entropy_coding_mode_flag = r.read_bit().map_err(map_err)? != 0;
        let pic_order_present_flag = r.read_bit().map_err(map_err)? != 0;
        let num_slice_groups_minus1 = r.read_ue().map_err(map_err)?;
        if num_slice_groups_minus1 != 0 {
            return Err(DecodeError::Unsupported);
        }
        let num_ref_idx_l0_default_active_minus1 = r.read_ue().map_err(map_err)?;
        let _num_ref_idx_l1_default_active_minus1 = r.read_ue().map_err(map_err)?;
        let weighted_pred_flag = r.read_bit().map_err(map_err)? != 0;
        let _weighted_bipred_idc = r.read_bits(2).map_err(map_err)?;
        let num_ref_idx_l0_default_active = num_ref_idx_l0_default_active_minus1
            .checked_add(1)
            .ok_or(DecodeError::InvalidInput)?;
        let pic_init_qp_minus26 = r.read_se().map_err(map_err)?;
        let pic_init_qs_minus26 = r.read_se().map_err(map_err)?;
        let chroma_qp_index_offset = r.read_se().map_err(map_err)?;
        let deblocking_filter_control_present_flag = r.read_bit().map_err(map_err)? != 0;
        let constrained_intra_pred_flag = r.read_bit().map_err(map_err)? != 0;
        let redundant_pic_cnt_present_flag = r.read_bit().map_err(map_err)? != 0;

        let second_chroma_qp_index_offset = if more_rbsp_data(rbsp, r.bits_read()) {
            let transform_8x8_mode_flag = r.read_bit().map_err(map_err)? != 0;
            let pic_scaling_matrix_present_flag = r.read_bit().map_err(map_err)? != 0;
            if transform_8x8_mode_flag || pic_scaling_matrix_present_flag {
                // High-profile-only extension fields; the SPS this crate accepts is always
                // baseline/main, so a stream setting these here is out of scope.
                return Err(DecodeError::Unsupported);
            }
            r.read_se().map_err(map_err)?
        } else {
            chroma_qp_index_offset
        };

        Ok(Self {
            pic_parameter_set_id,
            entropy_coding_mode_flag,
            pic_order_present_flag,
            num_ref_idx_l0_default_active,
            weighted_pred_flag,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            second_chroma_qp_index_offset,
            deblocking_filter_control_present_flag,
            constrained_intra_pred_flag,
            redundant_pic_cnt_present_flag,
        })
    }
}

/// `more_rbsp_data()` (ITU-T H.264 § 7.2): true when at least one bit remains before the
/// `rbsp_trailing_bits()` stop bit (the last `1` bit in the whole RBSP).
fn more_rbsp_data(rbsp: &[u8], bits_read: usize) -> bool {
    let total_bits = rbsp.len() * 8;
    if bits_read >= total_bits {
        return false;
    }
    for bit_idx in (0..total_bits).rev() {
        let byte = rbsp[bit_idx / 8];
        let shift = 7 - (bit_idx % 8);
        if (byte >> shift) & 1 == 1 {
            return bits_read < bit_idx;
        }
    }
    false
}
