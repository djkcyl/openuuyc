//! H.264 slice header fields needed to fill `VASliceParameterBufferH264`
//! (ITU-T H.264 § 7.3.3), restricted to this crate's decode scope: I and P slices, single slice
//! per picture (`first_mb_in_slice == 0`), single-forward-reference P slices
//! (`num_ref_idx_l0_active` fixed at exactly `1`, `RefPicList0[0]` only), no reference-list
//! reordering, no long-term references, no weighted prediction, no CABAC P-slices, no redundant
//! pictures. See `adr/linux/0002-vaapi-h264-p-slice-dpb.md` § Bitstream-parser changes.
//!
//! `dec_ref_pic_marking()`'s sliding-window-only scope cut and the P-slice-specific rejections
//! below port `vulkan/h264_slice.rs`'s identical reasoning — see the ADR's porting table.

use crate::decoder::platform::DecodeError;
use crate::decoder::platform::bitstream::BitReader;

use super::pps::Pps;
use super::sps::Sps;

/// Parsed slice header fields for one supported (I or single-forward-reference P, single-slice)
/// picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SliceHeader {
    /// `first_mb_in_slice` — always `0` (single slice per picture is enforced by
    /// [`Self::parse`]).
    pub(super) first_mb_in_slice: u32,
    /// `slice_type % 5` — `0` (P) or `2` (I) (enforced by [`Self::parse`]).
    pub(super) slice_type: u8,
    /// `pic_parameter_set_id` referenced by this slice.
    pub(super) pic_parameter_set_id: u32,
    /// `frame_num`.
    pub(super) frame_num: u32,
    /// Whether this slice belongs to an IDR access unit — derived from the caller's
    /// `NalUnitType`, not itself a bitstream field (`idr_pic_id` is read only when `true`, and
    /// `dec_ref_pic_marking()`'s two-flag IDR form is used instead of the sliding-window form).
    pub(super) is_idr: bool,
    /// `pic_order_cnt_lsb` (valid since `pic_order_cnt_type == 0` is enforced by
    /// [`Sps::parse`]).
    pub(super) pic_order_cnt_lsb: u32,
    /// Active `RefPicList0` size: `num_ref_idx_l0_active_minus1 + 1` when overridden, else
    /// `pps.num_ref_idx_l0_default_active`. Always `1` for P slices (any other value is rejected
    /// by [`Self::parse`] — this crate's single-forward-reference scope), `0` for I slices.
    pub(super) num_ref_idx_l0_active: u32,
    /// `slice_qp_delta`.
    pub(super) slice_qp_delta: i32,
    /// `disable_deblocking_filter_idc` (`0` when `deblocking_filter_control_present_flag` is
    /// unset — deblocking enabled by default).
    pub(super) disable_deblocking_filter_idc: u8,
    /// `slice_alpha_c0_offset_div2`.
    pub(super) slice_alpha_c0_offset_div2: i32,
    /// `slice_beta_offset_div2`.
    pub(super) slice_beta_offset_div2: i32,
    /// Bits consumed parsing this header, counted from the start of `rbsp` (i.e. *excluding*
    /// the 1-byte NAL header). Callers building `VASliceParameterBufferH264::slice_data_bit_offset`
    /// must add `8` for the NAL header byte — see ITU-T H.264's VA-API buffer contract comment
    /// ("relative to and includes the NAL unit byte").
    pub(super) bits_consumed: usize,
}

