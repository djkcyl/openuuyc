//! UI composition stays on the window thread and never submits a video frame.
//! UU hands a separate native video window to streamer. DirectComposition gives
//! our egui UI the equivalent independent layer above that child window.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use super::windows_presenter::{
    create_backbuffer, nonzero_size, title_bar_height_pixels, window_hwnd,
};

pub(super) struct VideoWindow {
    hwnd: HWND,
}

impl VideoWindow {
    pub(super) fn new(parent: &Window) -> Result<Self> {
        // A disabled child is skipped by hit testing, so the parent continues
        // to receive all local UI input, including menus drawn above the video.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("UU Video"),
                WS_CHILD | WS_VISIBLE | WS_DISABLED | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                Some(window_hwnd(parent)?),
                None,
                None,
                None,
            )
        }
        .context("create native video child window")?;
        let child = Self { hwnd };
        child.resize(parent, parent.inner_size())?;
        tracing::info!(
            parent = window_hwnd(parent)?.0 as usize,
            video = hwnd.0 as usize,
            "created independent UI and video presentation targets"
        );
        Ok(child)
    }

    pub(super) fn handle(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub(super) fn resize(
        &self,
        parent: &Window,
        size: PhysicalSize<u32>,
    ) -> Result<PhysicalSize<u32>> {
        let top = title_bar_height_pixels(parent);
        let video_size =
            PhysicalSize::new(size.width.max(1), size.height.saturating_sub(top).max(1));
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                top as i32,
                video_size.width as i32,
                video_size.height as i32,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .context("resize native video child window")?;
        Ok(video_size)
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        // The owner drops/join()s Video Render before destroying this HWND.
        if let Err(error) = unsafe { DestroyWindow(self.hwnd) } {
            tracing::debug!(%error, "destroy video child window");
        }
    }
}

pub(super) struct UiPresenter {
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
}

impl UiPresenter {
    pub(super) fn new(
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
            composition_target.SetRoot(&visual)?;
            composition.Commit()?;
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
        })
    }

    pub(super) fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
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

    pub(super) fn render(
        &mut self,
        context: &egui::Context,
        output: egui_directx11::RendererOutput,
        transparent: bool,
    ) -> Result<bool> {
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
            .context("draw player UI layer")?;
        self.pending_present = true;
        let presented = self.try_present()?;
        if !presented {
            context.request_repaint();
        }
        Ok(presented)
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

/// Opt-in, aggregated UI-only diagnostics. No RTP hot-path counters or HUD.
pub(super) struct UiTimingAudit {
    since: Instant,
    previous: Option<(Instant, bool)>,
    frames: u32,
    presented: u32,
    following_immediate: u32,
    immediate_gap: Duration,
    max_immediate_gap: Duration,
    max_layout: Duration,
    max_submit: Duration,
}

impl UiTimingAudit {
    pub(super) fn enabled() -> Option<Self> {
        tracing::enabled!(target: "openuuyc::ui_timing", tracing::Level::DEBUG)
            .then(|| Self::new(Instant::now()))
    }

    fn new(since: Instant) -> Self {
        Self {
            since,
            previous: None,
            frames: 0,
            presented: 0,
            following_immediate: 0,
            immediate_gap: Duration::ZERO,
            max_immediate_gap: Duration::ZERO,
            max_layout: Duration::ZERO,
            max_submit: Duration::ZERO,
        }
    }

    pub(super) fn record(
        &mut self,
        at: Instant,
        layout: Duration,
        submit: Duration,
        immediate: bool,
        presented: bool,
    ) {
        self.frames += 1;
        self.presented += u32::from(presented);
        if let Some((previous, true)) = self.previous {
            let gap = at.saturating_duration_since(previous);
            self.following_immediate += 1;
            self.immediate_gap += gap;
            self.max_immediate_gap = self.max_immediate_gap.max(gap);
        }
        self.previous = Some((at, immediate));
        self.max_layout = self.max_layout.max(layout);
        self.max_submit = self.max_submit.max(submit);
        if at.duration_since(self.since) >= Duration::from_secs(5) {
            tracing::debug!(target: "openuuyc::ui_timing",
                frames = self.frames, presented = self.presented,
                seconds = at.duration_since(self.since).as_secs_f64(),
                animation_intervals = self.following_immediate,
                animation_avg_ms = self.immediate_gap.as_secs_f64() * 1000.0 / f64::from(self.following_immediate.max(1)),
                animation_max_ms = self.max_immediate_gap.as_secs_f64() * 1000.0,
                layout_max_ms = self.max_layout.as_secs_f64() * 1000.0,
                submit_max_ms = self.max_submit.as_secs_f64() * 1000.0,
                "UI refresh audit");
            let previous = self.previous;
            *self = Self::new(at);
            self.previous = previous;
        }
    }
}
