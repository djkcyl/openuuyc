use crate::ui::d3d11::{create_backbuffer, nonzero_size, window_hwnd};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D_SRV_DIMENSION_TEXTURE2D,
    D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUNDSMALL, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, WaitForSingleObjectEx,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{BOOL, Interface};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{ResizeDirection, Window, WindowAttributes, WindowId};

use super::windows_ui::{UiPresenter, UiTimingAudit, VideoWindow};
use crate::decoder::RenderSurface;
use crate::performance::{PerformanceMonitor, RenderedFrameTiming};
use crate::stream_control::StreamControlHandle;
use crate::video_color::RenderColor;

use super::{
    ConnectionProgress, ConnectionProgressApp, DecodedVideoFrame, NativeViewerSession,
    PerformancePanelMode, StreamControlUi, ViewerDisplayHandle, ViewerPreferences,
    ViewerWindowEvent, configure_viewer_visuals, install_system_cjk_font, mutex_lock,
    show_stream_control_window, take_next_frame,
};

#[path = "screen_windows.rs"]
mod screen_windows;
use screen_windows::{ScreenTabBar, ScreenWindows};

pub(super) fn run(session: NativeViewerSession) -> Result<()> {
    let (sender, receiver) = std_mpsc::channel();
    let alias = session
        .title
        .strip_prefix(crate::VIEWER_TITLE_PREFIX)
        .unwrap_or(&session.title)
        .to_owned();
    sender
        .send(ViewerWindowEvent::Playing(Box::new(session)))
        .map_err(|_| anyhow!("queue existing viewer session"))?;
    run_player(
        ConnectingWindowsRunConfig {
            alias,
            progress: std_mpsc::channel().1,
            session: receiver,
            display_sender: tokio::sync::oneshot::channel().0,
        },
        false,
    )
}

pub(super) struct ConnectingWindowsRunConfig {
    pub alias: String,
    pub progress: std_mpsc::Receiver<ConnectionProgress>,
    pub session: std_mpsc::Receiver<ViewerWindowEvent>,
    pub display_sender: tokio::sync::oneshot::Sender<ViewerDisplayHandle>,
}

pub(super) fn run_connecting(config: ConnectingWindowsRunConfig) -> Result<()> {
    run_player(config, true)
}

struct UiRepaintEvent {
    window: WindowId,
    generation: u64,
    pass: u64,
    when: Instant,
}

fn ui_frame_interval(window: &Window) -> Duration {
    let millihertz = window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .filter(|rate| *rate != 0)
        .unwrap_or(60_000);
    Duration::from_secs_f64(1000.0 / f64::from(millihertz))
}

fn run_player(config: ConnectingWindowsRunConfig, needs_display: bool) -> Result<()> {
    let event_loop = EventLoop::<UiRepaintEvent>::with_user_event()
        .build()
        .context("create Windows connection/player event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut runner = ConnectingWindowsRunner {
        attributes: WindowAttributes::default()
            .with_title(format!("{}{}", crate::VIEWER_TITLE_PREFIX, config.alias))
            .with_window_icon(Some(crate::ui::branding::window_icon()))
            .with_decorations(false)
            .with_inner_size(LogicalSize::new(1280.0, 760.0))
            .with_min_inner_size(LogicalSize::new(760.0, 520.0)),
        alias: config.alias,
        progress: Some(config.progress),
        session: config.session,
        display_sender: needs_display.then_some(config.display_sender),
        window: None,
        connecting: None,
        playing: None,
        fatal_error: None,
        next_ui_update: Instant::now(),
        next_repaint: None,
        last_ui_frame: None,
        ui_frame_interval: Duration::from_secs_f64(1.0 / 60.0),
        repaint_proxy: event_loop.create_proxy(),
        ui_generation: 1,
        close_requested: false,
        preferences: ViewerPreferences::default(),
        screens: None,
    };
    event_loop
        .run_app(&mut runner)
        .map_err(|error| anyhow!("run Windows connection/player event loop: {error}"))?;
    if let Some(error) = runner.fatal_error {
        bail!(error);
    }
    Ok(())
}

struct ConnectingWindowsRunner {
    screens: Option<ScreenWindows>,
    preferences: ViewerPreferences,
    close_requested: bool,
    attributes: WindowAttributes,
    alias: String,
    progress: Option<std_mpsc::Receiver<ConnectionProgress>>,
    session: std_mpsc::Receiver<ViewerWindowEvent>,
    display_sender: Option<tokio::sync::oneshot::Sender<ViewerDisplayHandle>>,
    window: Option<Window>,
    connecting: Option<WindowsConnectionApp>,
    playing: Option<ThreadedWindowsApp>,
    fatal_error: Option<String>,
    next_ui_update: Instant,
    next_repaint: Option<Instant>,
    last_ui_frame: Option<Instant>,
    ui_frame_interval: Duration,
    repaint_proxy: EventLoopProxy<UiRepaintEvent>,
    ui_generation: u64,
}

impl ApplicationHandler<UiRepaintEvent> for ConnectingWindowsRunner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match event_loop.create_window(self.attributes.clone()) {
            Ok(window) => {
                crate::ui::branding::set_taskbar_icon(&window);
                configure_dwm_window(&window);
                let progress = self
                    .progress
                    .take()
                    .expect("connecting window is created only once");
                self.ui_frame_interval = ui_frame_interval(&window);
                match WindowsConnectionApp::new(
                    &window,
                    self.alias.clone(),
                    progress,
                    self.repaint_proxy.clone(),
                    self.ui_generation,
                ) {
                    Ok((mut app, display)) => {
                        if let Err(error) = app.render(&window) {
                            self.fail(
                                event_loop,
                                format!("draw first connection UI frame: {error:#}"),
                            );
                            return;
                        }
                        self.last_ui_frame = Some(Instant::now());
                        if self
                            .display_sender
                            .take()
                            .is_some_and(|sender| sender.send(display).is_err())
                        {
                            // The account/connection owner can be cancelled
                            // before the first window has finished initializing.
                            // A closed display receiver is not a D3D failure.
                            self.close_requested = true;
                            event_loop.exit();
                            return;
                        }
                        self.window = Some(window);
                        self.connecting = Some(app);
                        if let Some(window) = self.window.as_ref() {
                            window.request_redraw();
                        }
                    }
                    Err(error) => self.fail(
                        event_loop,
                        format!("initialize Windows connection UI: {error:#}"),
                    ),
                }
            }
            Err(error) => self.fail(
                event_loop,
                format!("create Windows connection/player window: {error}"),
            ),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(screens) = self.screens.as_mut() {
            screens.window_event(window_id, &event);
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        if event == WindowEvent::CloseRequested {
            if let Some(app) = self.playing.as_ref() {
                app.shutdown.store(true, Ordering::Release);
            }
            event_loop.exit();
            return;
        }
        if matches!(
            event,
            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            self.ui_frame_interval = ui_frame_interval(window);
        }
        if event == WindowEvent::RedrawRequested {
            // Input events still enter egui immediately; coalesce their draw
            // requests to the local monitor cadence, never the stream's FPS.
            if window.is_minimized().unwrap_or(false) || !window.is_visible().unwrap_or(true) {
                self.next_repaint = None;
                return;
            }
            if let Some(earliest) = self.last_ui_frame.map(|at| at + self.ui_frame_interval)
                && Instant::now() < earliest
            {
                self.next_repaint =
                    Some(self.next_repaint.map_or(earliest, |old| old.min(earliest)));
                return;
            }
            self.next_repaint = None;
            self.last_ui_frame = Some(Instant::now());
        }
        let result = if let Some(app) = self.playing.as_mut() {
            app.on_window_event(window, &event)
        } else if let Some(app) = self.connecting.as_mut() {
            app.on_window_event(window, &event)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            self.fail(
                event_loop,
                format!("Windows connection/player failed: {error:#}"),
            );
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.screens.take();
        self.playing.take();
        self.connecting.take();
        self.window.take();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiRepaintEvent) {
        if let Err(error) = self.receive_session_events() {
            self.fail(event_loop, format!("update player session: {error:#}"));
            return;
        }
        if let Some(screens) = self.screens.as_mut() {
            screens.repaint(event);
            return;
        }
        // Match egui's callback contract: requests from the just
        // completed pass are valid; older requests have already been serviced.
        // A replaced room has a new Context and must reject the old callbacks.
        let context = self
            .playing
            .as_ref()
            .map(|app| &app.egui_context)
            .or_else(|| self.connecting.as_ref().map(|app| &app.egui_context));
        if event.generation == self.ui_generation
            && let Some(context) = context
        {
            let current = context.cumulative_pass_nr();
            if current == event.pass || current == event.pass.saturating_add(1) {
                let when = self
                    .last_ui_frame
                    .map_or(event.when, |at| event.when.max(at + self.ui_frame_interval));
                self.next_repaint = Some(self.next_repaint.map_or(when, |old| old.min(when)));
            }
        }
        if self.close_requested
            || self
                .playing
                .as_ref()
                .is_some_and(|app| app.shutdown.load(Ordering::Acquire))
        {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.receive_session_events() {
            self.fail(event_loop, format!("update player session: {error:#}"));
            return;
        }
        if self.close_requested {
            event_loop.exit();
            return;
        }
        if let Some(screens) = self.screens.as_mut() {
            screens.update(event_loop);
            return;
        }
        if let Some(app) = self.playing.as_ref() {
            if app.shutdown.load(Ordering::Acquire) {
                if let Some(error) = mutex_lock(&app.fatal_error).clone() {
                    self.fatal_error = Some(error);
                }
                event_loop.exit();
                return;
            }
        } else if self.connecting.is_none() {
            return;
        }
        if self
            .connecting
            .as_ref()
            .is_some_and(|app| app.close_requested)
        {
            event_loop.exit();
            return;
        }
        let now = Instant::now();
        let maintenance_due = now >= self.next_ui_update;
        let repaint_due = self.next_repaint.is_some_and(|when| now >= when);
        if maintenance_due || repaint_due {
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
            self.next_repaint = None;
            if maintenance_due {
                self.next_ui_update = now
                    + if self.playing.is_some() {
                        Duration::from_millis(250)
                    } else {
                        Duration::from_millis(33)
                    };
            }
        }
        let wake_at = self
            .next_repaint
            .map_or(self.next_ui_update, |when| when.min(self.next_ui_update));
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake_at));
    }
}

impl ConnectingWindowsRunner {
    fn receive_session_events(&mut self) -> Result<()> {
        while let Ok(event) = self.session.try_recv() {
            if self.close_requested {
                continue;
            }
            match event {
                ViewerWindowEvent::Close => self.close_requested = true,
                ViewerWindowEvent::Playing(session) => self.start_playing(*session)?,
                ViewerWindowEvent::Reconnect { progress, display } => {
                    if let Some(mut screens) = self.screens.take() {
                        self.window = screens.take_window();
                    }
                    // Keep the OS window, but join and discard the old room's
                    // render/decoder owners before attaching a new media peer.
                    if let Some(playing) = self.playing.as_ref() {
                        self.preferences = ViewerPreferences {
                            performance_mode: playing.performance_mode,
                            aspect_locked: playing.aspect_locked,
                        };
                        tracing::debug!(?self.preferences, "preserved local viewer preferences across room replacement");
                    }
                    self.playing.take();
                    self.connecting.take();
                    self.ui_generation = self.ui_generation.wrapping_add(1);
                    self.next_repaint = None;
                    let window = self
                        .window
                        .as_ref()
                        .context("player window closed during reconnect")?;
                    let (mut app, new_display) = WindowsConnectionApp::new(
                        window,
                        self.alias.clone(),
                        progress,
                        self.repaint_proxy.clone(),
                        self.ui_generation,
                    )?;
                    app.render(window)?;
                    self.connecting = Some(app);
                    display
                        .send(new_display)
                        .map_err(|_| anyhow!("connection owner cancelled window reconnect"))?;
                    window.request_redraw();
                }
            }
        }
        Ok(())
    }

