use std::sync::{Arc, Mutex};

use crate::decoder::platform::windows::WindowsGpuVideoFrame;
use anyhow::{Context, Result, bail};
use mediaway_common::{GpuDeviceHandle, NativeHandle};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    IDXGIAdapter1, IDXGIDevice, IDXGIDevice1, IDXGIFactory1, IDXGIFactory6, IDXGIKeyedMutex,
    IDXGIResource,
};
use windows::core::Interface;

#[derive(Clone)]
pub(crate) struct D3D11SurfaceWriter {
    shared: Arc<D3D11Shared>,
}

pub(crate) struct D3D11Surface {
    frame: WindowsGpuVideoFrame,
    texture: ID3D11Texture2D,
    subresource: u32,
    desc: D3D11_TEXTURE2D_DESC,
    shared_handle: Option<isize>,
    shared: Arc<D3D11Shared>,
}

struct D3D11Shared {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Mutex<Option<(D3D11_TEXTURE2D_DESC, ID3D11Texture2D)>>,
    reported_format: Mutex<Option<windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT>>,
}

pub(crate) fn acquire_texture_sync(sync: &IDXGIKeyedMutex, timeout_ms: u32) -> Result<()> {
    // AcquireSync returns positive WAIT_TIMEOUT/WAIT_ABANDONED values too;
    // the generated Result wrapper treats those as successful HRESULTs.
    let status = unsafe { (Interface::vtable(sync).AcquireSync)(sync.as_raw(), 0, timeout_ms) };
    if status != windows::core::HRESULT(0) {
        bail!("acquire shared video texture failed: {status:?}");
    }
    Ok(())
}

struct TextureReadGuard(Option<IDXGIKeyedMutex>);
impl Drop for TextureReadGuard {
    fn drop(&mut self) {
        if let Some(sync) = &self.0
            && let Err(error) = unsafe { sync.ReleaseSync(0) }
        {
            tracing::warn!(%error, "release readback texture sync failed");
        }
    }
}

// The device is created with D3D11 multithread protection enabled. All immediate-context
// calls are serialized by the driver, while mutable readback-pool state is protected by a
// Mutex. The raw COM wrappers are reference-counted and remain alive through this Arc.
unsafe impl Send for D3D11Shared {}
unsafe impl Sync for D3D11Shared {}

impl std::fmt::Debug for D3D11Surface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D3D11Surface")
            .field("width", &self.desc.Width)
            .field("height", &self.desc.Height)
            .field("format", &self.desc.Format)
            .finish_non_exhaustive()
    }
}

impl D3D11SurfaceWriter {
    pub(crate) fn new() -> Result<Self> {
        Self::available()?
            .into_iter()
            .next()
            .context("no usable D3D11 adapter")
    }

    pub(crate) fn available() -> Result<Vec<Self>> {
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.context("enumerate D3D11 adapters")?;
        let preferred = factory.cast::<IDXGIFactory6>().ok();
        let mut writers = Vec::new();
        for index in 0.. {
            let adapter: windows::core::Result<IDXGIAdapter1> = unsafe {
                match &preferred {
                    Some(factory) => factory
                        .EnumAdapterByGpuPreference(index, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE),
                    None => factory.EnumAdapters1(index),
                }
            };
            let Ok(adapter) = adapter else {
                break;
            };
            if unsafe { adapter.GetDesc1() }
                .is_ok_and(|desc| desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0)
            {
                continue;
            }
            match Self::from_adapter(&adapter) {
                Ok(writer) => writers.push(writer),
                Err(error) => tracing::debug!(index, %error, "D3D11 adapter unavailable"),
            }
        }
        if writers.is_empty() {
            bail!("no usable hardware D3D11 adapter");
        }
        Ok(writers)
    }

    pub(crate) fn supports_codec(
        &self,
        codec: mediaway_common::CodecKind,
        width: u32,
        height: u32,
        depth: u8,
    ) -> bool {
        unsafe { self.shared.device.GetDeviceRemovedReason() }.is_ok()
            && crate::decoder::platform::windows::WindowsVideoDecoder::probe(
                self.device_handle(),
                codec,
                width,
                height,
                depth,
            )
    }

    pub(crate) fn for_codec(
        codec: mediaway_common::CodecKind,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        Self::available()?
            .into_iter()
            .find(|writer| writer.supports_codec(codec, width, height, 8))
            .context("no D3D11 adapter accepts the requested codec and dimensions")
    }

