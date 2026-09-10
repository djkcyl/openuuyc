use super::*;
use crate::stream_control::RemoteScreen;
use crate::viewer::screens::ScreenPlayback;
use std::collections::HashMap;

#[derive(Clone, Copy)]
enum TabCommand {
    Select(i32),
    Detach(i32, PhysicalPosition<i32>),
}

#[derive(Default)]
pub(super) struct ScreenTabBar {
    pub tabs: Vec<(i32, String)>,
    selected: i32,
    pending: Option<i32>,
    error: Option<String>,
    command: Option<TabCommand>,
    software_only: bool,
}

impl ScreenTabBar {
    pub fn draw(&mut self, ui: &mut egui::Ui, window: &Window) {
        let width = ui.available_width();
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(ui.max_rect())
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        row.spacing_mut().item_spacing.x = 3.0;
        egui::ScrollArea::horizontal()
            .id_salt("screen-tabs")
            .auto_shrink([false, false])
            .max_width(width)
            // A drag either detaches a tab or moves the caption background.
            // Overflow stays accessible via the wheel and scroll bar.
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .show(&mut row, |ui| {
                ui.horizontal_centered(|ui| {
                    for (id, name) in &self.tabs {
                        let selected = self.selected == *id;
                        let label = if self.pending == Some(*id) {
                            format!("{name} …")
                        } else {
                            name.clone()
                        };
                        let response = ui
                            .add_sized(
                                [140.0, 28.0],
                                egui::Button::new(egui::RichText::new(label).size(12.0))
                                    .selected(selected)
                                    .truncate()
                                    .sense(if self.software_only {
                                        egui::Sense::click()
                                    } else {
                                        egui::Sense::click_and_drag()
                                    }),
                            )
                            .on_hover_text(name);
                        if response.clicked() {
                            self.command = Some(TabCommand::Select(*id));
                        }
                        response.context_menu(|ui| {
                            if ui
                                .add_enabled(
                                    !self.software_only,
                                    egui::Button::new("在独立窗口打开"),
                                )
                                .on_disabled_hover_text(
                                    "软解仅允许一个播放窗口，请使用标签切换屏幕",
                                )
                                .clicked()
                            {
                                if let Ok(origin) = window.outer_position() {
                                    self.command = Some(TabCommand::Detach(
                                        *id,
                                        PhysicalPosition::new(origin.x + 80, origin.y + 100),
                                    ));
                                }
                                ui.close();
                            }
                        });
                        if response.drag_started()
                            && let Ok(handle) = window.window_handle()
                            && let RawWindowHandle::Win32(handle) = handle.as_raw()
                        {
                            unsafe {
                                SetCapture(HWND(handle.hwnd.get() as _));
                            }
                        }
                        if response.drag_stopped() {
                            let mut point = POINT::default();
                            if unsafe { GetCursorPos(&mut point) }.is_ok() {
                                self.command = Some(TabCommand::Detach(
                                    *id,
                                    PhysicalPosition::new(point.x, point.y),
                                ));
                            }
                            let _ = unsafe { ReleaseCapture() };
                        }
                    }
                });
            });
        if let Some(error) = self.error.clone() {
            egui::Modal::new(ui.id().with("screen-error")).show(ui.ctx(), |ui| {
                ui.set_width(300.0);
                ui.heading("无法显示屏幕");
                ui.add_space(12.0);
                ui.label(error);
                ui.add_space(16.0);
                if ui.button("确定").clicked() {
                    self.error = None;
                }
            });
        }
    }
}

