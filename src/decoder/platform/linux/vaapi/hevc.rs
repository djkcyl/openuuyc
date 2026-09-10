//! VA-API HEVC decode session — IDR I-slices and single-forward-reference P-slices, single
//! slice per picture, progressive, CPU NV12 output.
//!
//! See [ADR-0001](../../adr/0001-vaapi-h264-cpu-out.md) for this backend's original H.264-only
//! scope, [ADR-0002](../../adr/linux/0002-vaapi-h264-p-slice-dpb.md) for the H.264
//! sliding-window DPB sibling, and
//! [ADR-0003](../../adr/linux/0003-vaapi-hevc-p-slice-dpb.md) for this file's own design: a
//! **fresh** (not ported — no hardware-verified HEVC P-slice decode exists anywhere in this
//! workspace to port) single-slot DPB ([`hevc_dpb`]), grounded in `FFmpeg`'s real
//! `libavcodec/vaapi_hevc.c` conventions. Structurally mirrors [`super::h264`]'s
//! `Pipeline`/`decode_picture`/`decode_one` shape.
//!
//! Unlike this crate's sibling Vulkan HEVC decode module, VA-API's own
//! `PictureParameterBufferHEVC` carries **no VPS-derived field at all** (confirmed by reading
//! `cros-libva`'s real vendored source directly) — so this crate needs no `hevc_vps.rs`; VPS
//! NAL units are parsed by neither this crate nor the driver's own parameter-buffer contract and
//! are simply ignored (`push_packet`'s `HevcNalUnitType::Vps` arm is a no-op), a real
//! simplification versus the Vulkan decode module's `HevcVps`/`StdVideoH265VideoParameterSet`
//! requirement.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::decoder::platform::bitstream::{BitReader, split_annex_b};
use crate::decoder::platform::{
    DecodeError, VideoDecoder, VideoDecoderConfig, VideoOutputPreference,
};
use mediaway_common::{Bytes, Packet, PixelFormat, VideoFrame, VideoFrameStorage};

use cros_libva::{
    BufferType, Config, Context, Display, HevcLongSliceFlags, HevcPicFields,
    HevcSliceParsingFields, IQMatrix, IQMatrixBufferHEVC, Picture, PictureHEVC, PictureParameter,
    PictureParameterBufferHEVC, SliceParameter, SliceParameterBufferHEVC, Surface, VA_FOURCC_NV12,
    VA_INVALID_SURFACE, VA_PICTURE_HEVC_INVALID, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
    VA_RT_FORMAT_YUV420, VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAImageFormat,
};

use super::codec::hevc_profile_candidates;
use super::dpb::derive_pic_order_cnt_msb;
use super::hevc_dpb::{self, HEVC_SURFACE_POOL_SIZE, HevcDpb};
use super::hevc_nal::{HevcNalUnit, HevcNalUnitType};
use super::hevc_pps::HevcPps;
use super::hevc_slice::HevcSliceSegmentHeader;
use super::hevc_sps::HevcSps;
use super::nv12::copy_nv12_from_planes;

/// A decode pipeline bound to one negotiated profile + coded resolution, created lazily the
/// first time an SPS is available — mirrors [`super::h264::Pipeline`]'s identical role.
struct HevcPipeline {
    /// Kept alive for the context's lifetime — mirrors `super::h264::Pipeline::_config`'s
    /// identical rationale.
    _config: Config,
    context: Rc<Context>,
    /// DPB-slot-indexed: `surfaces[i]` is the physical surface backing slot `i`. Fixed
    /// [`HEVC_SURFACE_POOL_SIZE`] (`3`), not SPS-sized — see [`hevc_dpb`]'s module doc for why.
    surfaces: Vec<Option<Surface<()>>>,
    dpb: HevcDpb,
    /// Round-robin cursor for [`hevc_dpb::allocate_slot`].
    next_slot: usize,
    coded_width: u32,
    coded_height: u32,
    nv12_format: VAImageFormat,
}

/// VA-API HEVC decode session. See module docs for scope.
pub(crate) struct VaapiHevcDecoder {
    display: Rc<Display>,
    pipeline: Option<HevcPipeline>,
    sps: Option<HevcSps>,
    pps: Option<HevcPps>,
    declared_width: u32,
    declared_height: u32,
    pending: VecDeque<VideoFrame>,
    /// Carried across pictures for `derive_pic_order_cnt_msb` (ITU-T H.265 § 8.3.1, the same
    /// MSB/LSB-wraparound formula this crate's own H.264 sibling already implements — reused,
    /// not re-derived, see [`hevc_dpb`]'s module doc); reset to `0` on every IDR. HEVC has no
    /// field-coding pair to worry about, so `PicOrderCntVal` needs no top/bottom duplication the
    /// way H.264's does.
    prev_poc_msb: i32,
    prev_poc_lsb: u32,
}

