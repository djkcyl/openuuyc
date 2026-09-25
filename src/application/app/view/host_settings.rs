use super::*;

impl DeviceCenterApp {
    fn save_host_settings(&mut self) {
        if self
            .worker
            .commands
            .send(GuiCommand::SaveHostSettings {
                generation: self.login_generation,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设置服务已停止，请重新打开程序后确认被控设置");
        }
    }
    pub(super) fn host_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, "被控设置");
        let host = self.host.clone();
        let mut allowed = host.as_ref().is_some_and(|h| h.allowed());
        form_row(
            ui,
            "允许被控",
            "允许同一账号的其他设备连接本机",
            |ui| {
                if ui
                    .add_enabled_ui(host.is_some(), |ui| {
                        crate::ui::controls::service_switch(ui, &mut allowed)
                    })
                    .inner
                    .changed()
                    && let Some(host) = host.as_ref()
                {
                    host.set_allowed(allowed);
                    self.save_host_settings();
                }
            },
        );
        if let Some(host) = host.as_ref() {
            use crate::features::host::{EncoderCodec, EncoderMode, EncodingSettings};
            let original = host.encoding_settings();
            let mut selected = original;
            let capabilities = host.capabilities();
            let available = |settings: EncodingSettings| {
                settings.validate().is_ok()
                    && capabilities
                        .as_ref()
                        .is_none_or(|caps| caps.codecs.iter().any(|cap| settings.accepts(cap)))
            };
            form_row(
                ui,
                "被控编码方式",
                "下次连接生效；硬件优先允许软件回退",
                |ui| {
                    egui::ComboBox::from_id_salt("host-encoder-mode")
                        .width(238.0)
                        .selected_text(selected.mode.label())
                        .show_ui(ui, |ui| {
                            for mode in EncoderMode::ALL {
                                let enabled = available(EncodingSettings { mode, ..selected });
                                ui.add_enabled_ui(enabled, |ui| {
                                    ui.selectable_value(&mut selected.mode, mode, mode.label())
                                })
                                .inner
                                .on_disabled_hover_text("当前编码格式与已验证能力不支持此组合");
                            }
                        });
                },
            );
            form_row(
                ui,
                "被控编码格式",
                "下次连接生效；需与控制端解码能力匹配",
                |ui| {
                    egui::ComboBox::from_id_salt("host-encoder-codec")
                        .width(238.0)
                        .selected_text(selected.codec.label())
                        .show_ui(ui, |ui| {
                            for codec in EncoderCodec::ALL {
                                let enabled = available(EncodingSettings { codec, ..selected });
                                ui.add_enabled_ui(enabled, |ui| {
                                    ui.selectable_value(&mut selected.codec, codec, codec.label())
                                })
                                .inner
                                .on_disabled_hover_text("当前编码方式与已验证能力不支持此格式");
                            }
                        });
                },
            );
            if selected != original {
                match host.set_encoding_settings(selected) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
        }
        let status = host.as_ref().map(|host| host.status()).unwrap_or_default();
        self.center_ui
            .display_driver
            .show(ui, status.session_active);
        let message = if status.message.is_empty() {
            "已禁止被控"
        } else {
            &status.message
        };
        let message = if status.saving {
            format!("{message} · 正在保存设置…")
        } else {
            message.to_owned()
        };
        let action = if status.session_active {
            Some("断开连接")
        } else if allowed && !status.ready && status.error.is_some() {
            Some("重试")
        } else {
            None
        };
        if crate::ui::controls::status_row(
            ui,
            &message,
            if status.connected { GREEN } else { MUTED },
            action,
        ) && let Some(host) = host
        {
            if status.session_active {
                host.disconnect();
            } else {
                host.retry();
            }
        }
        for (source, error) in [
            ("host-settings-error", status.settings_error),
            ("host-session-error", status.error),
        ] {
            crate::ui::controls::observe_notice(
                ui.ctx(),
                (source, self.login_generation),
                "被控设置",
                crate::ui::controls::DialogIcon::Error,
                error.as_deref(),
            );
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}