    fn start_playing(&mut self, session: NativeViewerSession) -> Result<()> {
        let window = self
            .window
            .take()
            .context("connection/player window is unavailable")?;
        let connecting = self
            .connecting
            .take()
            .context("connection UI is unavailable")?;
        self.screens = Some(ScreenWindows::new(
            window,
            session,
            connecting,
            self.preferences,
            self.repaint_proxy.clone(),
            self.ui_generation,
        )?);
        Ok(())
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, message: String) {
        tracing::error!(%message);
        self.fatal_error = Some(message);
        if let Some(app) = self.playing.as_ref() {
            app.shutdown.store(true, Ordering::Release);
        }
        event_loop.exit();
    }
}

struct WindowsConnectionApp {
    progress: ConnectionProgressApp,
    presenter: UiPresenter,
    egui_context: egui::Context,
    egui_winit: egui_winit::State,
    close_requested: bool,
    timing_audit: Option<UiTimingAudit>,
}

impl WindowsConnectionApp {
    fn new(
        window: &Window,
        alias: String,
        receiver: std_mpsc::Receiver<ConnectionProgress>,
        repaint_proxy: EventLoopProxy<UiRepaintEvent>,
        generation: u64,
    ) -> Result<(Self, ViewerDisplayHandle)> {
        let egui_context = egui::Context::default();
        let window_id = window.id();
        egui_context.set_request_repaint_callback(move |info| {
            if info.viewport_id == egui::ViewportId::ROOT
                && let Some(when) = Instant::now().checked_add(info.delay)
            {
                let _ = repaint_proxy.send_event(UiRepaintEvent {
                    window: window_id,
                    generation,
                    pass: info.current_cumulative_pass_nr,
                    when,
                });
            }
        });
        install_system_cjk_font(&egui_context);
        configure_viewer_visuals(&egui_context);
        let egui_winit = egui_winit::State::new(
            egui_context.clone(),
            egui_context.viewport_id(),
            window,
            None,
            None,
            None,
        );
        let surface_writer = crate::decoder::windows_surface::D3D11SurfaceWriter::new()?;
        let (renderer_device, renderer_context) = surface_writer.create_renderer_device()?;
        let presenter = UiPresenter::new(window, renderer_device, renderer_context)?;
        Ok((
            Self {
                progress: ConnectionProgressApp::new(alias, receiver),
                presenter,
                egui_context,
                egui_winit,
                close_requested: false,
                timing_audit: UiTimingAudit::enabled(),
            },
            ViewerDisplayHandle {
                surface_writer: Some(surface_writer),
            },
        ))
    }

    fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> Result<()> {
        let response = self.egui_winit.on_window_event(window, event);
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            window.request_redraw();
        }
        match event {
            WindowEvent::Resized(size) if size.width != 0 && size.height != 0 => {
                self.presenter.resize(*size)?;
            }
            WindowEvent::RedrawRequested => self.render(window)?,
            _ => {}
        }
        Ok(())
    }

    fn render(&mut self, window: &Window) -> Result<()> {
        let started = Instant::now();
        let input = self.egui_winit.take_egui_input(window);
        let mut resize = None;
        let output = self.egui_context.run_ui(input, |ui| {
            resize = borderless_resize(ui, window, None);
            egui::Panel::top("connection-window-chrome")
                .frame(title_bar_frame())
                .show(ui, |ui| {
                    self.close_requested |= connection_title_bar(ui, window, &self.progress.alias);
                });
            self.progress.draw(ui);
        });
        let (renderer_output, platform_output, viewports) = egui_directx11::split_output(output);
        let immediate = viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport.repaint_delay.is_zero());
        self.close_requested |= viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| {
                viewport
                    .commands
                    .iter()
                    .any(|cmd| matches!(cmd, egui::ViewportCommand::Close))
            });
        self.egui_winit
            .handle_platform_output(window, platform_output);
        let layout_elapsed = started.elapsed();
        let presented = self
            .presenter
            .render(&self.egui_context, renderer_output, false)?;
        if let Some(audit) = &mut self.timing_audit {
            audit.record(
                started,
                layout_elapsed,
                started.elapsed().saturating_sub(layout_elapsed),
                immediate,
                presented,
            );
        }
        if let Some(direction) = resize {
            let _ = window.drag_resize_window(direction);
        }
        Ok(())
    }
}

fn title_bar_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(12, 16, 22))
        .inner_margin(egui::Margin::symmetric(10, 3))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(28, 35, 46)))
}

fn connection_title_bar(ui: &mut egui::Ui, window: &Window, alias: &str) -> bool {
    ui.set_min_height(36.0);
    let mut close = false;
    ui.horizontal_centered(|ui| {
        let available = (ui.available_width() - 112.0).max(180.0);
        let drag = ui
            .allocate_ui_with_layout(
                egui::vec2(available, 34.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(
                        egui::RichText::new("UU")
                            .size(13.0)
                            .strong()
                            .color(egui::Color32::from_rgb(96, 156, 255)),
                    );
                    ui.label(
                        egui::RichText::new(format!("正在连接  {alias}"))
                            .size(12.0)
                            .strong()
                            .color(egui::Color32::from_rgb(221, 228, 239)),
                    );
                },
            )
            .response
            .interact(egui::Sense::click_and_drag());
        if handle_title_drag(window, &drag) {
            let _ = window.drag_window();
        }
        close = window_buttons(ui, window);
    });
    close
}

struct PlayerTitleBar<'a> {
    screens: &'a mut ScreenTabBar,
    window: &'a Window,
    title: &'a str,
    performance: &'a PerformanceMonitor,
    performance_mode: &'a mut PerformancePanelMode,
    stream_control_ui: &'a mut StreamControlUi,
    aspect_locked: bool,
    move_state: Option<&'a mut WindowMoveState>,
}

