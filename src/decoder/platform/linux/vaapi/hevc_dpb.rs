//! Single-forward-reference HEVC DPB: at most one non-current picture is ever tracked, since
//! this crate's own slice-header parser ([`super::hevc_slice`]) already rejects any short-term
//! RPS shape other than "exactly one negative-direction entry, `delta_poc == -1`, used". This is
//! **not** a port of [`super::dpb`]'s general H.264 sliding-window `Dpb` (that type's
//! sliding-window eviction machinery has no reachable code path once occupancy is capped at
//! one) — closer in spirit to `mediaway-encoder::vulkan::hevc_gop::GopState`'s own single-slot
//! `last_written` design. See `adr/linux/0003-vaapi-hevc-p-slice-dpb.md` § DPB design for the
//! full rationale.
//!
//! `derive_pic_order_cnt_msb` (ITU-T H.265 § 8.3.1) is **not** re-derived here — this crate's
//! own [`super::hevc::VaapiHevcDecoder`] reuses `super::dpb::derive_pic_order_cnt_msb`
//! (`linux/vaapi/dpb.rs`, already `pub(super)`, landed for H.264 decode) directly: the ITU-T
//! H.265 §8.3.1 formula is the identical MSB/LSB-wraparound arithmetic H.264's §8.2.1.1 already
//! implements — reusing the already-landed, already-tested sibling function avoids a second copy
//! of one small, easy-to-get-wrong formula.
//!
//! No `cros_libva` types anywhere in this file — every function operates on plain data, so it is
//! unit-testable without any VA-API device (see `hevc_dpb_tests.rs`).

#![forbid(unsafe_code)]

/// Fixed physical-surface-pool size for HEVC decode: current + the one tracked reference + one
/// in-flight headroom slot, mirroring this crate's H.264 sibling's `+1` sizing comment's own
/// reasoning, applied to a design that only ever needs one reference instead of
/// `sps.max_dec_pic_buffering` many — a fixed, small constant, not computed from
/// `sps.max_dec_pic_buffering` the way the H.264 sibling's pool is, since this crate's own
/// RPS-shape validation ([`super::hevc_slice::ShortTermRefPicSet::is_single_forward_reference`])
/// already guarantees no stream this crate accepts ever needs more than one tracked reference
/// regardless of what `sps.max_dec_pic_buffering` itself declares.
pub(super) const HEVC_SURFACE_POOL_SIZE: usize = 3;

/// One optional reference slot's bookkeeping: the physical-surface-index it lives at, plus its
/// `PicOrderCntVal` — no pixel data, no VA-API surface handle (those stay in
/// `hevc::HevcPipeline::surfaces`, indexed the same way, mirroring [`super::dpb::DpbSlot`]'s own
/// "no pixel data, no VA-API surface handle" convention).
pub(super) struct HevcDpbSlot {
    pub(super) pic_order_cnt: i32,
}

/// One optional reference slot (the immediately preceding reference picture, if any).
pub(super) struct HevcDpb {
    reference: Option<(usize, HevcDpbSlot)>,
}

impl HevcDpb {
    pub(super) const fn new() -> Self {
        Self { reference: None }
    }

    /// An IDR picture clears the tracked reference (ITU-T H.265 § C.5.2.2 semantics: an IDR
    /// access unit empties the DPB of prior reference pictures).
    pub(super) const fn clear(&mut self) {
        self.reference = None;
    }

    pub(super) const fn reference(&self) -> Option<&(usize, HevcDpbSlot)> {
        self.reference.as_ref()
    }

    pub(super) const fn set_reference(&mut self, slot_index: usize, pic_order_cnt: i32) {
        self.reference = Some((slot_index, HevcDpbSlot { pic_order_cnt }));
    }
}

/// Allocates the next destination slot index by round-robin over [`HEVC_SURFACE_POOL_SIZE`]
/// physical surfaces, skipping whichever index `dpb` currently protects as its tracked
/// reference. A 2-line linear scan, not a general allocator — simpler than porting
/// [`super::dpb::Dpb::allocate_slot`]'s free-slot-or-evict logic, since with 3 physical slots
/// and at most 1 protected reference, a free (non-reference) slot always exists.
pub(super) fn allocate_slot(cursor: &mut usize, dpb: &HevcDpb) -> usize {
    let mut index = *cursor % HEVC_SURFACE_POOL_SIZE;
    if dpb
        .reference()
        .is_some_and(|(protected, _)| *protected == index)
    {
        index = (index + 1) % HEVC_SURFACE_POOL_SIZE;
    }
    *cursor = (index + 1) % HEVC_SURFACE_POOL_SIZE;
    index
}
