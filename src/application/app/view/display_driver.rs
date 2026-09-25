use super::*;
use crate::platform::display::install::{self, Operation, Status};

struct WorkResult {
    status: Result<Status, String>,
    error: Option<String>,
    reboot: bool,
}
#[derive(Default)]
pub(super) struct DriverUi {
    status: Option<Status>,
    error: Option<String>,
    pending: Option<std::sync::mpsc::Receiver<WorkResult>>,
    confirm: Option<Operation>,
    reboot: bool,
    first_home_checked: bool,
    first_home_pending: bool,
    first_offer: bool,
}
impl DriverUi {
    fn work(&mut self, operation: Option<Operation>, ctx: egui::Context) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = operation.map(install::request).transpose();
            // Re-read even after failure: device removal and package removal can
            // have different outcomes, and a cancelled UAC changes neither.
            let status = install::status().map_err(|e| format!("{e:#}"));
            let (reboot, error) = match result {
                Ok(value) => (value.unwrap_or(false), None),
                Err(error) => (false, Some(format!("{error:#}"))),
            };
            let _ = tx.send(WorkResult {
                status,
                error,
                reboot,
            });
            ctx.request_repaint();
        });
    }
    fn poll(&mut self) {
        if let Some(rx) = &self.pending {
            if let Ok(result) = rx.try_recv() {
                self.pending = None;
                self.error = result.error;
                self.reboot |= result.reboot;
                match result.status {
                    Ok(status) => self.status = Some(status),
                    Err(error) => {
                        self.status = None;
                        self.error.get_or_insert(error);
                    }
                }
            }
        }
    }
    pub(super) fn show(&mut self, ui: &mut egui::Ui, active: bool) {
        self.poll();
        if self.status.is_none() && self.pending.is_none() && self.error.is_none() {
            self.work(None, ui.ctx().clone());
        }
        let label = if self.reboot {
            "请重启 Windows 完成驱动操作"
        } else {
            self.status.as_ref().map_or("正在检查…", |s| s.label)
        };
        form_row(ui, "虚拟显示驱动", label, |ui| {
            ui.add_enabled_ui(self.pending.is_none() && !active && !self.reboot, |ui| {
                if let Some(status) = &self.status {
                    if !status.installed && ui.button("安装驱动").clicked() {
                        self.confirm = Some(Operation::Install);
                    }
                    if status.removable && ui.button("卸载驱动").clicked() {
                        self.confirm = Some(Operation::Uninstall);
                    }
                }
                if ui.button("重新检查").clicked() {
                    self.work(None, ui.ctx().clone());
                }
            })
            .response
            .on_disabled_hover_text(if active {
                "请先结束本机被控会话"
            } else if self.reboot {
                "Windows 要求重启后完成操作"
            } else {
                "正在处理"
            });
        });
    }
    pub(super) fn dialogs(&mut self, ctx: &egui::Context, active: bool, first_home: bool) {
        self.poll();
        if first_home && !self.first_home_checked {
            self.first_home_checked = true;
            match prompt_seen() {
                Ok(seen) => self.first_home_pending = !seen,
                Err(error) => self.error = Some(format!("读取首次启动设置失败：{error:#}")),
            }
        }
        if self.first_home_pending
            && self.status.is_none()
            && self.pending.is_none()
            && self.error.is_none()
        {
            self.work(None, ctx.clone());
        }
        if self.first_home_pending
            && first_home
            && !active
            && self.pending.is_none()
            && self.confirm.is_none()
            && self.error.is_none()
            && ctx.memory(|m| m.top_modal_layer().is_none())
            && !egui::Popup::is_any_open(ctx)
        {
            if let Some(status) = &self.status {
                let installed = status.installed;
                // Mark the opportunity as seen, including machines already equipped
                // with the driver. Uninstalling later must not re-enable onboarding.
                self.first_home_pending = false;
                match remember_prompt() {
                    Ok(()) if !installed => {
                        self.first_offer = true;
                        self.confirm = Some(Operation::Install);
                    }
                    Ok(()) => (),
                    Err(error) => self.error = Some(format!("保存首次启动设置失败：{error:#}")),
                }
            }
        }
        crate::ui::controls::observe_notice(
            ctx,
            "display-driver-error",
            "虚拟显示驱动",
            crate::ui::controls::DialogIcon::Error,
            self.error.as_deref(),
        );
        if let Some(operation) = self.confirm {
            let id = egui::Id::new("display-driver-confirm");
            if ctx.memory(|m| m.top_modal_layer().is_some_and(|layer| layer.id != id))
                || egui::Popup::is_any_open(ctx)
            {
                return;
            }
            let mut accepted = false;
            let mut dismiss = false;
            let response = egui::Modal::new(id)
                .frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
                    ui.set_width(480.0);
                    dismiss = crate::ui::controls::dialog_header(ui, operation.label(),
                        crate::ui::controls::DialogIcon::Warning, true);
                    let commit = match operation {
                        Operation::Install => {
                            if self.first_offer {
                                ui.label("是否安装虚拟显示驱动？用于本机被控时的虚拟屏和超级屏，可稍后在连接设置中安装。");
                            }
                            ui.label("安装可选的 SudoVDA 1.10.9.289，用于虚拟屏和超级屏。Windows 将请求管理员权限。");
                            ui.label("此驱动采用自签名证书。继续会将 sudovda@su.mk 加入本机的受信任根证书和受信任发布者。");
                            ui.label(egui::RichText::new("3C918FC73525AD8B1521B6DB26B71F694277CC49").monospace().small());
                            ui.hyperlink_to("驱动来源与许可证", "https://github.com/SudoMaker/SudoVDA");
                            "信任并安装"
                        }
                        Operation::Uninstall => {
                            ui.label("卸载 SudoVDA 设备和对应驱动包，需要管理员权限。卸载后本机的虚拟屏、超级屏不可用，物理屏仍可被控。");
                            ui.label("使用同一 SudoVDA 的其他程序也将无法创建虚拟屏。已有显示器或程序占用时不执行卸载。");
                            ui.label("保留显示偏好和共享签名证书，不删除其他显示驱动。");
                            "卸载"
                        }
                    };
                    let (yes, close) = crate::ui::controls::dialog_actions(ui,
                        Some(crate::ui::controls::DialogAction::new(commit).enabled(!active)), Some(if self.first_offer { "暂不安装" } else { "取消" }));
                    accepted = yes;
                    dismiss |= close;
                });
            if accepted {
                self.confirm = None;
                self.first_offer = false;
                self.work(Some(operation), ctx.clone());
            } else if dismiss || response.should_close() {
                self.confirm = None;
                self.first_offer = false;
            }
        }
        if self.pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn prompt_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(
        std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").context("本地设置目录不可用")?)
            .join("OpenUUYC/display-driver-prompt.seen"),
    )
}
fn prompt_seen() -> anyhow::Result<bool> {
    Ok(prompt_path()?.try_exists()?)
}
fn remember_prompt() -> anyhow::Result<()> {
    let path = prompt_path()?;
    std::fs::create_dir_all(path.parent().context("本地设置路径无效")?)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

impl DeviceCenterApp {
    pub(in crate::application::app) fn display_driver_dialogs(&mut self, ctx: &egui::Context) {
        if self.needs_login() || self.logout_pending {
            self.center_ui.display_driver.confirm = None;
            self.center_ui.display_driver.first_offer = false;
            return;
        }
        if self.close_confirmation {
            return;
        }
        let active = self
            .host
            .as_ref()
            .is_some_and(|h| h.status().session_active);
        self.center_ui
            .display_driver
            .dialogs(ctx, active, self.center_ui.page == Page::Mine);
    }
}
