pub(super) use crate::ui::chrome::title_bar_height_pixels;
use crate::ui::chrome::{
    WindowMoveState, WindowResizeState, update_nonmodal_window_move, update_nonmodal_window_resize,
};
use crate::ui::chrome::{
    configure_dwm_window, handle_title_drag, paint_brand_logo, title_bar_height, title_bar_panel,
    window_buttons, window_title_bar,
};
use crate::ui::controls::{
    ViewerCaptionIcon as TitleIcon, viewer_caption_button as title_icon_button,
};
use crate::ui::gfx::{create_backbuffer, nonzero_size, window_hwnd};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::ui::window_manager::{Event as UiEvent, Repaint as UiRepaintEvent};
use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D_SRV_DIMENSION_TEXTURE2D,
    D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, WaitForSingleObjectEx,
};
use windows::core::{BOOL, Interface};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::windows::EventLoopBuilderExtWindows;
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

#[path = "plugin_capture.rs"]
mod plugin_capture;
#[path = "plugin_video.rs"]
mod plugin_video;
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

pub(crate) struct ConnectingWindowsRunConfig {
    pub alias: String,
    pub progress: std_mpsc::Receiver<ConnectionProgress>,
    pub session: std_mpsc::Receiver<ViewerWindowEvent>,
    pub display_sender: tokio::sync::oneshot::Sender<ViewerDisplayHandle>,
}