impl VaapiHevcDecoder {
    /// Open per [`VideoDecoderConfig::output`].
    pub(crate) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        validate(config)?;
        if config.output != VideoOutputPreference::CpuFramesOk {
            // Zero-Copy DMA-BUF export deferred — see ADR-0001 § Scope.
            return Err(DecodeError::Unsupported);
        }

        // See `super::h264::VaapiH264Decoder::open`'s identical doc comment for why
        // `Display::open()` honestly returns `None` in this session's environment.
        let display: Rc<Display> = Display::open().ok_or(DecodeError::Unsupported)?;

        let (sps, pps) = seed_params(&config.extra_data);

        Ok(Self {
            display,
            pipeline: None,
            sps,
            pps,
            declared_width: config.width,
            declared_height: config.height,
            pending: VecDeque::new(),
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
        })
    }

    /// Create the `Config`/`Surface`/`Context`/`HevcDpb` pipeline on first use — mirrors
    /// [`super::h264::VaapiH264Decoder::ensure_pipeline`]'s identical role and "no dynamic
    /// resolution/profile renegotiation mid-session" disposition.
    fn ensure_pipeline(&mut self, sps: &HevcSps) -> Result<(), DecodeError> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        let coded_width = sps.pic_width_in_luma_samples;
        let coded_height = sps.pic_height_in_luma_samples;
        if coded_width == 0 || coded_height == 0 {
            return Err(DecodeError::InvalidInput);
        }
        if self.declared_width != 0
            && self.declared_height != 0
            && (coded_width > round_up_8(self.declared_width)
                || coded_height > round_up_8(self.declared_height))
        {
            return Err(DecodeError::InvalidInput);
        }

        let candidates = hevc_profile_candidates(sps.general_profile_idc)?;
        let supported = self
            .display
            .query_config_profiles()
            .map_err(|_| DecodeError::Backend)?;
        let mut chosen = None;
        for profile in candidates {
            if !supported.contains(&profile) {
                continue;
            }
            let entrypoints = self
                .display
                .query_config_entrypoints(profile)
                .map_err(|_| DecodeError::Backend)?;
            if entrypoints.contains(&VAEntrypoint::VAEntrypointVLD) {
                chosen = Some(profile);
                break;
            }
        }
        let profile = chosen.ok_or(DecodeError::Unsupported)?;

        let attrs = vec![VAConfigAttrib {
            type_: VAConfigAttribType::VAConfigAttribRTFormat,
            value: VA_RT_FORMAT_YUV420,
        }];
        let config = self
            .display
            .create_config(attrs, profile, VAEntrypoint::VAEntrypointVLD)
            .map_err(|_| DecodeError::Backend)?;

        let surfaces = self
            .display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(VA_FOURCC_NV12),
                coded_width,
                coded_height,
                None,
                vec![(); HEVC_SURFACE_POOL_SIZE],
            )
            .map_err(|_| DecodeError::Backend)?;

        let context = self
            .display
            .create_context(&config, coded_width, coded_height, Some(&surfaces), true)
            .map_err(|_| DecodeError::Backend)?;

        let nv12_format = self
            .display
            .query_image_formats()
            .map_err(|_| DecodeError::Backend)?
            .into_iter()
            .find(|f| f.fourcc == VA_FOURCC_NV12)
            .ok_or(DecodeError::Unsupported)?;

        self.pipeline = Some(HevcPipeline {
            _config: config,
            context,
            surfaces: surfaces.into_iter().map(Some).collect(),
            dpb: HevcDpb::new(),
            next_slot: 0,
            coded_width,
            coded_height,
            nv12_format,
        });
        Ok(())
    }

    /// Decode one I or P picture. Mirrors [`super::h264::VaapiH264Decoder::decode_picture`]'s
    /// per-picture ordering, simplified for this crate's own single-slot DPB (no sliding-window
    /// eviction, no `FrameNumWrap` refresh step): clear/reset on IDR, derive POC, resolve the
    /// sole reference (if any) from `HevcDpb`, allocate the destination slot, build parameter
    /// buffers + submit, then register the decoded picture as the new tracked reference if
    /// applicable.
    #[allow(
        clippy::too_many_lines,
        reason = "linear per-picture decode sequence — mirrors super::h264::decode_picture's \
                  identical allow/reasoning"
    )]
    #[allow(
        clippy::similar_names,
        reason = "poc_msb/poc_lsb name the two halves of one ITU-T H.265 § 8.3.1 state pair \
                  (PicOrderCntMsb, pic_order_cnt_lsb) — matching, not confusable, names, mirrors \
                  super::dpb::derive_pic_order_cnt_msb's identical allow"
    )]
    fn decode_picture(
        &mut self,
        packet: &Packet,
        original_nal: &[u8],
        is_idr: bool,
        is_reference: bool,
        sps: &HevcSps,
        pps: &HevcPps,
        header: &HevcSliceSegmentHeader,
    ) -> Result<VideoFrame, DecodeError> {
        if is_idr {
            self.pipeline
                .as_mut()
                .ok_or(DecodeError::Backend)?
                .dpb
                .clear();
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
        }

        let (pic_order_cnt, poc_msb, poc_lsb) = if is_idr {
            (0i32, 0i32, 0u32)
        } else {
            let poc_lsb = header.pic_order_cnt_lsb.ok_or(DecodeError::InvalidInput)?;
            let max_poc_lsb = 1u32
                .checked_shl(sps.log2_max_pic_order_cnt_lsb)
                .ok_or(DecodeError::InvalidInput)?;
            let poc_msb = derive_pic_order_cnt_msb(
                poc_lsb,
                self.prev_poc_msb,
                self.prev_poc_lsb,
                max_poc_lsb,
            );
            let poc = poc_msb
                .checked_add_unsigned(poc_lsb)
                .ok_or(DecodeError::InvalidInput)?;
            (poc, poc_msb, poc_lsb)
        };
        if is_reference {
            self.prev_poc_msb = poc_msb;
            self.prev_poc_lsb = poc_lsb;
        }

        let is_p_slice = header.short_term_rps.is_some();
        let reference_slot = if is_p_slice {
            let pipeline = self.pipeline.as_ref().ok_or(DecodeError::Backend)?;
            let (slot_index, dpb_slot) =
                pipeline.dpb.reference().ok_or(DecodeError::InvalidInput)?;
            let surface_id = pipeline.surfaces[*slot_index]
                .as_ref()
                .ok_or(DecodeError::Backend)?
                .id();
            Some((surface_id, dpb_slot.pic_order_cnt))
        } else {
            None
        };

        let (dst_slot_index, surface) = {
            let pipeline = self.pipeline.as_mut().ok_or(DecodeError::Backend)?;
            let index = hevc_dpb::allocate_slot(&mut pipeline.next_slot, &pipeline.dpb);
            let surface = pipeline.surfaces[index]
                .take()
                .ok_or(DecodeError::Backend)?;
            (index, surface)
        };

        let (returned_surface, result) = self.decode_one(
            surface,
            packet,
            original_nal,
            is_idr,
            sps,
            pps,
            header,
            pic_order_cnt,
            reference_slot,
        );
        if let Some(pipeline) = self.pipeline.as_mut() {
            pipeline.surfaces[dst_slot_index] = Some(returned_surface);
        }
        let frame = result?;

        if is_reference {
            self.pipeline
                .as_mut()
                .ok_or(DecodeError::Backend)?
                .dpb
                .set_reference(dst_slot_index, pic_order_cnt);
        }

        Ok(frame)
    }

    /// Build parameter buffers and run the VA-API decode sequence for one already-allocated
    /// surface — mirrors [`super::h264::VaapiH264Decoder::decode_one`]'s shape and `&self`
    /// (not `&mut self`) rationale.
    #[allow(clippy::too_many_arguments)]
    fn decode_one(
        &self,
        surface: Surface<()>,
        packet: &Packet,
        original_nal: &[u8],
        is_idr: bool,
        sps: &HevcSps,
        pps: &HevcPps,
        header: &HevcSliceSegmentHeader,
        pic_order_cnt: i32,
        reference_slot: Option<(cros_libva::VASurfaceID, i32)>,
    ) -> (Surface<()>, Result<VideoFrame, DecodeError>) {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return (surface, Err(DecodeError::Backend));
        };
        let context = Rc::clone(&pipeline.context);
        let coded_width = pipeline.coded_width;
        let coded_height = pipeline.coded_height;
        let nv12_format = pipeline.nv12_format;
        let surface_id = surface.id();

        let outcome = (|| -> Result<(Bytes, Surface<()>), DecodeError> {
            let pic_param = build_pic_param(
                sps,
                pps,
                is_idr,
                header,
                surface_id,
                pic_order_cnt,
                reference_slot,
            )?;
            let iq_matrix = IQMatrixBufferHEVC::new(
                [[16u8; 16]; 6],
                [[16u8; 64]; 6],
                [[16u8; 64]; 6],
                [[16u8; 64]; 2],
                [16u8; 6],
                [16u8; 2],
            );
            let slice_param = build_slice_param(header, original_nal.len(), reference_slot)?;
            let slice_data_bytes = original_nal.to_vec();

            let pic_param_buf = context
                .create_buffer(BufferType::PictureParameter(PictureParameter::HEVC(
                    pic_param,
                )))
                .map_err(|_| DecodeError::Backend)?;
            let iq_buf = context
                .create_buffer(BufferType::IQMatrix(IQMatrix::HEVC(iq_matrix)))
                .map_err(|_| DecodeError::Backend)?;
            let slice_param_buf = context
                .create_buffer(BufferType::SliceParameter(SliceParameter::HEVC(
                    slice_param,
                )))
                .map_err(|_| DecodeError::Backend)?;
            let slice_data_buf = context
                .create_buffer(BufferType::SliceData(slice_data_bytes))
                .map_err(|_| DecodeError::Backend)?;

            let timestamp = u64::try_from(packet.pts).unwrap_or(0);
            let mut picture = Picture::new(timestamp, Rc::clone(&context), surface);
            picture.add_buffer(pic_param_buf);
            picture.add_buffer(iq_buf);
            picture.add_buffer(slice_param_buf);
            picture.add_buffer(slice_data_buf);

            let picture = picture.begin::<()>().map_err(|_| DecodeError::Backend)?;
            let picture = picture.render().map_err(|_| DecodeError::Backend)?;
            let picture = picture.end().map_err(|_| DecodeError::Backend)?;
            let picture = picture
                .sync::<()>()
                .map_err(|(_e, _pic)| DecodeError::Backend)?;

            let image = picture
                .create_image::<()>(
                    nv12_format,
                    (coded_width, coded_height),
                    (coded_width, coded_height),
                )
                .map_err(|_| DecodeError::Backend)?;
            let va_image = *image.image();
            let bytes = copy_nv12_from_planes(
                image.as_ref(),
                coded_width,
                coded_height,
                va_image.pitches[0],
                va_image.offsets[0],
                va_image.pitches[1],
                va_image.offsets[1],
            );
            drop(image);

            let surface = picture.take_surface().map_err(|_| DecodeError::Backend)?;
            Ok((bytes, surface))
        })();

        match outcome {
            Ok((data, returned_surface)) => {
                let frame = VideoFrame {
                    pts: packet.pts,
                    duration: packet.duration,
                    width: coded_width,
                    height: coded_height,
                    format: PixelFormat::Nv12,
                    storage: VideoFrameStorage::Cpu { data },
                };
                (returned_surface, Ok(frame))
            }
            Err(e) => (
                fresh_surface_or_placeholder(&context, coded_width, coded_height),
                Err(e),
            ),
        }
    }
}