struct PendingScreen {
    id: i32,
    task: tokio::task::JoinHandle<()>,
    result: tokio::sync::oneshot::Receiver<Result<Arc<NativeViewerSession>>>,
    connecting: Option<WindowsConnectionApp>,
}
impl Drop for PendingScreen {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ScreenWindow {
    // Drop renderers before destroying their parent HWND.
    app: Option<ThreadedWindowsApp>,
    pending: Option<PendingScreen>,
    tabs: Vec<i32>,
    selected: Option<i32>,
    close: bool,
    last_frame: Option<Instant>,
    repaint: Option<Instant>,
    interval: Duration,
    window: Window,
}

pub(super) struct ScreenWindows {
    windows: HashMap<WindowId, ScreenWindow>,
    sessions: HashMap<i32, Arc<NativeViewerSession>>,
    factory: ScreenPlayback,
    shutdown: Arc<AtomicBool>,
    preferences: ViewerPreferences,
    proxy: EventLoopProxy<UiRepaintEvent>,
    generation: u64,
    next_refresh: Instant,
    catalog: Vec<RemoteScreen>,
}

impl ScreenWindows {
    pub fn new(
        window: Window,
        mut session: NativeViewerSession,
        connecting: WindowsConnectionApp,
        preferences: ViewerPreferences,
        proxy: EventLoopProxy<UiRepaintEvent>,
        generation: u64,
    ) -> Result<Self> {
        let factory = session
            .take_screen_playback()
            .context("screen playback owner unavailable")?;
        let shutdown = Arc::clone(&session.shutdown);
        let screen_id = session.screen_id;
        let session = Arc::new(session);
        factory.register(Arc::clone(&session));
        let app = ThreadedWindowsApp::from_session(
            &window,
            Arc::clone(&session),
            connecting,
            preferences,
        )?;
        let catalog = factory.screens();
        let tabs = catalog.iter().map(|screen| screen.id).collect();
        let slot = ScreenWindow {
            app: Some(app),
            pending: None,
            tabs,
            selected: Some(screen_id),
            close: false,
            last_frame: None,
            repaint: Some(Instant::now()),
            interval: ui_frame_interval(&window),
            window,
        };
        let mut group = Self {
            windows: HashMap::from([(slot.window.id(), slot)]),
            sessions: HashMap::from([(session.track_index, session)]),
            factory,
            shutdown,
            preferences,
            proxy,
            generation,
            next_refresh: Instant::now(),
            catalog,
        };
        group.sync_bars();
        Ok(group)
    }

    pub fn take_window(&mut self) -> Option<Window> {
        let id = self.windows.keys().next().copied()?;
        let mut slot = self.windows.remove(&id)?;
        slot.app.take();
        slot.pending.take();
        Some(slot.window)
    }

    pub fn window_event(&mut self, id: WindowId, event: &WindowEvent) {
        let Some(slot) = self.windows.get_mut(&id) else {
            return;
        };
        if *event == WindowEvent::CloseRequested {
            slot.close = true;
            return;
        }
        if matches!(
            event,
            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            slot.interval = ui_frame_interval(&slot.window);
        }
        if *event == WindowEvent::Focused(true)
            && let Some(id) = slot.selected
        {
            self.factory.focus(id);
        }
        if *event == WindowEvent::RedrawRequested {
            if slot.window.is_minimized().unwrap_or(false) {
                slot.repaint = None;
                return;
            }
            if let Some(earliest) = slot.last_frame.map(|at| at + slot.interval)
                && Instant::now() < earliest
            {
                slot.repaint = Some(earliest);
                return;
            }
            slot.repaint = None;
            slot.last_frame = Some(Instant::now());
        }
        if let Some(app) = slot.app.as_mut() {
            if let Err(error) = app.on_window_event(&slot.window, event) {
                tracing::error!(%error, "screen window presentation failed");
                app.screen_tabs.error = Some(format!("{error:#}"));
            }
        } else if let Some(app) = slot
            .pending
            .as_mut()
            .and_then(|pending| pending.connecting.as_mut())
        {
            if let Err(error) = app.on_window_event(&slot.window, event) {
                tracing::warn!(%error, "draw opening screen window failed");
                slot.close = true;
            }
            slot.close |= app.close_requested;
        }
    }

