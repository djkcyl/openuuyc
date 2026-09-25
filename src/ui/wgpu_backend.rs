//! wgpu presentation for the shared egui shell on X11 and Wayland.
//!
//! Mirrors the D3D11 presenter: one swap chain (surface) per window, egui
//! output deferred so a busy presentation never blocks the event loop, and a
//! premultiplied-alpha surface where the compositor offers one.
use anyhow::{Context, Result};
use std::sync::Arc;
use winit::dpi::PhysicalSize;
use winit::window::Window;

use super::gfx::nonzero_size;

/// egui drawing output, separated from the platform half of `FullOutput`.
pub(crate) struct RendererOutput {
    pub(crate) textures_delta: egui::TexturesDelta,
    pub(crate) shapes: Vec<egui::epaint::ClippedShape>,
    pub(crate) pixels_per_point: f32,
}

/// Split a [`egui::FullOutput`] into the drawing part and the platform parts,
/// matching `egui_directx11::split_output` so the shell stays platform-neutral.
pub(crate) fn split_output(
    full_output: egui::FullOutput,
) -> (
    RendererOutput,
    egui::PlatformOutput,
    egui::OrderedViewportIdMap<egui::ViewportOutput>,
) {
    (
        RendererOutput {
            textures_delta: full_output.textures_delta,
            shapes: full_output.shapes,
            pixels_per_point: full_output.pixels_per_point,
        },
        full_output.platform_output,
        full_output.viewport_output,
    )
}

/// The process-wide wgpu device shared by every shell window.
#[derive(Clone)]
pub(crate) struct Graphics {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    label: String,
}

impl Graphics {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}

/// One adapter and device for the whole process: windows open and close, but
/// re-creating a Vulkan device per window would stall each one on startup.
pub(crate) fn create_device() -> Result<Graphics> {
    static SHARED: std::sync::OnceLock<std::result::Result<Graphics, String>> =
        std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| create_graphics().map_err(|error| format!("{error:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn create_graphics() -> Result<Graphics> {
    // Vulkan first, GL as the fallback for drivers or VMs without it.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .context("没有可用的 GPU 适配器（需要 Vulkan 或 OpenGL）")?;
    let info = adapter.get_info();
    let (device, queue) = pollster::block_on(
        adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("openuuyc-ui"),
            required_features: wgpu::Features::empty(),
            // The UI stays within the baseline limits; video upload does too.
            required_limits: wgpu::Limits::downlevel_defaults()
                .using_resolution(adapter.limits())
                .using_alignment(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }),
    )
    .context("创建 GPU 设备失败")?;
    let label = format!("{} · {:?}", info.name, info.backend);
    Ok(Graphics {
        instance,
        adapter,
        device,
        queue,
        label,
    })
}

pub(crate) struct UiPresenter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    video: super::wgpu_video::VideoLayer,
    /// Where the next frame goes; `None` leaves the video layer unpainted.
    placement: Option<super::wgpu_video::VideoPlacement>,
    /// The YUV to RGB rows for an NV12 frame; `None` when the frame is RGBA.
    transform: Option<[[f32; 4]; 3]>,
    pending_output: Option<RendererOutput>,
    size: PhysicalSize<u32>,
    configured: bool,
}