fn player_title_bar(ui: &mut egui::Ui, mut bar: PlayerTitleBar<'_>) -> PlayerChromeAction {
    ui.set_min_height(36.0);
    let mut action = PlayerChromeAction::default();
    let stats = bar.performance.snapshot();
    let rect = egui::Rect::from_min_size(
        ui.available_rect_before_wrap().min,
        egui::vec2(ui.available_width(), 36.0),
    );
    const WINDOW_CONTROLS_WIDTH: f32 = 106.0;
    const VIEW_ACTIONS_WIDTH: f32 = 140.0;
    let identity_width = (rect.width() * 0.22).clamp(170.0, 210.0);
    let controls_rect = egui::Rect::from_min_max(
        egui::pos2(rect.max.x - WINDOW_CONTROLS_WIDTH, rect.min.y),
        rect.max,
    );
    let actions_rect = egui::Rect::from_min_max(
        egui::pos2(controls_rect.min.x - VIEW_ACTIONS_WIDTH, rect.min.y),
        egui::pos2(controls_rect.min.x, rect.max.y),
    );
    let identity_rect = egui::Rect::from_min_size(
        rect.min,
        egui::vec2(
            identity_width.min(actions_rect.min.x - rect.min.x),
            rect.height(),
        ),
    );
    let content_rect = egui::Rect::from_min_max(
        egui::pos2(identity_rect.max.x, rect.min.y),
        egui::pos2(actions_rect.min.x, rect.max.y),
    );
    let metrics_rect = egui::Rect::from_min_max(
        egui::pos2(
            (content_rect.max.x - 184.0).max(content_rect.min.x),
            rect.min.y,
        ),
        content_rect.max,
    );
    let tabs_rect =
        egui::Rect::from_min_max(content_rect.min, egui::pos2(metrics_rect.min.x, rect.max.y));

    // Register the caption background before the tabs. Actual tab buttons
    // take priority for detach gestures; unused tab space and stream metrics
    // remain part of the draggable caption. Keep action buttons outside it.
    let drag = ui.interact(
        egui::Rect::from_min_max(rect.min, metrics_rect.max),
        ui.id().with("player-title-drag"),
        egui::Sense::click_and_drag(),
    );
    if let Some(state) = bar.move_state.as_deref_mut() {
        update_nonmodal_window_move(ui.ctx(), bar.window, &drag, state);
    } else {
        action.drag_window = handle_title_drag(bar.window, &drag);
    }

    let mut identity = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(identity_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    identity.set_clip_rect(identity_rect);
    paint_device_mark(&mut identity);
    identity.add(
        egui::Label::new(
            egui::RichText::new(bar.title.trim_start_matches(crate::VIEWER_TITLE_PREFIX))
                .size(13.5)
                .strong()
                .color(egui::Color32::from_rgb(235, 240, 248)),
        )
        .truncate(),
    );

    if !bar.screens.tabs.is_empty() {
        let mut tabs = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(tabs_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        tabs.set_clip_rect(tabs_rect);
        bar.screens.draw(&mut tabs, bar.window);
    }
    let codec_info = if stats.video_format.contains("264") || stats.video_format.contains("265") {
        &stats.video_format
    } else {
        &stats.video_codec
    };
    let codec = if codec_info.contains("265") || codec_info.contains("HEVC") {
        "H.265"
    } else if codec_info.contains("264") || codec_info.contains("AVC") {
        "H.264"
    } else {
        "—"
    };
    let parameters = stats.decoded_resolution.map_or_else(
        || "等待画面".to_owned(),
        |(width, height)| {
            format!(
                "{width}×{height} · {:.0} FPS · {codec}",
                stats.actual_fps.max(1.0)
            )
        },
    );
    let mut metrics = ui.new_child(egui::UiBuilder::new().max_rect(metrics_rect).layout(
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
    ));
    metrics.set_clip_rect(metrics_rect);
    metrics
        .add(
            egui::Label::new(
                egui::RichText::new(parameters)
                    .size(10.5)
                    .color(egui::Color32::from_rgb(135, 150, 172)),
            )
            .truncate(),
        )
        .on_hover_text(&stats.video_format);

    let mut actions = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(actions_rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    actions.spacing_mut().item_spacing.x = 4.0;
    if title_icon_button(
        &mut actions,
        TitleIcon::Performance,
        !matches!(bar.performance_mode, PerformancePanelMode::Hidden),
        "性能面板（F3）",
    )
    .clicked()
    {
        *bar.performance_mode = bar.performance_mode.next();
    }
    let quality_button = title_icon_button(
        &mut actions,
        TitleIcon::Quality,
        bar.stream_control_ui.open,
        bar.stream_control_ui
            .budget_notice
            .as_deref()
            .unwrap_or("画质与串流设置"),
    );
    if bar.stream_control_ui.budget_notice.is_some() {
        actions.painter().circle_filled(
            quality_button.rect.right_top() + egui::vec2(-4.0, 4.0),
            2.5,
            super::warning_color(),
        );
    }
    if quality_button.clicked() {
        bar.stream_control_ui.open = !bar.stream_control_ui.open;
    }
    if title_icon_button(
        &mut actions,
        TitleIcon::OneToOne,
        false,
        "按实际像素调整窗口",
    )
    .clicked()
    {
        action.one_to_one = true;
    }
    if title_icon_button(
        &mut actions,
        TitleIcon::Aspect,
        bar.aspect_locked,
        "锁定画面比例",
    )
    .clicked()
    {
        action.toggle_aspect = true;
    }

    let mut controls = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(controls_rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    controls.spacing_mut().item_spacing.x = 4.0;
    action.close = window_buttons(&mut controls, bar.window);
    ui.painter().line_segment(
        [
            egui::pos2(controls_rect.min.x, rect.min.y + 8.0),
            egui::pos2(controls_rect.min.x, rect.max.y - 8.0),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 50, 64)),
    );
    ui.advance_cursor_after_rect(rect);
    action
}

#[derive(Clone, Copy, Debug, Default)]
struct PlayerChromeAction {
    close: bool,
    one_to_one: bool,
    toggle_aspect: bool,
    drag_window: bool,
}

#[derive(Clone, Copy)]
enum TitleIcon {
    Aspect,
    OneToOne,
    Quality,
    Performance,
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn title_icon_button(
    ui: &mut egui::Ui,
    icon: TitleIcon,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::click());
    let fill = if matches!(icon, TitleIcon::Close) && response.hovered() {
        egui::Color32::from_rgb(196, 43, 52)
    } else if selected {
        egui::Color32::from_rgba_premultiplied(62, 126, 236, 72)
    } else if response.hovered() {
        egui::Color32::from_white_alpha(18)
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    let color = if selected {
        egui::Color32::from_rgb(106, 162, 255)
    } else if response.hovered() {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(178, 189, 205)
    };
    paint_title_icon(ui.painter(), rect, icon, color);
    response.on_hover_text(tooltip)
}

fn paint_title_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: TitleIcon,
    color: egui::Color32,
) {
    let center = rect.center();
    let stroke = egui::Stroke::new(1.35, color);
    match icon {
        TitleIcon::Aspect => {
            let frame = egui::Rect::from_center_size(center, egui::vec2(15.0, 10.0));
            painter.rect_stroke(frame, 1.5, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    frame.left_top() + egui::vec2(2.0, 4.0),
                    frame.left_top() + egui::vec2(2.0, 2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    frame.left_top() + egui::vec2(2.0, 2.0),
                    frame.left_top() + egui::vec2(4.0, 2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    frame.right_bottom() - egui::vec2(2.0, 4.0),
                    frame.right_bottom() - egui::vec2(2.0, 2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    frame.right_bottom() - egui::vec2(2.0, 2.0),
                    frame.right_bottom() - egui::vec2(4.0, 2.0),
                ],
                stroke,
            );
        }
        TitleIcon::OneToOne => {
            let left = center.x - 7.0;
            let right = center.x + 7.0;
            let top = center.y - 6.0;
            let bottom = center.y + 6.0;
            for points in [
                [
                    egui::pos2(left, top + 4.0),
                    egui::pos2(left, top),
                    egui::pos2(left + 4.0, top),
                ],
                [
                    egui::pos2(right - 4.0, top),
                    egui::pos2(right, top),
                    egui::pos2(right, top + 4.0),
                ],
                [
                    egui::pos2(left, bottom - 4.0),
                    egui::pos2(left, bottom),
                    egui::pos2(left + 4.0, bottom),
                ],
                [
                    egui::pos2(right - 4.0, bottom),
                    egui::pos2(right, bottom),
                    egui::pos2(right, bottom - 4.0),
                ],
            ] {
                painter.line(points.to_vec(), stroke);
            }
            painter.circle_filled(center, 1.3, color);
        }
        TitleIcon::Quality => {
            for (offset, knob) in [(-5.0, -3.0), (0.0, 4.0), (5.0, -1.0)] {
                painter.line_segment(
                    [
                        egui::pos2(center.x - 7.0, center.y + offset),
                        egui::pos2(center.x + 7.0, center.y + offset),
                    ],
                    stroke,
                );
                painter.circle_filled(egui::pos2(center.x + knob, center.y + offset), 2.0, color);
            }
        }
        TitleIcon::Performance => {
            let bottom = center.y + 6.0;
            for (x, height) in [(-6.0, 5.0), (-1.0, 9.0), (4.0, 13.0)] {
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(center.x + x, bottom - height),
                        egui::pos2(center.x + x + 3.0, bottom),
                    ),
                    1.0,
                    color,
                );
            }
        }
        TitleIcon::Minimize => {
            painter.line_segment(
                [
                    egui::pos2(center.x - 6.0, center.y + 4.0),
                    egui::pos2(center.x + 6.0, center.y + 4.0),
                ],
                stroke,
            );
        }
        TitleIcon::Maximize => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(11.0, 9.0)),
                0.5,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        TitleIcon::Restore => {
            let back = egui::Rect::from_min_size(
                egui::pos2(center.x - 4.0, center.y - 6.0),
                egui::vec2(9.0, 8.0),
            );
            let front = back.translate(egui::vec2(-2.5, 2.5));
            painter.rect_stroke(back, 0.5, stroke, egui::StrokeKind::Inside);
            painter.rect_filled(front, 0.0, egui::Color32::from_rgb(12, 16, 22));
            painter.rect_stroke(front, 0.5, stroke, egui::StrokeKind::Inside);
        }
        TitleIcon::Close => {
            painter.line_segment(
                [center - egui::vec2(5.0, 5.0), center + egui::vec2(5.0, 5.0)],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-5.0, 5.0),
                    center + egui::vec2(5.0, -5.0),
                ],
                stroke,
            );
        }
    }
}

fn paint_device_mark(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 20.0), egui::Sense::hover());
    let center = rect.center();
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(center.x, center.y - 4.5),
            egui::pos2(center.x + 4.5, center.y),
            egui::pos2(center.x, center.y + 4.5),
            egui::pos2(center.x - 4.5, center.y),
        ],
        egui::Color32::from_rgb(76, 143, 255),
        egui::Stroke::NONE,
    ));
}

fn handle_title_drag(window: &Window, response: &egui::Response) -> bool {
    if response.double_clicked() {
        window.set_maximized(!window.is_maximized());
        false
    } else {
        response.drag_started()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct WindowMoveState {
    start: Option<(POINT, PhysicalPosition<i32>)>,
}

fn update_nonmodal_window_move(
    ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    state: &mut WindowMoveState,
) {
    if response.double_clicked() {
        state.start = None;
        let _ = unsafe { ReleaseCapture() };
        window.set_maximized(!window.is_maximized());
        return;
    }
    if response.drag_started() {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok()
            && let Ok(origin) = window.outer_position()
        {
            if window.is_maximized() {
                window.set_maximized(false);
            }
            state.start = Some((cursor, origin));
            tracing::debug!(x = origin.x, y = origin.y, "local title drag started");
            if let Ok(hwnd) = window_hwnd(window) {
                unsafe { SetCapture(hwnd) };
            }
        }
    }
    let primary_down = ctx.input(|input| input.pointer.primary_down());
    if primary_down && let Some((start_cursor, origin)) = state.start {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok() {
            let started = Instant::now();
            window.set_outer_position(PhysicalPosition::new(
                origin.x.saturating_add(cursor.x - start_cursor.x),
                origin.y.saturating_add(cursor.y - start_cursor.y),
            ));
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(50) {
                tracing::warn!(
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    "moving local viewer HWND blocked"
                );
            }
        }
    }
    if response.drag_stopped() || !primary_down {
        if state.start.is_some() {
            tracing::debug!("local title drag ended");
        }
        state.start = None;
        let _ = unsafe { ReleaseCapture() };
    }
}

fn window_buttons(ui: &mut egui::Ui, window: &Window) -> bool {
    let minimize = title_icon_button(ui, TitleIcon::Minimize, false, "最小化");
    if minimize.clicked() {
        window.set_minimized(true);
    }
    let maximize = title_icon_button(
        ui,
        if window.is_maximized() {
            TitleIcon::Restore
        } else {
            TitleIcon::Maximize
        },
        false,
        if window.is_maximized() {
            "还原"
        } else {
            "最大化"
        },
    );
    if maximize.clicked() {
        window.set_maximized(!window.is_maximized());
    }
    title_icon_button(ui, TitleIcon::Close, false, "关闭").clicked()
}

pub(super) fn title_bar_height_pixels(window: &Window) -> u32 {
    (42.0 * window.scale_factor()).round().max(1.0) as u32
}

fn configure_dwm_window(window: &Window) {
    let Ok(hwnd) = window_hwnd(window) else {
        return;
    };
    let dark_mode = 1_i32;
    let border_color = 0x008B654E_u32;
    let corner = DWMWCP_ROUNDSMALL;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark_mode).cast(),
            std::mem::size_of_val(&dark_mode) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&raw const border_color).cast(),
            std::mem::size_of_val(&border_color) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            std::mem::size_of_val(&corner) as u32,
        );
    }
}

fn monitor_work_area(window: &Window) -> Option<RECT> {
    let hwnd = window_hwnd(window).ok()?;
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }
        .as_bool()
        .then_some(info.rcWork)
}

fn constrain_window_aspect(
    window: &Window,
    size: PhysicalSize<u32>,
    video_size: Option<(u32, u32)>,
    aspect_locked: bool,
    last_window_size: &mut PhysicalSize<u32>,
    pending_aspect_size: &mut Option<PhysicalSize<u32>>,
) {
    if size.width == 0 || size.height == 0 || !aspect_locked || window.is_maximized() {
        *last_window_size = size;
        return;
    }
    if *pending_aspect_size == Some(size) {
        *pending_aspect_size = None;
        *last_window_size = size;
        return;
    }
    let Some((video_width, video_height)) = video_size else {
        *last_window_size = size;
        return;
    };
    let title_height = title_bar_height_pixels(window);
    let width_changed = size.width.abs_diff(last_window_size.width);
    let height_changed = size.height.abs_diff(last_window_size.height);
    let desired = if width_changed >= height_changed {
        PhysicalSize::new(
            size.width,
            ((u64::from(size.width) * u64::from(video_height) + u64::from(video_width) / 2)
                / u64::from(video_width)) as u32
                + title_height,
        )
    } else {
        let content_height = size.height.saturating_sub(title_height).max(1);
        PhysicalSize::new(
            ((u64::from(content_height) * u64::from(video_width) + u64::from(video_height) / 2)
                / u64::from(video_height)) as u32,
            size.height,
        )
    };
    *last_window_size = size;
    if desired.width.abs_diff(size.width) > 1 || desired.height.abs_diff(size.height) > 1 {
        *pending_aspect_size = Some(desired);
        let _ = window.request_inner_size(desired);
    }
}

