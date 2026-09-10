//! HEVC slice-segment-header parsing + short-term reference-picture-set (RPS) construction
//! (ITU-T H.265 § 7.3.6.1, § 7.4.8), restricted to this crate's decode scope: I and
//! single-forward-reference P slices, single slice per picture. See
//! `adr/linux/0003-vaapi-hevc-p-slice-dpb.md` § Slice-header parser extension for the full
//! design rationale.
//!
//! [`ShortTermRefPicSet::parse`] starts from `vulkan::hevc_slice::ShortTermRefPicSet::parse`
//! (cited, reused **verbatim** — pure ITU-T H.265 § 7.3.7 arithmetic, zero Vulkan dependency)
//! then this crate's own [`ShortTermRefPicSet::is_single_forward_reference`] adds a new,
//! this-ADR-specific validation the porting source never needed (Vulkan's own HEVC decode never
//! reached P-slice RPS parsing at all — see that ADR's § Context).
//!
//! [`HevcSliceSegmentHeader::parse`] **starts from**
//! `vulkan::hevc_slice::HevcSliceSegmentHeader::parse`'s structure through the RPS, then
//! continues well past where that function stops (its own module doc: "trailing per-slice
//! syntax... is left unread" — correct for Vulkan, since Vulkan Video hands the raw bitstream to
//! the hardware decoder; this crate's own app-must-parse VA-API model has no such luxury, the
//! same asymmetry this crate's H.264 [`super::slice`] module already established). Every
//! reachable syntax element for this crate's own accepted-stream shape (no B-slices, at most one
//! active reference, no weighted prediction, no deblocking-control override, no tiles/WPP, no
//! slice-header extension — each already rejected upstream by [`super::hevc_sps`]/
//! [`super::hevc_pps`]'s own scope cuts) is enumerated below, cited against real ITU-T H.265
//! § 7.3.6.1 syntax order (cross-checked via `FFmpeg`'s `libavcodec/hevc/hevcdec.c::hls_slice_header`).

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "every count here comes from an Exp-Golomb-decoded HEVC syntax element, always \
              small in practice — mirrors vulkan::hevc_slice's identical allow"
)]

use smallvec::SmallVec;

use crate::decoder::platform::DecodeError;
use crate::decoder::platform::bitstream::BitReader;

use super::hevc_pps::HevcPps;
use super::hevc_sps::HevcSps;

/// Decoded, normalized slice type (ITU-T H.265 Table 7-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HevcSliceType {
    /// Bi-predictive slice — rejected as `Unsupported` by [`HevcSliceSegmentHeader::parse`] (no
    /// B-slices this ADR's scope).
    B,
    /// Predictive slice.
    P,
    /// Intra-only slice.
    I,
}

impl HevcSliceType {
    #[must_use]
    const fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::B),
            1 => Some(Self::P),
            2 => Some(Self::I),
            _ => None,
        }
    }
}

/// One decoded short-term reference picture entry — a signed POC delta relative to the current
/// picture, and whether it is actually used as a reference by the current picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ShortTermRefPicEntry {
    /// `DeltaPocS0`/`DeltaPocS1` (ITU-T H.265 § 7.4.8) — signed, relative to the current
    /// picture's POC.
    pub(super) delta_poc: i32,
    /// `used_by_curr_pic_s0_flag`/`_s1_flag`.
    pub(super) used_by_curr_pic: bool,
}

/// Parsed `short_term_ref_pic_set(stRpsIdx = 0)` (the only index reachable given this crate's
/// SPS scope cut — `num_short_term_ref_pic_sets` is always `0`, so any slice-signaled RPS is
/// parsed at index `0`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ShortTermRefPicSet {
    /// `DeltaPocS0`/`UsedByCurrPicS0`, one entry per `num_negative_pics`.
    pub(super) s0: SmallVec<[ShortTermRefPicEntry; 8]>,
    /// `DeltaPocS1`/`UsedByCurrPicS1`, one entry per `num_positive_pics`.
    pub(super) s1: SmallVec<[ShortTermRefPicEntry; 8]>,
}