    fn from_adapter(adapter: &IDXGIAdapter1) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let mut result = Ok(());
        for flags in [
            flags,
            windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG(0),
        ] {
            device = None;
            context = None;
            result = unsafe {
                D3D11CreateDevice(
                    adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&raw mut device),
                    None,
                    Some(&raw mut context),
                )
            };
            if result.is_ok() {
                break;
            }
        }
        result.context("create D3D11 video device")?;
        let device = device.context("D3D11 did not return a device")?;
        let context = context.context("D3D11 did not return an immediate context")?;
        configure_device(&device, true);
        Ok(Self {
            shared: Arc::new(D3D11Shared {
                device,
                context,
                staging: Mutex::new(None),
                reported_format: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn device_handle(&self) -> GpuDeviceHandle {
        let handle = NativeHandle::new(Interface::as_raw(&self.shared.device) as usize)
            .expect("a live D3D11 device has a non-null COM pointer");
        GpuDeviceHandle::DirectX11(handle)
    }

    pub(crate) fn create_renderer_device(&self) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        create_renderer_device(&self.shared.device)
    }

    pub(crate) fn wrap_decoded_surface(&self, frame: WindowsGpuVideoFrame) -> Result<D3D11Surface> {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { frame.texture().GetDesc(&raw mut source_desc) };
        if source_desc.Format != DXGI_FORMAT_NV12 && source_desc.Format != DXGI_FORMAT_P010 {
            bail!(
                "Windows decoder returned unsupported D3D11 format {:?}",
                source_desc.Format
            );
        }
        let mut reported = self
            .shared
            .reported_format
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reported.replace(source_desc.Format) != Some(source_desc.Format) {
            tracing::info!(
                format = ?source_desc.Format,
                coded_width = source_desc.Width,
                coded_height = source_desc.Height,
                array_size = source_desc.ArraySize,
                bind_flags = source_desc.BindFlags,
                misc_flags = source_desc.MiscFlags,
                visible_x = frame.visible_x(),
                visible_y = frame.visible_y(),
                visible_width = frame.width(),
                visible_height = frame.height(),
                "Windows decoder output surface format changed"
            );
        }
        drop(reported);
        let source_is_shared =
            source_desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 != 0;
        let shared_handle = if source_is_shared {
            let resource: IDXGIResource = frame
                .texture()
                .cast()
                .context("query decoder shared DXGI resource")?;
            Some(
                unsafe { resource.GetSharedHandle() }
                    .context("get decoder shared texture handle")?
                    .0 as isize,
            )
        } else {
            None
        };
        // Keep the decoder sample/subresource intact. The presenter adopts this
        // device for non-shared input (UU SelectRendererAllocator, 0x180C9AE80).
        Ok(D3D11Surface {
            texture: frame.texture().clone(),
            subresource: frame.subresource(),
            desc: source_desc,
            shared_handle,
            frame,
            shared: Arc::clone(&self.shared),
        })
    }
}

fn create_renderer_device(
    decoder_device: &ID3D11Device,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let dxgi_device: IDXGIDevice = decoder_device.cast().context("query decoder DXGI device")?;
    let adapter = unsafe { dxgi_device.GetAdapter() }.context("get decoder DXGI adapter")?;
    let mut device = None;
    let mut context = None;
    let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut context),
        )
    }
    .context("create isolated D3D11 renderer device on decoder adapter")?;
    let device = device.context("D3D11 did not return an isolated renderer device")?;
    let context = context.context("D3D11 did not return an isolated renderer context")?;
    configure_device(&device, false);
    Ok((device, context))
}

fn configure_device(device: &ID3D11Device, limit_device_latency: bool) {
    if let Ok(multithread) = unsafe { device.GetImmediateContext() }
        .and_then(|context| context.cast::<ID3D11Multithread>())
    {
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
    }
    if let Ok(dxgi_device) = device.cast::<IDXGIDevice>()
        && let Err(error) = unsafe { dxgi_device.SetGPUThreadPriority(7) }
    {
        tracing::warn!(%error, "failed to apply the official DXGI GPU priority");
    }
    if limit_device_latency
        && let Ok(dxgi_device) = device.cast::<IDXGIDevice1>()
        && let Err(error) = unsafe { dxgi_device.SetMaximumFrameLatency(1) }
    {
        tracing::warn!(%error, "failed to apply the official decoder frame-latency limit");
    }
}

