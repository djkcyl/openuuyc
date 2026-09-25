// SPDX-License-Identifier: LGPL-2.1-or-later
//! Single-reference desktop analysis. Static/scroll classification and the
//! 320/80%/50% scene thresholds follow the verified OpenH264 screen path.
use super::search::{self, Mv};
use crate::picture::Plane;

pub(super) struct Analysis {
    pub scroll: Mv,
    scroll_detected: bool,
    unchanged_scroll: Option<bool>,
    pub large_change: bool,
    pub complexity: u64,
    pub mb_cost: Vec<u32>,
    pub static_kind: Vec<u8>,
    pub static_parts: Vec<[u8; 4]>,
    pub split: Vec<bool>,
    pub features: Features,
    frame_sad: u64,
    width: usize,
    height: usize,
}
#[inline]
fn same8(a: &[u8], b: &[u8], stride: usize) -> bool {
    (0..8).all(|y| a[y * stride..][..8] == b[y * stride..][..8])
}
/// T 7590B2 -> 873375: the unmasked product path searches nine vertical
/// regions. A successful zero displacement remains a positive detection.
fn scroll_motion(src: &[u8], old: &[u8], w: usize, h: usize) -> Option<i16> {
    let height = h as i32;
    let border = height >> 4;
    let region_width = (w as i32 - 2 * border) / 3;
    let width = region_width / 2;
    if width <= 12 {
        return None;
    }
    let region_height = height * 7 / 8;
    let stride_y = height * 5 / 24;
    let mut found = None;
    for region in 0..9 {
        let x = border + (region % 3) * region_width + width / 2;
        let y = -height * 7 / 48 + (region / 3) * stride_y;
        if x < 0 || x + width > w as i32 {
            continue;
        }
        found = scroll_region(src, old, w, h, x as usize, width as usize, y, region_height);
        if found.is_some_and(|dy| dy != 0) {
            break;
        }
    }
    found
}
fn textured_line(row: &[u8]) -> bool {
    // T 87307C accepts four colors, or four transitions among fewer colors.
    // Only the first three transitions can still fail either test. Repeated
    // pixels add no information and need no color-map update.
    let mut previous = row[0];
    let mut colors = [previous; 3];
    let mut count = 1;
    let mut changes = 0;
    for &v in &row[1..] {
        if v == previous {
            continue;
        }
        previous = v;
        changes += 1;
        if changes == 4 {
            return true;
        }
        if !colors[..count].contains(&v) {
            if count == 3 {
                return true;
            }
            colors[count] = v;
            count += 1;
        }
    }
    false
}

