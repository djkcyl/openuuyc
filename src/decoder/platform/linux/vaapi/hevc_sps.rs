//! HEVC SPS fields needed to fill VA-API's HEVC decode parameter buffers (ITU-T H.265 §
//! 7.3.2.2.1). Starts from `vulkan::hevc_params::HevcSps::parse` (cited, not imported — see
//! `adr/linux/0003-vaapi-hevc-p-slice-dpb.md` § Alternatives Considered), reusing its parse
//! sequence almost verbatim through `strong_intra_smoothing_enabled_flag` — every field this
//! crate's own decode needs is already parsed and retained there. Drops the fields that porting
//! source needed only for Vulkan's `StdVideoH265ProfileTierLevel`/`StdVideoH265LevelIdc`
//! conversion (VA-API decode negotiates profile via `Display::query_config_profiles`/
//! `create_config`, not a parameter-buffer field — see `codec.rs::hevc_profile_candidates`),
//! keeping only `general_profile_idc` from `profile_tier_level()`.
//!
//! Scope matches this crate's H.264 sibling ([`super::sps`]) and this ADR: 4:2:0 8-bit only,
//! Main profile, single tile, no WPP, no scaling lists, no PCM, single sub-layer, no SPS-level
//! short-term/long-term reference-picture-set lists (every accepted stream signals its
//! short-term RPS per-picture instead — see [`super::hevc_slice`]).

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "every count here comes from an Exp-Golomb-decoded HEVC syntax element, always \
              small in practice — mirrors vulkan::hevc_params's identical allow"
)]

use crate::decoder::platform::DecodeError;
use crate::decoder::platform::bitstream::BitReader;

