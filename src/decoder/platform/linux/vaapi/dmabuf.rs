//! Codec-agnostic VA-API DMA-BUF export: `Surface<D>::export_prime()` →
//! [`mediaway_common::DmaBufDescriptor`] → [`GpuBufferHandle::DmaBuf`].
//!
//! See [ADR-0006](../../../adr/linux/0006-vaapi-dmabuf-zero-copy-output.md) for the design this
//! module implements — in particular § "`cros-libva` already wraps the real VA-API DMA-BUF
//! export call, safely" and § Common-crate change. `export_prime()` is a safe `cros-libva`
//! wrapper around `vaExportSurfaceHandle` — no `unsafe` block is written in this crate (mirrors
//! `h264.rs`'s existing "every `unsafe` FFI call already lives inside `cros-libva`" posture).
//!
//! Scoped to the NV12 case this backend's `h264.rs` produces: `export_prime()`'s
//! `VA_EXPORT_SURFACE_COMPOSED_LAYERS` flag is expected to yield exactly one DRM object and one
//! layer with 1-2 planes on real drivers (unconfirmed — no VA-API hardware in this session, see
//! the ADR's Open questions). A driver reporting a shape outside that expectation is rejected as
//! [`DecodeError::Backend`], never silently truncated or reinterpreted.
//!
//! This module only manages surface export; codec-specific reference state stays in each decoder.

use std::os::fd::OwnedFd;

use cros_libva::{DrmPrimeSurfaceDescriptor, Surface, SurfaceMemoryDescriptor};
use mediaway_common::{DmaBufDescriptor, DmaBufPlane, GpuBufferHandle, NativeHandle};

use crate::decoder::platform::DecodeError;

/// The `OwnedFd`(s) backing a just-exported [`GpuBufferHandle::DmaBuf`], kept alive by the
/// caller (`h264.rs`'s `Pipeline::exported_fds`) for as long as the handle's documented validity
/// window lasts — see the owning ADR's § Fd lifetime contract. Dropping this closes the fd(s).
pub(crate) struct DmaBufFds {
    // Never read outside `dmabuf_tests.rs` — this struct's only production-code purpose is
    // RAII: holding the `OwnedFd`(s) open until `h264.rs::release_previous_gpu_export` drops the
    // whole value, which closes them. A plain `cargo check` without `--tests` never sees the
    // reads `dmabuf_tests.rs` performs on `fd1` to assert object count — mirrors this crate's
    // `dpb.rs::Dpb::capacity`/`is_outstanding` precedent for the same "test-only read" shape.
    #[allow(
        dead_code,
        reason = "RAII-only in production code; read by dmabuf_tests.rs"
    )]
    pub(crate) fd0: OwnedFd,
    #[allow(
        dead_code,
        reason = "RAII-only in production code; read by dmabuf_tests.rs"
    )]
    pub(crate) fd1: Option<OwnedFd>,
}

/// Export `surface`'s backing memory as a DMA-BUF and build the caller-facing
/// [`GpuBufferHandle::DmaBuf`] handle alongside the fd(s) this crate must keep open until the
/// handle's validity window ends.
///
/// # Errors
///
/// Returns [`DecodeError::Backend`] if `vaExportSurfaceHandle` fails, or if the driver reports a
/// DRM object/layer/plane shape outside this module's scoped NV12 expectation (see module docs).
pub(crate) fn build_handle<D: SurfaceMemoryDescriptor>(
    surface: &Surface<D>,
) -> Result<(GpuBufferHandle, DmaBufFds), DecodeError> {
    let desc = surface.export_prime().map_err(|_| DecodeError::Backend)?;
    build_from_prime(desc)
}

fn build_from_prime(
    mut desc: DrmPrimeSurfaceDescriptor,
) -> Result<(GpuBufferHandle, DmaBufFds), DecodeError> {
    if desc.objects.is_empty() || desc.objects.len() > 2 {
        return Err(DecodeError::Backend);
    }
    if desc.layers.len() != 1 {
        return Err(DecodeError::Backend);
    }
    let layer = &desc.layers[0];
    let plane_count = u8::try_from(layer.num_planes).map_err(|_| DecodeError::Backend)?;
    if plane_count == 0 || plane_count > 2 {
        return Err(DecodeError::Backend);
    }

    let mut planes = [DmaBufPlane {
        object_index: 0,
        offset: 0,
        pitch: 0,
    }; 2];
    for (plane, i) in planes.iter_mut().zip(0..usize::from(plane_count)) {
        *plane = DmaBufPlane {
            object_index: layer.object_index[i],
            offset: layer.offset[i],
            pitch: layer.pitch[i],
        };
    }

    // `desc.objects` ownership is consumed here (`remove(0)` twice, front-to-back) — matches
    // this module's sole responsibility of converting the export into this crate's owned
    // representation exactly once.
    let object0 = desc.objects.remove(0);
    let modifier = object0.drm_format_modifier;
    let fd0 = native_handle_from_fd(&object0.fd)?;

    let (fd1, owned1) = if desc.objects.is_empty() {
        (None, None)
    } else {
        let object1 = desc.objects.remove(0);
        let handle = native_handle_from_fd(&object1.fd)?;
        (Some(handle), Some(object1.fd))
    };

    let descriptor = DmaBufDescriptor {
        fd0,
        fd1,
        fourcc: desc.fourcc,
        modifier,
        width: desc.width,
        height: desc.height,
        planes,
        plane_count,
    };

    Ok((
        GpuBufferHandle::DmaBuf(Box::new(descriptor)),
        DmaBufFds {
            fd0: object0.fd,
            fd1: owned1,
        },
    ))
}

/// `NativeHandle` bits for a raw fd — offset by `+1` so fd `0` still round-trips through
/// `NativeHandle`'s non-zero representation (same convention `vulkan::zero_copy::build_handle`
/// uses for a `slot_index` of `0`; see [`mediaway_common::DmaBufDescriptor::fd0`]'s field doc).
/// Callers must subtract one to recover the real fd number.
fn native_handle_from_fd(fd: &OwnedFd) -> Result<NativeHandle, DecodeError> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    let bits = usize::try_from(raw)
        .ok()
        .and_then(|b| b.checked_add(1))
        .ok_or(DecodeError::Backend)?;
    NativeHandle::new(bits).ok_or(DecodeError::Backend)
}