    pub fn repaint(&mut self, event: UiRepaintEvent) {
        if event.generation != self.generation {
            return;
        }
        let Some(slot) = self.windows.get_mut(&event.window) else {
            return;
        };
        let context = slot.app.as_ref().map(|app| &app.egui_context).or_else(|| {
            slot.pending
                .as_ref()
                .and_then(|p| p.connecting.as_ref())
                .map(|app| &app.egui_context)
        });
        if let Some(context) = context {
            let pass = context.cumulative_pass_nr();
            if pass == event.pass || pass == event.pass.saturating_add(1) {
                let when = slot
                    .last_frame
                    .map_or(event.when, |at| event.when.max(at + slot.interval));
                slot.repaint = Some(slot.repaint.map_or(when, |old| old.min(when)));
            }
        }
    }

    fn begin_screen(&mut self, window: WindowId, screen_id: i32) -> Result<()> {
        let screen = self
            .catalog
            .iter()
            .find(|screen| screen.id == screen_id)
            .context("显示器已断开")?;
        let slot = self.windows.get_mut(&window).context("窗口已关闭")?;
        if slot.selected == Some(screen_id) && slot.app.is_some() {
            slot.pending.take();
            if let Some(app) = slot.app.as_mut() {
                app.screen_tabs.pending = None;
            }
            return Ok(());
        }
        if slot
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == screen_id)
        {
            return Ok(());
        }
        let cancelled_pending = slot.pending.take().map(|pending| pending.id);
        let (progress, receiver) = std_mpsc::channel();
        let _ = progress.send(ConnectionProgress::working(
            10,
            "正在打开显示器",
            screen_label(screen, &self.catalog),
        ));
        let (connecting, display) = if slot.app.is_some() {
            (
                None,
                ViewerDisplayHandle {
                    surface_writer: Some(
                        crate::decoder::windows_surface::D3D11SurfaceWriter::new()?
                    ),
                },
            )
        } else {
            let (connecting, display) = WindowsConnectionApp::new(
                &slot.window,
                "显示器".into(),
                receiver,
                self.proxy.clone(),
                self.generation,
            )?;
            (Some(connecting), display)
        };
        let (task, result) =
            self.factory
                .open(screen_id, display, slot.selected, cancelled_pending);
        slot.pending = Some(PendingScreen {
            id: screen_id,
            task,
            result,
            connecting,
        });
        if let Some(app) = slot.app.as_mut() {
            app.screen_tabs.pending = Some(screen_id);
        }
        slot.window.request_redraw();
        Ok(())
    }

    fn close_window(&mut self, id: WindowId) {
        let Some(mut slot) = self.windows.remove(&id) else {
            return;
        };
        slot.pending.take();
        slot.app.take();
        if let Some(target) = self.windows.values_mut().next() {
            for screen in slot.tabs {
                if !target.tabs.contains(&screen) {
                    target.tabs.push(screen);
                }
            }
            target.window.request_redraw();
        }
    }