impl ShortTermRefPicSet {
    /// Parse `short_term_ref_pic_set(stRpsIdx = 0)` (ITU-T H.265 § 7.3.7) — no
    /// `inter_ref_pic_set_prediction_flag` is read (only present when `stRpsIdx != 0`,
    /// unreachable given this crate's scope).
    ///
    /// # Errors
    ///
    /// [`DecodeError::Unsupported`] if the combined negative+positive picture count exceeds
    /// `VAPictureParameterBufferHEVC`'s own 8-entry-per-direction practical capacity (this
    /// crate's own [`ShortTermRefPicSet::is_single_forward_reference`] check rejects anything
    /// but exactly one entry anyway, but this bound is enforced before that check can even run).
    /// Other [`DecodeError::InvalidInput`] on truncated data or overflow.
    pub(super) fn parse(r: &mut BitReader<'_>) -> Result<Self, DecodeError> {
        let map_err = |_| DecodeError::InvalidInput;
        let num_negative_pics = r.read_ue().map_err(map_err)?;
        let num_positive_pics = r.read_ue().map_err(map_err)?;
        if num_negative_pics > 8 || num_positive_pics > 8 {
            return Err(DecodeError::Unsupported);
        }

        let mut s0 = SmallVec::new();
        let mut delta_poc = 0i32;
        for _ in 0..num_negative_pics {
            let delta_poc_s0_minus1 = r.read_ue().map_err(map_err)?;
            let used_by_curr_pic = r.read_bit().map_err(map_err)? != 0;
            let step = i32::try_from(delta_poc_s0_minus1)
                .ok()
                .and_then(|v| v.checked_add(1))
                .ok_or(DecodeError::InvalidInput)?;
            delta_poc = delta_poc
                .checked_sub(step)
                .ok_or(DecodeError::InvalidInput)?;
            s0.push(ShortTermRefPicEntry {
                delta_poc,
                used_by_curr_pic,
            });
        }

        let mut s1 = SmallVec::new();
        let mut delta_poc = 0i32;
        for _ in 0..num_positive_pics {
            let delta_poc_s1_minus1 = r.read_ue().map_err(map_err)?;
            let used_by_curr_pic = r.read_bit().map_err(map_err)? != 0;
            let step = i32::try_from(delta_poc_s1_minus1)
                .ok()
                .and_then(|v| v.checked_add(1))
                .ok_or(DecodeError::InvalidInput)?;
            delta_poc = delta_poc
                .checked_add(step)
                .ok_or(DecodeError::InvalidInput)?;
            s1.push(ShortTermRefPicEntry {
                delta_poc,
                used_by_curr_pic,
            });
        }

        Ok(Self { s0, s1 })
    }

    /// This ADR's own scope validation (not present in the Vulkan porting source, which stays
    /// general since Vulkan's own future multi-reference work would need the general case):
    /// exactly one active forward reference, always the immediately preceding picture —
    /// `num_negative_pics == 1, num_positive_pics == 0, delta_poc == -1,
    /// used_by_curr_pic_s0_flag == 1`.
    #[must_use]
    pub(super) fn is_single_forward_reference(&self) -> bool {
        self.s0.len() == 1
            && self.s1.is_empty()
            && self.s0[0].delta_poc == -1
            && self.s0[0].used_by_curr_pic
    }
}