impl VideoDecoder for VaapiHevcDecoder {
    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError> {
        if packet.is_discard {
            return Ok(());
        }
        let nals = split_annex_b(packet.payload.as_ref()).map_err(|_| DecodeError::InvalidInput)?;
        for nal in nals {
            let unit = HevcNalUnit::parse(nal)?;
            match unit.unit_type {
                HevcNalUnitType::Sps => {
                    self.sps = Some(HevcSps::parse(&unit.rbsp)?);
                }
                HevcNalUnitType::Pps => {
                    self.pps = Some(HevcPps::parse(&unit.rbsp)?);
                }
                HevcNalUnitType::Cra => {
                    // CRA / random-access pictures are a permanent scope cut this ADR does not
                    // decode (see `docs/roadmap.md`) — rejected honestly rather than silently
                    // skipped, since a CRA NAL carries real coded picture data this crate would
                    // otherwise drop.
                    return Err(DecodeError::Unsupported);
                }
                HevcNalUnitType::Idr | HevcNalUnitType::Trail => {
                    let is_idr = matches!(unit.unit_type, HevcNalUnitType::Idr);
                    let sps = self.sps.ok_or(DecodeError::InvalidInput)?;
                    let pps = self.pps.ok_or(DecodeError::InvalidInput)?;
                    let mut reader = BitReader::new(&unit.rbsp);
                    let header = HevcSliceSegmentHeader::parse(&mut reader, &sps, &pps, is_idr)?;
                    if header.slice_pic_parameter_set_id != pps.pps_pic_parameter_set_id {
                        return Err(DecodeError::InvalidInput);
                    }
                    self.ensure_pipeline(&sps)?;
                    let frame = self.decode_picture(
                        packet,
                        nal,
                        is_idr,
                        unit.is_reference,
                        &sps,
                        &pps,
                        &header,
                    )?;
                    self.pending.push_back(frame);
                }
                // VPS carries no field this crate's own parameter buffers need (see module
                // doc); SEI/AUD/other are ignored.
                HevcNalUnitType::Vps | HevcNalUnitType::Other(_) => {}
            }
        }
        Ok(())
    }

    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        Ok(self.pending.pop_front())
    }
}