fn scroll_region(
    src: &[u8],
    old: &[u8],
    stride: usize,
    height: usize,
    x: usize,
    width: usize,
    top: i32,
    region_height: i32,
) -> Option<i16> {
    fn row(pixels: &[u8], stride: usize, x: usize, width: usize, y: i32) -> &[u8] {
        let at = y as usize * stride + x;
        &pixels[at..at + width]
    }
    let source = |y| row(src, stride, x, width, y);
    let reference = |y| row(old, stride, x, width, y);
    let middle = top + region_height / 2;
    let mut selected = None;
    for distance in 0..region_height / 2 {
        for y in [middle + distance, middle - distance] {
            if y >= 0 && y < height as i32 && textured_line(source(y)) {
                selected = Some(y);
                break;
            }
        }
        if selected.is_some() {
            break;
        }
    }
    let test = selected?;
    let min = top.max(0);
    let max = (top + region_height - 1).min(height as i32 - 1);
    let limit = (test - min - 1).max(max - test).min(511);
    for distance in 0..=limit {
        for candidate in [test + distance, test - distance - 1] {
            if candidate < min || candidate > max || source(test) != reference(candidate) {
                continue;
            }
            let (above, count) = if candidate >= test {
                let below = (max - candidate).min(25);
                let count = (test - min + below).min(50);
                (count - below, count)
            } else {
                let above = (candidate - min).min(25);
                (above, (max - test + above).min(50))
            };
            if (0..count).all(|i| source(test - above + i) == reference(candidate - above + i)) {
                return Some((candidate - test) as i16);
            }
        }
    }
    None
}
impl Analysis {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            scroll: Mv::ZERO,
            scroll_detected: false,
            unchanged_scroll: None,
            large_change: false,
            complexity: 1,
            mb_cost: vec![1; w * h / 256],
            static_kind: vec![0; w * h / 256],
            static_parts: vec![[0; 4]; w * h / 256],
            split: vec![false; w * h / 256],
            features: Features::new(),
            frame_sad: 0,
            width: w,
            height: h,
        }
    }
    pub fn analyze(&mut self, src: &[u8], old: &[u8], has_reference: bool) {
        let (w, h) = (self.width, self.height);
        self.scroll = Mv::ZERO;
        self.scroll_detected = false;
        self.large_change = false;
        self.frame_sad = 0;
        if !has_reference {
            self.unchanged_scroll = None;
            self.split.fill(false);
            self.static_kind.fill(0);
            return;
        }
        let unchanged = src == old;
        if unchanged && let Some(detected) = self.unchanged_scroll {
            self.scroll_detected = detected;
            return;
        }
        if let Some(dy) = scroll_motion(src, old, w, h) {
            self.scroll = Mv { x: 0, y: dy * 4 };
            self.scroll_detected = true;
        }
        // Equal source planes have zero SAD and the scroll search can only
        // find distance zero (tested first), or no textured row. Cache that
        // distinction only across consecutive identical-source analyses.
        self.unchanged_scroll = unchanged.then_some(self.scroll_detected);
        if unchanged {
            self.split.fill(false);
            self.static_kind.fill(1);
            self.static_parts.fill([1; 4]);
            return;
        }
        let mut moving = 0u32;
        let total = (w * h / 64) as u32;
        for my in 0..h / 16 {
            for mx in 0..w / 16 {
                let mb = my * (w / 16) + mx;
                let at = my * 16 * w + mx * 16;
                let blocks = search::sad_quadrants(&src[at..], &old[at..], w);
                let sum = blocks.iter().sum::<u32>();
                self.frame_sad += u64::from(sum);
                // T 3CD058 consumes variance of the four source SADs before
                // scroll compensation. Keep that ordering while dropping the
                // frame-sized intermediate and second traversal.
                let average = sum >> 8;
                self.split[mb] = blocks
                    .iter()
                    .map(|&v| {
                        let d = (v >> 6) as i32 - average as i32;
                        d * d
                    })
                    .sum::<i32>()
                    >= 20;
                let mut parts = [0; 4];
                for (part, mut cost) in blocks.into_iter().enumerate() {
                    let x = mx * 16 + part % 2 * 8;
                    let y = my * 16 + part / 2 * 8;
                    let ry = y as i32 + i32::from(self.scroll.y) / 4;
                    let kind = if cost == 0 {
                        1
                    } else if self.scroll != Mv::ZERO
                        && ry >= 0
                        && ry as usize + 8 <= h
                        && same8(&src[y * w + x..], &old[ry as usize * w + x..], w)
                    {
                        cost = 0;
                        2
                    } else {
                        0
                    };
                    moving += u32::from(cost > 320);
                    parts[part] = kind;
                }
                self.static_parts[mb] = parts;
                self.static_kind[mb] = if parts.iter().all(|&v| v == parts[0]) {
                    parts[0]
                } else {
                    0
                };
            }
        }
        self.large_change = moving >= ((total as f32 * 0.8 + 0.5) as u32);
    }
    /// Screen RC complexity is independent of source scene/static analysis:
    /// T 3D0688 selects min(colocated reconstructed SAD, source H/V SAD).
    /// Its scroll flag is explicitly cleared by this product consumer.
    pub fn rate_complexity(&mut self, src: &[u8], reference: &Plane, idr: bool) {
        let w = self.width;
        self.complexity = 0;
        for y in (0..self.height).step_by(16) {
            for x in (0..w).step_by(16) {
                let source = &src[y * w + x..];
                let mut best = if idr {
                    u32::MAX
                } else {
                    let (r, rs) = reference
                        .footprint(x as i32, y as i32, 16, 16)
                        .expect("coded reference footprint");
                    search::sad(source, w, r, rs, 16)
                };
                best = search::source_intra_sad(src, w, x, y, best);
                if best == u32::MAX {
                    best = 0;
                }
                self.mb_cost[y / 16 * (w / 16) + x / 16] = best;
                self.complexity += u64::from(best);
            }
        }
    }
    pub fn prepare_features(&mut self, idr: bool, reference_qp: i32) {
        self.features.enabled = false;
        self.features.prepared = false;
        self.features.saving = 0;
        if idr {
            self.features.good_frames = 2;
        } else if self.scroll_detected
            || (self.features.good_frames > 0 && self.frame_sad / self.mb_cost.len() as u64 >= 31)
        {
            self.features.enabled = true;
        }
        let qstep = [10u32, 11, 13, 14, 16, 18][reference_qp as usize % 6] << (reference_qp / 6);
        let reference = self.features.next_picture ^ 1;
        if self.features.enabled {
            self.features.thresholds[reference] = 30 * (qstep + 160) >> 3;
        }
        // T 3CBA93 consumes the reference's retained threshold even when FME
        // is off. SetUnref only clears its calculated flag, not this field.
        self.features.threshold = self.features.thresholds[reference];
    }
    pub fn finish(&mut self) {
        self.features.next_picture ^= 1;
        if self.features.enabled {
            if self.features.saving / self.mb_cost.len() as u64 >= 3 {
                self.features.good_frames = (self.features.good_frames + 1).min(5);
            } else {
                self.features.good_frames = self.features.good_frames.saturating_sub(1);
            }
        }
    }
}

