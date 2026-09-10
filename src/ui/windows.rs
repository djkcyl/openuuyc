use super::d3d11::UiPresenter;
use super::{AppFactory, AppSession, WindowConfig};
use anyhow::{Context, Result, anyhow, bail};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};
use windows::core::Interface;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

struct Repaint {
    pass: u64,
    when: Instant,
}

pub(super) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    let event_loop = EventLoop::<Repaint>::with_user_event()
        .build()
        .context("create desktop event loop")?;
    let mut runner = Runner {
        config,
        factory: Some(factory),
        state: None,
        error: None,
        proxy: event_loop.create_proxy(),
    };
    event_loop
        .run_app(&mut runner)
        .context("run D3D11 desktop event loop")?;
    if let Some(error) = runner.error.take() {
        bail!(error);
    }
    Ok(())
}

struct Runner {
    config: WindowConfig,
    factory: Option<AppFactory>,
    state: Option<DesktopWindow>,
    error: Option<String>,
    proxy: EventLoopProxy<Repaint>,
}

struct DesktopWindow {
    // Shut down business workers first; release composition before the HWND.
    app: AppSession,
    presenter: UiPresenter,
    context: egui::Context,
    input: egui_winit::State,
    viewport: egui::ViewportInfo,
    close_requested: bool,
    next_repaint: Option<Instant>,
    last_frame: Option<Instant>,
    interval: Duration,
    window: Window,
}

impl Runner {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        self.error = Some(format!("{error:#}"));
        self.state.take();
        event_loop.exit();
    }

    fn create(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        let context = egui::Context::default();
        context.set_embed_viewports(true);
        let window = egui_winit::create_window(
            &context,
            event_loop,
            &self.config.viewport.clone().with_visible(false),
        )?;
        if self.config.centered
            && let Some(monitor) = window.current_monitor()
        {
            let outer = window.outer_size();
            let size = monitor.size();
            let origin = monitor.position();
            window.set_outer_position(PhysicalPosition::new(
                origin.x + (size.width as i64 - outer.width as i64).max(0) as i32 / 2,
                origin.y + (size.height as i64 - outer.height as i64).max(0) as i32 / 2,
            ));
        }
        let (device, immediate, graphics) = create_device()?;
        let presenter = UiPresenter::new(&window, device, immediate)?;
        let input = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let mut viewport = egui::ViewportInfo::default();
        egui_winit::update_viewport_info(&mut viewport, &context, &window, true);
        let proxy = self.proxy.clone();
        context.set_request_repaint_callback(move |info| {
            if info.viewport_id == egui::ViewportId::ROOT
                && let Some(when) = Instant::now().checked_add(info.delay)
            {
                let _ = proxy.send_event(Repaint {
                    pass: info.current_cumulative_pass_nr,
                    when,
                });
            }
        });
        let factory = self
            .factory
            .take()
            .context("desktop factory already consumed")?;
        let app = AppSession(factory(&context, Some(graphics)));
        let refresh = window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .filter(|rate| *rate != 0)
            .unwrap_or(60_000);
        self.state = Some(DesktopWindow {
            app,
            presenter,
            context,
            input,
            viewport,
            close_requested: false,
            next_repaint: Some(Instant::now()),
            last_frame: None,
            interval: Duration::from_secs_f64(1000.0 / f64::from(refresh)),
            window,
        });
        let state = self.state.as_mut().expect("created desktop state");
        state.render()?;
        state
            .window
            .set_visible(self.config.viewport.visible.unwrap_or(true));
        Ok(())
    }
}

impl DesktopWindow {
    fn schedule(&mut self, when: Instant) {
        let when = self
            .last_frame
            .map_or(when, |last| when.max(last + self.interval));
        self.next_repaint = Some(self.next_repaint.map_or(when, |old| old.min(when)));
    }