/// `PictureParameterBufferHEVC` construction — every SPS/PPS-derived scalar field maps directly
/// onto this crate's own already-parsed `HevcSps`/`HevcPps` values; every flag argument is a
/// direct echo of an already-parsed field (see this ADR's own "must be echoed exactly" theme,
/// § VA-API-specific plumbing).
#[allow(
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "PictureParameterBufferHEVC::new takes every SPS/PPS-derived field positionally — \
              mirrors super::h264::build_pic_param's identical allow"
)]
fn build_pic_param(
    sps: &HevcSps,
    pps: &HevcPps,
    is_idr: bool,
    header: &HevcSliceSegmentHeader,
    surface_id: cros_libva::VASurfaceID,
    pic_order_cnt: i32,
    reference_slot: Option<(cros_libva::VASurfaceID, i32)>,
) -> Result<PictureParameterBufferHEVC, DecodeError> {
    let curr_pic = PictureHEVC::new(surface_id, pic_order_cnt, 0);
    let mut reference_frames: [PictureHEVC; 15] = std::array::from_fn(|_| invalid_picture_hevc());
    if let Some((ref_surface_id, ref_poc)) = reference_slot {
        reference_frames[0] =
            PictureHEVC::new(ref_surface_id, ref_poc, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE);
    }

    let pic_fields = HevcPicFields::new(
        1, // chroma_format_idc: 4:2:0 (enforced by HevcSps::parse)
        0, // separate_colour_plane_flag: always 0 (enforced by HevcSps::parse)
        0, // pcm_enabled_flag: rejected upstream if 1
        0, // scaling_list_enabled_flag: rejected upstream if 1
        u32::from(pps.transform_skip_enabled_flag),
        u32::from(sps.amp_enabled_flag),
        u32::from(sps.strong_intra_smoothing_enabled_flag),
        u32::from(pps.sign_data_hiding_enabled_flag),
        u32::from(pps.constrained_intra_pred_flag),
        u32::from(pps.cu_qp_delta_enabled_flag),
        u32::from(pps.weighted_pred_flag),
        u32::from(pps.weighted_bipred_flag),
        u32::from(pps.transquant_bypass_enabled_flag),
        0, // tiles_enabled_flag: rejected upstream if 1
        0, // entropy_coding_sync_enabled_flag: rejected upstream if 1
        u32::from(pps.pps_loop_filter_across_slices_enabled_flag),
        0, // loop_filter_across_tiles_enabled_flag: unreachable, tiles disabled
        0, // pcm_loop_filter_disabled_flag: unreachable, PCM disabled
        // Real VA-API decode *hint* fields (not H.265 bitstream syntax elements at all) —
        // honestly `1` given this ADR's own permanent "no reordering, no B-slices" scope, not
        // independently confirmed against how a real driver actually uses these hints (ADR-0003
        // § Open questions #3).
        1, // no_pic_reordering_flag
        1, // no_bi_pred_flag
    );

    let slice_parsing_fields = HevcSliceParsingFields::new(
        u32::from(pps.lists_modification_present_flag),
        0, // long_term_ref_pics_present_flag: rejected upstream if 1
        u32::from(sps.sps_temporal_mvp_enabled_flag),
        u32::from(pps.cabac_init_present_flag),
        u32::from(pps.output_flag_present_flag),
        u32::from(pps.dependent_slice_segments_enabled_flag),
        u32::from(pps.pps_slice_chroma_qp_offsets_present_flag),
        u32::from(sps.sample_adaptive_offset_enabled_flag),
        0, // deblocking_filter_override_enabled_flag: unreachable, deblocking control disabled
        0, // pps_disable_deblocking_filter_flag: unreachable
        0, // slice_segment_header_extension_present_flag: rejected upstream if 1
        u32::from(is_idr), // rap_pic_flag: only IDR pictures are intra/random-access this scope
        u32::from(is_idr), // idr_pic_flag
        u32::from(is_idr), // intra_pic_flag
    );

    let sps_max_dec_pic_buffering_minus1 =
        u8::try_from(sps.max_dec_pic_buffering.saturating_sub(1))
            .map_err(|_| DecodeError::InvalidInput)?;
    let pic_width_in_luma_samples =
        u16::try_from(sps.pic_width_in_luma_samples).map_err(|_| DecodeError::InvalidInput)?;
    let pic_height_in_luma_samples =
        u16::try_from(sps.pic_height_in_luma_samples).map_err(|_| DecodeError::InvalidInput)?;
    let log2_min_luma_coding_block_size_minus3 =
        u8::try_from(sps.log2_min_cb_size.saturating_sub(3))
            .map_err(|_| DecodeError::InvalidInput)?;
    let log2_diff_max_min_luma_coding_block_size =
        u8::try_from(sps.log2_diff_max_min_cb_size).map_err(|_| DecodeError::InvalidInput)?;
    let log2_min_transform_block_size_minus2 = u8::try_from(sps.log2_min_tb_size.saturating_sub(2))
        .map_err(|_| DecodeError::InvalidInput)?;
    let log2_diff_max_min_transform_block_size =
        u8::try_from(sps.log2_diff_max_min_tb_size).map_err(|_| DecodeError::InvalidInput)?;
    let max_transform_hierarchy_depth_intra = u8::try_from(sps.max_transform_hierarchy_depth_intra)
        .map_err(|_| DecodeError::InvalidInput)?;
    let max_transform_hierarchy_depth_inter = u8::try_from(sps.max_transform_hierarchy_depth_inter)
        .map_err(|_| DecodeError::InvalidInput)?;
    let init_qp_minus26 = i8::try_from(pps.init_qp - 26).map_err(|_| DecodeError::InvalidInput)?;
    let diff_cu_qp_delta_depth =
        u8::try_from(pps.diff_cu_qp_delta_depth).map_err(|_| DecodeError::InvalidInput)?;
    let pps_cb_qp_offset =
        i8::try_from(pps.pps_cb_qp_offset).map_err(|_| DecodeError::InvalidInput)?;
    let pps_cr_qp_offset =
        i8::try_from(pps.pps_cr_qp_offset).map_err(|_| DecodeError::InvalidInput)?;
    let log2_parallel_merge_level_minus2 = u8::try_from(pps.log2_parallel_merge_level_minus2)
        .map_err(|_| DecodeError::InvalidInput)?;
    let log2_max_pic_order_cnt_lsb_minus4 =
        u8::try_from(sps.log2_max_pic_order_cnt_lsb.saturating_sub(4))
            .map_err(|_| DecodeError::InvalidInput)?;
    let num_ref_idx_l0_default_active_minus1 =
        u8::try_from(pps.num_ref_idx_l0_default_active.saturating_sub(1))
            .map_err(|_| DecodeError::InvalidInput)?;
    let num_ref_idx_l1_default_active_minus1 =
        u8::try_from(pps.num_ref_idx_l1_default_active.saturating_sub(1))
            .map_err(|_| DecodeError::InvalidInput)?;
    let num_extra_slice_header_bits =
        u8::try_from(pps.num_extra_slice_header_bits).map_err(|_| DecodeError::InvalidInput)?;

    Ok(PictureParameterBufferHEVC::new(
        curr_pic,
        reference_frames,
        pic_width_in_luma_samples,
        pic_height_in_luma_samples,
        &pic_fields,
        sps_max_dec_pic_buffering_minus1,
        0, // bit_depth_luma_minus8: 8-bit only
        0, // bit_depth_chroma_minus8: 8-bit only
        0, // pcm_sample_bit_depth_luma_minus1: unused, PCM disabled
        0, // pcm_sample_bit_depth_chroma_minus1: unused
        log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size,
        0, // log2_min_pcm_luma_coding_block_size_minus3: unused
        0, // log2_diff_max_min_pcm_luma_coding_block_size: unused
        max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter,
        init_qp_minus26,
        diff_cu_qp_delta_depth,
        pps_cb_qp_offset,
        pps_cr_qp_offset,
        log2_parallel_merge_level_minus2,
        0,          // num_tile_columns_minus1: no tiles
        0,          // num_tile_rows_minus1: no tiles
        [0u16; 19], // column_width_minus1: no tiles
        [0u16; 21], // row_height_minus1: no tiles
        &slice_parsing_fields,
        log2_max_pic_order_cnt_lsb_minus4,
        0, // num_short_term_ref_pic_sets: SPS-level RPS lists rejected upstream
        0, // num_long_term_ref_pic_sps: rejected upstream
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        0, // pps_beta_offset_div2: deblocking control rejected upstream
        0, // pps_tc_offset_div2: deblocking control rejected upstream
        num_extra_slice_header_bits,
        header.st_rps_bits,
    ))
}