/// Current single-layer screen path indexes only 8x8 sums of reconstructed
/// references. Compact counting buckets replace the old sparse HashMap.
/// Bucket entries retain raster order, including the official edge discard.
pub(super) struct Features {
    pub enabled: bool,
    pub threshold: u32,
    prepared: bool,
    good_frames: u8,
    thresholds: [u32; 2],
    next_picture: usize,
    pub saving: u64,
    offsets: Vec<u32>,
    positions: Vec<u32>,
    sums: Vec<u16>,
    columns: Vec<u16>,
    cursors: Vec<u32>,
}
impl Features {
    fn new() -> Self {
        Self {
            enabled: false,
            threshold: u32::MAX,
            prepared: false,
            good_frames: 2,
            thresholds: [u32::MAX; 2],
            next_picture: 0,
            saving: 0,
            offsets: vec![0; 16322],
            positions: Vec::new(),
            sums: Vec::new(),
            columns: Vec::new(),
            cursors: vec![0; 16321],
        }
    }
    pub fn prepare(&mut self, reference: &Plane) {
        if self.prepared {
            return;
        }
        // Same reference/index and enable policy, built only when an actual
        // P8 candidate reaches FME. P16/cross decisions do not consume it.
        let (w, h) = (reference.width - 8, reference.height - 8);
        self.offsets.fill(0);
        self.sums.resize(w * h, 0);
        self.positions.resize(w * h, 0);
        self.columns.resize(reference.width, 0);
        self.columns.fill(0);
        for y in 0..8 {
            for (sum, &v) in self.columns.iter_mut().zip(reference.row(y)) {
                *sum += u16::from(v);
            }
        }
        for y in 0..h {
            let row = &mut self.sums[y * w..][..w];
            #[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
            sum_row(&self.columns, row);
            #[cfg(not(all(target_arch = "x86_64", not(feature = "scalar-dsp"))))]
            {
                let mut sum = self.columns[..8].iter().map(|&v| u32::from(v)).sum::<u32>();
                for x in 0..w {
                    row[x] = sum as u16;
                    sum = sum + u32::from(self.columns[x + 8]) - u32::from(self.columns[x]);
                }
            }
            for group in row.chunks_exact(8) {
                if group.iter().all(|&v| v == group[0]) {
                    self.offsets[usize::from(group[0]) + 1] += 8;
                } else {
                    for &v in group {
                        self.offsets[usize::from(v) + 1] += 1;
                    }
                }
            }
            for ((sum, &old), &new) in self
                .columns
                .iter_mut()
                .zip(reference.row(y))
                .zip(reference.row(y + 8))
            {
                *sum = *sum + u16::from(new) - u16::from(old);
            }
        }
        for i in 1..self.offsets.len() {
            self.offsets[i] += self.offsets[i - 1];
        }
        self.cursors.copy_from_slice(&self.offsets[..16321]);
        // The histogram above accounts for every entry in these immutable
        // sums. Prefix intervals partition positions exactly, and each cursor
        // advances once per occurrence, so scatter stays in its own interval.
        #[allow(unsafe_code)]
        unsafe {
            for y in 0..h {
                for (group_index, group) in self.sums[y * w..][..w].chunks_exact(8).enumerate() {
                    let x = group_index * 8;
                    if group.iter().all(|&v| v == group[0]) {
                        let cursor = self.cursors.get_unchecked_mut(usize::from(group[0]));
                        let positions = self
                            .positions
                            .get_unchecked_mut(*cursor as usize..*cursor as usize + 8);
                        for (i, p) in positions.iter_mut().enumerate() {
                            *p = ((y as u32) << 16) | (x + i) as u32;
                        }
                        *cursor += 8;
                    } else {
                        for (i, &bucket) in group.iter().enumerate() {
                            let cursor = self.cursors.get_unchecked_mut(usize::from(bucket));
                            *self.positions.get_unchecked_mut(*cursor as usize) =
                                ((y as u32) << 16) | (x + i) as u32;
                            *cursor += 1;
                        }
                    }
                }
            }
        }
        self.prepared = true;
    }
    pub fn candidates(&self, src: &[u8], stride: usize) -> &[u32] {
        if !self.enabled {
            return &[];
        }
        let sum: usize = (0..8)
            .map(|row| {
                src[row * stride..][..8]
                    .iter()
                    .map(|&v| v as usize)
                    .sum::<usize>()
            })
            .sum();
        &self.positions[self.offsets[sum] as usize..self.offsets[sum + 1] as usize]
    }
}

