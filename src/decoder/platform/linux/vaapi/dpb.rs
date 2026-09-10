//! Sans-io DPB (decoded picture buffer) slot bookkeeping for VA-API H.264 P-slice decode.
//!
//! Ported from `crate::decoder::platform::vulkan::dpb` (see
//! `adr/linux/0002-vaapi-h264-p-slice-dpb.md`'s porting table) — same ITU-T H.264 § 8.2.4
//! (`FrameNumWrap`/sliding-window marking) / § 8.2.5.3 (DPB eviction) arithmetic, already
//! hardware-verified there against a real NVIDIA RTX 4090. This module additionally ports
//! `derive_pic_order_cnt_msb` (`vulkan/h264_params.rs`) and `default_ref_pic_list0`
//! (`vulkan/h264_slice.rs`) — grouped into this one file per the ADR's own porting table, not
//! split further.
//!
//! Re-adds the `outstanding`/`mark_outstanding`/`clear_outstanding` Zero-Copy handle bookkeeping
//! that was deliberately dropped by `adr/linux/0002-vaapi-h264-p-slice-dpb.md` (back when this
//! crate's decode path always copied pixels into an owned `Bytes` and never exposed a Zero-Copy
//! GPU handle). `adr/linux/0006-vaapi-dmabuf-zero-copy-output.md` reintroduces it: once
//! `VideoOutputPreference::ZeroCopyGpu` exports a real
//! [`mediaway_common::GpuBufferHandle::DmaBuf`] (`dmabuf.rs`), a DPB slot whose surface a caller
//! still holds a handle into must never be silently recycled — same "fail loudly, never silently
//! overwrite" contract `vulkan/dpb.rs` already established for its own DPB (ported back
//! verbatim; see that ADR's § Fd lifetime contract).
//!
//! No VA-API/`cros_libva` calls or types anywhere in this file — every function operates on
//! plain data, so it is unit-testable without any device (see `dpb_tests.rs`), mirroring
//! `vulkan/dpb.rs`'s own "no Vulkan/GPU calls anywhere in this file" doc-comment claim.

#![forbid(unsafe_code)]

use thiserror::Error;

/// H.264's spec-defined maximum DPB slot count.
pub(super) const H264_MAX_DPB_SLOTS: usize = 16;

/// One decoded-picture-buffer slot's reference-management bookkeeping.
///
/// Deliberately holds only the fields H.264's sliding-window reference process (ITU-T H.264
/// § 8.2.4 / § 8.2.5.3) needs — no pixel data, no VA-API surface handle (those live in
/// `Pipeline::surfaces`, indexed the same way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DpbSlot {
    /// `frame_num` as signaled in this picture's slice header.
    pub(super) frame_num: u32,
    /// `FrameNumWrap` (ITU-T H.264 § 8.2.4.1) — `frame_num`, or `frame_num - MaxFrameNum` when
    /// `frame_num` is greater than the current picture's `frame_num` (wrapped around
    /// `log2_max_frame_num`). Sliding-window eviction picks the occupied reference slot with the
    /// smallest `frame_num_wrap`.
    pub(super) frame_num_wrap: i32,
    /// `PicOrderCnt` — this crate supports `pic_order_cnt_type == 0` only, so top/bottom field
    /// order counts are always equal for the frame pictures this crate decodes; a single `i32`
    /// is enough.
    pub(super) pic_order_cnt: i32,
    /// Whether this picture is currently used as a short-term reference (`used_for_reference` in
    /// the spec's DPB bookkeeping).
    pub(super) used_for_reference: bool,
}

impl DpbSlot {
    /// Construct a new short-term reference slot.
    #[must_use]
    pub(super) const fn new_reference(
        frame_num: u32,
        frame_num_wrap: i32,
        pic_order_cnt: i32,
    ) -> Self {
        Self {
            frame_num,
            frame_num_wrap,
            pic_order_cnt,
            used_for_reference: true,
        }
    }
}

/// `FrameNumWrap` (ITU-T H.264 § 8.2.4.1, short-term picture case).
///
/// `frame_num` unchanged if not greater than the current picture's `frame_num`, otherwise
/// wrapped by subtracting `MaxFrameNum` (`1 << log2_max_frame_num`).
#[must_use]
#[allow(
    clippy::cast_possible_wrap,
    reason = "frame_num/max_frame_num are H.264 frame_num values, bounded by \
              log2_max_frame_num (<= 16 bits per the spec's own field width) — never close to \
              i32::MAX, so the wrap this lint warns about is unreachable in practice"
)]
pub(super) const fn compute_frame_num_wrap(
    frame_num: u32,
    current_frame_num: u32,
    max_frame_num: u32,
) -> i32 {
    if frame_num > current_frame_num {
        frame_num as i32 - max_frame_num as i32
    } else {
        frame_num as i32
    }
}