fn fit_window_to_aspect(
    window: &Window,
    (video_width, video_height): (u32, u32),
    pending_aspect_size: &mut Option<PhysicalSize<u32>>,
) {
    let current = window.inner_size();
    let title_height = title_bar_height_pixels(window);
    let mut desired = PhysicalSize::new(
        current.width,
        ((u64::from(current.width) * u64::from(video_height) + u64::from(video_width) / 2)
            / u64::from(video_width)) as u32
            + title_height,
    );
    if let Some(work) = monitor_work_area(window) {
        let max_width = (work.right - work.left).max(1) as u32;
        let max_height = (work.bottom - work.top).max(1) as u32;
        if desired.width > max_width || desired.height > max_height {
            let content_height = max_height.saturating_sub(title_height).max(1);
            desired = PhysicalSize::new(
                ((u64::from(content_height) * u64::from(video_width) + u64::from(video_height) / 2)
                    / u64::from(video_height)) as u32,
                max_height,
            );
        }
    }
    *pending_aspect_size = Some(desired);
    let _ = window.request_inner_size(desired);
}

fn resize_window_one_to_one(
    window: &Window,
    (video_width, video_height): (u32, u32),
    pending_aspect_size: &mut Option<PhysicalSize<u32>>,
) {
    let title_height = title_bar_height_pixels(window);
    let work = monitor_work_area(window).unwrap_or(RECT {
        left: 0,
        top: 0,
        right: i32::try_from(window.inner_size().width).unwrap_or(i32::MAX),
        bottom: i32::try_from(window.inner_size().height).unwrap_or(i32::MAX),
    });
    let max_width = (work.right - work.left).max(1) as u32;
    let max_height = (work.bottom - work.top).max(1) as u32;
    let max_content_height = max_height.saturating_sub(title_height).max(1);
    let scale = 1.0_f64
        .min(max_width as f64 / video_width.max(1) as f64)
        .min(max_content_height as f64 / video_height.max(1) as f64);
    let target = PhysicalSize::new(
        (video_width as f64 * scale).round().max(1.0) as u32,
        (video_height as f64 * scale).round().max(1.0) as u32 + title_height,
    );
    *pending_aspect_size = Some(target);
    let _ = window.request_inner_size(target);
    let left = work.left + ((work.right - work.left - target.width as i32).max(0) / 2);
    let top = work.top + ((work.bottom - work.top - target.height as i32).max(0) / 2);
    window.set_outer_position(PhysicalPosition::new(left, top));
}

#[derive(Clone, Copy, Debug, Default)]
struct WindowResizeState {
    start: Option<WindowResizeStart>,
    requested_render_size: Option<PhysicalSize<u32>>,
}

#[derive(Clone, Copy, Debug)]
struct WindowResizeStart {
    cursor: POINT,
    origin: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    direction: ResizeDirection,
}

fn update_nonmodal_window_resize(
    ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    direction: ResizeDirection,
    state: &mut WindowResizeState,
    aspect: Option<(u32, u32)>,
) {
    if response.drag_started() {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok()
            && let Ok(origin) = window.outer_position()
        {
            state.start = Some(WindowResizeStart {
                cursor,
                origin,
                size: window.inner_size(),
                direction,
            });
            if let Ok(hwnd) = window_hwnd(window) {
                unsafe { SetCapture(hwnd) };
            }
        }
    }
    let primary_down = ctx.input(|input| input.pointer.primary_down());
    if primary_down && let Some(start) = state.start {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok() {
            state.requested_render_size = Some(apply_absolute_resize(
                window,
                start,
                cursor.x - start.cursor.x,
                cursor.y - start.cursor.y,
                aspect,
            ));
        }
    }
    if response.drag_stopped() || !primary_down {
        state.start = None;
        let _ = unsafe { ReleaseCapture() };
    }
}

fn apply_absolute_resize(
    window: &Window,
    start: WindowResizeStart,
    dx: i32,
    dy: i32,
    aspect: Option<(u32, u32)>,
) -> PhysicalSize<u32> {
    let west = matches!(
        start.direction,
        ResizeDirection::West | ResizeDirection::NorthWest | ResizeDirection::SouthWest
    );
    let east = matches!(
        start.direction,
        ResizeDirection::East | ResizeDirection::NorthEast | ResizeDirection::SouthEast
    );
    let north = matches!(
        start.direction,
        ResizeDirection::North | ResizeDirection::NorthEast | ResizeDirection::NorthWest
    );
    let south = matches!(
        start.direction,
        ResizeDirection::South | ResizeDirection::SouthEast | ResizeDirection::SouthWest
    );
    let mut width = start.size.width as i32
        + if east {
            dx
        } else if west {
            -dx
        } else {
            0
        };
    let mut height = start.size.height as i32
        + if south {
            dy
        } else if north {
            -dy
        } else {
            0
        };
    const MIN_WIDTH: i32 = 640;
    const MIN_HEIGHT: i32 = 400;
    width = width.max(MIN_WIDTH);
    height = height.max(MIN_HEIGHT);

    if let Some((video_width, video_height)) = aspect {
        let title_height = title_bar_height_pixels(window) as i32;
        let horizontal_only = (west || east) && !north && !south;
        let vertical_only = (north || south) && !west && !east;
        let width_drives = horizontal_only
            || (!vertical_only
                && i64::from(dx.abs()) * i64::from(video_height)
                    >= i64::from(dy.abs()) * i64::from(video_width));
        if width_drives {
            height = ((i64::from(width) * i64::from(video_height) + i64::from(video_width) / 2)
                / i64::from(video_width)) as i32
                + title_height;
            height = height.max(MIN_HEIGHT);
        } else {
            let content_height = (height - title_height).max(1);
            width = ((i64::from(content_height) * i64::from(video_width)
                + i64::from(video_height) / 2)
                / i64::from(video_height)) as i32;
            width = width.max(MIN_WIDTH);
        }
    }

    let left = if west {
        start.origin.x + start.size.width as i32 - width
    } else {
        start.origin.x
    };
    let top = if north {
        start.origin.y + start.size.height as i32 - height
    } else {
        start.origin.y
    };
    window.set_outer_position(PhysicalPosition::new(left, top));
    let target = PhysicalSize::new(width as u32, height as u32);
    let _ = window.request_inner_size(target);
    target
}

fn borderless_resize(
    ui: &mut egui::Ui,
    window: &Window,
    mut manual: Option<(&mut WindowResizeState, Option<(u32, u32)>)>,
) -> Option<ResizeDirection> {
    if window.is_maximized() {
        return None;
    }
    let rect = ui.max_rect();
    let edge = 6.0;
    let corner = 12.0;
    let regions = [
        (
            egui::Rect::from_min_max(
                rect.min,
                egui::pos2(rect.min.x + corner, rect.min.y + corner),
            ),
            ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - corner, rect.min.y),
                egui::pos2(rect.max.x, rect.min.y + corner),
            ),
            ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.max.y - corner),
                egui::pos2(rect.min.x + corner, rect.max.y),
            ),
            ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - corner, rect.max.y - corner),
                rect.max,
            ),
            ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x + corner, rect.min.y),
                egui::pos2(rect.max.x - corner, rect.min.y + edge),
            ),
            ResizeDirection::North,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x + corner, rect.max.y - edge),
                egui::pos2(rect.max.x - corner, rect.max.y),
            ),
            ResizeDirection::South,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.min.y + corner),
                egui::pos2(rect.min.x + edge, rect.max.y - corner),
            ),
            ResizeDirection::West,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - edge, rect.min.y + corner),
                egui::pos2(rect.max.x, rect.max.y - corner),
            ),
            ResizeDirection::East,
            egui::CursorIcon::ResizeHorizontal,
        ),
    ];
    let mut requested = None;
    for (index, (region, direction, cursor)) in regions.into_iter().enumerate() {
        let response = ui
            .interact(region, ui.id().with(("resize", index)), egui::Sense::drag())
            .on_hover_cursor(cursor);
        if let Some((state, aspect)) = manual.as_mut() {
            update_nonmodal_window_resize(ui.ctx(), window, &response, direction, state, *aspect);
        } else if response.drag_started() {
            requested = Some(direction);
        }
    }
    requested
}

enum RenderCommand {
    Resize(PhysicalSize<u32>),
    Occluded(bool),
    Stop,
}

struct RenderWorker {
    frame_wake: super::FrameWake,
    commands: std_mpsc::Sender<RenderCommand>,
    wake: std::thread::Thread,
    thread: Option<std::thread::JoinHandle<()>>,
    current_video_size: Arc<Mutex<Option<(u32, u32, u16)>>>,
}

impl RenderWorker {
    fn spawn(
        hwnd: isize,
        mut size: PhysicalSize<u32>,
        session: &NativeViewerSession,
    ) -> Result<Self> {
        let frame_queue = Arc::clone(&session.frame_queue);
        let performance = session.performance.clone();
        let worker_shutdown = Arc::clone(&session.shutdown);
        let (commands, command_receiver) = std_mpsc::channel();
        let current_video_size = Arc::new(Mutex::new(None));
        let worker_video_size = Arc::clone(&current_video_size);
        let thread = std::thread::Builder::new()
            .name("Video Render".to_owned())
            .spawn(move || {
                if let Err(error) =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }
                {
                    tracing::warn!(%error, "set video render thread priority");
                }
                let mut presenter = None::<D3D11Presenter>;
                let mut current_frame = None::<DecodedVideoFrame>;
                let mut redraw = false;
                let mut occluded = false;
                'render: loop {
                    while let Ok(command) = command_receiver.try_recv() {
                        match command {
                            RenderCommand::Resize(new_size) => {
                                size = new_size;
                                redraw = true;
                            }
                            RenderCommand::Occluded(value) => {
                                occluded = value;
                                redraw = !value;
                                performance.pause_presentation();
                            }
                            RenderCommand::Stop => break 'render,
                        }
                    }
                    if worker_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if occluded || size.width == 0 || size.height == 0 {
                        current_frame = None;
                        let dropped = {
                            let mut queue = mutex_lock(&frame_queue);
                            let count = queue.len();
                            queue.clear();
                            performance.set_presentation_queue_frames(0);
                            count
                        };
                        for _ in 0..dropped {
                            performance.record_dropped_present_frame();
                        }
                        // Continue draining on decoded-frame notifications while minimized.
                        std::thread::park();
                        continue;
                    }
                    let replacement = take_next_frame(&mut mutex_lock(&frame_queue), &performance);
                    let is_new_submission = replacement.is_some();
                    if (redraw || is_new_submission)
                        && let Some(frame) = replacement.as_ref().or(current_frame.as_ref())
                    {
                        // SelectRendererAllocator adopts non-shared decoder resources.
                        // Release the old swap chain before constructing its replacement.
                        let changed = presenter.as_ref().is_some_and(|p| {
                            matches!(&frame.surface, RenderSurface::D3D11(surface)
                            if surface.shared_handle().is_none() && !p.accepts_surface(surface))
                        });
                        if changed {
                            presenter.take();
                        }
                        let result = (|| {
                            if presenter.is_none() {
                                presenter = Some(
                                    D3D11Presenter::for_frame(hwnd, size, frame)
                                        .map_err(VideoRenderError::prepare)?,
                                );
                                tracing::info!("initialized native video presentation pipeline");
                            }
                            let p = presenter.as_mut().expect("presenter initialized");
                            p.resize(size).map_err(VideoRenderError::prepare)?;
                            render_thread_frame(p, frame, &performance, is_new_submission)
                        })();
                        match result {
                            Ok(()) => {
                                if let Some(frame) = replacement {
                                    *mutex_lock(&worker_video_size) =
                                        Some((frame.width, frame.height, frame.rotation));
                                    current_frame = Some(frame);
                                }
                            }
                            Err(VideoRenderError {
                                error,
                                release_resources,
                            }) => {
                                tracing::warn!(error = %format!("{error:#}"), release_resources,
                                "D3D11 video frame was not presented");
                                if is_new_submission {
                                    performance.record_dropped_present_frame();
                                }
                                if release_resources {
                                    // Official errors release the output/allocator, then return.
                                    // The next frame reinitializes; no same-frame retry or session abort.
                                    presenter.take();
                                    current_frame = None;
                                }
                            }
                        }
                    }
                    redraw = false;
                    if mutex_lock(&frame_queue).is_empty() {
                        std::thread::park();
                    }
                }
                // GPU resources and retained samples go away before the child HWND.
                drop(presenter);
                drop(current_frame);
                mutex_lock(&frame_queue).clear();
                performance.set_presentation_queue_frames(0);
            })
            .context("create Video Render thread")?;
        let wake = thread.thread().clone();
        session.frame_wake.install_render_thread(wake.clone());
        wake.unpark();
        Ok(Self {
            frame_wake: session.frame_wake.clone(),
            commands,
            wake,
            thread: Some(thread),
            current_video_size,
        })
    }

    fn send(&self, command: RenderCommand) {
        if self.commands.send(command).is_ok() {
            self.wake.unpark();
        }
    }
}

