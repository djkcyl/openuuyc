use super::*;
use crate::logging::live::{Reader, Request};
use std::{collections::VecDeque, time::Instant};

const MAX_LINES: usize = 5000;
const MAX_BYTES: usize = 2 * 1024 * 1024;

pub(super) struct LiveView {
    reader: Option<Reader>,
    waiting: bool,
    next_poll: Instant,
    lines: VecDeque<String>,
    bytes: usize,
    paused: bool,
    follow: bool,
    wrap: bool,
    wrapped: WrappedLogs,
    search: String,
    level: Level,
    clear: bool,
    discard_batch: bool,
    error: Option<String>,
}

impl Default for LiveView {
    fn default() -> Self {
        Self {
            reader: None,
            waiting: false,
            next_poll: Instant::now(),
            lines: VecDeque::new(),
            bytes: 0,
            paused: false,
            follow: true,
            wrap: true,
            wrapped: WrappedLogs::default(),
            search: String::new(),
            level: Level::Debug,
            clear: false,
            discard_batch: false,
            error: None,
        }
    }
}

fn line_level(line: &str) -> Option<Level> {
    // The formatter writes: pid=… <UTC timestamp> <LEVEL> <thread> <target>.
    match line.split_whitespace().nth(2)? {
        "ERROR" => Some(Level::Error),
        "WARN" => Some(Level::Warn),
        "INFO" => Some(Level::Info),
        "DEBUG" => Some(Level::Debug),
        "TRACE" => Some(Level::Trace),
        _ => None,
    }
}

impl LiveView {
    pub(super) fn show(&mut self, ui: &mut egui::Ui, snapshot: &logging::Snapshot) {
        if self.reader.is_none() && self.error.is_none() {
            match Reader::start() {
                Ok(reader) => self.reader = Some(reader),
                Err(e) => self.error = Some(format!("启动日志查看失败：{e}")),
            }
        }
        // Pausing holds the displayed content; at most one bounded batch is pending.
        if !self.paused
            && let Some(reader) = &self.reader
            && let Ok(batch) = reader.batches.try_recv()
        {
            self.waiting = false;
            if !self.discard_batch {
                self.error = batch.error;
                if !batch.lines.is_empty() {
                    self.wrapped.rows.clear();
                }
                for line in batch.lines {
                    self.bytes += line.len();
                    self.lines.push_back(line);
                }
                self.lines
                    .make_contiguous()
                    .sort_by_cached_key(|line| logging::live::timestamp(line));
                while self.lines.len() > MAX_LINES || self.bytes > MAX_BYTES {
                    self.bytes -= self.lines.pop_front().unwrap().len();
                }
            }
            self.discard_batch = false;
        }
        ui.horizontal(|ui| {
            if ui
                .button(if self.paused { "继续" } else { "暂停" })
                .clicked()
            {
                self.paused = !self.paused;
            }
            ui.checkbox(&mut self.follow, "自动滚动");
            ui.checkbox(&mut self.wrap, "自动换行");
            if ui.button("清空").on_hover_text("仅清空显示").clicked() {
                self.lines.clear();
                self.wrapped.rows.clear();
                self.bytes = 0;
                self.clear = true;
                self.discard_batch = self.waiting;
            }
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("live-log-level")
                .width(90.0)
                .selected_text(self.level.label())
                .show_ui(ui, |ui| {
                    for level in Level::ALL.into_iter().filter(|level| *level != Level::Off) {
                        ui.selectable_value(&mut self.level, level, level.label());
                    }
                });
            ui.add(
                singleline_input(&mut self.search)
                    .hint_text("搜索模块或内容")
                    .desired_width((ui.available_width() - 72.0).max(100.0)),
            );
        });
        let search = self.search.to_lowercase();
        let filtered: Vec<_> = self
            .lines
            .iter()
            .filter(|line| {
                (self.level == Level::Trace
                    || line_level(line).is_some_and(|actual| actual <= self.level))
                    && (search.is_empty() || line.to_lowercase().contains(&search))
            })
            .collect();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} 条", filtered.len()))
                    .small()
                    .color(MUTED),
            );
            if ui
                .add_enabled(!filtered.is_empty(), egui::Button::new("复制"))
                .clicked()
            {
                ui.ctx().copy_text(
                    filtered
                        .iter()
                        .map(|line| line.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        });
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(RED));
        }
        ui.separator();
        if self.wrap {
            self.wrapped
                .show(ui, &filtered, &self.search, self.level, self.follow);
        } else {
            let height = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::both()
                .id_salt("live-log-content")
                .auto_shrink([false, false])
                .stick_to_bottom(self.follow)
                .show_rows(ui, height, filtered.len(), |ui, range| {
                    for index in range {
                        let line = filtered[index];
                        let color = log_color(ui, line);
                        ui.add(
                            egui::Label::new(RichText::new(line.as_str()).monospace().color(color))
                                .extend(),
                        );
                    }
                });
        }
        if !self.paused
            && !self.waiting
            && Instant::now() >= self.next_poll
            && let Some(reader) = &self.reader
        {
            let request = Request {
                directory: snapshot.directory.clone(),
                session: snapshot.session.clone(),
                explicit_file: (!snapshot.managed).then(|| snapshot.file.clone()),
                clear: self.clear,
            };
            if reader.requests.try_send(request).is_ok() {
                self.waiting = true;
                self.clear = false;
                self.next_poll = Instant::now() + Duration::from_millis(250);
            }
        }
        if !self.paused {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
}

