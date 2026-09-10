//! HEVC PPS fields needed to fill VA-API's HEVC decode parameter buffers (ITU-T H.265 §
//! 7.3.2.3.1). Starts from `vulkan::hevc_params::HevcPps::parse` (cited, not imported — see
//! `adr/linux/0003-vaapi-hevc-p-slice-dpb.md` § Alternatives Considered), reusing its parse
//! sequence verbatim through `deblocking_filter_control_present_flag`, then **extends** it with
//! a real PPS-parsing tail that porting source never needed (Vulkan Video hands the raw
//! bitstream to the hardware decoder, which parses its own slice/PPS syntax; this crate's own
//! app-must-parse VA-API model has no such luxury — same asymmetry
//! [`super::sps`]/[`super::pps`] (H.264) already established):
//!
//! ```text
//! pps_scaling_list_data_present_flag        // if 1: scaling_list_data() — rejected, not parsed
//! lists_modification_present_flag           // retained: needed for VASliceParsingFields
//! log2_parallel_merge_level_minus2          // retained: real PictureParameterBufferHEVC field
//! slice_segment_header_extension_present_flag  // if 1: extra slice-header bits — rejected
//! pps_extension_present_flag                // if 1: range/SCC/etc extensions — rejected
//! ```
//!
//! Scope matches [`super::hevc_sps::HevcSps`] and this ADR: single slice group / single tile, no
//! WPP, no scaling lists, no deblocking-control override, no slice-header extension, no PPS
//! range/SCC extensions.

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "every count here comes from an Exp-Golomb-decoded HEVC syntax element, always \
              small in practice — mirrors vulkan::hevc_params's identical allow"
)]

use crate::decoder::platform::DecodeError;
use crate::decoder::platform::bitstream::BitReader;

/// Parsed HEVC PPS fields this crate's VA-API decode session needs.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is a real, independent ITU-T H.265 PPS flag that must be echoed into \
              PictureParameterBufferHEVC/SliceParsingFields exactly as signaled — mirrors \
              vulkan::HevcPps's identical allow"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HevcPps {
    /// `pps_pic_parameter_set_id` — checked against each slice's own
    /// `slice_pic_parameter_set_id` before decode (mirrors this crate's H.264 sibling's
    /// identical `pic_parameter_set_id` consistency check).
    pub(super) pps_pic_parameter_set_id: u32,
    /// `dependent_slice_segments_enabled_flag`. Every *accepted* slice this crate decodes has
    /// `dependent_slice_segment_flag == 0` (single slice per picture, enforced by
    /// [`super::hevc_slice`]), but the PPS may still legally enable the capability without any
    /// given slice using it — echoed honestly regardless.
    pub(super) dependent_slice_segments_enabled_flag: bool,
    /// `output_flag_present_flag` — gates `pic_output_flag` in the slice segment header.
    pub(super) output_flag_present_flag: bool,
    /// `num_extra_slice_header_bits` — count of `slice_reserved_flag[i]` bits to skip in the
    /// slice segment header.
    pub(super) num_extra_slice_header_bits: u32,
    /// `sign_data_hiding_enabled_flag`.
    pub(super) sign_data_hiding_enabled_flag: bool,
    /// `cabac_init_present_flag`.
    pub(super) cabac_init_present_flag: bool,
    /// `num_ref_idx_l0_default_active_minus1 + 1`.
    pub(super) num_ref_idx_l0_default_active: u32,
    /// `num_ref_idx_l1_default_active_minus1 + 1`.
    pub(super) num_ref_idx_l1_default_active: u32,
    /// `init_qp_minus26 + 26`.
    pub(super) init_qp: i32,
    /// `constrained_intra_pred_flag`.
    pub(super) constrained_intra_pred_flag: bool,
    /// `transform_skip_enabled_flag` — gates a per-TU bit; a mismatch desyncs CABAC parsing.
    pub(super) transform_skip_enabled_flag: bool,
    /// `cu_qp_delta_enabled_flag` — gates per-CU-group bits; a mismatch desyncs CABAC parsing.
    pub(super) cu_qp_delta_enabled_flag: bool,
    /// `diff_cu_qp_delta_depth` — only meaningful when `cu_qp_delta_enabled_flag` is set.
    pub(super) diff_cu_qp_delta_depth: u32,
    /// `pps_cb_qp_offset` (`se(v)`).
    pub(super) pps_cb_qp_offset: i32,
    /// `pps_cr_qp_offset` (`se(v)`).
    pub(super) pps_cr_qp_offset: i32,
    /// `pps_slice_chroma_qp_offsets_present_flag` — gates `slice_cb_qp_offset`/
    /// `slice_cr_qp_offset` bits in every slice header.
    pub(super) pps_slice_chroma_qp_offsets_present_flag: bool,
    /// `weighted_pred_flag` (P slices only). This crate does not parse `pred_weight_table()`, so
    /// [`super::hevc_slice::HevcSliceSegmentHeader::parse`] rejects any P slice referencing a
    /// PPS with this set.
    pub(super) weighted_pred_flag: bool,
    /// `weighted_bipred_flag` (B slices only — unreachable, B-slices rejected upstream).
    pub(super) weighted_bipred_flag: bool,
    /// `transquant_bypass_enabled_flag` — gates a bit at the start of every `coding_unit()`.
    pub(super) transquant_bypass_enabled_flag: bool,
    /// `pps_loop_filter_across_slices_enabled_flag` — gates
    /// `slice_loop_filter_across_slices_enabled_flag` in every slice header (see
    /// [`super::hevc_slice`]).
    pub(super) pps_loop_filter_across_slices_enabled_flag: bool,
    /// `lists_modification_present_flag` — a real `VASliceParsingFields` value the driver-facing
    /// struct must carry honestly. Functionally inert for this ADR's scope, since
    /// `NumPicTotalCurr` is always `<= 1` for any stream this crate accepts (`ref_pic_lists_modification()`
    /// is provably unreachable — see [`super::hevc_slice`]'s module doc).
    pub(super) lists_modification_present_flag: bool,
    /// `log2_parallel_merge_level_minus2` — a real `PictureParameterBufferHEVC` top-level field
    /// with no Vulkan-side decode equivalent need (this ADR's own PPS-parsing extension).
    pub(super) log2_parallel_merge_level_minus2: u32,
}