/// Errors from [`Dpb`]'s slot bookkeeping.
///
/// Crate-internal (`pub(super)`, not `crate::decoder::platform::DecodeError`) — mapped to `DecodeError::Backend` at
/// every `h264.rs` call site, mirroring `vulkan/decoder.rs`'s identical disposition for
/// `vulkan::dpb::DpbError`.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(super) enum DpbError {
    /// The caller tried to insert into or evict a slot whose surface a caller still holds via
    /// an outstanding Zero-Copy [`mediaway_common::GpuBufferHandle::DmaBuf`] handle — recycling
    /// it now would silently invalidate that handle's contents underneath the caller. Never
    /// overwritten silently; the caller must recycle the handle (drop the frame / poll further)
    /// before this slot can be reused.
    #[error("DPB slot {index} still has an outstanding Zero-Copy handle")]
    SlotOutstanding {
        /// The slot index that could not be recycled.
        index: usize,
    },
    /// No free slot exists and no occupied reference slot is available to evict.
    #[error("no free DPB slot available (capacity {capacity})")]
    NoFreeSlot {
        /// Total DPB capacity at the time of the failed allocation.
        capacity: usize,
    },
    /// `index` is not a valid slot index for this DPB's capacity.
    #[error("DPB slot index {index} out of range (capacity {capacity})")]
    InvalidSlotIndex {
        /// The out-of-range index that was requested.
        index: usize,
        /// Total DPB capacity.
        capacity: usize,
    },
}

/// Fixed-capacity DPB slot array, sized once from the parsed SPS's `max_num_ref_frames` (see
/// [`Dpb::new`]) — a `Vec` allocated once at session-open time, not per-frame.
pub(super) struct Dpb {
    slots: Vec<Option<DpbSlot>>,
    /// Parallel array: `true` while a caller holds an outstanding Zero-Copy handle into that
    /// slot's surface. Kept separate from `DpbSlot` itself so an evicted-but-still-held slot can
    /// still report `SlotOutstanding` (the reference bookkeeping is gone, but the handle
    /// contract is not) — mirrors `vulkan/dpb.rs::Dpb`'s identical field.
    outstanding: Vec<bool>,
}

impl Dpb {
    /// Allocate a DPB with `capacity` slots, clamped to `1..=H264_MAX_DPB_SLOTS` (a decoder
    /// always needs at least one slot for the current picture, even when the stream signals
    /// `max_num_ref_frames == 0`).
    #[must_use]
    pub(super) fn new(capacity: usize) -> Self {
        let capacity = capacity.clamp(1, H264_MAX_DPB_SLOTS);
        Self {
            slots: vec![None; capacity],
            outstanding: vec![false; capacity],
        }
    }