/// `SliceParameterBufferHEVC` construction. `ref_pic_list`'s `[u8; 15]` entries are **indices**
/// into `ReferenceFrames[]` (`0xFF` = invalid) — a real, structural asymmetry versus this
/// crate's own H.264 decode (`SliceParameterBufferH264::ref_pic_list_0`, full `PictureH264`
/// structs) confirmed against Intel's real `va_dec_hevc.h`.
fn build_slice_param(
    header: &HevcSliceSegmentHeader,
    nal_len: usize,
    reference_slot: Option<(cros_libva::VASurfaceID, i32)>,
) -> Result<SliceParameterBufferHEVC, DecodeError> {
    let mut ref_pic_list0 = [0xFFu8; 15];
    let ref_pic_list1 = [0xFFu8; 15];
    if reference_slot.is_some() {
        ref_pic_list0[0] = 0;
    }

    // ITU-T H.265 Table 7-7's own numeric convention (0 = B, 1 = P, 2 = I) — matches this
    // crate's own [`super::hevc_slice::HevcSliceType`] discriminant order.
    let slice_type: u8 = match header.slice_type {
        super::hevc_slice::HevcSliceType::B => 0,
        super::hevc_slice::HevcSliceType::P => 1,
        super::hevc_slice::HevcSliceType::I => 2,
    };

    let long_slice_flags = HevcLongSliceFlags::new(
        1, // last_slice_of_pic: single slice per picture (enforced by HevcSliceSegmentHeader::parse)
        0, // dependent_slice_segment_flag: always 0, first_slice_segment_in_pic_flag required true
        u32::from(slice_type),
        0, // color_plane_id: unused, chroma_format_idc != 3
        u32::from(header.slice_sao_luma_flag),
        u32::from(header.slice_sao_chroma_flag),
        0, // mvd_l1_zero_flag: unused, no B slices
        u32::from(header.cabac_init_flag),
        u32::from(header.slice_temporal_mvp_enabled_flag),
        0, // slice_deblocking_filter_disabled_flag: deblocking control rejected upstream
        1, // collocated_from_l0_flag: default L0 (inert unless temporal MVP is active)
        u32::from(header.slice_loop_filter_across_slices_enabled_flag),
    );

    let slice_data_size = u32::try_from(nal_len).map_err(|_| DecodeError::InvalidInput)?;
    // "byte offset from NAL unit header to the beginning of slice_data()" (libva doc) — the
    // 2-byte NAL header, plus this crate's own parser's exact bit count through
    // byte_alignment() (already an exact multiple of 8 — see `HevcSliceSegmentHeader::bits_consumed`'s
    // own doc).
    let slice_data_byte_offset =
        u32::try_from(2usize + header.bits_consumed / 8).map_err(|_| DecodeError::InvalidInput)?;
    let num_ref_idx_l0_active_minus1 = u8::try_from(header.num_ref_idx_l0_active.saturating_sub(1))
        .map_err(|_| DecodeError::InvalidInput)?;
    let slice_qp_delta =
        i8::try_from(header.slice_qp_delta).map_err(|_| DecodeError::InvalidInput)?;
    let slice_cb_qp_offset =
        i8::try_from(header.slice_cb_qp_offset).map_err(|_| DecodeError::InvalidInput)?;
    let slice_cr_qp_offset =
        i8::try_from(header.slice_cr_qp_offset).map_err(|_| DecodeError::InvalidInput)?;
    let five_minus_max_num_merge_cand = u8::try_from(header.five_minus_max_num_merge_cand)
        .map_err(|_| DecodeError::InvalidInput)?;

    Ok(SliceParameterBufferHEVC::new(
        slice_data_size,
        0, // slice_data_offset: the SliceData buffer *is* this NAL, offset 0
        0, // slice_data_flag = VA_SLICE_DATA_FLAG_ALL: whole slice is in the buffer
        slice_data_byte_offset,
        0, // slice_segment_address: single slice, first_slice_segment_in_pic_flag required true
        [ref_pic_list0, ref_pic_list1],
        &long_slice_flags,
        0, // collocated_ref_idx: unused, no temporal MVP in this crate's design
        num_ref_idx_l0_active_minus1,
        0, // num_ref_idx_l1_active_minus1: unused, no B slices
        slice_qp_delta,
        slice_cb_qp_offset,
        slice_cr_qp_offset,
        0, // slice_beta_offset_div2: deblocking control rejected upstream
        0, // slice_tc_offset_div2: deblocking control rejected upstream
        0, // luma_log2_weight_denom: unused, no weighted prediction
        0, // delta_chroma_log2_weight_denom: unused
        [0i8; 15],
        [0i8; 15],
        [[0i8; 2]; 15],
        [[0i8; 2]; 15],
        [0i8; 15],
        [0i8; 15],
        [[0i8; 2]; 15],
        [[0i8; 2]; 15],
        five_minus_max_num_merge_cand,
        0, // num_entry_point_offsets: no tiles/WPP
        0, // entry_offset_to_subset_array: unused
        0, // slice_data_num_emu_prevn_bytes: not provided (sentinel per general VA-API convention)
    ))
}