struct VideoRenderError {
    error: anyhow::Error,
    release_resources: bool,
}

impl VideoRenderError {
    fn prepare(error: anyhow::Error) -> Self {
        Self {
            error,
            release_resources: true,
        }
    }
}

impl RenderWorker {
    fn stop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        self.frame_wake.visible.store(false, Ordering::Release);
        *mutex_lock(&self.frame_wake.render_thread) = None;
        let _ = self.commands.send(RenderCommand::Stop);
        self.wake.unpark();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RenderWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn render_thread_frame(
    presenter: &mut D3D11Presenter,
    frame: &DecodedVideoFrame,
    performance: &PerformanceMonitor,
    is_new_submission: bool,
) -> std::result::Result<(), VideoRenderError> {
    let render_queue_delay = frame.decoded_at.elapsed();
    let present_wait_delay = presenter.begin_frame().map_err(VideoRenderError::prepare)?;
    let video_started = Instant::now();
    match &frame.surface {
        RenderSurface::D3D11(surface) => presenter.draw_video(
            surface,
            frame.width,
            frame.height,
            frame.rotation,
            frame.color,
        ),
        RenderSurface::CpuRgba8(pixels) => {
            presenter.draw_cpu_video(pixels, frame.width, frame.height, frame.rotation)
        }
    }
    .map_err(VideoRenderError::prepare)?;
    let surface_transfer_delay = video_started.elapsed();
    presenter.present().map_err(|error| {
        let release_resources = is_device_lost(&error);
        VideoRenderError {
            error,
            release_resources,
        }
    })?;
    if is_new_submission {
        performance.record_rendered_frame(RenderedFrameTiming {
            is_new_picture: frame.is_new_picture,
            width: frame.width,
            height: frame.height,
            decoded_at: frame.decoded_at,
            local: frame.received_at.elapsed(),
            assembly: frame.assembly_delay,
            input_queue: frame.input_queue_delay,
            decode_pipeline: frame.decode_pipeline_delay,
            surface_transfer: surface_transfer_delay,
            present_wait: present_wait_delay,
            render_queue: render_queue_delay,
            sender_capture_at: frame.sender_timing.capture_at,
            sender_capture: frame.sender_timing.capture_delay,
            sender_encode: frame.sender_timing.encode_delay,
            sender_pacer: frame.sender_timing.pacer_delay,
            sender_total: frame.sender_timing.sending_delay,
            transport: frame.sender_timing.transport_delay,
        });
    }
    Ok(())
}

struct ThreadedWindowsApp {
    screen_tabs: ScreenTabBar,
    close_requested: bool,
    title: String,
    performance: PerformanceMonitor,
    stream_control: StreamControlHandle,
    stream_control_ui: StreamControlUi,
    shutdown: Arc<AtomicBool>,
    fatal_error: Arc<Mutex<Option<String>>>,
    egui_context: egui::Context,
    egui_winit: egui_winit::State,
    performance_mode: PerformancePanelMode,
    aspect_locked: bool,
    last_window_size: PhysicalSize<u32>,
    pending_aspect_size: Option<PhysicalSize<u32>>,
    last_aspect_video_size: Option<(u32, u32)>,
    window_move: WindowMoveState,
    window_resize: WindowResizeState,
    timing_audit: Option<UiTimingAudit>,
    // Field order is the shutdown order: join render, close decoder, destroy child, release UI.
    renderer: RenderWorker,
    _session: Arc<NativeViewerSession>,
    video_window: VideoWindow,
    ui_presenter: UiPresenter,
}

impl ThreadedWindowsApp {
    fn replace_session(
        &mut self,
        window: &Window,
        session: Arc<NativeViewerSession>,
    ) -> Result<()> {
        // One composition target per HWND. Keep UI composition and the video
        // child window; retire the old swap chain before attaching a new track.
        self.renderer.stop();
        self.performance.pause_presentation();
        let size = self.video_window.resize(window, window.inner_size())?;
        self.renderer = RenderWorker::spawn(self.video_window.handle(), size, &session)?;
        self.title = session.title.clone();
        self.performance = session.performance.clone();
        self.stream_control = session.stream_control.clone();
        self.shutdown = Arc::clone(&session.shutdown);
        self.fatal_error = Arc::clone(&session.fatal_error);
        self._session = session;
        self.last_aspect_video_size = None;
        Ok(())
    }

    fn from_session(
        window: &Window,
        session: Arc<NativeViewerSession>,
        connecting: WindowsConnectionApp,
        preferences: ViewerPreferences,
    ) -> Result<Self> {
        let title = session.title.clone();
        let performance = session.performance.clone();
        let stream_control = session.stream_control.clone();
        let shutdown = Arc::clone(&session.shutdown);
        let fatal_error = Arc::clone(&session.fatal_error);
        let video_window = VideoWindow::new(window)?;
        let size = video_window.resize(window, window.inner_size())?;
        let renderer = RenderWorker::spawn(video_window.handle(), size, &session)?;
        Ok(Self {
            screen_tabs: ScreenTabBar::default(),
            close_requested: false,
            title,
            performance,
            stream_control,
            stream_control_ui: StreamControlUi::default(),
            shutdown,
            fatal_error,
            egui_context: connecting.egui_context,
            egui_winit: connecting.egui_winit,
            performance_mode: preferences.performance_mode,
            aspect_locked: preferences.aspect_locked,
            last_window_size: window.inner_size(),
            pending_aspect_size: None,
            last_aspect_video_size: None,
            window_move: WindowMoveState::default(),
            window_resize: WindowResizeState::default(),
            timing_audit: connecting.timing_audit,
            renderer,
            _session: session,
            video_window,
            ui_presenter: connecting.presenter,
        })
    }

    fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> Result<()> {
        let response = self.egui_winit.on_window_event(window, event);
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            window.request_redraw();
        }
        match event {
            WindowEvent::Resized(size) => {
                self.enforce_aspect_after_resize(window, *size);
                self.resize_targets(window, *size)?;
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw_ui(window)?,
            WindowEvent::Occluded(occluded) => {
                self.renderer.send(RenderCommand::Occluded(*occluded));
            }
            _ => {}
        }
        Ok(())
    }

    fn draw_ui(&mut self, window: &Window) -> Result<()> {
        let started = Instant::now();
        let input = self.egui_winit.take_egui_input(window);
        let mut performance_mode = self.performance_mode;
        let mut chrome_action = PlayerChromeAction::default();
        let mut resize = None;
        let resize_aspect = self
            .aspect_locked
            .then(|| self.current_video_size())
            .flatten();
        let output = self.egui_context.run_ui(input, |ui| {
            if ui.ctx().input(|input| input.key_pressed(egui::Key::F3)) {
                performance_mode = performance_mode.next();
            }
            let ctx = ui.ctx().clone();
            resize = borderless_resize(ui, window, Some((&mut self.window_resize, resize_aspect)));
            egui::Panel::top("viewer-toolbar")
                .frame(title_bar_frame())
                .show(ui, |ui| {
                    chrome_action = player_title_bar(
                        ui,
                        PlayerTitleBar {
                            screens: &mut self.screen_tabs,
                            window,
                            title: &self.title,
                            performance: &self.performance,
                            performance_mode: &mut performance_mode,
                            stream_control_ui: &mut self.stream_control_ui,
                            aspect_locked: self.aspect_locked,
                            move_state: Some(&mut self.window_move),
                        },
                    );
                });
            show_stream_control_window(&ctx, &self.stream_control, &mut self.stream_control_ui);
            super::show_performance_overlay(
                &ctx,
                &self.performance,
                &self.stream_control.audio(),
                performance_mode,
                "performance-grid-d3d11",
            );
        });
        self.performance_mode = performance_mode;
        let (renderer_output, platform_output, viewports) = egui_directx11::split_output(output);
        let immediate = viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport.repaint_delay.is_zero());
        chrome_action.close |= viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| {
                viewport
                    .commands
                    .iter()
                    .any(|cmd| matches!(cmd, egui::ViewportCommand::Close))
            });
        self.egui_winit
            .handle_platform_output(window, platform_output);
        let layout_elapsed = started.elapsed();
        let presented = self
            .ui_presenter
            .render(&self.egui_context, renderer_output, true)?;
        if let Some(audit) = &mut self.timing_audit {
            audit.record(
                started,
                layout_elapsed,
                started.elapsed().saturating_sub(layout_elapsed),
                immediate,
                presented,
            );
        }
        if let Some(size) = self.window_resize.requested_render_size.take() {
            self.resize_targets(window, size)?;
        }
        let current_video_size = self.current_video_size();
        if self.aspect_locked
            && current_video_size.is_some()
            && current_video_size != self.last_aspect_video_size
        {
            self.fit_window_to_current_aspect(window);
        }
        self.last_aspect_video_size = current_video_size;
        if chrome_action.close {
            self.close_requested = true;
        }
        if chrome_action.toggle_aspect {
            self.aspect_locked = !self.aspect_locked;
            if self.aspect_locked {
                self.fit_window_to_current_aspect(window);
            }
        }
        if chrome_action.one_to_one {
            self.resize_window_one_to_one(window);
        }
        if chrome_action.drag_window {
            let _ = window.drag_window();
        }
        if let Some(direction) = resize {
            let _ = window.drag_resize_window(direction);
        }
        if let Some(error) = mutex_lock(&self.fatal_error).clone() {
            bail!(error);
        }
        Ok(())
    }

    fn resize_targets(&mut self, window: &Window, size: PhysicalSize<u32>) -> Result<()> {
        self.ui_presenter.resize(size)?;
        let video_size = self.video_window.resize(window, size)?;
        self.renderer.send(RenderCommand::Resize(
            if size.width == 0 || size.height == 0 {
                PhysicalSize::new(0, 0)
            } else {
                video_size
            },
        ));
        Ok(())
    }

    fn current_video_size(&self) -> Option<(u32, u32)> {
        mutex_lock(&self.renderer.current_video_size).map(|(width, height, rotation)| {
            if rotation == 90 || rotation == 270 {
                (height, width)
            } else {
                (width, height)
            }
        })
    }

    fn enforce_aspect_after_resize(&mut self, window: &Window, size: PhysicalSize<u32>) {
        constrain_window_aspect(
            window,
            size,
            self.current_video_size(),
            self.aspect_locked,
            &mut self.last_window_size,
            &mut self.pending_aspect_size,
        );
    }

    fn fit_window_to_current_aspect(&mut self, window: &Window) {
        if let Some(video_size) = self.current_video_size() {
            fit_window_to_aspect(window, video_size, &mut self.pending_aspect_size);
        }
    }

    fn resize_window_one_to_one(&mut self, window: &Window) {
        if let Some(video_size) = self.current_video_size() {
            resize_window_one_to_one(window, video_size, &mut self.pending_aspect_size);
        }
    }
}