    /// Whether `index` currently has an outstanding Zero-Copy handle.
    #[must_use]
    #[allow(
        dead_code,
        reason = "exercised by dpb_tests.rs (is_outstanding_defaults_false_and_out_of_range_is_false); \
                  a plain `cargo check` without --tests never sees that call site — kept for API \
                  parity with vulkan/dpb.rs's identical accessor, mirrors this file's own \
                  Dpb::capacity precedent"
    )]
    pub(super) fn is_outstanding(&self, index: usize) -> bool {
        self.outstanding.get(index).copied().unwrap_or(false)
    }

    /// Marks `index` as holding an outstanding Zero-Copy handle — call when handing a
    /// [`mediaway_common::GpuBufferHandle::DmaBuf`] out to a caller (see `dmabuf.rs`).
    ///
    /// # Errors
    /// Returns [`DpbError::InvalidSlotIndex`] if `index` is out of range.
    pub(super) fn mark_outstanding(&mut self, index: usize) -> Result<(), DpbError> {
        self.check_index(index)?;
        self.outstanding[index] = true;
        Ok(())
    }

    /// Clears `index`'s outstanding-handle mark — call once the caller has recycled the
    /// corresponding `VideoFrame` (the next `push_packet`/`poll_frame`/`flush` call that would
    /// otherwise want to reuse this slot, per `crate::decoder::platform::VideoDecoder`'s documented handle-lifetime
    /// contract).
    ///
    /// # Errors
    /// Returns [`DpbError::InvalidSlotIndex`] if `index` is out of range.
    pub(super) fn clear_outstanding(&mut self, index: usize) -> Result<(), DpbError> {
        self.check_index(index)?;
        self.outstanding[index] = false;
        Ok(())
    }

    /// Total slot count.
    #[must_use]
    #[allow(
        dead_code,
        reason = "exercised by dpb_tests.rs (new_clamps_capacity_to_*); a plain `cargo check` \
                  without --tests never sees that call site — kept for API parity with \
                  vulkan/dpb.rs's identical accessor, not because a non-test caller needs it yet"
    )]
    pub(super) const fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The slot at `index`, if occupied.
    #[must_use]
    pub(super) fn slot(&self, index: usize) -> Option<&DpbSlot> {
        self.slots.get(index).and_then(Option::as_ref)
    }

    /// First unoccupied, non-outstanding slot index, if any.
    ///
    /// A slot can be "unoccupied" in reference-management terms (`slots[i] == None`) while
    /// still holding an outstanding Zero-Copy handle for a **non-reference** picture (only
    /// [`Dpb::insert`]s a `DpbSlot` for `is_reference` pictures — a non-reference picture's slot
    /// is never inserted, but can still be [`Dpb::mark_outstanding`]-ed). Skipped here so
    /// [`Dpb::allocate_slot`]'s free-slot fast path never hands out a slot a caller still holds
    /// a handle into — the same "fail loudly, never silently overwrite" contract already applied
    /// to occupied (reference) slots via [`Dpb::insert`]/[`Dpb::evict`]'s own outstanding checks.
    #[must_use]
    pub(super) fn free_slot_index(&self) -> Option<usize> {
        self.slots
            .iter()
            .zip(self.outstanding.iter())
            .position(|(slot, outstanding)| slot.is_none() && !outstanding)
    }

    /// Every occupied slot, in slot-index order.
    pub(super) fn occupied_slots(&self) -> impl Iterator<Item = (usize, &DpbSlot)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|slot| (index, slot)))
    }

    const fn check_index(&self, index: usize) -> Result<(), DpbError> {
        if index >= self.slots.len() {
            Err(DpbError::InvalidSlotIndex {
                index,
                capacity: self.slots.len(),
            })
        } else {
            Ok(())
        }
    }

    /// Insert `slot` at `index`, marking it occupied.
    ///
    /// # Errors
    /// Returns [`DpbError::InvalidSlotIndex`] if `index` is out of range, or
    /// [`DpbError::SlotOutstanding`] if `index` still has an outstanding Zero-Copy handle —
    /// never silently overwritten.
    pub(super) fn insert(&mut self, index: usize, slot: DpbSlot) -> Result<(), DpbError> {
        self.check_index(index)?;
        if self.outstanding[index] {
            return Err(DpbError::SlotOutstanding { index });
        }
        self.slots[index] = Some(slot);
        Ok(())
    }

    /// Evict (clear) the slot at `index`.
    ///
    /// # Errors
    /// Returns [`DpbError::InvalidSlotIndex`] if `index` is out of range, or
    /// [`DpbError::SlotOutstanding`] if `index` still has an outstanding Zero-Copy handle.
    pub(super) fn evict(&mut self, index: usize) -> Result<(), DpbError> {
        self.check_index(index)?;
        if self.outstanding[index] {
            return Err(DpbError::SlotOutstanding { index });
        }
        self.slots[index] = None;
        Ok(())
    }

    /// The occupied short-term-reference slot with the smallest `frame_num_wrap` — the
    /// sliding-window eviction target (ITU-T H.264 § 8.2.5.3) when the DPB is full and a new
    /// reference picture needs a slot.
    #[must_use]
    pub(super) fn sliding_window_evict_target(&self) -> Option<usize> {
        self.occupied_slots()
            .filter(|(_, slot)| slot.used_for_reference)
            .min_by_key(|(_, slot)| slot.frame_num_wrap)
            .map(|(index, _)| index)
    }

    /// Recomputes every occupied slot's `frame_num_wrap` relative to `current_frame_num`
    /// (ITU-T H.264 § 8.2.4.1).
    ///
    /// `FrameNumWrap` is defined **relative to the picture currently being decoded**, not fixed
    /// when a slot was inserted — the caller must call this once per picture (with that
    /// picture's own `frame_num`) before [`Dpb::allocate_slot`]'s sliding-window eviction or
    /// [`default_ref_pic_list0`]'s list-building, or both would silently use stale values once
    /// `frame_num` wraps around `MaxFrameNum`.
    pub(super) fn refresh_frame_num_wraps(&mut self, current_frame_num: u32, max_frame_num: u32) {
        for slot in self.slots.iter_mut().flatten() {
            slot.frame_num_wrap =
                compute_frame_num_wrap(slot.frame_num, current_frame_num, max_frame_num);
        }
    }

    /// Evicts every occupied slot — an IDR picture empties the whole DPB (ITU-T H.264 § 8.2.5.1:
    /// an IDR access unit marks every prior reference picture "unused for reference").
    ///
    /// # Errors
    /// Returns [`DpbError::SlotOutstanding`] for the first occupied slot that still has an
    /// outstanding Zero-Copy handle — the caller must drain pending frames first; slots already
    /// evicted before the failing one stay evicted (partial progress, not rolled back — matches
    /// [`Dpb::evict`]'s own single-slot failure contract, mirrors `vulkan/dpb.rs::Dpb::clear_all`).
    pub(super) fn clear_all(&mut self) -> Result<(), DpbError> {
        let occupied: Vec<usize> = self.occupied_slots().map(|(index, _)| index).collect();
        for index in occupied {
            self.evict(index)?;
        }
        Ok(())
    }

    /// Allocate a slot index for a new picture to decode into: reuses a free slot if one exists,
    /// otherwise forces sliding-window eviction of the oldest short-term reference.
    ///
    /// # Errors
    /// Returns [`DpbError::SlotOutstanding`] if the eviction target still has an outstanding
    /// Zero-Copy handle (the caller must drain pending frames before this DPB can accept another
    /// picture), or [`DpbError::NoFreeSlot`] if every slot is empty of references and still
    /// somehow unavailable (unreachable in practice — a zero-capacity DPB is rejected by
    /// [`Dpb::new`]'s clamp — kept as an explicit error rather than a `panic!`/`unwrap`, matching
    /// the porting source's own reasoning).
    pub(super) fn allocate_slot(&mut self) -> Result<usize, DpbError> {
        if let Some(index) = self.free_slot_index() {
            return Ok(index);
        }
        let index = self
            .sliding_window_evict_target()
            .ok_or(DpbError::NoFreeSlot {
                capacity: self.slots.len(),
            })?;
        self.evict(index)?;
        Ok(index)
    }
}