/// Parsed HEVC slice-segment-header fields this crate's VA-API decode session needs.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is a real, independent ITU-T H.265 slice-header syntax element that \
              must be echoed into SliceParameterBufferHEVC exactly as signaled — mirrors \
              HevcSps/HevcPps's identical allow"
)]
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HevcSliceSegmentHeader {
    /// Decoded, normalized slice type — always `I` or `P` (enforced by
    /// [`HevcSliceSegmentHeader::parse`]).
    pub(super) slice_type: HevcSliceType,
    /// `slice_pic_parameter_set_id`.
    pub(super) slice_pic_parameter_set_id: u32,
    /// `slice_pic_order_cnt_lsb` — `None` for IDR pictures (the field is not present at all; IDR
    /// POC is always defined as `0`).
    pub(super) pic_order_cnt_lsb: Option<u32>,
    /// The short-term RPS this slice signaled inline, already validated as this crate's own
    /// single-forward-reference shape — `None` for IDR pictures (no RPS at all).
    pub(super) short_term_rps: Option<ShortTermRefPicSet>,
    /// `slice_sao_luma_flag` (`false` when `sps.sample_adaptive_offset_enabled_flag` is unset —
    /// the field is not present at all then).
    pub(super) slice_sao_luma_flag: bool,
    /// `slice_sao_chroma_flag`.
    pub(super) slice_sao_chroma_flag: bool,
    /// `slice_temporal_mvp_enabled_flag` (`false` for IDR or when
    /// `sps.sps_temporal_mvp_enabled_flag` is unset — the field is not present at all then).
    pub(super) slice_temporal_mvp_enabled_flag: bool,
    /// Active `RefPicList0` size: always `1` for P slices (any other value is rejected by
    /// [`HevcSliceSegmentHeader::parse`] — this crate's single-forward-reference scope), `0` for
    /// I slices.
    pub(super) num_ref_idx_l0_active: u32,
    /// `cabac_init_flag` (`false` when `pps.cabac_init_present_flag` is unset).
    pub(super) cabac_init_flag: bool,
    /// `five_minus_max_num_merge_cand` (P slices only; `0` for I slices, where the field is not
    /// present at all).
    pub(super) five_minus_max_num_merge_cand: u32,
    /// `slice_qp_delta`.
    pub(super) slice_qp_delta: i32,
    /// `slice_cb_qp_offset` (`0` when `pps.pps_slice_chroma_qp_offsets_present_flag` is unset).
    pub(super) slice_cb_qp_offset: i32,
    /// `slice_cr_qp_offset`.
    pub(super) slice_cr_qp_offset: i32,
    /// `slice_loop_filter_across_slices_enabled_flag` — present whenever
    /// `pps.pps_loop_filter_across_slices_enabled_flag` is set (this crate's own PPS/slice scope
    /// cuts make the real spec condition always simplify to that flag alone — see this ADR's own
    /// § Slice-header parser extension, step 7).
    pub(super) slice_loop_filter_across_slices_enabled_flag: bool,
    /// Number of bits `short_term_ref_pic_set()` occupied in this slice header — `0` for IDR
    /// pictures (no RPS read at all). `VAPictureParameterBufferHEVC::st_rps_bits`.
    pub(super) st_rps_bits: u32,
    /// Bits consumed parsing this header, counted from the start of `rbsp` (i.e. *excluding* the
    /// 2-byte NAL header) — measured immediately after `byte_alignment()` completes, so this
    /// value is always an exact multiple of `8`. Callers building
    /// `SliceParameterBufferHEVC::slice_data_byte_offset` add `2` for the NAL header bytes (see
    /// this ADR's own `slice_data_byte_offset` formula).
    pub(super) bits_consumed: usize,
}

