//! Reading a decoded VA surface back into packed NV12.
//!
//! Zero-copy would export the surface as a dmabuf and import it into the
//! renderer; until that exists, the picture is copied out once per frame and
//! takes the same CPU path as the software decoder's output.
use cros_libva::Image;

use crate::decoder::platform::DecodeError;

/// Packed NV12: a full-size Y plane followed by interleaved CbCr at half size.
pub(super) fn pack_nv12(
    image: &Image<'_>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, DecodeError> {
    let info = image.image();
    if info.format.fourcc != cros_libva::VA_FOURCC_NV12 {
        tracing::debug!(fourcc = info.format.fourcc, "unexpected VA image format");
        return Err(DecodeError::HardwareFailure);
    }
    let bytes = image.as_ref();
    let (width, height) = (width as usize, height as usize);
    let chroma_height = height.div_ceil(2);
    let luma_pitch = info.pitches[0] as usize;
    let chroma_pitch = info.pitches[1] as usize;
    let luma_offset = info.offsets[0] as usize;
    let chroma_offset = info.offsets[1] as usize;
    if luma_pitch < width || chroma_pitch < width {
        return Err(DecodeError::HardwareFailure);
    }
    let luma_end = luma_offset
        .checked_add(luma_pitch * (height - 1) + width)
        .ok_or(DecodeError::HardwareFailure)?;
    let chroma_end = chroma_offset
        .checked_add(chroma_pitch * (chroma_height - 1) + width)
        .ok_or(DecodeError::HardwareFailure)?;
    if bytes.len() < luma_end.max(chroma_end) {
        return Err(DecodeError::HardwareFailure);
    }
    let mut packed = vec![0u8; width * height + width * chroma_height];
    for row in 0..height {
        let source = luma_offset + row * luma_pitch;
        packed[row * width..(row + 1) * width].copy_from_slice(&bytes[source..source + width]);
    }
    let chroma_base = width * height;
    for row in 0..chroma_height {
        let source = chroma_offset + row * chroma_pitch;
        let target = chroma_base + row * width;
        packed[target..target + width].copy_from_slice(&bytes[source..source + width]);
    }
    Ok(packed)
}