/// Derives `PicOrderCntMsb` for `pic_order_cnt_type == 0` (ITU-T H.264 § 8.2.1.1).
///
/// Given the just-parsed `pic_order_cnt_lsb` and the previous reference picture's
/// `(PicOrderCntMsb, pic_order_cnt_lsb)`. Per the spec, only reference pictures update the
/// "previous" state the caller carries forward — non-reference pictures compute a POC without
/// perpetuating it. For an IDR picture, the caller passes `prev_msb = 0, prev_lsb = 0` (the
/// spec's IDR reset), so this reduces to `PicOrderCntMsb = 0`.
#[must_use]
#[allow(
    clippy::similar_names,
    reason = "prev_msb/prev_lsb name the two halves of one ITU-T H.264 § 8.2.1.1 state pair \
              (PicOrderCntMsb, pic_order_cnt_lsb) — matching, not confusable, names"
)]
#[allow(
    clippy::cast_possible_wrap,
    reason = "pic_order_cnt_lsb/prev_lsb/max_pic_order_cnt_lsb are H.264 POC-LSB values, bounded \
              by log2_max_pic_order_cnt_lsb (<= 16 bits per the spec's own field width) — never \
              close to i32::MAX, so the wrap this lint warns about is unreachable in practice; \
              mirrors compute_frame_num_wrap's identical allow above"
)]
pub(super) const fn derive_pic_order_cnt_msb(
    pic_order_cnt_lsb: u32,
    prev_msb: i32,
    prev_lsb: u32,
    max_pic_order_cnt_lsb: u32,
) -> i32 {
    let half = (max_pic_order_cnt_lsb / 2) as i32;
    let lsb = pic_order_cnt_lsb as i32;
    let prev_lsb = prev_lsb as i32;
    if lsb < prev_lsb && prev_lsb - lsb >= half {
        prev_msb + max_pic_order_cnt_lsb as i32
    } else if lsb > prev_lsb && lsb - prev_lsb > half {
        prev_msb - max_pic_order_cnt_lsb as i32
    } else {
        prev_msb
    }
}

/// Default `RefPicList0` initialization for a P slice (ITU-T H.264 § 8.2.4.2.1,
/// short-term-only case).
///
/// Every occupied reference slot, sorted by decreasing `frame_num_wrap` (`PicNum`, since this
/// crate does not support long-term references). This crate's caller only ever reads
/// `.first()` (single-forward-reference scope — see the ADR).
#[must_use]
pub(super) fn default_ref_pic_list0(dpb: &Dpb) -> Vec<usize> {
    let mut refs: Vec<(usize, i32)> = dpb
        .occupied_slots()
        .filter(|(_, slot)| slot.used_for_reference)
        .map(|(index, slot)| (index, slot.frame_num_wrap))
        .collect();
    refs.sort_by_key(|&(_, frame_num_wrap)| std::cmp::Reverse(frame_num_wrap));
    refs.into_iter().map(|(index, _)| index).collect()
}