pub(super) fn run_connecting(config: ConnectingWindowsRunConfig) -> Result<()> {
    run_player(config, true)
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
    let mut builder = EventLoop::<UiEvent>::with_user_event();
    let router = super::windows_mouse::router().clone();
    builder.with_msg_hook(move |message| {
        super::windows_keyboard::message(message) || router.message(message)
    });
    let event_loop = builder.build().context("create player event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    super::windows_keyboard::remove_unused_raw_keyboard()?;
    let _keyboard_hook = super::windows_keyboard::KeyboardHook::install()
        .map_err(
            |error| tracing::warn!(%error, "keyboard hook unavailable; viewing remains available"),
        )
        .ok();
    let mut runner =
        ConnectingWindowsRunner::new(config, needs_display, event_loop.create_proxy(), false);
    event_loop
        .run_app(&mut runner)
        .map_err(|error| anyhow!("run Windows connection/player event loop: {error}"))?;
    if let Some(error) = runner.fatal_error.take() {
        bail!(error);
    }
    Ok(())
}

pub(crate) struct ConnectingWindowsRunner {
    embedded: bool,
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
    repaint_proxy: EventLoopProxy<UiEvent>,
    ui_generation: u64,
}

impl ApplicationHandler<UiEvent> for ConnectingWindowsRunner {
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
                            self.exit(event_loop);
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
            screens.window_event(window_id, &event, event_loop);
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        if event == WindowEvent::CloseRequested {
            if let Some(app) = self.playing.as_mut() {
                app.mouse.release(window);
                app.shutdown.store(true, Ordering::Release);
            }
            self.exit(event_loop);
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
            app.on_window_event(window, &event, event_loop)
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
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        self.screens.take();
        self.playing.take();
        self.connecting.take();
        self.window.take();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        let UiEvent::Repaint(event) = event else {
            return;
        };
        if !self.owns(event.window) {
            return;
        }
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
                .is_some_and(|app| app.close_requested || app.shutdown.load(Ordering::Acquire))
        {
            self.exit(event_loop);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.receive_session_events() {
            self.fail(event_loop, format!("update player session: {error:#}"));
            return;
        }
        if self.close_requested {
            self.exit(event_loop);
            return;
        }
        if let Some(screens) = self.screens.as_mut() {
            if !screens.update(event_loop) {
                self.exit(event_loop);
            }
            return;
        }
        if let Some(app) = self.playing.as_mut() {
            if app.close_requested {
                self.exit(event_loop);
                return;
            }
            if let Some(window) = &self.window {
                app.refresh_mouse(window, event_loop);
            }
            if app.shutdown.load(Ordering::Acquire) {
                if let Some(error) = mutex_lock(&app.fatal_error).clone() {
                    self.fatal_error = Some(error);
                }
                self.exit(event_loop);
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
            self.exit(event_loop);
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
    pub(crate) fn new(
        config: ConnectingWindowsRunConfig,
        needs_display: bool,
        proxy: EventLoopProxy<UiEvent>,
        embedded: bool,
    ) -> Self {
        Self {
            attributes: WindowAttributes::default()
                .with_visible(false)
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
            repaint_proxy: proxy,
            embedded,
            ui_generation: 1,
            close_requested: false,
            preferences: ViewerPreferences::default(),
            screens: None,
        }
    }
    fn exit(&mut self, event_loop: &ActiveEventLoop) {
        self.close_requested = true;
        if !self.embedded {
            event_loop.exit();
        }
    }
    pub(crate) fn closed(&self) -> bool {
        self.close_requested
    }
    pub(crate) fn error(&self) -> Option<String> {
        self.fatal_error.clone()
    }
    pub(crate) fn owns(&self, id: WindowId) -> bool {
        self.window.as_ref().is_some_and(|w| w.id() == id)
            || self.screens.as_ref().is_some_and(|s| s.owns(id))
    }
    pub(crate) fn focus(&self) {
        if let Some(window) = &self.window {
            window.set_visible(true);
            window.focus_window();
        } else if let Some(screens) = &self.screens {
            screens.focus();
        }
    }
    fn receive_session_events(&mut self) -> Result<()> {
        while let Ok(event) = self.session.try_recv() {
            if self.close_requested {
                continue;
            }
            match event {
                ViewerWindowEvent::Close => self.close_requested = true,
                ViewerWindowEvent::Playing(session) => self.start_playing(*session)?,
                ViewerWindowEvent::Reconnect {
                    alias,
                    window: preferred,
                    progress,
                    display,
                } => {
                    self.alias = alias;
                    if let Some(mut screens) = self.screens.take() {
                        self.preferences = screens.viewer_preferences(preferred);
                        self.window = screens.take_window(preferred);
                    }
                    // Keep the OS window, but join and discard the old room's
                    // render/decoder owners before attaching a new media peer.
                    if let Some(playing) = self.playing.as_ref() {
                        self.preferences = ViewerPreferences {
                            performance_mode: playing.performance_mode,
                            intercept_shortcuts: playing.intercept_shortcuts,
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
                    window.set_title(&format!("{}{}", crate::VIEWER_TITLE_PREFIX, self.alias));
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
        self.exit(event_loop);
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
        repaint_proxy: EventLoopProxy<UiEvent>,
        generation: u64,
    ) -> Result<(Self, ViewerDisplayHandle)> {
        let egui_context = egui::Context::default();
        let window_id = window.id();
        egui_context.set_request_repaint_callback(move |info| {
            if info.viewport_id == egui::ViewportId::ROOT
                && let Some(when) = Instant::now().checked_add(info.delay)
            {
                let _ = repaint_proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                    window: window_id,
                    generation,
                    pass: info.current_cumulative_pass_nr,
                    when,
                }));
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
        let presenter = UiPresenter::from_device(window, renderer_device, renderer_context)?;
        Ok((
            Self {
                progress: ConnectionProgressApp::new(alias, receiver),
                presenter,
                egui_context,
                egui_winit,
                close_requested: false,
                timing_audit: None,
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
            title_bar_panel(ui, "connection-window-chrome", title_bar_height(), |ui| {
                self.close_requested |= window_title_bar(ui, window, &self.progress.alias, None);
            });
            self.progress.draw(ui);
            crate::ui::controls::show_notices(ui.ctx());
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
        if presented && window.is_visible() == Some(false) {
            window.set_visible(true);
        }
        if let Some(audit) = UiTimingAudit::active(&mut self.timing_audit, started) {
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

struct PlayerTitleBar<'a> {
    screens: &'a mut ScreenTabBar,
    window: &'a Window,
    title: &'a str,
    performance: &'a PerformanceMonitor,
    stream_control: &'a StreamControlHandle,
    stream_control_ui: &'a mut StreamControlUi,
    plugin_menu_open: &'a mut bool,
    move_state: Option<&'a mut WindowMoveState>,
}

fn player_title_bar(ui: &mut egui::Ui, mut bar: PlayerTitleBar<'_>) -> PlayerChromeAction {
    ui.set_min_height(crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT);
    let mut action = PlayerChromeAction::default();
    let stats = bar.performance.snapshot();
    let rect = egui::Rect::from_min_size(
        ui.available_rect_before_wrap().min,
        egui::vec2(
            ui.available_width(),
            crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT,
        ),
    );
    const VIEW_ACTIONS_WIDTH: f32 = crate::ui::theme::VIEWER_ACTIONS_WIDTH;
    let title = bar.title.trim_start_matches(crate::VIEWER_TITLE_PREFIX);
    let title_width = if bar.screens.device_switch.is_some() {
        crate::ui::controls::viewer_device_button_width(ui, title)
    } else {
        ui.painter()
            .layout_no_wrap(
                title.into(),
                egui::FontId::proportional(crate::ui::theme::BODY),
                crate::ui::theme::TEXT,
            )
            .size()
            .x
    };
    let identity_width =
        (crate::ui::theme::WINDOW_LOGO_SIZE + ui.spacing().item_spacing.x + title_width)
            .min(crate::ui::theme::VIEWER_IDENTITY_MAX_WIDTH);
    let controls_rect = egui::Rect::from_min_max(
        egui::pos2(
            rect.max.x - crate::ui::theme::WINDOW_CONTROLS_WIDTH,
            rect.min.y,
        ),
        rect.max,
    );
    let actions_rect = egui::Rect::from_min_max(
        egui::pos2(controls_rect.min.x - VIEW_ACTIONS_WIDTH, rect.min.y),
        egui::pos2(controls_rect.min.x, rect.max.y),
    );
    let identity_rect = egui::Rect::from_min_size(
        rect.min,
        egui::vec2(
            identity_width.min((actions_rect.min.x - rect.min.x).max(0.0)),
            rect.height(),
        ),
    );
    let content_rect = egui::Rect::from_min_max(
        egui::pos2(
            identity_rect.max.x + crate::ui::theme::VIEWER_IDENTITY_GAP,
            rect.min.y,
        ),
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
        egui::Rect::from_min_max(
            egui::pos2(
                if bar.screens.device_switch.is_some() {
                    identity_rect.max.x
                } else {
                    rect.min.x
                },
                rect.min.y,
            ),
            metrics_rect.max,
        ),
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
    paint_brand_logo(&mut identity);
    if let Some(switcher) = &bar.screens.device_switch {
        let response = crate::ui::controls::viewer_device_button(
            &mut identity,
            bar.title.trim_start_matches(crate::VIEWER_TITLE_PREFIX),
        );
        switcher.menu(&response, bar.window.id());
    } else {
        identity.add(
            egui::Label::new(
                egui::RichText::new(bar.title.trim_start_matches(crate::VIEWER_TITLE_PREFIX))
                    .size(crate::ui::theme::BODY)
                    .strong()
                    .color(crate::ui::theme::TEXT),
            )
            .truncate(),
        );
    }

    crate::ui::controls::viewer_caption_separator(
        ui,
        egui::pos2(
            identity_rect.right() + crate::ui::theme::VIEWER_IDENTITY_GAP / 2.0,
            rect.center().y,
        ),
    );

    if !bar.screens.tabs.is_empty() {
        let mut tabs = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(tabs_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        tabs.set_clip_rect(tabs_rect);
        bar.screens.draw(&mut tabs, bar.window, bar.stream_control);
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
                    .size(crate::ui::theme::TINY)
                    .color(crate::ui::theme::MUTED),
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
    let quality_button = title_icon_button(
        &mut actions,
        TitleIcon::Quality,
        bar.stream_control_ui.open,
        "画质与串流设置",
    );
    if quality_button.clicked() {
        bar.stream_control_ui.open = !bar.stream_control_ui.open;
        if bar.stream_control_ui.open {
            *bar.plugin_menu_open = false;
        }
    }
    if title_icon_button(
        &mut actions,
        TitleIcon::Plugins,
        *bar.plugin_menu_open,
        "插件",
    )
    .clicked()
    {
        *bar.plugin_menu_open = !*bar.plugin_menu_open;
        if *bar.plugin_menu_open {
            bar.stream_control_ui.open = false;
        }
    }
    let annotation = bar.stream_control.annotation_snapshot();
    action.toggle_annotation = actions
        .add_enabled_ui(
            !annotation.toggling
                && (annotation.enabled
                    || annotation.supported
                    || bar.stream_control.snapshot().ready
                        && bar.stream_control.remote_upgrade().is_some()),
            |ui| title_icon_button(ui, TitleIcon::Annotation, annotation.enabled, "批注"),
        )
        .inner
        .on_disabled_hover_text("当前设备暂不支持批注，或仍在连接中")
        .clicked();
    action.one_to_one = actions
        .add_enabled_ui(
            !bar.window.is_maximized() && bar.window.fullscreen().is_none(),
            |ui| {
                title_icon_button(
                    ui,
                    TitleIcon::OneToOne,
                    false,
                    "1:1 实际像素（固定左上角，空间不足时等比缩小）",
                )
            },
        )
        .inner
        .on_disabled_hover_text("请先还原窗口")
        .clicked();
    let control = bar.stream_control.snapshot();
    let mic = bar.stream_control.microphone().snapshot();
    let mic_hint = if mic.pending {
        "麦克风：等待远端确认"
    } else if mic.error.is_some() {
        "麦克风错误（在高级设置中查看）"
    } else if mic.capturing {
        "正在将麦克风发送到远端，点击关闭"
    } else if mic.enabled {
        "麦克风已开启，等待远端应用使用，点击关闭"
    } else {
        "将所选麦克风发送到远端（设备可在高级设置中切换）"
    };
    if actions
        .add_enabled_ui(
            mic.enabled
                || (bar.stream_control.microphone_available()
                    && control.ready
                    && control.mouse_mode != crate::remote_input::MouseMode::View),
            |ui| title_icon_button(ui, TitleIcon::Microphone, mic.enabled, mic_hint),
        )
        .inner
        .on_disabled_hover_text("请先连接支持麦克风的 Windows 设备并开启键鼠控制")
        .clicked()
    {
        if let Err(error) = bar.stream_control.set_microphone_enabled(!mic.enabled) {
            bar.stream_control_ui.local_error = Some(error.to_string());
        }
    }
    let enabled =
        control.mouse_mode != crate::remote_input::MouseMode::View || control.mouse_pending;
    action.toggle_mouse = actions
        .add_enabled_ui(enabled || control.ready, |ui| {
            let hint = if bar.stream_control.mouse().waiting_for_neutral() {
                format!(
                    "等待松开全部键鼠；退出控制快捷键：{}",
                    crate::viewer_shortcuts::label(ViewerShortcut::ReleaseMouse)
                )
            } else if enabled {
                format!(
                    "退出控制（{}）",
                    crate::viewer_shortcuts::label(ViewerShortcut::ReleaseMouse)
                )
            } else {
                if bar.stream_control.mouse().keyboard_supported() {
                    "开启键鼠控制".into()
                } else {
                    "开启鼠标控制（此平台暂未适配键盘）".into()
                }
            };

            title_icon_button(ui, TitleIcon::Mouse, enabled, &hint)
        })
        .inner
        .clicked();

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
        egui::Stroke::new(1.0, crate::ui::theme::LINE),
    );
    ui.advance_cursor_after_rect(rect);
    action
}

#[derive(Clone, Copy, Debug, Default)]
struct PlayerChromeAction {
    close: bool,
    one_to_one: bool,
    toggle_mouse: bool,
    toggle_annotation: bool,
    drag_window: bool,
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
    last_window_size: &mut PhysicalSize<u32>,
    pending_aspect_size: &mut Option<PhysicalSize<u32>>,
) {
    if size.width == 0 || size.height == 0 || window.is_maximized() || window.fullscreen().is_some()
    {
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
    if window.is_maximized() || window.fullscreen().is_some() {
        return;
    }
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
    let size = window.inner_size();
    let (max_width, max_height) = match (monitor_work_area(window), window.outer_position()) {
        (Some(work), Ok(origin)) => {
            let outer = window.outer_size();
            // Only grow to the right/bottom from the existing outer origin.
            // Reserve non-client pixels as well; never move the window to fit.
            (
                (work.right.saturating_sub(origin.x).max(1) as u32)
                    .saturating_sub(outer.width.saturating_sub(size.width))
                    .max(1),
                (work.bottom.saturating_sub(origin.y).max(1) as u32)
                    .saturating_sub(outer.height.saturating_sub(size.height))
                    .max(1),
            )
        }
        _ => (size.width.max(1), size.height.max(1)),
    };
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
}

fn borderless_resize(
    ui: &mut egui::Ui,
    window: &Window,
    mut manual: Option<(&mut WindowResizeState, Option<(u32, u32)>)>,
) -> Option<ResizeDirection> {
    let ctx = ui.ctx().clone();
    let mut requested = None;
    crate::ui::chrome::resize_regions(ui, window, |response, direction| {
        if let Some((state, aspect)) = manual.as_mut() {
            update_nonmodal_window_resize(&ctx, window, response, direction, state, *aspect);
        } else if response.drag_started() {
            requested = Some(direction);
        }
    });
    requested
}

enum RenderCommand {
    Resize(PhysicalSize<u32>),
    Occluded(bool),
    Stop,
}

struct RenderWorker {
    plugins: crate::plugins::Controller,
    frame_wake: super::FrameWake,
    commands: std_mpsc::Sender<RenderCommand>,
    wake: std::thread::Thread,
    thread: Option<std::thread::JoinHandle<()>>,
    current_video_size: Arc<Mutex<Option<(u32, u32, u16)>>>,
    first_presented: Arc<AtomicBool>,
}

impl RenderWorker {
    fn spawn(
        hwnd: isize,
        mut size: PhysicalSize<u32>,
        session: &NativeViewerSession,
        context: &egui::Context,
        cpu_device: Arc<Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>>>,
    ) -> Result<Self> {
        let started = Instant::now();
        let screen_id = session.screen_id();
        let stream_control = session.stream_control.clone();
        let plugins = crate::plugins::Controller::new(context.clone());
        let plugin_state = plugins.shared.clone();
        let frame_queue = Arc::clone(&session.frame_queue);
        let decoder_wake = session.manager_wake.clone();
        let performance = session.performance.clone();
        let worker_shutdown = Arc::clone(&session.shutdown);
        let (commands, command_receiver) = std_mpsc::channel();
        let current_video_size = Arc::new(Mutex::new(None));
        let worker_video_size = Arc::clone(&current_video_size);
        let first_presented = Arc::new(AtomicBool::new(false));
        let worker_first_presented = Arc::clone(&first_presented);
        let first_frame_repaint = context.clone();
        let thread = std::thread::Builder::new()
            .name("Video Render".to_owned())
            .spawn(move || {
                if let Err(error) =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }
                {
                    tracing::warn!(%error, "set video render thread priority");
                }
                let mut first_present = true;
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
                        if let Some(p) = presenter.as_mut() {
                            if let Some(effects) = p.effects.as_mut() {
                                effects.clear_history();
                            }
                            p.captures = plugin_video::Captures::default();
                        }
                        let dropped = {
                            let mut queue = mutex_lock(&frame_queue);
                            let count = queue.len();
                            queue.clear();
                            performance.set_presentation_queue_frames(0);
                            count
                        };
                        decoder_wake.unpark();
                        for _ in 0..dropped {
                            performance.record_dropped_present_frame();
                        }
                        // Continue draining on decoded-frame notifications while minimized.
                        std::thread::park();
                        continue;
                    }
                    let chain_changed = presenter.as_ref().is_some_and(|p| {
                        p.effect_revision != plugin_state.revision.load(Ordering::Acquire)
                    });
                    redraw |= chain_changed;
                    let replacement = take_next_frame(&mut mutex_lock(&frame_queue), &performance);
                    if replacement.is_some() {
                        decoder_wake.unpark();
                    }
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
                                    D3D11Presenter::for_frame(hwnd, size, frame, &cpu_device)
                                        .map_err(VideoRenderError::prepare)?,
                                );
                                tracing::info!("initialized native video presentation pipeline");
                            }
                            let p = presenter.as_mut().expect("presenter initialized");
                            p.plugin_state = Some(plugin_state.clone());
                            if p.effect_revision != plugin_state.revision.load(Ordering::Acquire) {
                                let fps = performance.snapshot().receive_fps;
                                p.nominal_fps = if fps >= 15.0 { fps } else { 60.0 };
                            }
                            p.analysis_at = frame.received_at;
                            p.analysis_new = is_new_submission || chain_changed;
                            p.resize(size).map_err(VideoRenderError::prepare)?;
                            render_thread_frame(p, frame, &performance, is_new_submission)
                        })();
                        match result {
                            Ok(()) => {
                                if is_new_submission && presenter.as_ref().is_some_and(|p| p.drew) {
                                    stream_control
                                        .topology_frame_presented(screen_id, frame.received_at);
                                }
                                if first_present && presenter.as_ref().is_some_and(|p| p.drew) {
                                    first_present = false;
                                    worker_first_presented.store(true, Ordering::Release);
                                    first_frame_repaint.request_repaint();
                                    tracing::debug!(
                                        screen_id,
                                        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                                        "screen renderer first frame presented"
                                    );
                                }
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
                        if let Some(due) = presenter
                            .as_ref()
                            .and_then(|p| p.effects.as_ref())
                            .and_then(|e| e.next_due())
                        {
                            std::thread::park_timeout(
                                due.saturating_duration_since(Instant::now()),
                            );
                            redraw = true;
                        } else {
                            std::thread::park();
                        }
                    }
                }
                // GPU resources and retained samples go away before the child HWND.
                drop(presenter);
                drop(current_frame);
                if let Some((_, context)) = mutex_lock(&cpu_device).as_ref() {
                    // Release bindings held by the retained context before another track uses it.
                    unsafe {
                        context.ClearState();
                    }
                }
                mutex_lock(&frame_queue).clear();
                performance.set_presentation_queue_frames(0);
                decoder_wake.unpark();
            })
            .context("create Video Render thread")?;
        let wake = thread.thread().clone();
        *mutex_lock(&plugins.shared.wake) = Some(wake.clone());
        session.frame_wake.install_render_thread(wake.clone());
        wake.unpark();
        Ok(Self {
            plugins,
            frame_wake: session.frame_wake.clone(),
            commands,
            wake,
            thread: Some(thread),
            current_video_size,
            first_presented,
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
    presenter.submission_wait = Duration::ZERO;
    let wait = Duration::ZERO;
    let started = Instant::now();
    presenter.drew = false;
    presenter.effect_output = None;
    presenter.effect_generated = false;
    presenter.effect_input = is_new_submission.then_some(plugin_video::Metadata {
        received_at: frame.received_at,
        timing: RenderedFrameTiming {
            is_new_picture: frame.is_new_picture,
            width: frame.width,
            height: frame.height,
            decoded_at: frame.decoded_at,
            local: Duration::ZERO,
            assembly: frame.assembly_delay,
            input_queue: frame.input_queue_delay,
            decode_pipeline: frame.decode_pipeline_delay,
            surface_transfer: Duration::ZERO,
            present_wait: wait,
            render_queue: frame.decoded_at.elapsed(),
            sender_capture_at: frame.sender_timing.capture_at,
            sender_capture: frame.sender_timing.capture_delay,
            sender_encode: frame.sender_timing.encode_delay,
            sender_pacer: frame.sender_timing.pacer_delay,
            sender_total: frame.sender_timing.sending_delay,
            transport: frame.sender_timing.transport_delay,
        },
    });
    let result = if !presenter.analysis_new && presenter.effects.is_some() {
        presenter.draw_effect_output(frame.color, true)
    } else {
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
    };
    result.map_err(VideoRenderError::prepare)?;
    if !presenter.drew {
        presenter.release_active_input_sync();
        return Ok(());
    }
    let wait = presenter.submission_wait;
    let transfer = started.elapsed().saturating_sub(wait);
    presenter.present().map_err(|error| VideoRenderError {
        release_resources: is_device_lost(&error),
        error,
    })?;
    if presenter.effect_generated {
        if let Some(state) = &presenter.plugin_state {
            state.generated.fetch_add(1, Ordering::Relaxed);
        }
    } else if let Some(mut metadata) = presenter.effect_output {
        metadata.timing.local = metadata.received_at.elapsed();
        metadata.timing.surface_transfer = transfer;
        metadata.timing.present_wait = wait;
        metadata.timing.render_queue =
            started.saturating_duration_since(metadata.timing.decoded_at);
        performance.record_rendered_frame(metadata.timing);
    }
    Ok(())
}

pub(super) use crate::viewer_shortcuts::Action as ViewerShortcut;
fn viewer_shortcut(
    modifiers: winit::keyboard::ModifiersState,
    key: winit::keyboard::PhysicalKey,
) -> Option<ViewerShortcut> {
    if crate::viewer_shortcuts::suspended() {
        return None;
    }
    crate::viewer_shortcuts::match_key(
        crate::viewer_shortcuts::physical_key(key)?,
        crate::viewer_shortcuts::modifiers(modifiers),
    )
}

struct ThreadedWindowsApp {
    annotation: super::annotation::AnnotationUi,
    modifiers: winit::keyboard::ModifiersState,
    mouse: super::windows_mouse::WindowMouse,
    last_mouse_mode: crate::remote_input::MouseMode,
    screen_tabs: ScreenTabBar,
    close_requested: bool,
    title: String,
    performance: PerformanceMonitor,
    stream_control: StreamControlHandle,
    stream_control_ui: StreamControlUi,
    plugin_menu_open: bool,
    display_transition_visible: bool,
    bound_screen: u64,
    shutdown: Arc<AtomicBool>,
    fatal_error: Arc<Mutex<Option<String>>>,
    egui_context: egui::Context,
    egui_winit: egui_winit::State,
    performance_mode: PerformancePanelMode,
    intercept_shortcuts: bool,
    startup_backdrop: Option<ConnectionProgressApp>,
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
    // Only successive video workers use this context; each previous worker is joined first.
    cpu_device: Arc<Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>>>,
}

impl ThreadedWindowsApp {
    fn replace_session(
        &mut self,
        window: &Window,
        session: Arc<NativeViewerSession>,
    ) -> Result<()> {
        // One composition target per HWND. Keep UI composition and the video
        // child window; retire the old swap chain before attaching a new track.
        self.stream_control
            .cancel_display_change(self.bound_screen as u32 as i32);
        self.annotation.bind(session.stream_control.clone());
        self.stream_control_ui = StreamControlUi::default();
        self.mouse.release(window);
        self.mouse = super::windows_mouse::WindowMouse::new(
            window,
            &session.stream_control,
            &self.egui_context,
        );
        self.last_mouse_mode = crate::remote_input::MouseMode::View;
        self.renderer.stop();
        self.performance.pause_presentation();
        let size = self.video_window.resize(window, window.inner_size())?;
        self.renderer = RenderWorker::spawn(
            self.video_window.handle(),
            size,
            &session,
            &self.egui_context,
            Arc::clone(&self.cpu_device),
        )?;
        self.title = session.title.clone();
        self.performance = session.performance.clone();
        self.stream_control = session.stream_control.clone();
        self.shutdown = Arc::clone(&session.shutdown);
        self.fatal_error = Arc::clone(&session.fatal_error);
        self.bound_screen = session.screen_binding();
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
        let cpu_device = Arc::new(Mutex::new(None));
        let renderer = RenderWorker::spawn(
            video_window.handle(),
            size,
            &session,
            &connecting.egui_context,
            Arc::clone(&cpu_device),
        )?;
        Ok(Self {
            annotation: super::annotation::AnnotationUi::new(
                stream_control.clone(),
                window_hwnd(window)?.0 as u64,
            ),
            mouse: super::windows_mouse::WindowMouse::new(
                window,
                &stream_control,
                &connecting.egui_context,
            ),
            last_mouse_mode: crate::remote_input::MouseMode::View,
            modifiers: winit::keyboard::ModifiersState::empty(),
            screen_tabs: ScreenTabBar::default(),
            close_requested: false,
            title,
            performance,
            stream_control,
            stream_control_ui: StreamControlUi::default(),
            plugin_menu_open: false,
            display_transition_visible: false,
            bound_screen: session.screen_binding(),
            shutdown,
            fatal_error,
            egui_context: connecting.egui_context,
            egui_winit: connecting.egui_winit,
            performance_mode: preferences.performance_mode,
            intercept_shortcuts: preferences.intercept_shortcuts,
            startup_backdrop: Some(connecting.progress),
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
            cpu_device,
        })
    }

    fn on_window_event(
        &mut self,
        window: &Window,
        event: &WindowEvent,
        event_loop: &ActiveEventLoop,
    ) -> Result<()> {
        if let WindowEvent::ModifiersChanged(modifiers) = event {
            self.modifiers = modifiers.state();
        }
        if let WindowEvent::KeyboardInput {
            event: key,
            is_synthetic: false,
            ..
        } = event
            && window.has_focus()
            && !self.egui_context.text_edit_focused()
            && key.state == winit::event::ElementState::Pressed
            && let Some(shortcut) = viewer_shortcut(self.modifiers, key.physical_key)
        {
            if !key.repeat {
                self.apply_shortcut(window, shortcut)?;
                window.request_redraw();
                if !self.close_requested {
                    self.refresh_mouse(window, event_loop);
                }
            }
            return Ok(());
        }
        let response = self.egui_winit.on_window_event(window, event);
        if matches!(
            event,
            WindowEvent::Focused(false) | WindowEvent::Occluded(true)
        ) {
            self.annotation.finish();
            self.renderer.plugins.disarm();
            self.mouse.release(window);
        }
        if matches!(event, WindowEvent::Focused(false)) {
            self.modifiers = winit::keyboard::ModifiersState::empty();
            if let Ok(hwnd) = window_hwnd(window) {
                crate::viewer_shortcuts::set_text_owner(hwnd.0 as u64, false);
            }
        }
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
        self.refresh_mouse(window, event_loop);
        if let WindowEvent::MouseWheel { delta, .. } = event {
            self.mouse.wheel(*delta);
        }
        Ok(())
    }

    fn apply_shortcut(&mut self, window: &Window, shortcut: ViewerShortcut) -> Result<()> {
        self.renderer.plugins.disarm();
        match shortcut {
            ViewerShortcut::ReleaseMouse => {
                self.mouse.release(window);
                if let Err(error) = self
                    .stream_control
                    .set_mouse_mode(crate::remote_input::MouseMode::View)
                {
                    self.stream_control.mouse().fail(error.to_string());
                }
            }
            ViewerShortcut::Fullscreen => {
                self.pending_aspect_size = None;
                self.window_move = WindowMoveState::default();
                self.window_resize = WindowResizeState::default();
                self.stream_control_ui.open = false;
                let fullscreen = window
                    .fullscreen()
                    .is_none()
                    .then(|| winit::window::Fullscreen::Borderless(window.current_monitor()));
                window.set_fullscreen(fullscreen);
                configure_dwm_window(window);
                // A maximized window may keep the same outer size. Update
                // the video origin even if no size-change event follows.
                self.resize_targets(window, window.inner_size())?;
            }
            ViewerShortcut::Close => {
                self.mouse.release(window);
                self.close_requested = true;
            }
            ViewerShortcut::Performance => {
                self.performance_mode = self.performance_mode.next();
            }
        }
        window.request_redraw();
        Ok(())
    }

    pub(super) fn refresh_mouse(&mut self, window: &Window, event_loop: &ActiveEventLoop) {
        if let Ok(hwnd) = window_hwnd(window) {
            super::windows_keyboard::finish_lock_releases(hwnd.0 as u64);
        }
        if let Ok(hwnd) = window_hwnd(window)
            && let Some(shortcut) = super::windows_keyboard::take_shortcut(hwnd.0 as u64)
            && window.has_focus()
            && let Err(error) = self.apply_shortcut(window, shortcut)
        {
            self.stream_control.mouse().fail(error.to_string());
        }
        if self.close_requested {
            self.renderer.plugins.disarm();
            self.mouse.release(window);
            return;
        }
        let mode = self.stream_control.mouse().mode();
        if self.last_mouse_mode != crate::remote_input::MouseMode::View
            && mode == crate::remote_input::MouseMode::View
            && self.stream_control.mouse().error().is_some()
        {
            let _ = self
                .stream_control
                .set_mouse_mode(crate::remote_input::MouseMode::View);
        }
        if self.last_mouse_mode != mode && mode != crate::remote_input::MouseMode::View {
            self.stream_control_ui.open = false;
            window.request_redraw();
        }
        self.last_mouse_mode = mode;
        // The visible transition owns the obscured desktop, like a local menu.
        // Release held input while it is shown; the title bar remains interactive.
        let display_transition = self.stream_control.annotation_snapshot().enabled
            || self
                .stream_control
                .remote_upgrade()
                .is_some_and(|upgrade| upgrade.owns_input(window.id()))
            || self.startup_backdrop.is_some()
            || self.display_transition_visible
            || super::stream_menu::topology_menu::owns_input(&self.egui_context)
            || self.egui_context.memory(|m| m.top_modal_layer().is_some())
            || self.bound_screen != self._session.screen_binding()
            || self
                .stream_control
                .display_input_blocked(self._session.track_index, self.bound_screen)
            || self
                .stream_control
                .pending_display_resolution(self._session.screen_id())
                .is_some();
        self.renderer.plugins.input_context(
            self.stream_control.mouse().clone(),
            window_hwnd(window).map_or(0, |h| h.0 as u64),
            window.has_focus()
                && !window.is_minimized().unwrap_or(false)
                && !self.plugin_menu_open
                && !self.stream_control_ui.open
                && !display_transition
                && !self.screen_tabs.is_pending()
                && !self.egui_context.any_popup_open()
                && !self.egui_context.text_edit_focused(),
        );
        self.mouse.refresh(
            window,
            &self.egui_context,
            &self.stream_control,
            self._session.track_index,
            &self.renderer.current_video_size,
            self.intercept_shortcuts,
            self.stream_control_ui.open
                || self.plugin_menu_open
                || self.screen_tabs.is_pending()
                || display_transition,
            event_loop,
        );
    }

    fn draw_ui(&mut self, window: &Window) -> Result<()> {
        crate::viewer_shortcuts::refresh();
        self.egui_context
            .request_repaint_after(Duration::from_millis(500));
        if self.renderer.first_presented.load(Ordering::Acquire) {
            self.startup_backdrop = None;
        }
        let started = Instant::now();
        let input = self.egui_winit.take_egui_input(window);
        let mut view = super::stream_menu::LocalViewSettings {
            performance_mode: self.performance_mode,
            intercept_shortcuts: self.intercept_shortcuts,
            send_ctrl_alt_del: false,
        };
        let mut chrome_action = PlayerChromeAction::default();
        let fullscreen = window.fullscreen().is_some();
        if fullscreen {
            self.plugin_menu_open = false;
            self.stream_control_ui.open = false;
        }
        let mut resize = None;
        let resize_aspect = self.current_video_size();
        let plugin_video_size = self.current_video_size();
        let output = self.egui_context.run_ui(input, |ui| {
            let ctx = ui.ctx().clone();
            if !fullscreen {
                resize =
                    borderless_resize(ui, window, Some((&mut self.window_resize, resize_aspect)));
                title_bar_panel(
                    ui,
                    "viewer-toolbar",
                    title_bar_height_pixels(window) as f32 / ctx.pixels_per_point(),
                    |ui| {
                        chrome_action = player_title_bar(
                            ui,
                            PlayerTitleBar {
                                screens: &mut self.screen_tabs,
                                window,
                                title: &self.title,
                                performance: &self.performance,
                                stream_control: &self.stream_control,
                                stream_control_ui: &mut self.stream_control_ui,
                                plugin_menu_open: &mut self.plugin_menu_open,
                                move_state: Some(&mut self.window_move),
                            },
                        );
                    },
                );
            }
            if let Some(loading) = &mut self.startup_backdrop {
                loading.draw(ui);
                return;
            }
            self.display_transition_visible = super::display_transition::show(
                &ctx,
                &self.stream_control,
                self._session.screen_id(),
                ui.available_rect_before_wrap(),
            );
            show_stream_control_window(
                &ctx,
                &self.stream_control,
                &mut self.stream_control_ui,
                &mut view,
                self._session.screen_id(),
                window
                    .current_monitor()
                    .map(|m| (m.size().width, m.size().height)),
            );
            self.renderer.plugins.window(
                &ctx,
                &mut self.plugin_menu_open,
                super::stream_menu::menu_style,
                &self.stream_control,
            );
            let size = window.inner_size();
            let top = title_bar_height_pixels(window);
            if let Some((w, h)) = plugin_video_size {
                let r = fit_rect(w, h, size.width, size.height.saturating_sub(top));
                let scale = ctx.pixels_per_point();
                self.renderer.plugins.paint(
                    &ctx,
                    size.width as f32 / scale,
                    size.height as f32 / scale,
                    [
                        r.left as f32 / scale,
                        (r.top as f32 + top as f32) / scale,
                        r.right as f32 / scale,
                        (r.bottom as f32 + top as f32) / scale,
                    ],
                );
            }
            let annotation_rect = plugin_video_size.map(|(w, h)| {
                let r = fit_rect(w, h, size.width, size.height.saturating_sub(top));
                let scale = ctx.pixels_per_point();
                egui::Rect::from_min_max(
                    egui::pos2(r.left as f32 / scale, (r.top as f32 + top as f32) / scale),
                    egui::pos2(
                        r.right as f32 / scale,
                        (r.bottom as f32 + top as f32) / scale,
                    ),
                )
            });
            self.annotation.draw(
                ui,
                self._session.screen_id(),
                annotation_rect,
                self.display_transition_visible
                    || self.screen_tabs.is_pending()
                    || self.stream_control_ui.open
                    || self.plugin_menu_open
                    || self
                        .stream_control
                        .display_input_blocked(self._session.track_index, self.bound_screen),
                window.has_focus(),
            );
            super::show_performance_overlay(
                &ctx,
                &self.performance,
                &self.stream_control.audio(),
                view.performance_mode,
                "performance-grid-d3d11",
            );
            if let Some(upgrade) = self.stream_control.remote_upgrade() {
                upgrade.show(&ctx, window.id(), &self.stream_control);
            }
            crate::ui::controls::show_notices(&ctx);
        });
        self.performance_mode = view.performance_mode;
        if self.intercept_shortcuts != view.intercept_shortcuts {
            self.mouse.release(window);
            self.intercept_shortcuts = view.intercept_shortcuts;
        }
        if view.send_ctrl_alt_del && window.has_focus() {
            self.mouse.release(window);
            let result = window_hwnd(window)
                .and_then(|hwnd| self.stream_control.mouse().send_ctrl_alt_del(hwnd.0 as u64));
            match result {
                Ok(()) => {
                    self.stream_control_ui.local_error = None;
                    self.stream_control_ui.open = false;
                }
                Err(error) => self.stream_control_ui.local_error = Some(error.to_string()),
            }
            window.request_redraw();
        }
        if let Ok(hwnd) = window_hwnd(window) {
            crate::viewer_shortcuts::set_text_owner(
                hwnd.0 as u64,
                self.egui_context.text_edit_focused()
                    || crate::plugins::capturing_shortcut(&self.egui_context),
            );
        }
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
        if let Some(audit) = UiTimingAudit::active(&mut self.timing_audit, started) {
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
        if current_video_size.is_some() && current_video_size != self.last_aspect_video_size {
            self.fit_window_to_current_aspect(window);
        }
        self.last_aspect_video_size = current_video_size;
        if chrome_action.close {
            self.mouse.release(window);
            self.close_requested = true;
        }
        if chrome_action.toggle_annotation {
            self.mouse.release(window);
            self.renderer.plugins.disarm();
            self.stream_control_ui.open = false;
            self.plugin_menu_open = false;
            if !self.stream_control.annotation_snapshot().supported
                && !self.stream_control.annotation_snapshot().enabled
                && let Some(upgrade) = self.stream_control.remote_upgrade()
            {
                upgrade.prompt(window.id(), "批注");
            } else {
                self.annotation.toggle();
            }
            window.request_redraw();
        }
        if chrome_action.toggle_mouse {
            let state = self.stream_control.snapshot();
            let mode = if state.mouse_mode != crate::remote_input::MouseMode::View
                || state.mouse_pending
            {
                self.mouse.release(window);
                crate::remote_input::MouseMode::View
            } else {
                state.mouse_preference
            };
            if mode != crate::remote_input::MouseMode::View {
                crate::ui::controls::clear_notice(&self.egui_context, "remote-input-error");
            }
            if let Err(error) = self.stream_control.set_mouse_mode(mode) {
                self.stream_control.mouse().fail(error.to_string());
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
        self.stream_control
            .cancel_display_change(self.bound_screen as u32 as i32);
        self.renderer.stop();
        self.performance.pause_presentation();
    }
}

struct D3D11Presenter {
    hwnd: HWND,
    hdr_output: bool,
    input_hdr: bool,
    hdr_output_unavailable: bool,
    output_monitor: isize,
    output_monitor_hdr: bool,
    output_hdr_checked: Option<Instant>,
    captures: plugin_video::Captures,
    plugin_state: Option<Arc<crate::plugins::ChainShared>>,
    effects: Option<plugin_video::Engine>,
    effect_revision: u64,
    effects_cache_revision: u64,
    nominal_fps: f64,
    effect_input: Option<plugin_video::Metadata>,
    effect_output: Option<plugin_video::Metadata>,
    effect_generated: bool,
    drew: bool,
    submission_wait: Duration,
    analysis_at: Instant,
    analysis_new: bool,
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
    ayuv_pixel_shader: ID3D11PixelShader,
    ayuv_array_pixel_shader: ID3D11PixelShader,
    y410_pixel_shader: ID3D11PixelShader,
    y410_array_pixel_shader: ID3D11PixelShader,
    rgba_pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    point_sampler: ID3D11SamplerState,
    rasterizer: ID3D11RasterizerState,
    color_buffer: Option<(RenderColor, u8, bool, ID3D11Buffer)>,
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
    hdr: [f32; 4],
}

const OFFICIAL_SHARED_TEXTURE_CACHE_SIZE: usize = 32;
const OFFICIAL_TEXTURE_SYNC_TIMEOUT_MS: u32 = 100;

#[derive(Clone, Copy)]
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
        let mut ayuv_pixel_shader = None;
        let mut ayuv_array_pixel_shader = None;
        let mut y410_pixel_shader = None;
        let mut y410_array_pixel_shader = None;
        let mut rgba_pixel_shader = None;
        let mut sampler = None;
        let mut point_sampler = None;
        let mut rasterizer = None;
        unsafe {
            device.CreatePixelShader(
                include_bytes!("shaders/video_ayuv_ps.cso"),
                None,
                Some(&mut ayuv_pixel_shader),
            )?;
            device.CreatePixelShader(
                include_bytes!("shaders/video_ayuv_array_ps.cso"),
                None,
                Some(&mut ayuv_array_pixel_shader),
            )?;
            device.CreatePixelShader(
                include_bytes!("shaders/video_y410_ps.cso"),
                None,
                Some(&mut y410_pixel_shader),
            )?;
            device.CreatePixelShader(
                include_bytes!("shaders/video_y410_array_ps.cso"),
                None,
                Some(&mut y410_array_pixel_shader),
            )?;
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
            ayuv_pixel_shader: ayuv_pixel_shader.context("missing AYUV shader")?,
            ayuv_array_pixel_shader: ayuv_array_pixel_shader
                .context("missing AYUV array shader")?,
            y410_pixel_shader: y410_pixel_shader.context("missing Y410 shader")?,
            y410_array_pixel_shader: y410_array_pixel_shader
                .context("missing Y410 array shader")?,
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
        // A former render target must be unbound before it becomes an input.
        unsafe {
            context.OMSetRenderTargets(None, None);
        }
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
        let yuv =
            plane_1.is_some() || matches!(view.input_format, DXGI_FORMAT_AYUV | DXGI_FORMAT_Y410);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&raw mut desc) };
        let pixel_shader = if view.input_format == DXGI_FORMAT_AYUV {
            if desc.ArraySize > 1 {
                &self.ayuv_array_pixel_shader
            } else {
                &self.ayuv_pixel_shader
            }
        } else if view.input_format == DXGI_FORMAT_Y410 {
            if desc.ArraySize > 1 {
                &self.y410_array_pixel_shader
            } else {
                &self.y410_pixel_shader
            }
        } else if yuv && desc.ArraySize > 1 {
            &self.yuv_array_pixel_shader
        } else if yuv {
            &self.yuv_pixel_shader
        } else {
            &self.rgba_pixel_shader
        };
        let color_buffer = if yuv || view.color.hdr_peak_nits.is_some() {
            let bit_depth = if matches!(view.input_format, DXGI_FORMAT_P010 | DXGI_FORMAT_Y410) {
                10
            } else {
                8
            };
            let target_texture: ID3D11Texture2D = unsafe { render_target.GetResource() }?.cast()?;
            let mut target_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                target_texture.GetDesc(&mut target_desc);
            }
            let hdr_output = target_desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
            if self
                .color_buffer
                .as_ref()
                .is_none_or(|(color, depth, output, _)| {
                    *color != view.color || *depth != bit_depth || *output != hdr_output
                })
            {
                let buffer = create_video_constant_buffer(
                    device,
                    VideoColorTransform {
                        rows: view.color.transform(bit_depth),
                        hdr: [
                            view.color
                                .hdr_peak_nits
                                .map_or(0.0, |_| if hdr_output { 1.0 } else { 2.0 }),
                            view.color.hdr_peak_nits.unwrap_or(1000) as f32,
                            0.0,
                            0.0,
                        ],
                    },
                )?;
                tracing::info!(color = ?view.color, bit_depth, "updated video YUV color transform");
                self.color_buffer = Some((view.color, bit_depth, hdr_output, buffer));
            }
            self.color_buffer
                .as_ref()
                .map(|(_, _, _, buffer)| buffer.clone())
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
        let texture_key = texture.as_raw() as usize;
        if let Some(index) = self.views.iter().position(|cached| {
            cached.texture_key == texture_key
                && cached.array_slice == view.array_slice
                && cached.format == view.input_format
        }) {
            let cached = self.views.remove(index).expect("checked SRV cache index");
            let result = (cached.plane_0.clone(), cached.plane_1.clone());
            self.views.push_back(cached);
            return Ok(result);
        }
        let (y_format, uv_format) = match view.input_format {
            DXGI_FORMAT_NV12 => (DXGI_FORMAT_R8_UNORM, Some(DXGI_FORMAT_R8G8_UNORM)),
            DXGI_FORMAT_P010 => (DXGI_FORMAT_R16_UNORM, Some(DXGI_FORMAT_R16G16_UNORM)),
            DXGI_FORMAT_AYUV => (DXGI_FORMAT_R8G8B8A8_UNORM, None),
            DXGI_FORMAT_Y410 => (DXGI_FORMAT_R10G10B10A2_UNORM, None),
            DXGI_FORMAT_R8G8B8A8_UNORM => (DXGI_FORMAT_R8G8B8A8_UNORM, None),
            DXGI_FORMAT_R16G16B16A16_FLOAT => (DXGI_FORMAT_R16G16B16A16_FLOAT, None),
            format => bail!("unsupported D3D11 shader input format {format:?}"),
        };
        let plane_0 =
            create_video_shader_resource_view(device, texture, y_format, view.array_slice)?;
        let plane_1 = uv_format
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

    fn for_frame(
        hwnd: isize,
        size: PhysicalSize<u32>,
        frame: &DecodedVideoFrame,
        cpu_device: &Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>>,
    ) -> Result<Self> {
        let (device, context) = match &frame.surface {
            RenderSurface::D3D11(surface) if surface.shared_handle().is_none() => {
                (surface.device().clone(), surface.context().clone())
            }
            RenderSurface::D3D11(surface) => surface.create_renderer_device()?,
            RenderSurface::CpuRgba8(_) => {
                let mut cached = mutex_lock(cpu_device);
                if cached
                    .as_ref()
                    .is_some_and(|(device, _)| unsafe { device.GetDeviceRemovedReason().is_err() })
                {
                    cached.take();
                }
                if cached.is_none() {
                    *cached = Some(
                        crate::decoder::windows_surface::D3D11SurfaceWriter::new()?
                            .create_renderer_device()?,
                    );
                }
                cached.as_ref().expect("CPU presentation device").clone()
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
            hwnd,
            hdr_output: false,
            input_hdr: false,
            hdr_output_unavailable: false,
            output_monitor: 0,
            output_monitor_hdr: false,
            output_hdr_checked: None,
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
            captures: plugin_video::Captures::default(),
            plugin_state: None,
            effects: None,
            effect_revision: 0,
            effects_cache_revision: 0,
            nominal_fps: 60.0,
            effect_input: None,
            effect_output: None,
            effect_generated: false,
            drew: false,
            submission_wait: Duration::ZERO,
            analysis_at: Instant::now(),
            analysis_new: false,
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
            self.active_input_sync = crate::decoder::windows_surface::acquire_owned_texture_sync(
                surface.texture(),
                OFFICIAL_TEXTURE_SYNC_TIMEOUT_MS,
            )?;
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
        let hdr = view.color.hdr_peak_nits.is_some();
        self.ensure_output_color(hdr)?;
        if self.input_hdr != hdr {
            // Effect/history surfaces carry different transfer functions across
            // SDR/HDR. Never present an old gamma frame as linear HDR, or vice versa.
            self.effects = None;
            self.effect_revision = u64::MAX;
            self.effects_cache_revision = 0;
            self.captures = plugin_video::Captures::default();
            self.input_hdr = hdr;
        }
        let state = self.plugin_state.clone();
        if let Some(state) = &state {
            let graph = state.graph.try_lock().ok().map(|g| g.clone());
            if let Some(graph) = graph
                && graph.revision != self.effect_revision
            {
                self.effect_revision = graph.revision;
                let candidate = if graph.nodes.is_empty() {
                    Ok(None)
                } else {
                    plugin_video::Engine::new(&self.device, &graph, self.nominal_fps).map(Some)
                };
                match candidate {
                    Ok(engine) => {
                        self.effects = engine;
                        self.effects_cache_revision = 0;
                        self.video_renderer.reset_input_cache();
                        state.video_failed.store(0, Ordering::Release);
                        state
                            .applied_revision
                            .store(graph.revision, Ordering::Release);
                    }
                    Err(error) => {
                        *mutex_lock(&state.error) = Some(format!("GPU节点准备失败：{error}"));
                        state
                            .failed_revision
                            .store(graph.revision, Ordering::Release);
                    }
                }
            }
        }
        let taps = state
            .as_ref()
            .and_then(|s| {
                s.taps.try_lock().ok().map(|t| {
                    t.iter()
                        .filter(|t| {
                            t.revision == s.applied_revision.load(Ordering::Acquire)
                                && (t.source == 0
                                    || s.video_failed.load(Ordering::Acquire) != t.revision)
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        if self.analysis_new {
            self.captures.feed(
                0,
                &self.device,
                &self.context,
                texture,
                view,
                &taps,
                self.analysis_at,
                true,
            );
        }
        if let Some(engine) = self.effects.as_mut() {
            let result = if self.analysis_new {
                engine.process(
                    &self.device,
                    &self.context,
                    texture,
                    view,
                    self.analysis_at,
                    self.effect_input,
                    &mut self.captures,
                    &taps,
                )
            } else {
                Ok(())
            };
            if let Err(error) = result {
                if let Some(state) = &state {
                    state.fail(format!("视频链已停止：{error}"));
                }
                self.effects = None;
                self.video_renderer.reset_input_cache();
            } else {
                return self.draw_effect_output(view.color, !self.analysis_new);
            }
        }
        self.submission_wait = self.begin_frame()?;
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
        )?;
        self.drew = true;
        self.effect_output = self.effect_input;
        Ok(())
    }
    fn draw_effect_output(&mut self, color: RenderColor, redraw: bool) -> Result<()> {
        let Some(engine) = self.effects.as_mut() else {
            return Ok(());
        };
        let Some(state) = &self.plugin_state else {
            return Ok(());
        };
        if engine.cache_revision != self.effects_cache_revision {
            self.video_renderer.reset_input_cache();
            self.effects_cache_revision = engine.cache_revision;
        }
        let due = engine.take_due(&state.skipped);
        let fresh = due.is_some();
        let picture = due.or_else(|| if redraw { engine.last() } else { None });
        if let Some(picture) = picture {
            self.submission_wait = self.begin_frame()?;
            self.video_renderer.draw(
                &self.device,
                &self.context,
                self.render_target
                    .as_ref()
                    .context("effect presentation target")?,
                &picture.target.texture,
                plugin_video::view(&picture.target, color),
                self.size,
                self.content_top,
            )?;
            self.drew = true;
            if fresh {
                self.effect_output = picture.metadata;
                self.effect_generated = picture.generated;
            }
        }
        Ok(())
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
        self.resize_backbuffer(
            size,
            if self.hdr_output {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
        )?;
        if self.hdr_output {
            unsafe {
                self.swap_chain
                    .cast::<IDXGISwapChain3>()?
                    .SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)
            }
            .context("preserve scRGB color space after resizing")?;
        }
        Ok(())
    }

    fn ensure_output_color(&mut self, hdr_source: bool) -> Result<()> {
        if hdr_source {
            let monitor =
                unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) }.0 as isize;
            if monitor != self.output_monitor
                || self
                    .output_hdr_checked
                    .is_none_or(|checked| checked.elapsed() >= Duration::from_secs(1))
            {
                // Capability polling is independent of decoding; also refresh
                // immediately when the window moves to a different monitor.
                let enabled = crate::display_hdr::monitor_is_hdr(monitor);
                if monitor != self.output_monitor || enabled != self.output_monitor_hdr {
                    self.hdr_output_unavailable = false;
                }
                self.output_monitor = monitor;
                self.output_monitor_hdr = enabled;
                self.output_hdr_checked = Some(Instant::now());
            }
        }
        let requested = hdr_source && self.output_monitor_hdr && !self.hdr_output_unavailable;
        if requested == self.hdr_output {
            return Ok(());
        }
        let chain = self.swap_chain.cast::<IDXGISwapChain3>().ok();
        if requested {
            if chain.is_none() {
                self.hdr_output_unavailable = true;
                return Ok(());
            }
            let result = self
                .resize_backbuffer(self.size, DXGI_FORMAT_R16G16B16A16_FLOAT)
                .and_then(|_| {
                    let chain = chain.as_ref().unwrap();
                    let flags = unsafe {
                        chain.CheckColorSpaceSupport(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)
                    }?;
                    anyhow::ensure!(
                        flags & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 != 0,
                        "swap chain does not support scRGB presentation"
                    );
                    unsafe { chain.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709) }
                        .context("select scRGB HDR color space")
                });
            if let Err(error) = result {
                if is_device_lost(&error) {
                    return Err(error);
                }
                self.hdr_output_unavailable = true;
                self.resize_backbuffer(self.size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
                if let Some(chain) = &chain {
                    unsafe { chain.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709) }?;
                }
                tracing::warn!(%error,"HDR swap chain unavailable; using SDR tone mapping");
                return Ok(());
            }
        } else {
            self.resize_backbuffer(self.size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            if let Some(chain) = &chain {
                unsafe { chain.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709) }
                    .context("restore SDR color space")?;
            }
        }
        self.hdr_output = requested;
        tracing::info!(
            hdr_output = self.hdr_output,
            hdr_source,
            "updated native display color output"
        );
        Ok(())
    }

    fn resize_backbuffer(&mut self, size: PhysicalSize<u32>, format: DXGI_FORMAT) -> Result<()> {
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
                format,
                self.swap_chain_flags,
            )
        }
        .context("resize D3D11 video swap chain")?;
        let (backbuffer, target) = create_backbuffer(&self.device, &self.swap_chain)?;
        self.backbuffer = Some(backbuffer);
        self.render_target = Some(target);
        if let Some(target) = &self.render_target {
            unsafe {
                self.context
                    .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
            }
        }
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

pub(super) fn fit_rect(
    video_width: u32,
    video_height: u32,
    output_width: u32,
    output_height: u32,
) -> RECT {
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