    fn detach_tab(
        &mut self,
        event_loop: &ActiveEventLoop,
        from: WindowId,
        screen: i32,
        at: PhysicalPosition<i32>,
    ) -> Result<()> {
        let source = self.windows.get(&from).context("原窗口已关闭")?;
        if !source.tabs.contains(&screen) {
            return Ok(());
        }
        if let Ok(origin) = source.window.inner_position()
            && at.x >= origin.x
            && at.x < origin.x + source.window.inner_size().width as i32
            && at.y >= origin.y
            && at.y < origin.y + title_bar_height_pixels(&source.window) as i32
        {
            return Ok(());
        }
        if source.tabs.len() == 1 {
            source
                .window
                .set_outer_position(PhysicalPosition::new(at.x - 100, at.y - 18));
            return Ok(());
        }
        if source
            .app
            .as_ref()
            .is_some_and(|app| app._session.is_software())
        {
            bail!("软解仅允许一个播放窗口，请使用顶部标签切换屏幕");
        }
        let target = {
            let window = event_loop.create_window(
                WindowAttributes::default()
                    .with_title(source.window.title())
                    .with_decorations(false)
                    .with_inner_size(source.window.inner_size())
                    .with_min_inner_size(LogicalSize::new(760.0, 520.0))
                    .with_position(PhysicalPosition::new(at.x - 100, at.y - 18)),
            )?;
            configure_dwm_window(&window);
            let id = window.id();
            self.windows.insert(
                id,
                ScreenWindow {
                    interval: ui_frame_interval(&window),
                    window,
                    app: None,
                    pending: None,
                    tabs: Vec::new(),
                    selected: None,
                    close: false,
                    last_frame: None,
                    repaint: Some(Instant::now()),
                },
            );
            id
        };
        self.windows
            .get_mut(&target)
            .expect("target exists")
            .tabs
            .push(screen);
        if let Err(error) = self.begin_screen(target, screen) {
            if let Some(slot) = self.windows.get_mut(&target) {
                slot.tabs.retain(|id| *id != screen);
            }
            if self
                .windows
                .get(&target)
                .is_some_and(|slot| slot.tabs.is_empty())
            {
                self.close_window(target);
            }
            return Err(error);
        }
        let source = self.windows.get_mut(&from).expect("source exists");
        source.tabs.retain(|id| *id != screen);
        if source.selected == Some(screen) {
            source.app.take();
            source.selected = None;
        }
        if source
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == screen)
        {
            source.pending.take();
        }
        if let Some(next) = self
            .windows
            .get(&from)
            .filter(|slot| slot.selected.is_none())
            .and_then(|slot| slot.tabs.first())
            .copied()
        {
            self.begin_screen(from, next)?;
        }
        if self
            .windows
            .get(&from)
            .is_some_and(|slot| slot.tabs.is_empty())
        {
            self.close_window(from);
        }
        Ok(())
    }

    fn sync_bars(&mut self) {
        for slot in self.windows.values_mut() {
            if let Some(app) = slot.app.as_mut() {
                app.screen_tabs.software_only = app._session.is_software();
                app.screen_tabs.tabs = slot
                    .tabs
                    .iter()
                    .filter_map(|id| self.catalog.iter().find(|s| s.id == *id))
                    .map(|screen| (screen.id, screen_label(screen, &self.catalog)))
                    .collect();
                app.screen_tabs.selected = slot.selected.unwrap_or(-1);
                app.screen_tabs.pending = slot.pending.as_ref().map(|pending| pending.id);
            }
        }
        let mut visible: Vec<_> = self
            .windows
            .values()
            .flat_map(|slot| {
                slot.selected
                    .filter(|_| {
                        !(slot.pending.is_some()
                            && slot
                                .app
                                .as_ref()
                                .is_some_and(|app| app._session.is_software()))
                    })
                    .into_iter()
                    .chain(slot.pending.as_ref().map(|pending| pending.id))
            })
            .collect();
        visible.sort_unstable();
        visible.dedup();
        self.factory.set_visible(visible);
    }

    pub fn update(&mut self, event_loop: &ActiveEventLoop) {
        if self.shutdown.load(Ordering::Acquire) || self.windows.is_empty() {
            event_loop.exit();
            return;
        }
        let ids: Vec<_> = self.windows.keys().copied().collect();
        for id in &ids {
            let slot = self.windows.get_mut(id).expect("window exists");
            if slot.app.as_ref().is_some_and(|app| app.close_requested) || slot.close {
                self.close_window(*id);
                continue;
            }
            let ready = slot
                .pending
                .as_mut()
                .and_then(|pending| match pending.result.try_recv() {
                    Ok(result) => Some(result),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        Some(Err(anyhow!("屏幕加载已取消")))
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
                });
            if let Some(result) = ready {
                let mut pending = slot.pending.take().expect("pending screen");
                match result {
                    Ok(session) => {
                        let connected = if let Some(app) = slot.app.as_mut() {
                            app.replace_session(&slot.window, Arc::clone(&session))
                        } else {
                            ThreadedWindowsApp::from_session(
                                &slot.window,
                                Arc::clone(&session),
                                pending.connecting.take().expect("screen UI"),
                                self.preferences,
                            )
                            .map(|app| {
                                slot.app = Some(app);
                            })
                        };
                        match connected {
                            Ok(()) => {
                                self.sessions.insert(session.track_index, session);
                                slot.selected = Some(pending.id);
                                self.factory.focus(pending.id);
                            }
                            Err(error) => {
                                tracing::error!(%error, "initialize screen presenter failed");
                                slot.close = true;
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(app) = slot.app.as_mut() {
                            app.screen_tabs.error = Some(format!("{error:#}"));
                        } else {
                            tracing::warn!(%error, "open detached screen failed");
                            slot.close = true;
                        }
                    }
                }
            }
        }
        let commands: Vec<_> = self
            .windows
            .iter_mut()
            .filter_map(|(id, slot)| {
                slot.app
                    .as_mut()
                    .and_then(|app| app.screen_tabs.command.take())
                    .map(|command| (*id, command))
            })
            .collect();
        for (window, command) in commands {
            let result = match command {
                TabCommand::Select(screen) => self.begin_screen(window, screen),
                TabCommand::Detach(screen, at) => self.detach_tab(event_loop, window, screen, at),
            };
            if let Err(error) = result
                && let Some(app) = self
                    .windows
                    .get_mut(&window)
                    .and_then(|slot| slot.app.as_mut())
            {
                app.screen_tabs.error = Some(format!("{error:#}"));
            }
        }
        let now = Instant::now();
        if now >= self.next_refresh {
            let catalog = self.factory.screens();
            if catalog != self.catalog {
                self.catalog = catalog;
                for slot in self.windows.values_mut() {
                    slot.tabs
                        .retain(|id| self.catalog.iter().any(|screen| screen.id == *id));
                }
                let assigned: Vec<_> = self
                    .windows
                    .values()
                    .flat_map(|slot| slot.tabs.iter().copied())
                    .collect();
                if let Some(slot) = self.windows.values_mut().next() {
                    slot.tabs.extend(
                        self.catalog
                            .iter()
                            .filter(|screen| !assigned.contains(&screen.id))
                            .map(|screen| screen.id),
                    );
                }
                let replacements: Vec<_> = self
                    .windows
                    .iter_mut()
                    .filter_map(|(id, slot)| {
                        if slot.pending.is_some() {
                            return None;
                        }
                        let selected = slot
                            .selected
                            .and_then(|id| self.catalog.iter().find(|screen| screen.id == id));
                        let target = match selected {
                            Some(screen)
                                if slot.app.as_ref().is_some_and(|app| {
                                    app._session.track_index == screen.video_track_index
                                }) =>
                            {
                                return None;
                            }
                            Some(screen) => Some(screen.id),
                            None => slot.tabs.first().copied(),
                        };
                        slot.selected = None;
                        target.map(|screen| (*id, screen))
                    })
                    .collect();
                for (window, screen) in replacements {
                    if let Err(error) = self.begin_screen(window, screen)
                        && let Some(app) = self
                            .windows
                            .get_mut(&window)
                            .and_then(|slot| slot.app.as_mut())
                    {
                        app.screen_tabs.error = Some(error.to_string());
                    }
                }
            }
            self.next_refresh = now + Duration::from_millis(250);
            for slot in self.windows.values() {
                slot.window.request_redraw();
            }
        }
        self.sync_bars();
        let mut wake = self.next_refresh;
        for slot in self.windows.values_mut() {
            if let Some(at) = slot.repaint {
                if at <= now {
                    slot.window.request_redraw();
                    slot.repaint = None;
                } else {
                    wake = wake.min(at);
                }
            }
        }
        if self.windows.is_empty() {
            event_loop.exit();
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
        }
    }
}

impl Drop for ScreenWindows {
    fn drop(&mut self) {
        for session in self.sessions.values() {
            session.close_handle().close();
        }
        self.windows.clear();
        self.sessions.clear();
        self.factory.close_decoders();
    }
}

fn screen_label(screen: &RemoteScreen, catalog: &[RemoteScreen]) -> String {
    let number = catalog.iter().position(|s| s.id == screen.id).unwrap_or(0) + 1;
    if screen.name.is_empty() {
        format!("显示屏 {number}")
    } else {
        format!("显示屏 {number}（{}）", screen.name)
    }
}