/// An unused `VAPictureHEVC` reference-list slot.
fn invalid_picture_hevc() -> PictureHEVC {
    PictureHEVC::new(VA_INVALID_SURFACE, 0, VA_PICTURE_HEVC_INVALID)
}

/// After a `Picture` consumes its surface, this crate cannot recover the exact same [`Surface`]
/// object on any of `decode_one`'s error paths — mirrors
/// `super::h264::fresh_surface_or_placeholder`'s identical rationale.
fn fresh_surface_or_placeholder(
    context: &Rc<Context>,
    coded_width: u32,
    coded_height: u32,
) -> Surface<()> {
    let display = context.display();
    display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            coded_width,
            coded_height,
            None,
            vec![()],
        )
        .ok()
        .and_then(|mut v| v.pop())
        .unwrap_or_else(|| placeholder_surface(display, coded_width, coded_height))
}

/// Last-resort placeholder when even a minimal surface allocation fails — mirrors
/// `super::h264::placeholder_surface`'s identical rationale.
fn placeholder_surface(display: &Rc<Display>, coded_width: u32, coded_height: u32) -> Surface<()> {
    #[allow(
        clippy::expect_used,
        reason = "session is already unrecoverable by this point; see caller doc comment"
    )]
    display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            coded_width,
            coded_height,
            None,
            vec![()],
        )
        .expect("VA-API display known-good at session open time")
        .pop()
        .expect("exactly one surface requested")
}

