use super::*;
use crate::app::updates::State;

#[derive(Clone, Copy)]
pub(super) enum LegalDocument {
    License,
    ThirdParty,
}

impl DeviceCenterApp {
    pub(super) fn about_page(&mut self, ui: &mut egui::Ui) {
        if self.center_ui.legal_document.is_some() {
            self.legal_document(ui);
            return;
        }
        ui.label(RichText::new("关于").size(25.0).strong());
        ui.add_space(28.0);
        egui::ScrollArea::vertical()
            .id_salt("about-page")
            .show(ui, |ui| {
                ui.set_max_width(720.0);
                ui.horizontal(|ui| {
                    let (icon, _) = ui.allocate_exact_size(vec2(88.0, 88.0), Sense::hover());
                    crate::ui::branding::paint(ui.painter(), icon, &self.brand_texture);
                    ui.add_space(16.0);
                    ui.vertical(|ui| {
                        ui.add_space(5.0);
                        ui.label(RichText::new(crate::APP_NAME).size(30.0).strong());
                        ui.label(
                            RichText::new("兼容 UU 远程协议的独立第三方 Rust 客户端")
                                .size(14.0)
                                .color(MUTED),
                        );
                        ui.label(
                            RichText::new(format!(
                                "v{}  ·  {} {}",
                                env!("CARGO_PKG_VERSION"),
                                match std::env::consts::OS {
                                    "windows" => "Windows",
                                    "macos" => "macOS",
                                    "linux" => "Linux",
                                    other => other,
                                },
                                std::env::consts::ARCH
                            ))
                            .size(12.0)
                            .color(MUTED),
                        );
                    });
                });

                ui.add_space(28.0);
                ui.separator();
                ui.add_space(20.0);
                self.about_updates(ui);
                ui.add_space(20.0);
                ui.separator();
                ui.add_space(20.0);

                ui.label(RichText::new("项目链接").size(16.0).strong());
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.hyperlink_to("GitHub 项目", "https://github.com/djkcyl/openuuyc");
                    ui.add_space(16.0);
                    ui.hyperlink_to("版本发布", "https://github.com/djkcyl/openuuyc/releases");
                });

                ui.add_space(28.0);
                ui.label(RichText::new("著作权、协议与许可").size(16.0).strong());
                ui.add_space(6.0);
                ui.label(
                    RichText::new("本项目不代表网易，不主张拥有 UU 的协议、软件或标识权益。")
                        .size(13.0)
                        .color(MUTED),
                );
                ui.label(
                    RichText::new(
                        "包含 FFmpeg 派生组件（LGPL-2.1-or-later）；第三方署名与许可独立适用。",
                    )
                    .size(13.0)
                    .color(MUTED),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.link("著作权与许可声明").clicked() {
                        self.center_ui.legal_document = Some(LegalDocument::License);
                    }
                    ui.add_space(16.0);
                    if ui.link("第三方来源与许可").clicked() {
                        self.center_ui.legal_document = Some(LegalDocument::ThirdParty);
                    }
                });
            });
    }

    fn legal_document(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.center_ui.legal_document else {
            return;
        };
        let (title, text) = match document {
            LegalDocument::License => (
                "Copyright and License Notice",
                include_str!("../../../LICENSE"),
            ),
            LegalDocument::ThirdParty => (
                "Third-Party Notices and Licenses",
                concat!(
                    include_str!("../../../THIRD_PARTY_NOTICES"),
                    "\n\nAppendix: GNU LGPL 2.1 (covered components are licensed LGPL-2.1-or-later)\n\n",
                    include_str!("../../decoder/COPYING.FFmpeg")
                ),
            ),
        };
        let mut back = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(25.0).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                back = ui.link("返回关于").clicked();
            });
        });
        if back || ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.center_ui.legal_document = None;
            ui.ctx().request_repaint();
            return;
        }
        ui.add_space(18.0);
        ui.separator();
        ui.add_space(18.0);
        egui::ScrollArea::vertical()
            .id_salt(("legal-page", title))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(RichText::new(text).monospace().size(14.0))
                        .wrap()
                        .selectable(true),
                );
            });
    }

    fn about_updates(&mut self, ui: &mut egui::Ui) {
        let (message, color, destination) = match &self.updates.state {
            State::Checking => ("正在检查最新正式版…".into(), MUTED, None),
            State::Current => ("已是最新正式版".into(), GREEN, None),
            State::Ahead => ("当前版本高于已发布的最新正式版".into(), MUTED, None),
            State::NoRelease => ("暂无公开正式版本".into(), MUTED, None),
            State::Failed(error) => (format!("检查失败：{error}"), AMBER, None),
            State::Available { version, url } => {
                (format!("发现新版本 v{version}"), BLUE, Some(url.clone()))
            }
        };
        let checking = matches!(self.updates.state, State::Checking);
        let wait = self.updates.retry_wait();
        let enabled = !checking && (destination.is_some() || wait == 0);
        let button = if destination.is_some() {
            "查看新版本"
        } else if checking {
            "检查中…"
        } else {
            "检查更新"
        };
        let mut clicked = false;
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                vec2((ui.available_width() - 128.0).max(120.0), 56.0),
                egui::Layout::top_down(Align::Min),
                |ui| {
                    ui.label(RichText::new("软件更新").size(16.0).strong());
                    ui.add(
                        egui::Label::new(RichText::new(&message).size(13.0).color(color))
                            .truncate(),
                    )
                    .on_hover_text(&message);
                },
            );
            let response = ui.add_enabled(enabled, primary(button));
            clicked = response.clicked();
            if !enabled && !checking {
                response.on_disabled_hover_text(format!("{wait} 秒后可重新检查"));
            }
        });
        if clicked {
            if let Some(url) = destination {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            } else {
                self.updates.request(ui.ctx());
            }
        }
    }
}