impl D3D11Surface {
    pub(crate) fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub(crate) const fn subresource(&self) -> u32 {
        self.subresource
    }

    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.shared.device
    }

    pub(crate) fn belongs_to_device(&self, device: &ID3D11Device) -> bool {
        Interface::as_raw(&self.shared.device) == Interface::as_raw(device)
    }

    pub(crate) const fn shared_handle(&self) -> Option<isize> {
        self.shared_handle
    }

    pub(crate) fn create_renderer_device(&self) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        create_renderer_device(&self.shared.device)
    }

    pub(crate) fn context(&self) -> &ID3D11DeviceContext {
        &self.shared.context
    }

    /// Allocation dimensions reported by the decoder texture. Hardware
    /// decoders may align these beyond the visible picture dimensions (for
    /// example a 1920x1200 picture in a 1920x1216 NV12 allocation).
    pub(crate) fn coded_size(&self) -> (u32, u32) {
        (self.desc.Width, self.desc.Height)
    }

    pub(crate) const fn visible_origin(&self) -> (u32, u32) {
        (self.frame.visible_x(), self.frame.visible_y())
    }

    pub(crate) fn format(&self) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
        self.desc.Format
    }

    pub(crate) fn readback_nv12(&self, width: u32, height: u32) -> Result<Vec<u8>> {
        if self.desc.Format != DXGI_FORMAT_NV12 {
            bail!(
                "CPU readback only supports NV12, got {:?}",
                self.desc.Format
            );
        }
        let sync = if self.shared_handle.is_some() {
            let sync = self
                .texture
                .cast::<IDXGIKeyedMutex>()
                .context("query readback texture sync")?;
            acquire_texture_sync(&sync, 100)?;
            Some(sync)
        } else {
            None
        };
        let _sync = TextureReadGuard(sync);
        let mut staging = self
            .shared
            .staging
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let matches = staging.as_ref().is_some_and(|(desc, _)| {
            desc.Width == self.desc.Width
                && desc.Height == self.desc.Height
                && desc.Format == self.desc.Format
        });
        if !matches {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: self.desc.Width,
                Height: self.desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: self.desc.Format,
                SampleDesc: self.desc.SampleDesc,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            *staging = Some((
                staging_desc,
                create_texture(&self.shared.device, &staging_desc)?,
            ));
        }
        let staging_texture = staging
            .as_ref()
            .expect("staging texture was initialized")
            .1
            .clone();
        unsafe {
            self.shared.context.CopySubresourceRegion(
                &staging_texture,
                0,
                0,
                0,
                0,
                self.texture(),
                self.subresource(),
                None,
            );
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.shared.context.Map(
                &staging_texture,
                0,
                D3D11_MAP_READ,
                0,
                Some(&raw mut mapped),
            )
        }
        .context("map owned D3D11 NV12 surface")?;
        let result = copy_mapped_nv12(
            &mapped,
            self.desc.Height,
            self.frame.visible_x(),
            self.frame.visible_y(),
            width,
            height,
        );
        unsafe { self.shared.context.Unmap(&staging_texture, 0) };
        result
    }
}

fn create_texture(device: &ID3D11Device, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
    let mut texture = None;
    unsafe { device.CreateTexture2D(desc, None, Some(&raw mut texture)) }
        .context("create D3D11 video surface")?;
    texture.context("D3D11 did not return a video surface")
}

fn copy_mapped_nv12(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    coded_height: u32,
    visible_x: u32,
    visible_y: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    if mapped.pData.is_null() || mapped.RowPitch < width {
        bail!("mapped D3D11 NV12 texture has an invalid pointer or row pitch");
    }
    let width = usize::try_from(width).context("NV12 width does not fit usize")?;
    let height = usize::try_from(height).context("NV12 height does not fit usize")?;
    let coded_height =
        usize::try_from(coded_height).context("NV12 coded height does not fit usize")?;
    let visible_x = usize::try_from(visible_x).context("NV12 crop x does not fit usize")?;
    let visible_y = usize::try_from(visible_y).context("NV12 crop y does not fit usize")?;
    let row_pitch = usize::try_from(mapped.RowPitch).context("NV12 pitch does not fit usize")?;
    if visible_x.saturating_add(width) > row_pitch
        || visible_y.saturating_add(height) > coded_height
    {
        bail!("NV12 visible aperture exceeds the mapped coded surface");
    }
    let luma_len = width
        .checked_mul(height)
        .context("NV12 luma size overflow")?;
    let mut output = vec![0u8; luma_len + luma_len / 2];
    let source = mapped.pData.cast::<u8>();
    unsafe {
        for row in 0..height {
            std::ptr::copy_nonoverlapping(
                source.add((visible_y + row) * row_pitch + visible_x),
                output.as_mut_ptr().add(row * width),
                width,
            );
        }
        let source_uv = source.add((coded_height + visible_y / 2) * row_pitch + visible_x);
        let output_uv = output.as_mut_ptr().add(luma_len);
        for row in 0..height / 2 {
            std::ptr::copy_nonoverlapping(
                source_uv.add(row * row_pitch),
                output_uv.add(row * width),
                width,
            );
        }
    }
    Ok(output)
}