impl UiPresenter {
    pub(crate) fn new(window: Arc<Window>, graphics: &Graphics) -> Result<Self> {
        let size = nonzero_size(window.inner_size());
        let surface = graphics
            .instance
            .create_surface(window)
            .context("创建窗口绘制表面失败")?;
        let capabilities = surface.get_capabilities(&graphics.adapter);
        let format = surface_format(&capabilities);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width,
            height: size.height,
            present_mode: present_mode(&capabilities),
            desired_maximum_frame_latency: 2,
            alpha_mode: alpha_mode(&capabilities),
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: Vec::new(),
        };
        surface.configure(&graphics.device, &config);
        let renderer = egui_wgpu::Renderer::new(
            &graphics.device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: true,
                ..Default::default()
            },
        );
        let video = super::wgpu_video::VideoLayer::new(&graphics.device, format);
        Ok(Self {
            surface,
            device: graphics.device.clone(),
            queue: graphics.queue.clone(),
            config,
            renderer,
            video,
            placement: None,
            transform: None,
            pending_output: None,
            size,
            configured: true,
        })
    }

    /// Replace the video frame drawn under the UI with packed RGBA8 pixels.
    pub(crate) fn upload_video(&mut self, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
        self.transform = None;
        self.video
            .upload_rgba(&self.device, &self.queue, width, height, pixels)
    }

    /// Replace it with packed NV12, converted to RGB by the fragment shader.
    pub(crate) fn upload_video_nv12(
        &mut self,
        width: u32,
        height: u32,
        data: &[u8],
        transform: [[f32; 4]; 3],
    ) -> Result<()> {
        self.transform = Some(transform);
        self.video
            .upload_nv12(&self.device, &self.queue, width, height, data)
    }

    pub(crate) fn set_video_placement(
        &mut self,
        placement: Option<super::wgpu_video::VideoPlacement>,
    ) {
        self.placement = placement;
    }

    pub(crate) fn has_video(&self) -> bool {
        self.video.has_frame()
    }

    pub(crate) fn clear_video(&mut self) {
        self.video.clear();
        self.placement = None;
    }

    pub(crate) fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 || size == self.size {
            return Ok(());
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.size = size;
        self.configured = true;
        Ok(())
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        output: RendererOutput,
        transparent: bool,
    ) -> Result<bool> {
        self.defer_output(output);
        let Some(mut output) = self.pending_output.take() else {
            return Ok(false);
        };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // Still drawable; the next resize reconfigures it properly.
                self.configured = false;
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                // The compositor dropped the swap chain, commonly on a monitor
                // or scale change. Reconfigure and draw on the next repaint.
                self.surface.configure(&self.device, &self.config);
                self.pending_output = Some(output);
                self.flush_writes();
                context.request_repaint();
                return Ok(false);
            }
            other => {
                // Timeout or occluded: keep the frame and try again later.
                tracing::debug!(?other, "窗口绘制表面暂时不可用");
                self.pending_output = Some(output);
                self.flush_writes();
                context.request_repaint();
                return Ok(false);
            }
        };
        let pixels_per_point = output.pixels_per_point;
        let jobs = context.tessellate(output.shapes, pixels_per_point);
        let descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("openuuyc-ui"),
            });
        let user_buffers = self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &jobs,
            &descriptor,
        );
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("openuuyc-ui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: if transparent { 0.0 } else { 1.0 },
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            if let Some(placement) = self.placement {
                self.video.draw(
                    &self.queue,
                    &mut pass,
                    (self.config.width, self.config.height),
                    placement,
                    self.transform.unwrap_or_default(),
                );
            }
            self.renderer.render(&mut pass, &jobs, &descriptor);
        }
        self.queue
            .submit(user_buffers.into_iter().chain(Some(encoder.finish())));
        // Freeing after submit keeps a texture alive while the frame still uses it.
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        // Every delta has been handed to the renderer; epaint asserts on drop
        // if any is still pending.
        output.textures_delta.clear();
        self.queue.present(frame);
        // Nothing else reclaims what a finished submission held. Each frame's
        // `write_texture` takes a staging buffer from the device, and neither
        // submitting nor presenting hands it back -- only maintaining the device
        // does. Without this the buffers pile up at the video bitrate, a few
        // hundred megabytes a second, until an upload fails with out of memory.
        if let Err(error) = self.device.poll(wgpu::PollType::Poll) {
            tracing::debug!(?error, "维护 GPU 设备失败");
        }
        Ok(true)
    }

    /// Hand any queued `write_texture` data to the GPU without drawing.
    ///
    /// A write is only staged until the next submission, so a frame uploaded
    /// for a pass that then bails would otherwise hold its staging copy inside
    /// the device forever. An empty submission is enough to release it.
    fn flush_writes(&self) {
        self.queue.submit(std::iter::empty());
        if let Err(error) = self.device.poll(wgpu::PollType::Poll) {
            tracing::debug!(?error, "维护 GPU 设备失败");
        }
    }

    pub(crate) fn defer_output(&mut self, output: RendererOutput) {
        // Same merge rule as egui::FullOutput::append: only the newest shapes,
        // but all required texture/font changes.
        if let Some(pending) = &mut self.pending_output {
            pending.textures_delta.append(output.textures_delta);
            pending.shapes = output.shapes;
            pending.pixels_per_point = output.pixels_per_point;
        } else {
            self.pending_output = Some(output);
        }
    }
}

impl Drop for UiPresenter {
    fn drop(&mut self) {
        // A deferred frame still owns texture deltas that will never be applied.
        if let Some(mut pending) = self.pending_output.take() {
            pending.textures_delta.clear();
        }
    }
}

/// egui writes gamma-space colors, so a non-sRGB 8-bit format is the match.
fn surface_format(capabilities: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    const PREFERRED: [wgpu::TextureFormat; 2] = [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
    ];
    PREFERRED
        .into_iter()
        .find(|format| capabilities.formats.contains(format))
        .or_else(|| capabilities.formats.first().copied())
        .unwrap_or(wgpu::TextureFormat::Bgra8Unorm)
}

/// Mailbox never blocks the event loop; Fifo is the always-present fallback.
fn present_mode(capabilities: &wgpu::SurfaceCapabilities) -> wgpu::PresentMode {
    if capabilities
        .present_modes
        .contains(&wgpu::PresentMode::Mailbox)
    {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    }
}

fn alpha_mode(capabilities: &wgpu::SurfaceCapabilities) -> wgpu::CompositeAlphaMode {
    if capabilities
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        wgpu::CompositeAlphaMode::Auto
    }
}