impl Drop for ThreadedWindowsApp {
    fn drop(&mut self) {
        self.renderer.stop();
        self.performance.pause_presentation();
    }
}

struct D3D11Presenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    frame_latency_waitable: isize,
    render_target: Option<ID3D11RenderTargetView>,
    backbuffer: Option<ID3D11Texture2D>,
    video_renderer: VideoShaderRenderer,
    cpu_upload: Option<CpuUploadTexture>,
    shared_textures: VecDeque<ImportedSharedTexture>,
    active_input_sync: Option<IDXGIKeyedMutex>,
    allow_tearing: bool,
    swap_chain_flags: DXGI_SWAP_CHAIN_FLAG,
    buffer_count: u32,
    size: PhysicalSize<u32>,
    content_top: u32,
    display_audit: Option<DisplayAudit>,
}

// Temporary, explicitly enabled diagnostic. Does not change presentation policy
// or write to disk on the render path. DXGI samples are not necessarily updated
// on every call (notably with hardware flip queues); preserve raw counters/errors.
struct DisplayAudit {
    path: std::path::PathBuf,
    origin: Instant,
    samples: Vec<(f64, u32, i32, DXGI_FRAME_STATISTICS, f64)>,
}

impl DisplayAudit {
    fn from_environment() -> Option<Self> {
        std::env::var_os("OPENUUYC_DISPLAY_AUDIT").map(|path| Self {
            path: path.into(),
            origin: Instant::now(),
            // Covers the full ~3-minute mixed 30/60/90/120 FPS matrix,
            // including its final high-bitrate 120 FPS segment.
            samples: Vec::with_capacity(32_768),
        })
    }

    fn sample(&mut self, swap_chain: &IDXGISwapChain1) {
        if self.samples.len() == self.samples.capacity() {
            return;
        }
        let started = Instant::now();
        let elapsed_ms = self.origin.elapsed().as_secs_f64() * 1_000.0;
        let submitted = unsafe { swap_chain.GetLastPresentCount() }.unwrap_or(0);
        let mut statistics = DXGI_FRAME_STATISTICS::default();
        let status = unsafe { swap_chain.GetFrameStatistics(&mut statistics) }
            .err()
            .map_or(0, |error| error.code().0);
        self.samples.push((
            elapsed_ms,
            submitted,
            status,
            statistics,
            started.elapsed().as_secs_f64() * 1_000_000.0,
        ));
    }

    fn save(&self) -> std::io::Result<()> {
        use std::io::Write;
        // Never overwrite an earlier observation, including after device recovery.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        let mut output = std::io::BufWriter::new(file);
        writeln!(
            output,
            "elapsed_ms,submitted,status,present_count,present_refresh,sync_refresh,sync_qpc,query_us"
        )?;
        for (elapsed, submitted, status, stats, query_us) in &self.samples {
            writeln!(
                output,
                "{elapsed:.6},{submitted},{status},{},{},{},{},{query_us:.3}",
                stats.PresentCount,
                stats.PresentRefreshCount,
                stats.SyncRefreshCount,
                stats.SyncQPCTime
            )?;
        }
        output.flush()
    }
}

struct CpuUploadTexture {
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
}

struct ImportedSharedTexture {
    handle: isize,
    texture: ID3D11Texture2D,
    sync: IDXGIKeyedMutex,
}

struct VideoShaderRenderer {
    input_layout: ID3D11InputLayout,
    vertex_shader: ID3D11VertexShader,
    yuv_pixel_shader: ID3D11PixelShader,
    yuv_array_pixel_shader: ID3D11PixelShader,
    rgba_pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    point_sampler: ID3D11SamplerState,
    rasterizer: ID3D11RasterizerState,
    color_buffer: Option<(RenderColor, u8, ID3D11Buffer)>,
    geometry: Option<VideoShaderGeometry>,
    views: VecDeque<CachedVideoShaderViews>,
}

struct VideoShaderGeometry {
    key: VideoGeometryKey,
    vertex_buffer: ID3D11Buffer,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct VideoGeometryKey {
    visible_x: u32,
    visible_y: u32,
    coded_width: u32,
    coded_height: u32,
    width: u32,
    height: u32,
    output_width: u32,
    output_height: u32,
    content_top: u32,
    rotation: u16,
}

struct CachedVideoShaderViews {
    texture_key: usize,
    array_slice: u32,
    format: DXGI_FORMAT,
    plane_0: ID3D11ShaderResourceView,
    plane_1: Option<ID3D11ShaderResourceView>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VideoVertex {
    position: [f32; 2],
    texcoord: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VideoColorTransform {
    rows: [[f32; 4]; 3],
}

const OFFICIAL_SHARED_TEXTURE_CACHE_SIZE: usize = 32;
const OFFICIAL_TEXTURE_SYNC_TIMEOUT_MS: u32 = 100;

struct VideoTextureView {
    color: RenderColor,
    array_slice: u32,
    input_format: DXGI_FORMAT,
    visible_x: u32,
    visible_y: u32,
    coded_width: u32,
    coded_height: u32,
    width: u32,
    height: u32,
    rotation: u16,
}

impl VideoShaderRenderer {
    const VERTEX_SHADER: &'static [u8] = include_bytes!("shaders/video_vs.cso");
    const YUV_PIXEL_SHADER: &'static [u8] = include_bytes!("shaders/video_yuv_ps.cso");
    const YUV_ARRAY_PIXEL_SHADER: &'static [u8] = include_bytes!("shaders/video_yuv_array_ps.cso");
    const RGBA_PIXEL_SHADER: &'static [u8] = include_bytes!("shaders/video_rgba_ps.cso");

    fn new(device: &ID3D11Device) -> Result<Self> {
        let input_elements = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("POSITION"),
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("TEXCOORD"),
                Format: DXGI_FORMAT_R32G32_FLOAT,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            },
        ];
        let mut input_layout = None;
        let mut vertex_shader = None;
        let mut yuv_pixel_shader = None;
        let mut yuv_array_pixel_shader = None;
        let mut rgba_pixel_shader = None;
        let mut sampler = None;
        let mut point_sampler = None;
        let mut rasterizer = None;
        unsafe {
            device.CreateInputLayout(
                &input_elements,
                Self::VERTEX_SHADER,
                Some(&mut input_layout),
            )?;
            device.CreateVertexShader(Self::VERTEX_SHADER, None, Some(&mut vertex_shader))?;
            device.CreatePixelShader(Self::YUV_PIXEL_SHADER, None, Some(&mut yuv_pixel_shader))?;
            device.CreatePixelShader(
                Self::YUV_ARRAY_PIXEL_SHADER,
                None,
                Some(&mut yuv_array_pixel_shader),
            )?;
            device.CreatePixelShader(
                Self::RGBA_PIXEL_SHADER,
                None,
                Some(&mut rgba_pixel_shader),
            )?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut point_sampler),
            )?;
            device.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                Some(&mut rasterizer),
            )?;
        }
        Ok(Self {
            input_layout: input_layout.context("D3D11 did not return the video input layout")?,
            vertex_shader: vertex_shader.context("D3D11 did not return the video vertex shader")?,
            yuv_pixel_shader: yuv_pixel_shader
                .context("D3D11 did not return the YUV pixel shader")?,
            yuv_array_pixel_shader: yuv_array_pixel_shader
                .context("D3D11 did not return the YUV array pixel shader")?,
            rgba_pixel_shader: rgba_pixel_shader
                .context("D3D11 did not return the RGBA pixel shader")?,
            sampler: sampler.context("D3D11 did not return the video sampler")?,
            point_sampler: point_sampler.context("D3D11 did not return the point sampler")?,
            rasterizer: rasterizer.context("D3D11 did not return the video rasterizer")?,
            color_buffer: None,
            geometry: None,
            views: VecDeque::with_capacity(OFFICIAL_SHARED_TEXTURE_CACHE_SIZE),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        render_target: &ID3D11RenderTargetView,
        texture: &ID3D11Texture2D,
        view: VideoTextureView,
        output_size: PhysicalSize<u32>,
        content_top: u32,
    ) -> Result<()> {
        let (plane_0, plane_1) = self.shader_views(device, texture, &view)?;
        let geometry_key = VideoGeometryKey {
            visible_x: view.visible_x,
            visible_y: view.visible_y,
            coded_width: view.coded_width,
            coded_height: view.coded_height,
            width: view.width,
            height: view.height,
            output_width: output_size.width,
            output_height: output_size.height,
            content_top,
            rotation: view.rotation,
        };
        if self
            .geometry
            .as_ref()
            .is_none_or(|geometry| geometry.key != geometry_key)
        {
            let vertices = video_vertices(geometry_key);
            self.geometry = Some(VideoShaderGeometry {
                key: geometry_key,
                vertex_buffer: create_video_vertex_buffer(device, &vertices)?,
            });
        }
        let geometry = self
            .geometry
            .as_ref()
            .context("video shader geometry was not created")?;
        let yuv = plane_1.is_some();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&raw mut desc) };
        let pixel_shader = if yuv && desc.ArraySize > 1 {
            &self.yuv_array_pixel_shader
        } else if yuv {
            &self.yuv_pixel_shader
        } else {
            &self.rgba_pixel_shader
        };
        let color_buffer = if yuv {
            let bit_depth = if view.input_format == DXGI_FORMAT_P010 {
                10
            } else {
                8
            };
            if self
                .color_buffer
                .as_ref()
                .is_none_or(|(color, depth, _)| *color != view.color || *depth != bit_depth)
            {
                let buffer = create_video_constant_buffer(
                    device,
                    VideoColorTransform {
                        rows: view.color.transform(bit_depth),
                    },
                )?;
                tracing::info!(color = ?view.color, bit_depth, "updated video YUV color transform");
                self.color_buffer = Some((view.color, bit_depth, buffer));
            }
            self.color_buffer
                .as_ref()
                .map(|(_, _, buffer)| buffer.clone())
        } else {
            None
        };
        let stride = std::mem::size_of::<VideoVertex>() as u32;
        let offset = 0_u32;
        let resources = [Some(plane_0), plane_1];
        // ResizeRendererViewport (0x180C9F340) selects point sampling when
        // rotated visible dimensions match the video HWND within one pixel.
        let (display_width, display_height) = if view.rotation == 90 || view.rotation == 270 {
            (view.height, view.width)
        } else {
            (view.width, view.height)
        };
        let sampler = if display_width.abs_diff(output_size.width) <= 1
            && display_height.abs_diff(output_size.height.saturating_sub(content_top)) <= 1
        {
            &self.point_sampler
        } else {
            &self.sampler
        };
        unsafe {
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.IASetInputLayout(&self.input_layout);
            context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(geometry.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(pixel_shader, None);
            context.PSSetShaderResources(0, Some(&resources));
            context.PSSetSamplers(0, Some(&[Some(sampler.clone())]));
            context.PSSetConstantBuffers(0, Some(&[color_buffer]));
            context.RSSetState(&self.rasterizer);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: output_size.width as f32,
                Height: output_size.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            context.OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            context.OMSetBlendState(None, Some(&[0.0; 4]), u32::MAX);
            context.Draw(4, 0);
            context.PSSetShaderResources(0, Some(&[None, None]));
        }
        Ok(())
    }

    fn shader_views(
        &mut self,
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        view: &VideoTextureView,
    ) -> Result<(ID3D11ShaderResourceView, Option<ID3D11ShaderResourceView>)> {
        let texture_key = Interface::as_raw(texture) as usize;
        if let Some(index) = self.views.iter().position(|cached| {
            cached.texture_key == texture_key
                && cached.array_slice == view.array_slice
                && cached.format == view.input_format
        }) {
            let cached = self
                .views
                .remove(index)
                .expect("cached video SRV index was checked");
            let result = (cached.plane_0.clone(), cached.plane_1.clone());
            self.views.push_back(cached);
            return Ok(result);
        }
        let (plane_0_format, plane_1_format) = match view.input_format {
            DXGI_FORMAT_NV12 => (DXGI_FORMAT_R8_UNORM, Some(DXGI_FORMAT_R8G8_UNORM)),
            DXGI_FORMAT_P010 => (DXGI_FORMAT_R16_UNORM, Some(DXGI_FORMAT_R16G16_UNORM)),
            DXGI_FORMAT_R8G8B8A8_UNORM => (DXGI_FORMAT_R8G8B8A8_UNORM, None),
            format => bail!("unsupported D3D11 shader input format {format:?}"),
        };
        let plane_0 =
            create_video_shader_resource_view(device, texture, plane_0_format, view.array_slice)?;
        let plane_1 = plane_1_format
            .map(|format| {
                create_video_shader_resource_view(device, texture, format, view.array_slice)
            })
            .transpose()?;
        self.views.push_back(CachedVideoShaderViews {
            texture_key,
            array_slice: view.array_slice,
            format: view.input_format,
            plane_0: plane_0.clone(),
            plane_1: plane_1.clone(),
        });
        while self.views.len() > OFFICIAL_SHARED_TEXTURE_CACHE_SIZE {
            self.views.pop_front();
        }
        Ok((plane_0, plane_1))
    }

    fn reset_geometry(&mut self) {
        self.geometry = None;
    }

    fn reset_input_cache(&mut self) {
        self.geometry = None;
        self.views.clear();
    }
}

