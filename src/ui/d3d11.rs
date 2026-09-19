//! D3D11 egui composition shared by the device center and playback windows.
use super::gfx::nonzero_size;
use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;
use winit::dpi::PhysicalSize;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

pub(crate) struct UiPresenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    target: Option<ID3D11RenderTargetView>,
    backbuffer: Option<ID3D11Texture2D>,
    renderer: egui_directx11::Renderer,
    size: PhysicalSize<u32>,
    // Keep the entire composition tree alive until its swap chain is released.
    composition: IDCompositionDevice,
    composition_target: IDCompositionTarget,
    visual: IDCompositionVisual,
    pending_output: Option<egui_directx11::RendererOutput>,
    pending_present: bool,
    attached: bool,
}

impl UiPresenter {
    pub(crate) fn new(
        window: std::sync::Arc<Window>,
        graphics: &super::gfx::Graphics,
    ) -> Result<Self> {
        Self::from_device(&window, graphics.device.clone(), graphics.context.clone())
    }

    /// The player already owns a device shared with its decoder surfaces.
    pub(crate) fn from_device(
        window: &Window,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    ) -> Result<Self> {
        let size = nonzero_size(window.inner_size());
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi.GetAdapter() }?;
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.width,
            Height: size.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swap_chain =
            unsafe { factory.CreateSwapChainForComposition(&device, &desc, None::<&IDXGIOutput>) }
                .context("create independent UI composition swap chain")?;
        let composition: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi) }?;
        let composition_target =
            unsafe { composition.CreateTargetForHwnd(window_hwnd(window)?, true) }?;
        let visual = unsafe { composition.CreateVisual() }?;
        unsafe {
            visual.SetContent(&swap_chain)?;
        }
        let (backbuffer, target) = create_backbuffer(&device, &swap_chain)?;
        let renderer = egui_directx11::Renderer::new(&device)?;
        Ok(Self {
            device,
            context,
            swap_chain,
            target: Some(target),
            backbuffer: Some(backbuffer),
            renderer,
            size,
            composition,
            composition_target,
            visual,
            pending_output: None,
            pending_present: false,
            attached: false,
        })
    }

    pub(crate) fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 || size == self.size {
            return Ok(());
        }
        unsafe {
            self.context.ClearState();
            self.context.Flush();
        }
        self.target.take();
        self.backbuffer.take();
        self.pending_present = false;
        unsafe {
            self.swap_chain.ResizeBuffers(
                2,
                size.width,
                size.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .context("resize UI composition swap chain")?;
        let (buffer, target) = create_backbuffer(&self.device, &self.swap_chain)?;
        self.backbuffer = Some(buffer);
        self.target = Some(target);
        self.size = size;
        Ok(())
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        output: egui_directx11::RendererOutput,
        transparent: bool,
    ) -> Result<bool> {
        self.defer_output(output);
        if self.pending_present && !self.try_present()? {
            context.request_repaint();
            return Ok(false);
        }
        let output = self.pending_output.take().expect("queued UI output");
        let target = self
            .target
            .as_ref()
            .context("UI render target unavailable")?;
        let alpha = if transparent { 0.0 } else { 1.0 };
        unsafe {
            self.context
                .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, alpha]);
        }
        self.renderer
            .render(&self.context, target, context, output)
            .context("draw UI layer")?;
        self.pending_present = true;
        let presented = self.try_present()?;
        if !presented {
            context.request_repaint();
        }
        Ok(presented)
    }

    pub(crate) fn defer_output(&mut self, output: egui_directx11::RendererOutput) {
        // Same merge rule as egui::FullOutput::append: only the newest shapes,
        // but all required texture/font changes. This is UI output, not a
        // video-frame queue. Keep processing input while DXGI is occupied.
        if let Some(pending) = &mut self.pending_output {
            pending.textures_delta.append(output.textures_delta);
            pending.shapes = output.shapes;
            pending.pixels_per_point = output.pixels_per_point;
        } else {
            self.pending_output = Some(output);
        }
    }

    fn try_present(&mut self) -> Result<bool> {
        let started = Instant::now();
        // A UI message-pump thread must never wait for the composition queue.
        // WAS_STILL_DRAWING is backpressure, not a device failure. Retry on a
        // later repaint, without repeatedly uploading/drawing the same frame.
        let status = unsafe { self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) };
        let elapsed = started.elapsed();
        if status == DXGI_ERROR_WAS_STILL_DRAWING {
            return Ok(false);
        }
        if elapsed >= Duration::from_millis(50) || status != windows::core::HRESULT(0) {
            tracing::warn!(
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                hresult = status.0,
                "player UI Present completed with delay or nonzero status"
            );
        }
        status.ok().context("present player UI layer")?;
        if !self.attached {
            // Do not expose an uninitialized swap chain during startup or replacement.
            unsafe {
                self.composition_target.SetRoot(&self.visual)?;
                self.composition.Commit()?;
            }
            self.attached = true;
        }
        self.pending_present = false;
        Ok(true)
    }
}

impl Drop for UiPresenter {
    fn drop(&mut self) {
        if let Some(mut pending) = self.pending_output.take() {
            pending.textures_delta.clear();
        }
        unsafe {
            let _ = self.visual.SetContent(None::<&windows::core::IUnknown>);
            let _ = self
                .composition_target
                .SetRoot(None::<&IDCompositionVisual>);
            let _ = self.composition.Commit();
            self.context.ClearState();
            self.context.Flush();
        }
    }
}

pub(crate) fn window_hwnd(window: &Window) -> Result<HWND> {
    let RawWindowHandle::Win32(handle) = window
        .window_handle()
        .context("get Windows video window handle")?
        .as_raw()
    else {
        bail!("video window did not expose a Win32 handle");
    };
    Ok(HWND(handle.hwnd.get() as *mut std::ffi::c_void))
}

pub(crate) fn create_backbuffer(
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain1,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
    let backbuffer = unsafe { swap_chain.GetBuffer::<ID3D11Texture2D>(0) }
        .context("get D3D11 swap-chain backbuffer")?;
    let mut target = None;
    unsafe { device.CreateRenderTargetView(&backbuffer, None, Some(&raw mut target)) }
        .context("create D3D11 swap-chain render target")?;
    Ok((
        backbuffer,
        target.context("D3D11 did not return a render target")?,
    ))
}
