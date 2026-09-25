//! The D3D11 device behind each shell window.
use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};
use windows::core::Interface;

pub(crate) struct Graphics {
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
    label: String,
}

impl Graphics {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}

pub(crate) fn create_device() -> Result<Graphics> {
    let mut last_error = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[
                    D3D_FEATURE_LEVEL_11_0,
                    D3D_FEATURE_LEVEL_10_1,
                    D3D_FEATURE_LEVEL_10_0,
                ]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match result {
            Ok(()) => {
                let device = device.context("D3D11 did not return a GUI device")?;
                let context = context.context("D3D11 did not return a GUI context")?;
                let adapter: IDXGIAdapter1 =
                    unsafe { device.cast::<IDXGIDevice>()?.GetAdapter() }?.cast()?;
                let desc = unsafe { adapter.GetDesc1() }?;
                let length = desc
                    .Description
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(desc.Description.len());
                let label = format!(
                    "{} · D3D11",
                    String::from_utf16_lossy(&desc.Description[..length])
                );
                return Ok(Graphics {
                    device,
                    context,
                    label,
                });
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow!("create GUI D3D11 device: {:?}", last_error))
}