fn create_video_vertex_buffer(
    device: &ID3D11Device,
    vertices: &[VideoVertex; 4],
) -> Result<ID3D11Buffer> {
    let mut buffer = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of_val(vertices) as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA {
                pSysMem: vertices.as_ptr().cast(),
                ..Default::default()
            }),
            Some(&mut buffer),
        )
    }
    .context("create official-style video quad vertex buffer")?;
    buffer.context("D3D11 did not return the video quad vertex buffer")
}

fn create_video_constant_buffer(
    device: &ID3D11Device,
    transform: VideoColorTransform,
) -> Result<ID3D11Buffer> {
    let mut buffer = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<VideoColorTransform>() as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA {
                pSysMem: (&raw const transform).cast(),
                ..Default::default()
            }),
            Some(&mut buffer),
        )
    }
    .context("create video color-transform constant buffer")?;
    buffer.context("D3D11 did not return the video color-transform buffer")
}

fn create_video_shader_resource_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    array_slice: u32,
) -> Result<ID3D11ShaderResourceView> {
    let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&raw mut texture_desc) };
    let (dimension, anonymous) = if texture_desc.ArraySize > 1 {
        (
            D3D_SRV_DIMENSION_TEXTURE2DARRAY,
            D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: array_slice,
                    ArraySize: 1,
                },
            },
        )
    } else {
        (
            D3D_SRV_DIMENSION_TEXTURE2D,
            D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        )
    };
    let mut resource_view = None;
    unsafe {
        device.CreateShaderResourceView(
            texture,
            Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: dimension,
                Anonymous: anonymous,
            }),
            Some(&mut resource_view),
        )
    }
    .with_context(|| format!("create D3D11 video plane SRV {format:?}"))?;
    resource_view.context("D3D11 did not return the video plane SRV")
}

fn video_vertices(key: VideoGeometryKey) -> [VideoVertex; 4] {
    let (display_width, display_height) = if key.rotation == 90 || key.rotation == 270 {
        (key.height, key.width)
    } else {
        (key.width, key.height)
    };
    let mut destination = fit_rect(
        display_width,
        display_height,
        key.output_width,
        key.output_height.saturating_sub(key.content_top).max(1),
    );
    destination.top += key.content_top as i32;
    destination.bottom += key.content_top as i32;
    let left = destination.left as f32 / key.output_width.max(1) as f32 * 2.0 - 1.0;
    let right = destination.right as f32 / key.output_width.max(1) as f32 * 2.0 - 1.0;
    let top = 1.0 - destination.top as f32 / key.output_height.max(1) as f32 * 2.0;
    let bottom = 1.0 - destination.bottom as f32 / key.output_height.max(1) as f32 * 2.0;
    let u0 = key.visible_x as f32 / key.coded_width.max(1) as f32;
    let v0 = key.visible_y as f32 / key.coded_height.max(1) as f32;
    let u1 = key.visible_x.saturating_add(key.width) as f32 / key.coded_width.max(1) as f32;
    let v1 = key.visible_y.saturating_add(key.height) as f32 / key.coded_height.max(1) as f32;
    let texcoords = match key.rotation {
        90 => [[u0, v1], [u0, v0], [u1, v1], [u1, v0]],
        180 => [[u1, v1], [u0, v1], [u1, v0], [u0, v0]],
        270 => [[u1, v0], [u1, v1], [u0, v0], [u0, v1]],
        _ => [[u0, v0], [u1, v0], [u0, v1], [u1, v1]],
    };
    [
        VideoVertex {
            position: [left, top],
            texcoord: texcoords[0],
        },
        VideoVertex {
            position: [right, top],
            texcoord: texcoords[1],
        },
        VideoVertex {
            position: [left, bottom],
            texcoord: texcoords[2],
        },
        VideoVertex {
            position: [right, bottom],
            texcoord: texcoords[3],
        },
    ]
}

impl D3D11Presenter {
    fn accepts_surface(&self, surface: &crate::decoder::windows_surface::D3D11Surface) -> bool {
        surface.belongs_to_device(&self.device)
    }

    fn for_frame(hwnd: isize, size: PhysicalSize<u32>, frame: &DecodedVideoFrame) -> Result<Self> {
        let (device, context) = match &frame.surface {
            RenderSurface::D3D11(surface) if surface.shared_handle().is_none() => {
                (surface.device().clone(), surface.context().clone())
            }
            RenderSurface::D3D11(surface) => surface.create_renderer_device()?,
            RenderSurface::CpuRgba8(_) => {
                crate::decoder::windows_surface::D3D11SurfaceWriter::new()?
                    .create_renderer_device()?
            }
        };
        Self::from_device_for_window(HWND(hwnd as *mut std::ffi::c_void), size, device, context)
    }

    fn from_device_for_window(
        hwnd: HWND,
        size: PhysicalSize<u32>,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    ) -> Result<Self> {
        let swap_chain_creation = create_swap_chain(&device, hwnd, size)?;
        let swap_chain = swap_chain_creation.swap_chain;
        let allow_tearing = swap_chain_creation.allow_tearing;
        let (backbuffer, render_target) = create_backbuffer(&device, &swap_chain)?;
        let frame_latency_waitable = if swap_chain_creation.waitable {
            let swap_chain2 = swap_chain
                .cast::<IDXGISwapChain2>()
                .context("query low-latency DXGI swap chain")?;
            if let Err(error) = unsafe { swap_chain2.SetMaximumFrameLatency(1) } {
                tracing::warn!(%error, "failed to set swap-chain maximum frame latency to 1");
            }
            let waitable = unsafe { swap_chain2.GetFrameLatencyWaitableObject() };
            if waitable.is_invalid() {
                tracing::warn!("DXGI did not return a frame-latency waitable object");
            }
            waitable
        } else {
            HANDLE::default()
        };
        let video_renderer = VideoShaderRenderer::new(&device)?;
        Ok(Self {
            device,
            context,
            swap_chain,
            frame_latency_waitable: frame_latency_waitable.0 as isize,
            render_target: Some(render_target),
            backbuffer: Some(backbuffer),
            video_renderer,
            cpu_upload: None,
            shared_textures: VecDeque::with_capacity(OFFICIAL_SHARED_TEXTURE_CACHE_SIZE),
            active_input_sync: None,
            allow_tearing,
            swap_chain_flags: swap_chain_creation.flags,
            buffer_count: swap_chain_creation.buffer_count,
            size,
            content_top: 0,
            display_audit: DisplayAudit::from_environment(),
        })
    }

    fn begin_frame(&self) -> Result<std::time::Duration> {
        let waiting_started = Instant::now();
        if self.frame_latency_waitable != 0 {
            let wait = unsafe {
                WaitForSingleObjectEx(
                    HANDLE(self.frame_latency_waitable as *mut std::ffi::c_void),
                    500,
                    true,
                )
            };
            if wait != WAIT_OBJECT_0 {
                tracing::warn!(
                    status = wait.0,
                    "DXGI frame-latency wait was not signaled within 500 ms"
                );
            }
        }
        let waited = waiting_started.elapsed();
        let target = self
            .render_target
            .as_ref()
            .context("D3D11 render target is unavailable")?;
        unsafe {
            self.context
                .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
        }
        Ok(waited)
    }

    fn reset_video_resources(&mut self) {
        self.release_active_input_sync();
        self.video_renderer.reset_input_cache();
        self.cpu_upload = None;
        self.shared_textures.clear();
        unsafe {
            self.context.ClearState();
            self.context.Flush();
        }
    }

    fn draw_video(
        &mut self,
        surface: &crate::decoder::windows_surface::D3D11Surface,
        width: u32,
        height: u32,
        rotation: u16,
        color: RenderColor,
    ) -> Result<()> {
        let (coded_width, coded_height) = surface.coded_size();
        let (visible_x, visible_y) = surface.visible_origin();
        validate_visible_geometry(
            visible_x,
            visible_y,
            width,
            height,
            coded_width,
            coded_height,
        )
        .with_context(|| format!("decoded D3D11 texture format is {:?}", surface.format()))?;
        let (texture, array_slice) = self.acquire_surface_texture(surface)?;
        let result = self.draw_texture(
            &texture,
            VideoTextureView {
                array_slice,
                color,
                input_format: surface.format(),
                visible_x,
                visible_y,
                coded_width,
                coded_height,
                width,
                height,
                rotation,
            },
        );
        if result.is_err() {
            self.release_active_input_sync();
        }
        result
    }

