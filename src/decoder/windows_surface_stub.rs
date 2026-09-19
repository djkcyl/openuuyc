//! Placeholder for the D3D11 zero-copy surface path on non-Windows targets.
//! The types stay in the signatures so the shared decoder plumbing is identical;
//! they can never be constructed, so every Linux frame takes the CPU path.
use anyhow::{Result, bail};
use mediaway_common::GpuDeviceHandle;

#[derive(Clone)]
pub(crate) enum D3D11SurfaceWriter {}

#[derive(Debug)]
pub(crate) enum D3D11Surface {}

impl D3D11SurfaceWriter {
    pub(crate) fn new() -> Result<Self> {
        bail!("当前平台没有 D3D11 零拷贝解码表面")
    }

    pub(crate) fn available() -> Result<Vec<Self>> {
        Ok(Vec::new())
    }

    pub(crate) fn device_handle(&self) -> GpuDeviceHandle {
        match *self {}
    }

    pub(crate) fn wrap_decoded_surface(
        &self,
        _frame: std::convert::Infallible,
    ) -> Result<D3D11Surface> {
        match *self {}
    }
}

impl D3D11Surface {
    pub(crate) fn coded_size(&self) -> (u32, u32) {
        match *self {}
    }

    pub(crate) const fn visible_origin(&self) -> (u32, u32) {
        match *self {}
    }
}
