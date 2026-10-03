use super::*;

impl DeviceCenterApp {
    pub(super) fn notification_settings(&mut self, ui: &mut egui::Ui) {
        section(ui, "远程访问通知");
        let mut mode = self.notifications.mode;
        form_row(
            ui,
            "通知方式",
            "申请连接与正在被控的提示；Windows 通知受系统通知和勿扰设置影响",
            |ui| {
                egui::ComboBox::from_id_salt("remote-notification-mode")
                    .width(238.)
                    .selected_text(mode.label())
                    .show_ui(ui, |ui| {
                        for item in super::super::notifications::Mode::ALL {
                            ui.selectable_value(&mut mode, item, item.label());
                        }
                    });
            },
        );
        if mode != self.notifications.mode {
            self.notifications.set_mode(mode);
        }
        if let Some(error) = &self.notifications.error {
            ui.label(egui::RichText::new(error).color(theme::AMBER));
            if ui.button("重试通知").clicked() {
                self.notifications.set_mode(mode);
            }
        }
        if ui
            .add_enabled(
                self.notifications.available(),
                crate::ui::controls::secondary("查看当前通知"),
            )
            .clicked()
        {
            self.notifications.reopen();
        }
    }
    pub(super) fn save_host_settings(&mut self) {
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
        ui.add_enabled_ui(!self.maintenance_busy(), |ui| {
            self.host_settings_controls(ui)
        });
    }
    fn host_settings_controls(&mut self, ui: &mut egui::Ui) {
        section(ui, "被控设置");
        let host = self.host.clone();
        let mut allowed = host.as_ref().is_some_and(|h| h.allowed());
        form_row(
            ui,
            "允许被控",
            "允许授权设备连接本机，远程协助需单独开启",
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
            if host.request_audio_devices()
                && self
                    .worker
                    .commands
                    .send(GuiCommand::RefreshHostAudioDevices {
                        generation: self.login_generation,
                    })
                    .is_err()
            {
                host.audio_devices_failed();
            }
            let inventory = host.audio_devices();
            let mut quality = host.audio_quality();
            let mut quality_changed = false;
            form_row(
                ui,
                "桌面声音音质",
                "控制本机发送的桌面声音，修改立即生效",
                |ui| {
                    quality_changed = crate::ui::controls::audio_quality(ui, &mut quality);
                },
            );
            if quality_changed {
                match host.set_audio_quality(quality) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            let original_defaults = host.audio_defaults();
            let mut selected_defaults = original_defaults;
            form_row(
                ui,
                "默认音频设备",
                "调整为虚拟设备，断开后恢复；修改立即生效",
                |ui| {
                    egui::ComboBox::from_id_salt("host-audio-defaults")
                        .width(238.0)
                        .selected_text(selected_defaults.label())
                        .show_ui(ui, |ui| {
                            for choice in crate::features::host::audio::DefaultDevices::ALL {
                                ui.selectable_value(&mut selected_defaults, choice, choice.label());
                            }
                        });
                },
            );
            if selected_defaults != original_defaults {
                match host.set_audio_defaults(selected_defaults) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
            if let Some(error) = host.status().microphone.routing_error {
                ui.colored_label(RED, error);
            }
            let original_device = host.audio_device();
            let mut selected_device = original_device.clone();
            let selected_name = match original_device.as_ref() {
                None => "跟随系统默认".to_owned(),
                Some(device) => inventory
                    .devices
                    .iter()
                    .find(|d| d.id == device.id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| {
                        if inventory.updated.is_some() && inventory.error.is_none() {
                            format!("{}（未连接）", device.name)
                        } else {
                            device.name.clone()
                        }
                    }),
            };
            form_row(ui, "声音采集设备", "切换后立即生效", |ui| {
                egui::ComboBox::from_id_salt("host-audio-device")
                    .width(238.0)
                    .truncate()
                    .selected_text(&selected_name)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut selected_device, None, "跟随系统默认");
                        for device in &inventory.devices {
                            ui.push_id(&device.id, |ui| {
                                let chosen =
                                    selected_device.as_ref().is_some_and(|d| d.id == device.id);
                                if ui.selectable_label(chosen, &device.name).clicked() {
                                    selected_device = Some(device.clone());
                                }
                            });
                        }
                        if let Some(device) = original_device.as_ref()
                            && !inventory.devices.iter().any(|d| d.id == device.id)
                        {
                            ui.add_enabled(false, egui::Label::new(&selected_name));
                        }
                        if let Some(error) = &inventory.error {
                            ui.colored_label(RED, error);
                        } else if inventory.pending && inventory.updated.is_none() {
                            ui.label("正在读取设备…");
                        } else if inventory.devices.is_empty() {
                            ui.label("暂无可用播放设备");
                        }
                    });
            });
            if selected_device != original_device {
                match host.set_audio_device(selected_device) {
                    Ok(()) => self.save_host_settings(),
                    Err(error) => self.status = StatusMessage::error(error.to_string()),
                }
            }
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
            // Service maintenance intentionally stops the resident endpoint;
            // its temporary polling failure is not a settings failure.
            if self.maintenance_busy() {
                crate::ui::controls::clear_notice(ui.ctx(), (source, self.login_generation));
                continue;
            }
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
