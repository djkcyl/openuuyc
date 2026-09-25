use super::*;
use crate::application::app::updates::State;

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
        ui.label(RichText::new("关于").size(crate::ui::theme::TITLE).strong());
        ui.add_space(28.0);
        crate::ui::controls::page_scroll("about-page").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(vec2(88.0, 88.0), Sense::hover());
                crate::ui::branding::paint(ui.painter(), icon, &self.brand_texture);
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.add_space(5.0);
                    ui.label(
                        RichText::new(crate::APP_NAME)
                            .size(crate::ui::theme::TITLE)
                            .strong(),
                    );
                    ui.label(
                        RichText::new("兼容 UU 远程协议的独立第三方 Rust 客户端")
                            .size(crate::ui::theme::BODY)
                            .color(MUTED),
                    );
                    ui.label(
                        RichText::new(format!(
                            "v{}  ·  {} {}",
                            env!("CARGO_PKG_VERSION"),
                            "Windows",
                            std::env::consts::ARCH
                        ))
                        .size(crate::ui::theme::SMALL)
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

            ui.label(
                RichText::new("项目链接")
                    .size(crate::ui::theme::SECTION)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.hyperlink_to("GitHub 项目", "https://github.com/djkcyl/openuuyc");
                ui.add_space(16.0);
                ui.hyperlink_to("版本发布", "https://github.com/djkcyl/openuuyc/releases");
            });

            ui.add_space(28.0);
            ui.label(
                RichText::new("著作权、协议与许可")
                    .size(crate::ui::theme::SECTION)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("本项目不代表网易，不主张拥有 UU 的协议、软件或标识权益。")
                    .size(crate::ui::theme::COMPACT_TEXT)
                    .color(MUTED),
            );
            ui.label(
                RichText::new(
                    "包含 FFmpeg 派生组件（LGPL-2.1-or-later）；第三方署名与许可独立适用。",
                )
                .size(crate::ui::theme::COMPACT_TEXT)
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
                include_str!("../../../../LICENSE"),
            ),
            LegalDocument::ThirdParty => (
                "Third-Party Notices and Licenses",
                concat!(
                    include_str!("../../../../THIRD_PARTY_NOTICES"),
                    "\n\nAppendix: SpeexDSP BSD license (Rust resampler adaptation)\n\n",
                    include_str!("../../../media/audio/COPYING.SpeexDSP"),
                    "\n\nAppendix: GNU LGPL 2.1 (covered components are licensed LGPL-2.1-or-later)\n\n",
                    include_str!("../../../media/decoder/COPYING.FFmpeg"),
                    "\n\nAppendix: SudoVDA attribution and Microsoft Public License\n\n",
                    include_str!("../../../../assets/drivers/sudovda/SudoVDA-README.md"),
                    "\n\n",
                    include_str!("../../../../assets/drivers/sudovda/Microsoft-LICENSE.txt")
                ),
            ),
        };
        let back = crate::ui::controls::back_button(ui, "返回关于").clicked();
        ui.add_space(12.0);
        ui.label(RichText::new(title).size(theme::DIALOG_TITLE).strong());
        if back || ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.center_ui.legal_document = None;
            ui.ctx().request_repaint();
            return;
        }
        ui.add_space(18.0);
        ui.separator();
        ui.add_space(18.0);
        crate::ui::controls::page_scroll(("legal-page", title)).show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text).monospace().size(crate::ui::theme::BODY))
                    .wrap()
                    .selectable(true),
            );
        });
    }

    fn about_updates(&mut self, ui: &mut egui::Ui) {
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "client-update-error",
            "检查更新失败",
            crate::ui::controls::DialogIcon::Error,
            match &self.updates.state {
                State::Failed(error) => Some(error.as_str()),
                _ => None,
            },
        );
        let (message, color, destination) = match &self.updates.state {
            State::Checking => ("正在检查最新正式版…".into(), MUTED, None),
            State::Current => ("已是最新正式版".into(), GREEN, None),
            State::Ahead => ("当前版本高于已发布的最新正式版".into(), MUTED, None),
            State::NoRelease => ("暂无公开正式版本".into(), MUTED, None),
            State::Failed(_) => ("未能检查更新".into(), MUTED, None),
            State::Available { version, url, .. } => {
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
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::hover());
        let mut row_ui = ui.new_child(egui::UiBuilder::new().max_rect(row));
        row_ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let response = ui.add_enabled(enabled, primary(button));
            clicked = response.clicked();
            if !enabled && !checking {
                response.on_disabled_hover_text(format!("{wait} 秒后可重新检查"));
            }
            ui.with_layout(egui::Layout::top_down(Align::Min), |ui| {
                ui.label(RichText::new("软件更新").size(theme::SECTION).strong());
                ui.add(
                    egui::Label::new(
                        RichText::new(&message)
                            .size(theme::COMPACT_TEXT)
                            .color(color),
                    )
                    .truncate(),
                )
                .on_hover_text(&message);
            });
        });
        if clicked {
            if destination.is_some() {
                self.updates.dialog_open = true;
            } else {
                self.updates.request(ui.ctx());
            }
        }
    }
}
