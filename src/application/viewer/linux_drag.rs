//! One viewer drop target, bound to its current video geometry and peer, as
//! the Windows viewer's OLE target is.
//!
//! winit takes the XDND drop on this window and reports the files, but not
//! where they hover: the dragging application holds the pointer, so no motion
//! reaches the window either. While files hover, the pointer is read from the
//! X server instead.
use crate::{
    features::{
        clipboard::drag::{Submission, SubmissionState},
        drag_drop::{Stage, Ticket, controller::Controller},
        stream_control::StreamControlHandle,
    },
    protocol::drag_drop::{COPY, Point},
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use winit::{dpi::PhysicalPosition, event::WindowEvent, window::Window};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;

pub(super) struct WindowDrag {
    owner: u64,
    controller: Controller,
    control: StreamControlHandle,
    /// Files over the window, and whether the gesture has been offered yet.
    paths: Vec<PathBuf>,
    hovering: bool,
    entered: bool,
    /// Files dropped since the last tick; winit reports them one by one.
    dropped: Vec<PathBuf>,
    native: bool,
    official: Vec<(Arc<Submission>, std::time::Instant)>,
    preview: Option<Arc<Ticket>>,
    transfers: Vec<Arc<Ticket>>,
    error: Option<String>,
    pointer: Option<(x11rb::rust_connection::RustConnection, u32)>,
}
impl WindowDrag {
    pub fn new(owner: u64, control: &StreamControlHandle) -> Self {
        Self {
            owner,
            controller: control.drag_drop().clone(),
            control: control.clone(),
            paths: Vec::new(),
            hovering: false,
            entered: false,
            dropped: Vec::new(),
            native: false,
            official: Vec::new(),
            preview: None,
            transfers: Vec::new(),
            error: None,
            pointer: None,
        }
    }
    /// Take the window's drop events; true for one of them.
    pub fn event(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::HoveredFile(path) => {
                if !self.hovering {
                    self.leave();
                    self.hovering = true;
                }
                self.paths.push(path.clone());
                true
            }
            WindowEvent::DroppedFile(path) => {
                self.dropped.push(path.clone());
                true
            }
            WindowEvent::HoveredFileCancelled => {
                self.leave();
                true
            }
            _ => false,
        }
    }
    /// Where the pointer is in the window, while something else holds it.
    fn local_pointer(&mut self, window: &Window) -> Option<PhysicalPosition<f64>> {
        if self.pointer.is_none() {
            let (connection, screen) = x11rb::connect(None).ok()?;
            let root = connection.setup().roots[screen].root;
            self.pointer = Some((connection, root));
        }
        let (connection, root) = self.pointer.as_ref()?;
        let reply = connection.query_pointer(*root).ok()?.reply().ok()?;
        let origin = window.inner_position().ok()?;
        Some(PhysicalPosition::new(
            f64::from(i32::from(reply.root_x) - origin.x),
            f64::from(i32::from(reply.root_y) - origin.y),
        ))
    }
    /// Follow the hovering files. `point` maps a window position onto the
    /// remote screen, or gives none off the video; `available` is false while
    /// a menu or anything else covers the video.
    pub fn tick(
        &mut self,
        window: &Window,
        ctx: &egui::Context,
        available: bool,
        point: impl Fn(PhysicalPosition<f64>) -> Option<Point>,
    ) {
        if !self.dropped.is_empty() {
            let position = self.local_pointer(window);
            let target = position.and_then(&point).filter(|_| available);
            let paths = std::mem::take(&mut self.dropped);
            if self.paths.is_empty() {
                self.paths = paths;
            }
            self.drop_at(target);
            self.hovering = false;
            self.entered = false;
            return;
        }
        if !self.hovering {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(30));
        let target = self
            .local_pointer(window)
            .and_then(&point)
            .filter(|_| available);
        if !self.entered {
            self.entered = true;
            self.native = self.controller.is_native();
            self.error = None;
        }
        self.over(target);
    }
    pub fn show(&mut self, ctx: &egui::Context) {
        let reverse = self.controller.take_reverse(self.owner);
        self.transfers.extend(reverse);
        self.transfers.retain(|ticket| {
            let snapshot = ticket.snapshot();
            !matches!(snapshot.stage, Stage::Complete | Stage::Cancelled)
        });
        self.official.retain(|(ticket, at)| {
            !matches!(ticket.state(), SubmissionState::HandedOff)
                || at.elapsed() < Duration::from_secs(3)
        });
        if self.preview.is_none()
            && self.transfers.is_empty()
            && self.official.is_empty()
            && self.error.is_none()
        {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::Area::new(egui::Id::new("native-file-drag"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0., -18.])
            .order(egui::Order::Foreground)
            // A passive drag hint must not become a forbidden drop target and
            // repeatedly cancel/restart the preview underneath itself.
            .interactable(self.paths.is_empty())
            .movable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(360.);
                    if let Some(ticket) = &self.preview {
                        let snapshot = ticket.snapshot();
                        ui.label(match snapshot.stage {
                            Stage::Preparing => "正在准备文件…",
                            Stage::Dragging if snapshot.effect == COPY => "松开以复制到此处",
                            Stage::Dragging => "此处不能接收文件",
                            _ => "拖放已停止",
                        });
                    }
                    let mut dismiss_official = Vec::new();
                    for (index, (ticket, _)) in self.official.iter().enumerate() {
                        ui.horizontal(|ui| match ticket.state() {
                            SubmissionState::Preparing => {
                                ui.label("正在准备文件…");
                            }
                            SubmissionState::Waiting => {
                                ui.label("等待远端接收…");
                            }
                            SubmissionState::HandedOff => {
                                ui.label("已交给远端处理");
                            }
                            SubmissionState::Complete { .. } => {
                                ui.label("文件已接收");
                            }
                            SubmissionState::Failed(error) => {
                                ui.label(error);
                                if ui.button("关闭").clicked() {
                                    dismiss_official.push(index);
                                }
                            }
                        });
                    }
                    for index in dismiss_official.into_iter().rev() {
                        self.official.remove(index);
                    }
                    let mut dismiss = Vec::new();
                    for (index, ticket) in self.transfers.iter().enumerate() {
                        let snapshot = ticket.snapshot();
                        ui.horizontal(|ui| {
                            if snapshot.stage == Stage::Failed {
                                ui.label(snapshot.error.as_deref().unwrap_or("文件拖放失败"));
                                if ui.button("关闭").clicked() {
                                    dismiss.push(index);
                                }
                            } else {
                                ui.label(match snapshot.stage {
                                    Stage::Preparing => "正在准备文件…".to_owned(),
                                    Stage::Dragging if snapshot.effect == COPY => {
                                        "松开以复制到此处".to_owned()
                                    }
                                    Stage::Dragging => "此处不能接收文件".to_owned(),
                                    Stage::Submitted => "等待目标接收…".to_owned(),
                                    _ => format!(
                                        "正在复制文件 · {:.1} MiB",
                                        snapshot.bytes_read as f64 / 1048576.
                                    ),
                                });
                                if ui.button("取消").clicked() {
                                    ticket.cancel();
                                }
                            }
                        });
                    }
                    for index in dismiss.into_iter().rev() {
                        self.transfers.remove(index);
                    }
                    if let Some(error) = self.error.clone() {
                        ui.horizontal(|ui| {
                            ui.label(error);
                            if ui.button("关闭").clicked() {
                                self.error = None;
                            }
                        });
                    }
                });
            });
    }
    fn cancel_preview(&mut self) {
        if let Some(ticket) = self.preview.take() {
            ticket.cancel();
        }
    }
    fn over(&mut self, point: Option<Point>) {
        // No legacy operation is submitted until the drop. A late native Hello
        // can still select the native protocol before this gesture sends anything.
        self.native |= self.controller.is_native();
        if !self.native {
            return;
        }
        let Some(point) = point.filter(|_| self.controller.available()) else {
            self.cancel_preview();
            return;
        };
        if self.preview.is_none() && !self.paths.is_empty() {
            match self.controller.begin(self.paths.clone(), point) {
                Ok(ticket) => {
                    // End any ordinary input ownership before the remote drag
                    // source takes over. This does not change saved control mode.
                    self.control.mouse().pause_owner(self.owner);
                    self.preview = Some(ticket);
                }
                Err(error) => {
                    self.error = Some(error.to_string());
                    return;
                }
            }
        }
        if let Some(ticket) = &self.preview {
            ticket.position(point);
        }
    }
    fn leave(&mut self) {
        self.cancel_preview();
        self.paths.clear();
        self.hovering = false;
        self.entered = false;
    }
    fn drop_at(&mut self, point: Option<Point>) {
        self.native |= self.controller.is_native();
        let Some(point) = point else {
            self.leave();
            return;
        };
        if !self.native {
            let paths = std::mem::take(&mut self.paths);
            match self.control.drop_files(paths, point) {
                Ok(ticket) => self.official.push((ticket, std::time::Instant::now())),
                Err(error) => self.error = Some(error.to_string()),
            }
            return;
        }
        self.paths.clear();
        let Some(ticket) = self.preview.take() else {
            return;
        };
        match ticket.commit(point) {
            Ok(()) => self.transfers.push(ticket),
            Err(error) => {
                ticket.cancel();
                self.error = Some(error.to_string());
            }
        }
    }
}
impl Drop for WindowDrag {
    fn drop(&mut self) {
        self.cancel_preview();
        for ticket in &self.transfers {
            ticket.cancel();
        }
        for (ticket, _) in &self.official {
            if !matches!(ticket.state(), SubmissionState::HandedOff) {
                ticket.cancel();
            }
        }
    }
}