impl SliceHeader {
    /// Parse a slice header from an I or P slice NAL's RBSP (NAL header byte and
    /// emulation-prevention bytes already removed).
    ///
    /// `nal_ref_idc` comes from the NAL header byte (`(header >> 5) & 0b11`).
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidInput`] on truncated/malformed data or a
    /// `pic_parameter_set_id`/redundant-picture inconsistency, or [`DecodeError::Unsupported`]
    /// for anything outside this crate's scope: multiple slices per picture
    /// (`first_mb_in_slice != 0`), B/SP/SI slice types, `pps.weighted_pred_flag` or
    /// `pps.entropy_coding_mode_flag` set on a P slice (unparsed `pred_weight_table()`/
    /// `cabac_init_idc` — see the module doc), `num_ref_idx_l0_active != 1` on a P slice,
    /// `ref_pic_list_modification_flag_l0 == true`, `adaptive_ref_pic_marking_mode_flag == true`
    /// on a non-IDR picture, or a non-zero `redundant_pic_cnt`.
    pub(super) fn parse(
        rbsp: &[u8],
        nal_ref_idc: u8,
        is_idr: bool,
        sps: &Sps,
        pps: &Pps,
    ) -> Result<Self, DecodeError> {
        let mut r = BitReader::new(rbsp);
        let map_err = |_| DecodeError::InvalidInput;

        let first_mb_in_slice = r.read_ue().map_err(map_err)?;
        if first_mb_in_slice != 0 {
            return Err(DecodeError::Unsupported);
        }
        let slice_type_raw = r.read_ue().map_err(map_err)?;
        let slice_type = u8::try_from(slice_type_raw % 5).unwrap_or(u8::MAX);
        if !matches!(slice_type, 0 | 2) {
            // Only P (0) and I (2) slices are supported this round — B/SP/SI rejected.
            return Err(DecodeError::Unsupported);
        }
        let is_p_slice = slice_type == 0;
        if is_p_slice {
            // pred_weight_table() / cabac_init_idc are not parsed by this crate's P-slice header
            // reader (see adr/linux/0002 § Bitstream-parser changes) — reject honestly *before*
            // reading any further P-slice-specific fields rather than silently misaligning the
            // bit position downstream.
            if pps.weighted_pred_flag || pps.entropy_coding_mode_flag {
                return Err(DecodeError::Unsupported);
            }
        }

        let pic_parameter_set_id = r.read_ue().map_err(map_err)?;
        let frame_num_bits = sps.log2_max_frame_num_minus4 + 4;
        let frame_num = r.read_bits(frame_num_bits).map_err(map_err)?;

        if is_idr {
            let _idr_pic_id = r.read_ue().map_err(map_err)?;
        }

        let poc_lsb_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;
        let pic_order_cnt_lsb = r.read_bits(poc_lsb_bits).map_err(map_err)?;
        if pps.pic_order_present_flag {
            let _delta_pic_order_cnt_bottom = r.read_se().map_err(map_err)?;
        }

        if pps.redundant_pic_cnt_present_flag {
            let redundant_pic_cnt = r.read_ue().map_err(map_err)?;
            if redundant_pic_cnt != 0 {
                return Err(DecodeError::Unsupported);
            }
        }

        let num_ref_idx_l0_active = if is_p_slice {
            let num_ref_idx_active_override_flag = r.read_bit().map_err(map_err)? != 0;
            let active = if num_ref_idx_active_override_flag {
                r.read_ue()
                    .map_err(map_err)?
                    .checked_add(1)
                    .ok_or(DecodeError::InvalidInput)?
            } else {
                pps.num_ref_idx_l0_default_active
            };
            if active != 1 {
                // Single-forward-reference scope only — see adr/linux/0002 § Bitstream-parser
                // changes for why any other count is rejected rather than silently truncated to
                // one reference.
                return Err(DecodeError::Unsupported);
            }

            let ref_pic_list_modification_flag_l0 = r.read_bit().map_err(map_err)? != 0;
            if ref_pic_list_modification_flag_l0 {
                // No reference-list reordering support this round —
                // apply_ref_pic_list_modifications is explicitly not ported (see the ADR's
                // porting table): a single-candidate RefPicList0 has nothing to reorder.
                return Err(DecodeError::Unsupported);
            }
            active
        } else {
            0
        };

        if nal_ref_idc != 0 {
            parse_dec_ref_pic_marking(&mut r, is_idr)?;
        }

        let slice_qp_delta = r.read_se().map_err(map_err)?;
        let (disable_deblocking_filter_idc, slice_alpha_c0_offset_div2, slice_beta_offset_div2) =
            if pps.deblocking_filter_control_present_flag {
                let idc_raw = r.read_ue().map_err(map_err)?;
                let idc = u8::try_from(idc_raw).map_err(|_| DecodeError::InvalidInput)?;
                if idc == 1 {
                    (idc, 0, 0)
                } else {
                    let alpha = r.read_se().map_err(map_err)?;
                    let beta = r.read_se().map_err(map_err)?;
                    (idc, alpha, beta)
                }
            } else {
                (0u8, 0i32, 0i32)
            };

        let bits_consumed = r.bits_read();

        Ok(Self {
            first_mb_in_slice,
            slice_type,
            pic_parameter_set_id,
            frame_num,
            is_idr,
            pic_order_cnt_lsb,
            num_ref_idx_l0_active,
            slice_qp_delta,
            disable_deblocking_filter_idc,
            slice_alpha_c0_offset_div2,
            slice_beta_offset_div2,
            bits_consumed,
        })
    }
}

/// Parse `dec_ref_pic_marking()` (ITU-T H.264 § 7.3.3.3). Only sliding-window marking is
/// supported — `adaptive_ref_pic_marking_mode_flag == 1` streams are rejected. Values are not
/// retained: this crate's own `Dpb::allocate_slot` (`dpb.rs`) implements the sliding-window
/// process directly from `frame_num_wrap`, so only correct bit-position advancement (and
/// detecting the unsupported adaptive case) matters here. Ports `vulkan/h264_slice.rs`'s
/// identical scope cut.
fn parse_dec_ref_pic_marking(r: &mut BitReader<'_>, is_idr: bool) -> Result<(), DecodeError> {
    let map_err = |_| DecodeError::InvalidInput;
    if is_idr {
        let _no_output_of_prior_pics_flag = r.read_bit().map_err(map_err)?;
        let _long_term_reference_flag = r.read_bit().map_err(map_err)?;
        return Ok(());
    }
    let adaptive_ref_pic_marking_mode_flag = r.read_bit().map_err(map_err)? != 0;
    if adaptive_ref_pic_marking_mode_flag {
        return Err(DecodeError::Unsupported);
    }
    Ok(())
}
