use super::*;

#[derive(Default)]
pub(super) struct Ui {
    screens: Vec<crate::host::capture::Screen>,
    selected: Option<String>,
    error: Option<String>,
}
impl Ui {
    pub(super) fn refresh(&mut self) {
        match crate::host::capture::screens() {
            Ok(screens) => {
                self.screens = screens;
                self.error = None;
            }
            Err(error) => self.error = Some(format!("读取本机屏幕失败：{error:#}")),
        }
        if self.selected.is_none() {
            self.selected = self
                .screens
                .iter()
                .find(|s| s.primary)
                .or(self.screens.first())
                .map(|s| s.name.clone());
        }
    }
}
impl DeviceCenterApp {
    pub(super) fn host_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("本机画面共享");
        ui.add_space(12.0);
        ui.label("选择要共享的屏幕，开启后可从同一账号的其他设备连接本机。");
        if let Some(devices) = &self.devices {
            ui.label(format!("本机设备：{}", devices.current_device.alias));
        }
        ui.label(
            RichText::new("当前开发阶段支持普通桌面画面；声音、键鼠和其他被控操作尚未开放。")
                .color(MUTED),
        );
        ui.add_space(12.0);
        let Some(host) = self.host.clone() else {
            ui.label("正在准备本机在线会话…");
            return;
        };
        let state = host.status();
        let requested = host.requested();
        ui.add_enabled_ui(!requested, |ui| {
            let label = self
                .center_ui
                .host
                .selected
                .as_deref()
                .unwrap_or("请选择屏幕");
            egui::ComboBox::from_id_salt("host-screen")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for screen in &self.center_ui.host.screens {
                        ui.selectable_value(
                            &mut self.center_ui.host.selected,
                            Some(screen.name.clone()),
                            format!(
                                "{} · {}×{}{}",
                                screen.name,
                                screen.width,
                                screen.height,
                                if screen.primary { " · 主屏幕" } else { "" }
                            ),
                        );
                    }
                });
            if ui
                .add(crate::ui::controls::secondary("刷新屏幕列表"))
                .clicked()
            {
                self.center_ui.host.refresh();
            }
        });
        ui.add_space(12.0);
        if requested {
            if ui.add(crate::ui::controls::secondary("停止共享")).clicked() {
                host.stop();
            }
        } else if ui
            .add_enabled(
                self.center_ui.host.selected.is_some(),
                crate::ui::controls::primary("开启画面共享"),
            )
            .clicked()
        {
            if let Some(screen) = self
                .center_ui
                .host
                .screens
                .iter()
                .find(|s| Some(&s.name) == self.center_ui.host.selected.as_ref())
                .cloned()
            {
                host.start(screen);
            }
        }
        ui.add_space(12.0);
        ui.label(
            RichText::new(if state.message.is_empty() {
                "画面共享已关闭"
            } else {
                &state.message
            })
            .color(if state.connected { GREEN } else { MUTED }),
        );
        if state.connected {
            ui.label(format!("已发送 {} 帧", state.frames));
        }
        if let Some(error) = &state.error {
            ui.colored_label(RED, error);
        }
        if let Some(error) = &self.center_ui.host.error {
            ui.colored_label(RED, error);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}