/// Best-effort seed of SPS/PPS from `extra_data` at `open()` time — mirrors
/// `super::h264::seed_params`'s identical disposition (parse failures here are not fatal since
/// in-band SPS/PPS from `push_packet` can seed them instead).
fn seed_params(extra_data: &Bytes) -> (Option<HevcSps>, Option<HevcPps>) {
    if extra_data.is_empty() {
        return (None, None);
    }
    let Ok(nals) = split_annex_b(extra_data.as_ref()) else {
        return (None, None);
    };
    let mut sps = None;
    let mut pps = None;
    for nal in nals {
        let Ok(unit) = HevcNalUnit::parse(nal) else {
            continue;
        };
        match unit.unit_type {
            HevcNalUnitType::Sps => {
                if let Ok(s) = HevcSps::parse(&unit.rbsp) {
                    sps = Some(s);
                }
            }
            HevcNalUnitType::Pps => {
                if let Ok(p) = HevcPps::parse(&unit.rbsp) {
                    pps = Some(p);
                }
            }
            _ => {}
        }
    }
    (sps, pps)
}

fn validate(config: &VideoDecoderConfig) -> Result<(), DecodeError> {
    // Reject configurations intended for another codec before allocating resources.
    if config.codec != mediaway_common::CodecKind::Hevc {
        return Err(DecodeError::Unsupported);
    }
    if config.pixel_format != PixelFormat::Nv12 {
        return Err(DecodeError::Unsupported);
    }
    if config.time_base.den == 0 {
        return Err(DecodeError::InvalidInput);
    }
    Ok(())
}

const fn round_up_8(value: u32) -> u32 {
    value.div_ceil(8) * 8
}