impl HevcPps {
    /// Parse a PPS RBSP (2-byte NAL header already stripped).
    ///
    /// # Errors
    ///
    /// [`DecodeError::Unsupported`] when `num_slice_groups_minus1 > 0` (tiles/FMO/ASO, folded
    /// into this crate's own `tiles_enabled_flag`/`entropy_coding_sync_enabled_flag` checks
    /// below since HEVC's own PPS carries them directly, unlike H.264's separate
    /// `num_slice_groups_minus1`), `tiles_enabled_flag == 1`, `entropy_coding_sync_enabled_flag
    /// == 1` (WPP), `deblocking_filter_control_present_flag == 1`,
    /// `pps_scaling_list_data_present_flag == 1`, `slice_segment_header_extension_present_flag
    /// == 1`, or `pps_extension_present_flag == 1` — all out of scope, per the module doc.
    /// Other [`DecodeError::InvalidInput`] on truncated/overflowing data.
    #[allow(
        clippy::too_many_lines,
        reason = "linear ITU-T H.265 § 7.3.2.3.1 syntax-element sequence through the fields this \
                  crate's decode session needs, including this ADR's own new PPS-parsing tail \
                  (see module doc) — mirrors vulkan::HevcPps::parse's identical allow"
    )]
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self, DecodeError> {
        let mut r = BitReader::new(rbsp);
        let map_err = |_| DecodeError::InvalidInput;

        let pps_pic_parameter_set_id = r.read_ue().map_err(map_err)?;
        let _pps_seq_parameter_set_id = r.read_ue().map_err(map_err)?;
        let dependent_slice_segments_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let output_flag_present_flag = r.read_bit().map_err(map_err)? != 0;
        let num_extra_slice_header_bits = r.read_bits(3).map_err(map_err)?;
        let sign_data_hiding_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let cabac_init_present_flag = r.read_bit().map_err(map_err)? != 0;
        let num_ref_idx_l0_default_active = r
            .read_ue()
            .map_err(map_err)?
            .checked_add(1)
            .ok_or(DecodeError::InvalidInput)?;
        let num_ref_idx_l1_default_active = r
            .read_ue()
            .map_err(map_err)?
            .checked_add(1)
            .ok_or(DecodeError::InvalidInput)?;
        let init_qp_minus26 = r.read_se().map_err(map_err)?;
        let constrained_intra_pred_flag = r.read_bit().map_err(map_err)? != 0;
        let transform_skip_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let cu_qp_delta_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let diff_cu_qp_delta_depth = if cu_qp_delta_enabled_flag {
            r.read_ue().map_err(map_err)?
        } else {
            0
        };
        let pps_cb_qp_offset = r.read_se().map_err(map_err)?;
        let pps_cr_qp_offset = r.read_se().map_err(map_err)?;
        let pps_slice_chroma_qp_offsets_present_flag = r.read_bit().map_err(map_err)? != 0;
        let weighted_pred_flag = r.read_bit().map_err(map_err)? != 0;
        let weighted_bipred_flag = r.read_bit().map_err(map_err)? != 0;
        let transquant_bypass_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let tiles_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let entropy_coding_sync_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        if tiles_enabled_flag {
            return Err(DecodeError::Unsupported);
        }
        if entropy_coding_sync_enabled_flag {
            return Err(DecodeError::Unsupported);
        }
        // `pps_loop_filter_across_slices_enabled_flag` gates a real slice-header bit whenever
        // SAO or (undisabled) deblocking is active — see `super::hevc_slice`'s module doc.
        let pps_loop_filter_across_slices_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let deblocking_filter_control_present_flag = r.read_bit().map_err(map_err)? != 0;
        if deblocking_filter_control_present_flag {
            return Err(DecodeError::Unsupported);
        }

        // This ADR's own new PPS-parsing tail (see module doc) — real ITU-T H.265 § 7.3.2.3.1
        // syntax `vulkan::hevc_params::HevcPps::parse` never reads at all.
        let pps_scaling_list_data_present_flag = r.read_bit().map_err(map_err)? != 0;
        if pps_scaling_list_data_present_flag {
            return Err(DecodeError::Unsupported);
        }
        let lists_modification_present_flag = r.read_bit().map_err(map_err)? != 0;
        let log2_parallel_merge_level_minus2 = r.read_ue().map_err(map_err)?;
        let slice_segment_header_extension_present_flag = r.read_bit().map_err(map_err)? != 0;
        if slice_segment_header_extension_present_flag {
            return Err(DecodeError::Unsupported);
        }
        let pps_extension_present_flag = r.read_bit().map_err(map_err)? != 0;
        if pps_extension_present_flag {
            return Err(DecodeError::Unsupported);
        }
        // rbsp_trailing_bits() follows — not needed for this crate's decode path.

        let init_qp = init_qp_minus26
            .checked_add(26)
            .ok_or(DecodeError::InvalidInput)?;
        Ok(Self {
            pps_pic_parameter_set_id,
            dependent_slice_segments_enabled_flag,
            output_flag_present_flag,
            num_extra_slice_header_bits,
            sign_data_hiding_enabled_flag,
            cabac_init_present_flag,
            num_ref_idx_l0_default_active,
            num_ref_idx_l1_default_active,
            init_qp,
            constrained_intra_pred_flag,
            transform_skip_enabled_flag,
            cu_qp_delta_enabled_flag,
            diff_cu_qp_delta_depth,
            pps_cb_qp_offset,
            pps_cr_qp_offset,
            pps_slice_chroma_qp_offsets_present_flag,
            weighted_pred_flag,
            weighted_bipred_flag,
            transquant_bypass_enabled_flag,
            pps_loop_filter_across_slices_enabled_flag,
            lists_modification_present_flag,
            log2_parallel_merge_level_minus2,
        })
    }
}