    fn render(&mut self) -> Result<()> {
        self.next_repaint = None;
        self.last_frame = Some(Instant::now());
        egui_winit::update_viewport_info(&mut self.viewport, &self.context, &self.window, false);
        let mut input = self.input.take_egui_input(&self.window);
        input
            .viewports
            .insert(egui::ViewportId::ROOT, self.viewport.clone());
        if self.close_requested {
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .expect("root viewport")
                .events
                .push(egui::ViewportEvent::Close);
        }
        self.viewport.events.clear();
        let output = self.context.run_ui(input, |ui| self.app.0.ui(ui));
        let (drawing, platform, mut viewports) = egui_directx11::split_output(output);
        self.input.handle_platform_output(&self.window, platform);
        if let Some(root) = viewports.remove(&egui::ViewportId::ROOT) {
            for command in &root.commands {
                match command {
                    egui::ViewportCommand::Close => self.close_requested = true,
                    egui::ViewportCommand::CancelClose => self.close_requested = false,
                    _ => {}
                }
            }
            let mut actions = Vec::new();
            egui_winit::process_viewport_commands(
                &self.context,
                &mut self.viewport,
                root.commands.into_iter().filter(|command| {
                    !matches!(
                        command,
                        egui::ViewportCommand::Close | egui::ViewportCommand::CancelClose
                    )
                }),
                &self.window,
                &mut actions,
            );
            for action in actions {
                let event = match action {
                    egui_winit::ActionRequested::Cut => Some(egui::Event::Cut),
                    egui_winit::ActionRequested::Copy => Some(egui::Event::Copy),
                    egui_winit::ActionRequested::Paste => self
                        .input
                        .clipboard_text()
                        .map(|text| text.replace("\r\n", "\n"))
                        .filter(|text| !text.is_empty())
                        .map(egui::Event::Paste),
                    // No current application screen requests framebuffer screenshots.
                    egui_winit::ActionRequested::Screenshot(_) => None,
                };
                if let Some(event) = event {
                    self.input.egui_input_mut().events.push(event);
                    self.schedule(Instant::now());
                }
            }
            if let Some(when) = Instant::now().checked_add(root.repaint_delay) {
                self.schedule(when);
            }
        }
        if !self.close_requested && self.window.is_minimized() != Some(true) {
            self.presenter.render(&self.context, drawing, false)?;
        } else if !self.close_requested {
            // QR/font updates must survive a minimized window and upload on restore.
            self.presenter.defer_output(drawing);
        }
        Ok(())
    }
}

impl ApplicationHandler<Repaint> for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create(event_loop) {
            self.fail(event_loop, error);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut().filter(|state| state.window.id() == id) else {
            return;
        };
        let response = state.input.on_window_event(&state.window, &event);
        let result = match event {
            WindowEvent::CloseRequested => {
                state.close_requested = true;
                state.render()
            }
            WindowEvent::Destroyed => {
                event_loop.exit();
                Ok(())
            }
            WindowEvent::Resized(size) => {
                state.schedule(Instant::now());
                state.presenter.resize(size)
            }
            WindowEvent::RedrawRequested => state.render(),
            _ => {
                if response.repaint {
                    state.schedule(Instant::now());
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            self.fail(event_loop, error);
            return;
        }
        if self
            .state
            .as_ref()
            .is_some_and(|state| state.close_requested)
        {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: Repaint) {
        if let Some(state) = &mut self.state {
            let current = state.context.cumulative_pass_nr();
            if current == event.pass || current == event.pass.saturating_add(1) {
                state.schedule(event.when);
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            if let Some(when) = state.next_repaint {
                if when <= Instant::now() {
                    state.next_repaint = None;
                    state.window.request_redraw();
                    event_loop.set_control_flow(ControlFlow::Wait);
                } else {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(when));
                }
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.state.take();
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, String)> {
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
                let graphics = format!(
                    "{} · D3D11",
                    String::from_utf16_lossy(&desc.Description[..length])
                );
                return Ok((device, context, graphics));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow!("create GUI D3D11 device: {:?}", last_error))
}