    fn acquire_surface_texture(
        &mut self,
        surface: &crate::decoder::windows_surface::D3D11Surface,
    ) -> Result<(ID3D11Texture2D, u32)> {
        self.release_active_input_sync();
        if self.accepts_surface(surface) {
            return Ok((surface.texture().clone(), surface.subresource()));
        }
        let handle = surface
            .shared_handle()
            .context("decoder returned a non-shared texture for an isolated renderer device")?;
        let imported = if let Some(index) = self
            .shared_textures
            .iter()
            .position(|entry| entry.handle == handle)
        {
            self.shared_textures
                .remove(index)
                .expect("shared texture cache index was checked")
        } else {
            let mut texture = None;
            unsafe {
                self.device.OpenSharedResource::<ID3D11Texture2D>(
                    HANDLE(handle as *mut std::ffi::c_void),
                    &raw mut texture,
                )
            }
            .context("open decoder shared texture on isolated renderer device")?;
            let texture = texture.context("D3D11 did not return an imported shared texture")?;
            let sync: IDXGIKeyedMutex = texture
                .cast()
                .context("query decoder shared texture keyed mutex")?;
            tracing::debug!(
                shared_handle = handle,
                "imported decoder shared texture into the isolated renderer device"
            );
            ImportedSharedTexture {
                handle,
                texture,
                sync,
            }
        };
        crate::decoder::windows_surface::acquire_texture_sync(
            &imported.sync,
            OFFICIAL_TEXTURE_SYNC_TIMEOUT_MS,
        )
        .context("acquire decoder shared texture for rendering")?;
        let texture = imported.texture.clone();
        self.active_input_sync = Some(imported.sync.clone());
        self.shared_textures.push_back(imported);
        while self.shared_textures.len() > OFFICIAL_SHARED_TEXTURE_CACHE_SIZE {
            self.shared_textures.pop_front();
        }
        Ok((texture, surface.subresource()))
    }

    fn release_active_input_sync(&mut self) {
        if let Some(sync) = self.active_input_sync.take()
            && let Err(error) = unsafe { sync.ReleaseSync(0) }
        {
            tracing::warn!(%error, "failed to release decoder shared texture sync");
        }
    }

    fn draw_cpu_video(
        &mut self,
        pixels: &[crate::decoder::Rgba8],
        width: u32,
        height: u32,
        rotation: u16,
    ) -> Result<()> {
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .context("CPU decoded dimensions overflow")?;
        if pixels.len() < expected {
            bail!("CPU decoded surface is shorter than its dimensions");
        }
        let recreate = self
            .cpu_upload
            .as_ref()
            .is_none_or(|upload| upload.width != width || upload.height != height);
        if recreate {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut texture = None;
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&raw mut texture))
            }
            .context("create D3D11 CPU fallback upload texture")?;
            self.cpu_upload = Some(CpuUploadTexture {
                width,
                height,
                texture: texture.context("D3D11 did not return a CPU upload texture")?,
            });
        }
        let upload = self
            .cpu_upload
            .as_ref()
            .context("CPU upload texture was not initialized")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context.Map(
                &upload.texture,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&raw mut mapped),
            )
        }
        .context("map D3D11 CPU fallback upload texture")?;
        let source = bytemuck::cast_slice::<crate::decoder::Rgba8, u8>(&pixels[..expected]);
        let source_pitch = usize::try_from(width).expect("u32 fits usize") * 4;
        let destination_pitch = mapped.RowPitch as usize;
        for row in 0..usize::try_from(height).expect("u32 fits usize") {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr().add(row * source_pitch),
                    (mapped.pData as *mut u8).add(row * destination_pitch),
                    source_pitch,
                );
            }
        }
        unsafe { self.context.Unmap(&upload.texture, 0) };
        let texture = upload.texture.clone();
        self.draw_texture(
            &texture,
            VideoTextureView {
                array_slice: 0,
                color: RenderColor::default(), // Already converted RGBA; no YUV transform.
                input_format: DXGI_FORMAT_R8G8B8A8_UNORM,
                visible_x: 0,
                visible_y: 0,
                coded_width: width,
                coded_height: height,
                width,
                height,
                rotation,
            },
        )
    }

    fn draw_texture(&mut self, texture: &ID3D11Texture2D, view: VideoTextureView) -> Result<()> {
        self.video_renderer.draw(
            &self.device,
            &self.context,
            self.render_target
                .as_ref()
                .context("D3D11 render target is unavailable")?,
            texture,
            view,
            self.size,
            self.content_top,
        )
    }

    fn present(&mut self) -> Result<()> {
        let flags = if self.allow_tearing {
            DXGI_PRESENT_ALLOW_TEARING
        } else {
            DXGI_PRESENT(0)
        };
        let started = Instant::now();
        let status = unsafe { self.swap_chain.Present(0, flags) };
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(50) || status != windows::core::HRESULT(0) {
            tracing::warn!(
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                hresult = status.0,
                "D3D11 video Present completed with delay or nonzero status"
            );
        }
        let result = status.ok().context("present D3D11 video swap chain");
        self.release_active_input_sync();
        if let Some(audit) = &mut self.display_audit {
            audit.sample(&self.swap_chain);
        }
        result
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let size = nonzero_size(size);
        if size == self.size {
            return Ok(());
        }
        self.video_renderer.reset_geometry();
        // ResizeBuffers requires every immediate-context reference to the old
        // backbuffer/output view to be released. The video renderer
        // binds resources on this context during the preceding frame.
        unsafe {
            self.context.ClearState();
            self.context.Flush();
        }
        self.render_target.take();
        self.backbuffer.take();
        let buffer_size = swap_chain_output_size(size);
        unsafe {
            self.swap_chain.ResizeBuffers(
                self.buffer_count,
                buffer_size.width,
                buffer_size.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                self.swap_chain_flags,
            )
        }
        .context("resize D3D11 video swap chain")?;
        let (backbuffer, target) = create_backbuffer(&self.device, &self.swap_chain)?;
        self.backbuffer = Some(backbuffer);
        self.render_target = Some(target);
        self.size = size;
        Ok(())
    }
}

impl Drop for D3D11Presenter {
    fn drop(&mut self) {
        if let Some(audit) = self.display_audit.take()
            && let Err(error) = audit.save()
        {
            tracing::warn!(%error, "could not save opt-in DXGI display audit");
        }
        self.release_active_input_sync();
        self.reset_video_resources();
        if self.frame_latency_waitable != 0 {
            if let Err(error) =
                unsafe { CloseHandle(HANDLE(self.frame_latency_waitable as *mut std::ffi::c_void)) }
            {
                tracing::debug!(%error, "failed to close DXGI frame-latency waitable object");
            }
            self.frame_latency_waitable = 0;
        }
    }
}

struct SwapChainCreation {
    swap_chain: IDXGISwapChain1,
    allow_tearing: bool,
    waitable: bool,
    flags: DXGI_SWAP_CHAIN_FLAG,
    buffer_count: u32,
}

fn create_swap_chain(
    device: &ID3D11Device,
    hwnd: HWND,
    size: PhysicalSize<u32>,
) -> Result<SwapChainCreation> {
    let dxgi_device: IDXGIDevice = device.cast().context("query DXGI device")?;
    let adapter = unsafe { dxgi_device.GetAdapter() }.context("get DXGI adapter")?;
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }.context("get DXGI factory")?;
    let allow_tearing = factory.cast::<IDXGIFactory5>().ok().is_some_and(|factory| {
        let mut supported = BOOL::default();
        unsafe {
            factory.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                (&raw mut supported).cast(),
                std::mem::size_of::<BOOL>() as u32,
            )
        }
        .is_ok()
            && supported.as_bool()
    });
    let buffer_size = swap_chain_output_size(size);
    let mut selected = None;
    for (effect, buffer_count, waitable, tearing) in [
        (DXGI_SWAP_EFFECT_FLIP_DISCARD, 2, true, allow_tearing),
        (DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, 2, true, allow_tearing),
        (DXGI_SWAP_EFFECT_DISCARD, 1, false, false),
    ] {
        let flags = if waitable {
            swap_chain_flags(tearing)
        } else {
            DXGI_SWAP_CHAIN_FLAG(0)
        };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: buffer_size.width,
            Height: buffer_size.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: buffer_count,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: effect,
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            Flags: flags.0 as u32,
        };
        match unsafe {
            factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None::<&IDXGIOutput>)
        } {
            Ok(swap_chain) => {
                selected = Some(SwapChainCreation {
                    swap_chain,
                    allow_tearing: tearing,
                    waitable,
                    flags,
                    buffer_count,
                });
                tracing::info!(
                    swap_effect = effect.0,
                    buffer_count,
                    waitable,
                    tearing,
                    "created D3D11 video swap chain"
                );
                break;
            }
            Err(error) => tracing::debug!(
                %error,
                swap_effect = effect.0,
                "D3D11 swap-chain mode unavailable"
            ),
        }
    }
    let selected = selected.context("create a supported D3D11 video swap chain")?;
    unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }
        .context("disable DXGI Alt+Enter handling")?;
    Ok(selected)
}

fn swap_chain_flags(allow_tearing: bool) -> DXGI_SWAP_CHAIN_FLAG {
    if allow_tearing {
        DXGI_SWAP_CHAIN_FLAG(
            DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0
                | DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0,
        )
    } else {
        DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT
    }
}

fn is_device_lost(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|error| {
            matches!(
                error.code(),
                DXGI_ERROR_DEVICE_REMOVED
                    | DXGI_ERROR_DEVICE_RESET
                    | DXGI_ERROR_DRIVER_INTERNAL_ERROR
            )
        })
}

fn swap_chain_output_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    nonzero_size(size)
}

fn validate_visible_geometry(
    visible_x: u32,
    visible_y: u32,
    visible_width: u32,
    visible_height: u32,
    coded_width: u32,
    coded_height: u32,
) -> Result<()> {
    if visible_width == 0
        || visible_height == 0
        || visible_x.saturating_add(visible_width) > coded_width
        || visible_y.saturating_add(visible_height) > coded_height
    {
        bail!(
            "invalid D3D11 video geometry: visible={}x{}+{},{} coded={}x{}",
            visible_width,
            visible_height,
            visible_x,
            visible_y,
            coded_width,
            coded_height
        );
    }
    Ok(())
}

fn fit_rect(video_width: u32, video_height: u32, output_width: u32, output_height: u32) -> RECT {
    let video_aspect = video_width as f64 / video_height.max(1) as f64;
    let output_aspect = output_width as f64 / output_height.max(1) as f64;
    let (width, height) = if video_aspect > output_aspect {
        (
            output_width,
            (output_width as f64 / video_aspect).round() as u32,
        )
    } else {
        (
            (output_height as f64 * video_aspect).round() as u32,
            output_height,
        )
    };
    let left = output_width.saturating_sub(width) / 2;
    let top = output_height.saturating_sub(height) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: left.saturating_add(width) as i32,
        bottom: top.saturating_add(height) as i32,
    }
}