fn log_color(ui: &egui::Ui, line: &str) -> egui::Color32 {
    match line_level(line) {
        Some(Level::Error) => RED,
        Some(Level::Warn) => AMBER,
        Some(Level::Debug | Level::Trace) => MUTED,
        _ => ui.visuals().text_color(),
    }
}

#[derive(Default)]
struct WrappedLogs {
    rows: Vec<(std::sync::Arc<egui::Galley>, f32)>,
    width: f32,
    scale: f32,
    font: Option<egui::FontId>,
    search: String,
    level: Option<Level>,
    height: f32,
}

impl WrappedLogs {
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        lines: &[&String],
        search: &str,
        level: Level,
        follow: bool,
    ) {
        egui::ScrollArea::vertical()
            .id_salt("live-log-wrapped")
            .auto_shrink([false, false])
            .stick_to_bottom(follow)
            .show_viewport(ui, |ui, viewport| {
                let width = ui.available_width().max(1.0);
                let font = egui::TextStyle::Monospace.resolve(ui.style());
                let scale = ui.ctx().pixels_per_point();
                if self.rows.len() != lines.len()
                    || self.width != width
                    || self.scale != scale
                    || self.font.as_ref() != Some(&font)
                    || self.search != search
                    || self.level != Some(level)
                {
                    self.rows.clear();
                    self.height = 0.0;
                    self.width = width;
                    self.scale = scale;
                    self.font = Some(font.clone());
                    self.search = search.to_owned();
                    self.level = Some(level);
                    for line in lines {
                        let galley = ui.fonts_mut(|fonts| {
                            fonts.layout(
                                (*line).clone(),
                                font.clone(),
                                egui::Color32::PLACEHOLDER,
                                width,
                            )
                        });
                        let height = galley.size().y;
                        self.rows.push((galley, self.height));
                        self.height += height + ui.spacing().item_spacing.y;
                    }
                }
                // Keep wrapping bounded to the visible viewport, including very long records.
                ui.set_min_size(vec2(width, self.height));
                let origin = ui.min_rect().min;
                let start = self
                    .rows
                    .partition_point(|(galley, y)| y + galley.size().y < viewport.min.y);
                for (index, (galley, y)) in self.rows.iter().enumerate().skip(start) {
                    if *y > viewport.max.y {
                        break;
                    }
                    ui.painter().galley(
                        origin + vec2(0.0, *y),
                        galley.clone(),
                        log_color(ui, lines[index]),
                    );
                }
            });
    }
}