#[cfg(all(target_arch = "x86_64", not(feature = "scalar-dsp")))]
#[allow(unsafe_code)]
fn sum_row(columns: &[u16], out: &mut [u16]) {
    use std::arch::x86_64::*;
    assert_eq!(columns.len(), out.len() + 8);
    assert_eq!(out.len() % 8, 0);
    let initial = columns[..8].iter().copied().sum::<u16>();
    // Column sums are <=2040 and any eight-column sum <=16320. Signed
    // prefix deltas therefore fit i16; this is the exact rolling sum.
    unsafe {
        let mut sum = _mm_set1_epi16(initial as i16);
        let mut a = _mm_loadu_si128(columns.as_ptr().cast());
        for x in (0..out.len()).step_by(8) {
            let b = _mm_loadu_si128(columns.as_ptr().add(x + 8).cast());
            let mut prefix = _mm_sub_epi16(b, a);
            prefix = _mm_add_epi16(prefix, _mm_slli_si128::<2>(prefix));
            prefix = _mm_add_epi16(prefix, _mm_slli_si128::<4>(prefix));
            prefix = _mm_add_epi16(prefix, _mm_slli_si128::<8>(prefix));
            let values = _mm_add_epi16(sum, _mm_slli_si128::<2>(prefix));
            _mm_storeu_si128(out.as_mut_ptr().add(x).cast(), values);
            sum = _mm_add_epi16(
                sum,
                _mm_shuffle_epi32::<255>(_mm_shufflehi_epi16::<255>(prefix)),
            );
            a = b;
        }
    }
}