impl HevcSliceSegmentHeader {
    /// Parse a slice-segment-header from `reader`, positioned at the first bit after the 2-byte
    /// NAL header (RBSP already de-emulated, e.g. via [`super::hevc_nal::HevcNalUnit::parse`]).
    ///
    /// # Errors
    ///
    /// [`DecodeError::Unsupported`] for B-slices, `first_slice_segment_in_pic_flag == 0`
    /// (multi-slice pictures), `short_term_ref_pic_set_sps_flag == 1` (SPS-level RPS list,
    /// always empty given this crate's own [`HevcSps::parse`] scope cut), any short-term RPS
    /// shape other than this crate's single-forward-reference requirement (see
    /// [`ShortTermRefPicSet::is_single_forward_reference`]), `num_ref_idx_l0_active != 1` on a P
    /// slice, `pps.weighted_pred_flag` set on a P slice (`pred_weight_table()` is not parsed —
    /// mirrors this crate's H.264 [`super::slice`]'s identical disposition), or
    /// `alignment_bit_equal_to_one != 1`. Other [`DecodeError::InvalidInput`] on truncated data.
    #[allow(
        clippy::too_many_lines,
        reason = "linear ITU-T H.265 § 7.3.6.1 syntax-element sequence through every reachable \
                  field this crate's own accepted-stream shape has — mirrors \
                  vulkan::HevcSliceSegmentHeader::parse's identical allow, extended well past \
                  that function's own documented stopping point (see module doc)"
    )]
    pub(super) fn parse(
        r: &mut BitReader<'_>,
        sps: &HevcSps,
        pps: &HevcPps,
        is_idr: bool,
    ) -> Result<Self, DecodeError> {
        let map_err = |_| DecodeError::InvalidInput;

        let first_slice_segment_in_pic_flag = r.read_bit().map_err(map_err)? != 0;
        if !first_slice_segment_in_pic_flag {
            return Err(DecodeError::Unsupported);
        }
        if is_idr {
            let _no_output_of_prior_pics_flag = r.read_bit().map_err(map_err)?;
        }
        let slice_pic_parameter_set_id = r.read_ue().map_err(map_err)?;
        // first_slice_segment_in_pic_flag is required `true` above, so
        // dependent_slice_segment_flag/slice_segment_address (only present otherwise) are never
        // read.
        for _ in 0..pps.num_extra_slice_header_bits {
            let _slice_reserved_flag = r.read_bit().map_err(map_err)?;
        }
        let slice_type = HevcSliceType::from_raw(r.read_ue().map_err(map_err)?)
            .ok_or(DecodeError::InvalidInput)?;
        if matches!(slice_type, HevcSliceType::B) {
            return Err(DecodeError::Unsupported);
        }
        let is_p_slice = matches!(slice_type, HevcSliceType::P);
        if pps.output_flag_present_flag {
            let _pic_output_flag = r.read_bit().map_err(map_err)?;
        }
        // separate_colour_plane_flag is always 0 in this crate's scope (chroma_format_idc == 1,
        // never 3 — see HevcSps::parse), so no colour_plane_id bits follow.

        let mut pic_order_cnt_lsb = None;
        let mut short_term_rps = None;
        let mut st_rps_bits = 0u32;
        let mut slice_temporal_mvp_enabled_flag = false;
        if !is_idr {
            pic_order_cnt_lsb = Some(
                r.read_bits(sps.log2_max_pic_order_cnt_lsb)
                    .map_err(map_err)?,
            );
            let short_term_ref_pic_set_sps_flag = r.read_bit().map_err(map_err)? != 0;
            if short_term_ref_pic_set_sps_flag {
                return Err(DecodeError::Unsupported);
            }
            let bits_before_rps = r.bits_read();
            let rps = ShortTermRefPicSet::parse(r)?;
            st_rps_bits = u32::try_from(r.bits_read() - bits_before_rps)
                .map_err(|_| DecodeError::InvalidInput)?;
            if !rps.is_single_forward_reference() {
                return Err(DecodeError::Unsupported);
            }
            short_term_rps = Some(rps);
            // long_term_ref_pics_present_flag is always false (rejected upstream at SPS parse,
            // see HevcSps::parse), so no long-term RPS syntax follows.
            if sps.sps_temporal_mvp_enabled_flag {
                slice_temporal_mvp_enabled_flag = r.read_bit().map_err(map_err)? != 0;
            }
        }

        let (slice_sao_luma_flag, slice_sao_chroma_flag) =
            if sps.sample_adaptive_offset_enabled_flag {
                let luma = r.read_bit().map_err(map_err)? != 0;
                // chroma_format_idc == 1 (never 0) in this crate's scope, so
                // slice_sao_chroma_flag is always present alongside slice_sao_luma_flag.
                let chroma = r.read_bit().map_err(map_err)? != 0;
                (luma, chroma)
            } else {
                (false, false)
            };

        let (num_ref_idx_l0_active, cabac_init_flag, five_minus_max_num_merge_cand) = if is_p_slice
        {
            if pps.weighted_pred_flag {
                // pred_weight_table() is not parsed by this crate's P-slice header reader —
                // reject honestly *before* reading any further P-slice-specific fields rather
                // than silently misaligning the bit position downstream. Mirrors this crate's
                // H.264 sibling's identical `weighted_pred_flag`/`entropy_coding_mode_flag`
                // rejection.
                return Err(DecodeError::Unsupported);
            }
            let num_ref_idx_active_override_flag = r.read_bit().map_err(map_err)? != 0;
            let num_ref_idx_l0_active = if num_ref_idx_active_override_flag {
                r.read_ue()
                    .map_err(map_err)?
                    .checked_add(1)
                    .ok_or(DecodeError::InvalidInput)?
            } else {
                pps.num_ref_idx_l0_default_active
            };
            if num_ref_idx_l0_active != 1 {
                // Single-forward-reference scope only — mirrors this crate's H.264 sibling's
                // identical num_ref_idx_l0_active-must-equal-1 scope cut.
                return Err(DecodeError::Unsupported);
            }
            // pps.lists_modification_present_flag && NumPicTotalCurr > 1 gates
            // ref_pic_lists_modification() — NumPicTotalCurr is always exactly 1 here (the RPS
            // shape validated above guarantees it), so that syntax is provably unreachable
            // regardless of pps.lists_modification_present_flag's own value; no bits are read
            // for it. mvd_l1_zero_flag is B-slice-only, also unreachable (B rejected above).
            let cabac_init_flag = if pps.cabac_init_present_flag {
                r.read_bit().map_err(map_err)? != 0
            } else {
                false
            };
            // slice_temporal_mvp_enabled_flag (parsed above, non-IDR only): for a P slice,
            // collocated_from_l0_flag is not read (inferred 1, only B slices read it), and
            // collocated_ref_idx is only read if num_ref_idx_l0_active_minus1 > 0 — also
            // provably unreachable here (num_ref_idx_l0_active is always exactly 1).
            let five_minus_max_num_merge_cand = r.read_ue().map_err(map_err)?;
            (
                num_ref_idx_l0_active,
                cabac_init_flag,
                five_minus_max_num_merge_cand,
            )
        } else {
            (0u32, false, 0u32)
        };

        let slice_qp_delta = r.read_se().map_err(map_err)?;
        let (slice_cb_qp_offset, slice_cr_qp_offset) =
            if pps.pps_slice_chroma_qp_offsets_present_flag {
                (r.read_se().map_err(map_err)?, r.read_se().map_err(map_err)?)
            } else {
                (0, 0)
            };
        // deblocking_filter_control_present_flag's sub-syntax is provably unreachable — this
        // ADR's own hevc_pps.rs rejects any PPS with that flag set to 1.

        // pps.pps_loop_filter_across_slices_enabled_flag && (sao_luma || sao_chroma ||
        // !disable_deblocking_filter_flag): since deblocking is always unreachable/disabled
        // (disable_deblocking_filter_flag's spec-default is 0), !disable_deblocking_filter_flag
        // is always true — this condition always simplifies to
        // pps.pps_loop_filter_across_slices_enabled_flag alone for any stream this crate accepts.
        let slice_loop_filter_across_slices_enabled_flag =
            if pps.pps_loop_filter_across_slices_enabled_flag {
                r.read_bit().map_err(map_err)? != 0
            } else {
                false
            };

        // Tile/WPP entry-point-offset syntax and slice_segment_header_extension_present_flag's
        // syntax are provably unreachable — both rejected upstream at PPS parse.

        let alignment_bit_equal_to_one = r.read_bit().map_err(map_err)? != 0;
        if !alignment_bit_equal_to_one {
            return Err(DecodeError::Unsupported);
        }
        let pad_bits = (8 - (r.bits_read() % 8)) % 8;
        if pad_bits > 0 {
            let _alignment_bit_equal_to_zero = r
                .read_bits(u32::try_from(pad_bits).map_err(|_| DecodeError::InvalidInput)?)
                .map_err(map_err)?;
        }
        let bits_consumed = r.bits_read();

        Ok(Self {
            slice_type,
            slice_pic_parameter_set_id,
            pic_order_cnt_lsb,
            short_term_rps,
            slice_sao_luma_flag,
            slice_sao_chroma_flag,
            slice_temporal_mvp_enabled_flag,
            num_ref_idx_l0_active,
            cabac_init_flag,
            five_minus_max_num_merge_cand,
            slice_qp_delta,
            slice_cb_qp_offset,
            slice_cr_qp_offset,
            slice_loop_filter_across_slices_enabled_flag,
            st_rps_bits,
            bits_consumed,
        })
    }
}
