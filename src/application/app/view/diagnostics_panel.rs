use super::*;
use crate::features::host::format::{Backend, Codec};
use crate::ui::controls::{diagnostics_empty, diagnostics_row, diagnostics_table};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Device,
    Encoding,
    Decoding,
    Sessions,
}
#[derive(Default)]
pub(super) struct ViewState {
    tab: Tab,
    encoder: Option<(u64, Backend)>,
    decoder: usize,
}

impl DeviceCenterApp {
    pub(super) fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("本机诊断").size(theme::TITLE).strong());
        ui.add_space(18.0);
        crate::ui::controls::page_scroll("center-diagnostics-scroll").show(ui, |ui| {
            self.diagnostics_panel(ui);
        });
    }

    fn diagnostics_panel(&mut self, ui: &mut egui::Ui) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = vec2(theme::DIAGNOSTICS_GAP, 4.0);
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Device, "设备概览"),
                    (Tab::Encoding, "编码能力"),
                    (Tab::Decoding, "解码检查"),
                    (Tab::Sessions, "当前会话"),
                ] {
                    ui.selectable_value(&mut self.center_ui.diagnostics.tab, tab, label);
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.link("日志").clicked() {
                        self.center_ui.page = Page::Logs;
                    }
                });
            });
            ui.separator();
            let tab = self.center_ui.diagnostics.tab;
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), theme::DIAGNOSTICS_BODY_MIN_HEIGHT),
                egui::Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_height(theme::DIAGNOSTICS_BODY_MIN_HEIGHT);
                    match tab {
                        Tab::Device => self.diagnostic_device(ui),
                        Tab::Encoding => self.diagnostic_encoding(ui),
                        Tab::Decoding => self.diagnostic_decoding(ui),
                        Tab::Sessions => self.diagnostic_sessions(ui),
                    }
                },
            );
        });
    }

    fn diagnostic_device(&self, ui: &mut egui::Ui) {
        diagnostics_row(
            ui,
            "显示器",
            &format!(
                "{} × {} · {} Hz",
                self.local_display.width, self.local_display.height, self.local_display.refresh_hz
            ),
        );
        for (label, value) in &self.diagnostics.rows {
            diagnostics_row(ui, label, value);
        }
        for value in &self.diagnostics.graphics {
            diagnostics_row(ui, "渲染设备", value);
        }
    }

    fn diagnostic_encoding(&mut self, ui: &mut egui::Ui) {
        let Some(caps) = self.host.as_ref().and_then(|host| host.capabilities()) else {
            diagnostics_empty(ui, "暂无有效编码能力，连接时会重新检查");
            return;
        };
        let mut groups = Vec::new();
        for cap in &caps.codecs {
            let key = (cap.adapter, cap.backend);
            if !groups.contains(&key) {
                groups.push(key);
            }
        }
        if self
            .center_ui
            .diagnostics
            .encoder
            .is_none_or(|key| !groups.contains(&key))
        {
            self.center_ui.diagnostics.encoder = groups.first().copied();
        }
        let Some((adapter, backend)) = self.center_ui.diagnostics.encoder else {
            diagnostics_empty(ui, "暂无可用编码器");
            return;
        };
        let backend_label = |key: (u64, Backend)| {
            if key.1 == Backend::Software {
                format!("{} · 软件", key.1.name())
            } else {
                let device = caps
                    .adapters
                    .iter()
                    .find(|a| a.luid == key.0)
                    .map_or("图形适配器", |a| a.name.as_str());
                format!("{} · {}", key.1.name(), device)
            }
        };
        ui.horizontal(|ui| {
            crate::ui::controls::diagnostics_label(ui, "编码器");
            egui::ComboBox::from_id_salt("diagnostics-encoder")
                .width(ui.available_width())
                .selected_text(backend_label((adapter, backend)))
                .show_ui(ui, |ui| {
                    for key in groups {
                        let label = backend_label(key);
                        ui.selectable_value(
                            &mut self.center_ui.diagnostics.encoder,
                            Some(key),
                            label,
                        );
                    }
                });
        });
        let device = caps
            .adapters
            .iter()
            .find(|a| a.luid == adapter)
            .map_or("软件编码", |a| a.name.as_str());
        let processing_device = diagnostics_row(
            ui,
            "处理设备",
            if backend == Backend::Software {
                "CPU"
            } else {
                device
            },
        );
        if backend == Backend::Software {
            diagnostics_row(ui, "图形输入设备", device)
                .on_hover_text(format!("适配器标识 {adapter:016X}"));
        } else {
            processing_device.on_hover_text(format!("适配器标识 {adapter:016X}"));
        }
        diagnostics_row(
            ui,
            "检查源",
            &format!(
                "{} · {} × {}",
                caps.screen.label(),
                caps.screen.width,
                caps.screen.height
            ),
        );
        ui.add_space(theme::DIAGNOSTICS_GAP);
        let rows: Vec<_> = caps
            .codecs
            .iter()
            .filter(|c| c.adapter == adapter && c.backend == backend)
            .map(|c| {
                vec![
                    codec_label(c.format.codec).into(),
                    chroma_label(c.format.chroma).into(),
                    format!("{} bit", c.format.depth),
                    format!("{} × {}", c.maximum.0, c.maximum.1),
                ]
            })
            .collect();
        diagnostics_table(
            ui,
            &["编码", "色彩", "位深", "尺寸上限"],
            &rows,
            |_, _| TEXT,
        );
    }

    fn diagnostic_decoding(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let busy = self.diagnostics.busy();
            if crate::ui::controls::diagnostics_action(
                ui,
                busy || self.active_session.is_none(),
                if busy { "停止检查" } else { "完整检查" },
            )
            .clicked()
            {
                if busy {
                    self.diagnostics.cancel_probe();
                } else {
                    self.diagnostics.probe(self.local_display);
                }
            }
        });
        ui.add_space(theme::DIAGNOSTICS_GAP);
        let Some(report) = self.diagnostics.probe.as_ref() else {
            return;
        };
        let total: usize = report
            .backends
            .iter()
            .flat_map(|b| &b.rows)
            .map(|r| r.cells.len())
            .sum();
        let done = report
            .backends
            .iter()
            .flat_map(|b| &b.rows)
            .flat_map(|r| &r.cells)
            .filter(|c| c.status != crate::media::decoder::diagnostics::Status::Pending)
            .count();
        diagnostics_row(
            ui,
            "检查状态",
            &if self.diagnostics.busy() {
                format!("已处理 {done} / {total}")
            } else {
                report.message.clone()
            },
        );
        if report.backends.is_empty() {
            return;
        }
        self.center_ui.diagnostics.decoder = self
            .center_ui
            .diagnostics
            .decoder
            .min(report.backends.len() - 1);
        ui.horizontal(|ui| {
            crate::ui::controls::diagnostics_label(ui, "解码器");
            egui::ComboBox::from_id_salt("diagnostics-decoder")
                .width(ui.available_width())
                .selected_text(&report.backends[self.center_ui.diagnostics.decoder].name)
                .show_ui(ui, |ui| {
                    for (index, backend) in report.backends.iter().enumerate() {
                        ui.selectable_value(
                            &mut self.center_ui.diagnostics.decoder,
                            index,
                            &backend.name,
                        );
                    }
                });
        });
        let backend = &report.backends[self.center_ui.diagnostics.decoder];
        diagnostics_row(ui, "处理设备", backend.device.as_deref().unwrap_or("CPU"));
        ui.add_space(theme::DIAGNOSTICS_GAP);
        let mut headers = vec!["格式".to_owned()];
        headers.extend(backend.sizes.iter().map(|&(w, h)| match (w, h) {
            (1280, 720) => "720p".to_owned(),
            (1920, 1080) => "1080p".to_owned(),
            (2560, 1440) => "1440p".to_owned(),
            (3840, 2160) => "4K".to_owned(),
            _ => format!("{w}×{h}"),
        }));
        let rows: Vec<_> = backend
            .rows
            .iter()
            .map(|r| {
                std::iter::once(r.format.clone())
                    .chain(
                        r.cells
                            .iter()
                            .map(|c| format!("{}\n{}", c.status.label(), c.detail)),
                    )
                    .collect()
            })
            .collect();
        diagnostics_table(
            ui,
            &headers.iter().map(String::as_str).collect::<Vec<_>>(),
            &rows,
            |row, column| {
                use crate::media::decoder::diagnostics::Status;
                if column == 0 {
                    return TEXT;
                }
                match backend.rows[row].cells[column - 1].status {
                    Status::Passed => theme::GREEN,
                    Status::Failed => theme::RED,
                    Status::Busy => theme::AMBER,
                    Status::Pending => theme::ACCENT,
                    Status::Unsupported | Status::Unchecked => theme::MUTED,
                }
            },
        );
    }

    fn diagnostic_sessions(&self, ui: &mut egui::Ui) {
        let host = self.host.as_ref().map(|host| host.status());
        let viewing = self.active_session.as_ref().and_then(|s| s.handle.info());
        if !host.as_ref().is_some_and(|host| host.session_active) && viewing.is_none() {
            diagnostics_empty(ui, "暂无活动会话 · 连接后显示实际编解码信息");
            return;
        }
        if let Some(host) = host.filter(|host| host.session_active) {
            ui.strong("本机被控");
            diagnostics_row(ui, "连接状态", &host.message);
            for (index, stream) in host.streams {
                let Some(active) = stream.video else {
                    continue;
                };
                ui.add_space(theme::DIAGNOSTICS_GAP);
                if let Some(screen) = stream.screen {
                    ui.strong(format!("{} · 视频轨道 {}", screen.label(), index + 1));
                }
                diagnostics_row(ui, "编码器", active.backend.name())
                    .on_hover_text(format!("适配器标识 {:016X}", active.adapter));
                diagnostics_row(
                    ui,
                    "采集方式",
                    stream.capture.as_deref().unwrap_or("等待采集"),
                );
                diagnostics_row(
                    ui,
                    "输出格式",
                    &format!(
                        "{} · {} · {} bit",
                        codec_label(active.format.codec),
                        chroma_label(active.format.chroma),
                        active.format.depth
                    ),
                );
                diagnostics_row(
                    ui,
                    "输出配置",
                    &format!(
                        "{} × {} · {} FPS 上限",
                        active.size.0, active.size.1, active.fps
                    ),
                );
                diagnostics_row(
                    ui,
                    "目标码率",
                    &format!("{:.2} Mbps", active.target_bps as f64 / 1_000_000.0),
                );
            }
        }
        if let Some(info) = viewing {
            ui.add_space(theme::DIAGNOSTICS_GAP);
            ui.strong("当前观看");
            for (label, value) in [
                ("本机解码器", info.decoder),
                ("接收码流", info.video_format),
                ("连接线路", info.connection),
                ("远端编码器", info.remote_encoder),
                ("远端采集", info.remote_capture),
            ] {
                diagnostics_row(
                    ui,
                    label,
                    if value.is_empty() {
                        "等待会话建立"
                    } else {
                        &value
                    },
                );
            }
        }
    }
}

fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::H265 => "H.265",
    }
}
fn chroma_label(chroma: u8) -> &'static str {
    if chroma == 3 { "4:4:4" } else { "4:2:0" }
}
