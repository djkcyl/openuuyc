//! Copy an NV12 `VAImage`'s mapped planes into a tightly packed `Bytes` buffer.
//!
//! Pure byte-copy logic, independent of `cros-libva` / a real VA-API display so it is unit
//! testable without hardware — the caller ([`super::h264`]) supplies `pitches`/`offsets` read
//! from a real `vaGetImage`-mapped `VAImage` buffer. Output layout is `width * height` luma bytes
//! followed by `width * height / 2` interleaved chroma bytes, both tightly packed (stride
//! removed).

use mediaway_common::Bytes;

/// Copy stride-padded NV12 plane data out of `data` into tightly packed `Bytes`.
///
/// `y_pitch`/`uv_pitch` are the mapped image's scanline strides in bytes
/// (`VAImage::pitches`), `y_offset`/`uv_offset` the byte offset of each plane's first scanline
/// within `data` (`VAImage::offsets`). Rows that would read past `data`'s end are left zeroed
/// (defensive: a driver reporting inconsistent pitches/offsets should not panic this path).
pub(super) fn copy_nv12_from_planes(
    data: &[u8],
    width: u32,
    height: u32,
    y_pitch: u32,
    y_offset: u32,
    uv_pitch: u32,
    uv_offset: u32,
) -> Bytes {
    let width = width as usize;
    let height = height as usize;
    let y_pitch = y_pitch as usize;
    let y_offset = y_offset as usize;
    let uv_pitch = uv_pitch as usize;
    let uv_offset = uv_offset as usize;
    let uv_rows = height / 2;

    let mut out = vec![0u8; width * height + width * uv_rows];

    for row in 0..height {
        let Some(src_start) = y_offset.checked_add(row * y_pitch) else {
            break;
        };
        let src_end = src_start + width;
        if src_end > data.len() {
            break;
        }
        let dst_start = row * width;
        out[dst_start..dst_start + width].copy_from_slice(&data[src_start..src_end]);
    }

    let y_plane_bytes = width * height;
    for row in 0..uv_rows {
        let Some(src_start) = uv_offset.checked_add(row * uv_pitch) else {
            break;
        };
        let src_end = src_start + width;
        if src_end > data.len() {
            break;
        }
        let dst_start = y_plane_bytes + row * width;
        out[dst_start..dst_start + width].copy_from_slice(&data[src_start..src_end]);
    }

    Bytes::from(out)
}