/// Parsed HEVC SPS fields this crate's VA-API decode session needs.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is a real, independent ITU-T H.265 SPS flag that must be echoed into \
              PictureParameterBufferHEVC exactly as signaled — mirrors vulkan::HevcSps's \
              identical allow"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HevcSps {
    /// `general_profile_idc` (from `profile_tier_level`) — used only to pick a `VAProfile`
    /// candidate ([`super::codec::hevc_profile_candidates`]); VA-API decode's own parameter
    /// buffers carry no profile field at all (see module doc).
    pub(super) general_profile_idc: u8,
    /// `pic_width_in_luma_samples`.
    pub(super) pic_width_in_luma_samples: u32,
    /// `pic_height_in_luma_samples`.
    pub(super) pic_height_in_luma_samples: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4 + 4`.
    pub(super) log2_max_pic_order_cnt_lsb: u32,
    /// `sps_max_dec_pic_buffering_minus1[0] + 1` (single sub-layer only, enforced by
    /// [`HevcSps::parse`]).
    pub(super) max_dec_pic_buffering: u32,
    /// `log2_min_luma_coding_block_size_minus3 + 3`.
    pub(super) log2_min_cb_size: u32,
    /// `log2_diff_max_min_luma_coding_block_size`.
    pub(super) log2_diff_max_min_cb_size: u32,
    /// `log2_min_luma_transform_block_size_minus2 + 2`.
    pub(super) log2_min_tb_size: u32,
    /// `log2_diff_max_min_luma_transform_block_size`.
    pub(super) log2_diff_max_min_tb_size: u32,
    /// `max_transform_hierarchy_depth_inter`.
    pub(super) max_transform_hierarchy_depth_inter: u32,
    /// `max_transform_hierarchy_depth_intra`.
    pub(super) max_transform_hierarchy_depth_intra: u32,
    /// `amp_enabled_flag` — must be echoed exactly into `PictureParameterBufferHEVC`'s
    /// `pic_fields`, same "must match or desync the driver's own parser" reasoning as
    /// `vulkan::hevc_params::HevcSps::amp_enabled_flag`'s identical doc.
    pub(super) amp_enabled_flag: bool,
    /// Must be echoed exactly, and gates a real per-slice bit in **every** slice header this
    /// crate parses ([`super::hevc_slice`]) — the same real hardware lesson
    /// `vulkan::hevc_params::HevcSps::sample_adaptive_offset_enabled_flag`'s doc already
    /// recorded.
    pub(super) sample_adaptive_offset_enabled_flag: bool,
    /// Gates `slice_temporal_mvp_enabled_flag` in the slice header (non-IDR only) — must be
    /// echoed exactly, same reasoning as `amp_enabled_flag`.
    pub(super) sps_temporal_mvp_enabled_flag: bool,
    /// Does not add slice-header syntax bits, but changes decoded intra-prediction sample
    /// values for 32x32 intra blocks — echoed for output correctness.
    pub(super) strong_intra_smoothing_enabled_flag: bool,
}

impl HevcSps {
    /// Parse an SPS RBSP (2-byte NAL header already stripped, e.g. via
    /// [`super::hevc_nal::HevcNalUnit::parse`]).
    ///
    /// # Errors
    ///
    /// [`DecodeError::Unsupported`] when `sps_max_sub_layers_minus1 != 0` (temporal
    /// scalability), `chroma_format_idc != 1`, `scaling_list_enabled_flag == 1`,
    /// `pcm_enabled_flag == 1`, or `num_short_term_ref_pic_sets > 0`/
    /// `long_term_ref_pics_present_flag == 1` (SPS-level RPS lists are not parsed — see module
    /// doc). Other [`DecodeError::InvalidInput`] on truncated/overflowing data.
    #[allow(
        clippy::too_many_lines,
        reason = "linear ITU-T H.265 § 7.3.2.2.1 syntax-element sequence through the fields this \
                  crate's decode session needs — mirrors vulkan::HevcSps::parse's identical allow"
    )]
    #[allow(
        clippy::similar_names,
        reason = "log2_min_cb_size/log2_min_tb_size and log2_diff_max_min_cb_size/ \
                  log2_diff_max_min_tb_size are the real ITU-T H.265 § 7.3.2.2.1 syntax element \
                  names (coding-block vs transform-block size) — mirrors vulkan::HevcSps::parse's \
                  identical allow"
    )]
    pub(super) fn parse(rbsp: &[u8]) -> Result<Self, DecodeError> {
        let mut r = BitReader::new(rbsp);
        let map_err = |_| DecodeError::InvalidInput;

        let _sps_video_parameter_set_id = r.read_bits(4).map_err(map_err)?;
        let sps_max_sub_layers_minus1 = r.read_bits(3).map_err(map_err)?;
        if sps_max_sub_layers_minus1 != 0 {
            return Err(DecodeError::Unsupported);
        }
        let _sps_temporal_id_nesting_flag = r.read_bit().map_err(map_err)?;
        let general_profile_idc = parse_general_profile_idc(&mut r)?;

        let _sps_seq_parameter_set_id = r.read_ue().map_err(map_err)?;
        let chroma_format_idc = r.read_ue().map_err(map_err)?;
        if chroma_format_idc == 3 {
            let _separate_colour_plane_flag = r.read_bit().map_err(map_err)?;
        }
        if chroma_format_idc != 1 {
            return Err(DecodeError::Unsupported);
        }
        let pic_width_in_luma_samples = r.read_ue().map_err(map_err)?;
        let pic_height_in_luma_samples = r.read_ue().map_err(map_err)?;
        if r.read_bit().map_err(map_err)? != 0 {
            // conformance_window_flag
            let _conf_win_left_offset = r.read_ue().map_err(map_err)?;
            let _conf_win_right_offset = r.read_ue().map_err(map_err)?;
            let _conf_win_top_offset = r.read_ue().map_err(map_err)?;
            let _conf_win_bottom_offset = r.read_ue().map_err(map_err)?;
        }
        let _bit_depth_luma_minus8 = r.read_ue().map_err(map_err)?;
        let _bit_depth_chroma_minus8 = r.read_ue().map_err(map_err)?;
        let log2_max_pic_order_cnt_lsb = r
            .read_ue()
            .map_err(map_err)?
            .checked_add(4)
            .ok_or(DecodeError::InvalidInput)?;

        let sps_sub_layer_ordering_info_present_flag = r.read_bit().map_err(map_err)? != 0;
        let start = if sps_sub_layer_ordering_info_present_flag {
            0
        } else {
            sps_max_sub_layers_minus1
        };
        let mut max_dec_pic_buffering = 1u32;
        for _ in start..=sps_max_sub_layers_minus1 {
            max_dec_pic_buffering = r
                .read_ue()
                .map_err(map_err)?
                .checked_add(1)
                .ok_or(DecodeError::InvalidInput)?;
            let _sps_max_num_reorder_pics = r.read_ue().map_err(map_err)?;
            let _sps_max_latency_increase_plus1 = r.read_ue().map_err(map_err)?;
        }

        let log2_min_cb_size = r
            .read_ue()
            .map_err(map_err)?
            .checked_add(3)
            .ok_or(DecodeError::InvalidInput)?;
        let log2_diff_max_min_cb_size = r.read_ue().map_err(map_err)?;
        let log2_min_tb_size = r
            .read_ue()
            .map_err(map_err)?
            .checked_add(2)
            .ok_or(DecodeError::InvalidInput)?;
        let log2_diff_max_min_tb_size = r.read_ue().map_err(map_err)?;
        let max_transform_hierarchy_depth_inter = r.read_ue().map_err(map_err)?;
        let max_transform_hierarchy_depth_intra = r.read_ue().map_err(map_err)?;

        if r.read_bit().map_err(map_err)? != 0 {
            return Err(DecodeError::Unsupported);
        }
        let amp_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let sample_adaptive_offset_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let pcm_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        if pcm_enabled_flag {
            return Err(DecodeError::Unsupported);
        }

        let num_short_term_ref_pic_sets = r.read_ue().map_err(map_err)?;
        if num_short_term_ref_pic_sets > 0 {
            return Err(DecodeError::Unsupported);
        }
        let long_term_ref_pics_present_flag = r.read_bit().map_err(map_err)? != 0;
        if long_term_ref_pics_present_flag {
            return Err(DecodeError::Unsupported);
        }
        let sps_temporal_mvp_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        let strong_intra_smoothing_enabled_flag = r.read_bit().map_err(map_err)? != 0;
        // vui_parameters_present_flag / sps_extension_present_flag follow — not needed for this
        // crate's decode path (mirrors vulkan::HevcSps::parse's identical stopping point).

        Ok(Self {
            general_profile_idc,
            pic_width_in_luma_samples,
            pic_height_in_luma_samples,
            log2_max_pic_order_cnt_lsb,
            max_dec_pic_buffering,
            log2_min_cb_size,
            log2_diff_max_min_cb_size,
            log2_min_tb_size,
            log2_diff_max_min_tb_size,
            max_transform_hierarchy_depth_inter,
            max_transform_hierarchy_depth_intra,
            amp_enabled_flag,
            sample_adaptive_offset_enabled_flag,
            sps_temporal_mvp_enabled_flag,
            strong_intra_smoothing_enabled_flag,
        })
    }
}

/// Parse `profile_tier_level(profilePresentFlag=1, maxNumSubLayersMinus1=0)` (ITU-T H.265
/// § 7.3.3) far enough to extract `general_profile_idc` — every other field is read (to advance
/// the bit position correctly for the SPS fields that follow) and discarded, since VA-API
/// decode negotiates profile via `Display::query_config_profiles`/`create_config`, not a
/// parameter-buffer field (see module doc). `max_num_sub_layers_minus1` is always `0` in this
/// crate's scope (enforced by the caller before this function runs, mirrors
/// `vulkan::hevc_params::hevc_ptl::parse_profile_tier_level`'s own sub-layer loop, which is
/// likewise always a no-op here), so no sub-layer profile/level bits are read.
fn parse_general_profile_idc(r: &mut BitReader<'_>) -> Result<u8, DecodeError> {
    let map_err = |_| DecodeError::InvalidInput;
    let _general_profile_space = r.read_bits(2).map_err(map_err)?;
    let _general_tier_flag = r.read_bit().map_err(map_err)?;
    let general_profile_idc = r.read_bits(5).map_err(map_err)? as u8;
    for _ in 0..32 {
        let _general_profile_compatibility_flag = r.read_bit().map_err(map_err)?;
    }
    let _general_progressive_source_flag = r.read_bit().map_err(map_err)?;
    let _general_interlaced_source_flag = r.read_bit().map_err(map_err)?;
    let _general_non_packed_constraint_flag = r.read_bit().map_err(map_err)?;
    let _general_frame_only_constraint_flag = r.read_bit().map_err(map_err)?;
    // 44 general reserved/constraint bits (32 + 12), then general_level_idc — same bit layout
    // `vulkan::hevc_params::hevc_ptl::parse_profile_tier_level` reads.
    let _ = r.read_bits(32).map_err(map_err)?;
    let _ = r.read_bits(12).map_err(map_err)?;
    let _general_level_idc = r.read_bits(8).map_err(map_err)?;
    Ok(general_profile_idc)
}
