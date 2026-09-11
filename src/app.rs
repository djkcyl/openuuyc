//! Native graphical device center and host-presence lifecycle.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::api::{DeviceInfo, DeviceList};
use crate::client::AuthenticatedClient;
use crate::login::{self, LoginProgress};
use crate::media::{
    CodecPreference, ConnectionMediaOptions, FrameRateChoice, LocalDisplayInfo, TransportChoice,
    detect_local_display,
};
use crate::presence::{ActivePresence, PresenceEvent, PresenceState};
use crate::viewer_owner::ViewerOwner;

mod assist;
mod catalog;
mod diagnostics;
#[cfg(windows)]
pub mod instance;
mod phone;
mod updates;
mod view;
use assist::{AssistOperation, AssistResult, AssistUi};
use phone::{LoginMethod, PhoneForm};
use view::{CenterUi, configure_visuals};

const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const WORKER_TICK: Duration = Duration::from_millis(250);

pub struct GuiOptions {
    pub refresh_interval: Duration,
    pub media: ConnectionMediaOptions,
}

pub fn run(options: GuiOptions) -> Result<()> {
    crate::ui::ensure_supported()?;
    let (local_display, display_warning) = match detect_local_display() {
        Ok(display) => (display, None),
        Err(error) => (
            LocalDisplayInfo::FALLBACK,
            Some(format!(
                "显示器探测失败（{error:#}），暂用 1920×1080 @ 60 Hz"
            )),
        ),
    };
    options
        .media
        .resolve(local_display)
        .context("invalid initial GUI media options")?;

    let refresh_interval = options.refresh_interval.max(MIN_REFRESH_INTERVAL);
    let viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} · 控制中心", crate::APP_NAME))
        .with_icon(crate::ui::branding::icon())
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([900.0, 620.0]);
    crate::ui::run(
        crate::ui::WindowConfig {
            viewport,
            centered: true,
        },
        Box::new(move |ctx, graphics| {
            crate::viewer::install_system_cjk_font(ctx);
            configure_visuals(ctx);
            ctx.request_repaint();
            let mut app = DeviceCenterApp::new(
                ctx,
                refresh_interval,
                local_display,
                options.media,
                display_warning,
            );
            if let Some(graphics) = graphics {
                app.diagnostics.graphics.push(graphics);
            }
            Box::new(app)
        }),
    )
}

struct DeviceCenterApp {
    brand_texture: egui::TextureHandle,
    updates: updates::UpdateCheck,
    center_ui: CenterUi,
    assist: AssistUi,
    worker: GuiWorker,
    devices: Option<DeviceList>,
    catalog: Option<catalog::Catalog>,
    catalog_error: Option<String>,
    extra_details:
        std::collections::HashMap<String, std::result::Result<crate::api::DeviceDetail, String>>,
    detail_pending: Option<String>,
    account_name: String,
    diagnostics: diagnostics::LocalDiagnostics,
    mutation_pending: bool,
    queued_mutation: Option<DeviceMutation>,
    selected_device_id: Option<String>,
    local_display: LocalDisplayInfo,
    media: ConnectionMediaOptions,
    presence: PresenceState,
    status: StatusMessage,
    refreshed_at: Option<Instant>,
    refresh_pending: bool,
    active_session: Option<ChildSession>,
    closing_session: bool,
    logout_confirmation: bool,
    logout_pending: bool,
    logout_sent: bool,
    login_generation: u64,
    qr_generation: u64,
    qr_running: bool,
    phone: PhoneForm,
    login_restoring: bool,
    login_running: bool,
    login_status: String,
    login_qr: Option<egui::TextureHandle>,
    login_error: Option<String>,
}

impl DeviceCenterApp {
    fn new(
        ctx: &egui::Context,
        refresh_interval: Duration,
        local_display: LocalDisplayInfo,
        media: ConnectionMediaOptions,
        display_warning: Option<String>,
    ) -> Self {
        Self {
            brand_texture: crate::ui::branding::load_texture(ctx),
            updates: updates::UpdateCheck::start(ctx),
            center_ui: CenterUi::default(),
            assist: AssistUi::default(),
            worker: GuiWorker::spawn(refresh_interval),
            devices: None,
            catalog: None,
            catalog_error: None,
            extra_details: Default::default(),
            detail_pending: None,
            account_name: String::new(),
            diagnostics: diagnostics::LocalDiagnostics::start(),
            mutation_pending: false,
            queued_mutation: None,
            selected_device_id: None,
            local_display,
            media,
            presence: PresenceState::Connecting,
            status: display_warning.map_or_else(
                || StatusMessage::info("正在读取设备并建立本机在线状态"),
                StatusMessage::warning,
            ),
            refreshed_at: None,
            refresh_pending: true,
            active_session: None,
            closing_session: false,
            logout_confirmation: false,
            logout_pending: false,
            logout_sent: false,
            login_generation: 0,
            qr_generation: 0,
            qr_running: false,
            phone: PhoneForm::default(),
            login_restoring: true,
            login_running: false,
            login_status: String::new(),
            login_qr: None,
            login_error: None,
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        self.updates.poll();
        self.diagnostics.poll();
        while let Ok(event) = self.worker.events.try_recv() {
            match event {
                GuiEvent::Presence(presence) => self.presence = presence,
                GuiEvent::Devices(generation, devices) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.devices = Some(devices);
                    self.normalize_selection();
                    self.refreshed_at = Some(Instant::now());
                    self.refresh_pending = false;
                    if !self.status.kind.is_alert() {
                        self.status = StatusMessage::success("设备状态已刷新");
                    }
                    self.login_restoring = false;
                }
                GuiEvent::Catalog(generation, result, account_name) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.account_name = account_name;
                    match result {
                        Ok(catalog) => {
                            self.catalog = Some(catalog);
                            self.catalog_error = None;
                            self.login_restoring = false;
                        }
                        Err(error) => self.catalog_error = Some(error),
                    }
                }
                GuiEvent::MutationFinished(result) => {
                    self.extra_details.clear();
                    self.mutation_pending = false;
                    self.status = match result {
                        Ok(message) => StatusMessage::success(message),
                        Err(message) => StatusMessage::warning(message),
                    };
                    self.center_ui.close_details();
                }
                GuiEvent::Detail(generation, id, result) => {
                    if generation == self.login_generation
                        && !self.logout_pending
                        && !self.login_running
                    {
                        if self.detail_pending.as_deref() == Some(id.as_str()) {
                            self.detail_pending = None;
                        }
                        self.extra_details.insert(id, result);
                    }
                }
                GuiEvent::Working(message) => {
                    if !self.logout_pending && !self.status.kind.is_alert() {
                        self.status = StatusMessage::info(message);
                    }
                }
                GuiEvent::Warning(message) => {
                    self.login_restore_error(&message);
                    self.refresh_pending = false;
                    self.status = StatusMessage::warning(message);
                }
                GuiEvent::Error(message) => {
                    self.login_restore_error(&message);
                    self.refresh_pending = false;
                    self.status = StatusMessage::error(message);
                }
                GuiEvent::SessionUnavailable(message) => {
                    self.clear_catalog();
                    self.devices = None;
                    self.selected_device_id = None;
                    self.refresh_pending = false;
                    self.status = StatusMessage::warning(message);
                    self.login_restoring = false;
                    self.login_error = Some(self.status.text.clone());
                    self.login_status = "无法恢复登录".into();
                    self.login_qr = None;
                }
                GuiEvent::SignedOut => {
                    self.clear_catalog();
                    self.devices = None;
                    self.selected_device_id = None;
                    self.refresh_pending = false;
                    self.login_restoring = false;
                    self.login_error = None;
                    self.login_status.clear();
                    self.status = StatusMessage::info("");
                }
                GuiEvent::LoginProgress(method, generation, attempt, progress) => {
                    if generation != self.login_generation {
                        continue;
                    }
                    if method == LoginMethod::Phone {
                        if attempt == self.phone.generation && self.phone.submitting {
                            self.phone.error = None;
                            self.phone.status = match progress {
                                LoginProgress::SubmittingSms => "正在验证验证码…",
                                _ => "正在准备登录…",
                            }
                            .into();
                        }
                        continue;
                    }
                    if attempt != self.qr_generation || !self.qr_running {
                        continue;
                    }
                    self.login_error = None;
                    match progress {
                        LoginProgress::ValidatingSavedSession => {
                            self.login_status = "正在恢复登录…".to_owned();
                        }
                        LoginProgress::RegisteringDevice => {
                            self.login_status = "正在准备登录…".to_owned();
                        }
                        LoginProgress::DeviceRegistered => {
                            self.login_status = "正在获取二维码…".to_owned();
                        }
                        LoginProgress::QrReady(content) => match qr_texture(ctx, &content) {
                            Ok(texture) => {
                                self.login_qr = Some(texture);
                                self.login_status = "使用 UU 远程手机端扫码".to_owned();
                            }
                            Err(error) => {
                                self.cancel_qr_login();
                                self.login_status = "二维码生成失败".into();
                                self.login_error = Some(format!("生成登录二维码失败：{error:#}"));
                            }
                        },
                        LoginProgress::WaitingForScan => {
                            self.login_status = "使用 UU 远程手机端扫码".to_owned();
                        }
                        LoginProgress::Scanned => {
                            self.login_status = "已扫码，请在手机端确认".to_owned();
                        }
                        LoginProgress::CanceledRefreshing => {
                            self.login_qr = None;
                            self.login_status = "正在刷新二维码…".to_owned();
                        }
                        LoginProgress::Confirmed => {
                            self.login_status = "正在登录…".to_owned();
                        }
                        LoginProgress::SubmittingSms => {
                            self.login_status = "正在验证验证码…".into()
                        }
                    }
                }
                GuiEvent::LoginFinished(method, generation, attempt, result) => {
                    if generation != self.login_generation {
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            // The worker accepted this session and invalidated
                            // both old flows before exposing the new epoch.
                            self.login_generation = generation.wrapping_add(1);
                            self.qr_running = false;
                            self.login_qr = None;
                            self.phone.clear_private();
                            self.login_restoring = true;
                            self.login_status = "登录成功，正在加载设备…".to_owned();
                            self.status = StatusMessage::success("登录成功，正在加载设备");
                            self.refresh_pending = true;
                        }
                        Err(message) if method == LoginMethod::Phone => {
                            if attempt != self.phone.generation || !self.phone.submitting {
                                continue;
                            }
                            self.phone.submitting = false;
                            self.phone.status = message.clone();
                            self.phone.error = Some(message);
                        }
                        Err(message) => {
                            if attempt != self.qr_generation || !self.qr_running {
                                continue;
                            }
                            self.qr_running = false;
                            self.login_qr = None;
                            self.login_restoring = false;
                            self.login_error = Some(message.clone());
                            self.login_status = "登录未完成".to_owned();
                            self.status = StatusMessage::error(format!("登录失败：{message}"));
                        }
                    }
                }
                GuiEvent::AccountEnded(message) => {
                    self.cancel_login();
                    self.phone.clear_private();
                    self.clear_catalog();
                    if let Some(session) = &self.active_session {
                        session.owner.request_close();
                    }
                    self.devices = None;
                    self.selected_device_id = None;
                    self.presence = PresenceState::Offline;
                    self.status = StatusMessage::warning(message);
                    self.refresh_pending = false;
                    self.login_status = "登录已失效".into();
                }
                GuiEvent::LoggedOut(outcome) => {
                    self.qr_running = false;
                    self.phone.clear_private();
                    self.clear_catalog();
                    self.logout_pending = false;
                    self.logout_sent = false;
                    self.devices = None;
                    self.selected_device_id = None;
                    self.presence = PresenceState::Offline;
                    self.status = match (outcome.remote_error, outcome.local_error) {
                        (_, Some(error)) => StatusMessage::error(format!(
                            "账号会话已停止，但本地凭据删除失败：{error}"
                        )),
                        (Some(error), None) => StatusMessage::warning(format!(
                            "已清除本地登录，服务端退出未确认：{error}"
                        )),
                        (None, None) => {
                            StatusMessage::success("已退出登录，本虚拟设备已从账号移除")
                        }
                    };
                    self.login_restoring = false;
                    self.login_running = false;
                    self.login_qr = None;
                    self.login_error = self
                        .status
                        .kind
                        .is_alert()
                        .then(|| self.status.text.clone());
                    self.login_status = if self.login_error.is_some() {
                        "退出未完成".into()
                    } else {
                        String::new()
                    };
                }
                // Cooldown survives cancellation of either login flow.
                GuiEvent::SmsCooldown(deadline) => self.phone.resend_at = deadline,
                GuiEvent::SmsCodeFinished {
                    generation,
                    attempt,
                    phone,
                    dispatched,
                    result,
                } => {
                    if generation != self.login_generation
                        || attempt != self.phone.generation
                        || !self.phone.sending
                    {
                        continue;
                    }
                    self.phone.sending = false;
                    if dispatched {
                        self.phone.requested = Some(phone);
                    }
                    match result {
                        Ok(()) => {
                            self.phone.status = "验证码已发送".into();
                            self.phone.error = None;
                        }
                        Err(error) => {
                            self.phone.status = error.clone();
                            self.phone.error = Some(error);
                        }
                    }
                }
                GuiEvent::AssistLists(generation, result) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.assist.loading = false;
                    match result {
                        Ok(lists) => {
                            if let Some(error) = &lists.code_error {
                                self.assist.list_failure(error.clone());
                            } else {
                                self.assist.last_list_error = None;
                            }
                            self.assist.lists = Some(lists);
                        }
                        Err(error) => {
                            self.assist.list_failure(error);
                        }
                    }
                }
                GuiEvent::AssistOperation(generation, sequence, result) => {
                    if generation != self.login_generation
                        || sequence != self.assist.sequence
                        || self.logout_pending
                    {
                        continue;
                    }
                    self.finish_assist_operation(result);
                }
            }
            self.sync_login_running();
        }

        let finished = self
            .active_session
            .as_mut()
            .and_then(|session| session.child.try_wait().ok().flatten())
            .map(|status| {
                let alias = self
                    .active_session
                    .as_ref()
                    .map(|session| session.alias.clone())
                    .unwrap_or_default();
                (alias, status.success(), status.code())
            });
        if let Some((alias, success, code)) = finished {
            tracing::info!(device = %alias, success, exit_code = ?code, "viewer child process ended");
            self.active_session = None;
            self.closing_session = false;
            self.status = if success {
                StatusMessage::success(format!("{alias} 的观看窗口已关闭"))
            } else {
                StatusMessage::error(format!(
                    "{alias} 的观看进程异常退出{}，请查看诊断日志",
                    code.map_or_else(String::new, |value| format!("（代码 {value}）"))
                ))
            };
            if self.devices.is_some() && !self.logout_pending {
                self.refresh_pending = self.worker.commands.send(GuiCommand::Refresh).is_ok();
                self.request_assist_refresh();
            }
        }
        if self.logout_pending && !self.logout_sent && self.active_session.is_none() {
            self.logout_sent = self.worker.commands.send(GuiCommand::Logout).is_ok();
            if !self.logout_sent {
                self.logout_pending = false;
                self.status = StatusMessage::error("设备后台服务已经停止，尚未执行账号退出");
            }
        }
        if self.active_session.is_none()
            && let Some(change) = self.queued_mutation.take()
        {
            self.send_mutation(change);
        }
    }

    fn begin_login(&mut self) {
        if self.logout_pending || self.active_session.is_some() || self.mutation_pending {
            return;
        }
        self.clear_catalog();
        self.devices = None;
        self.selected_device_id = None;
        self.qr_generation = self.qr_generation.wrapping_add(1);
        self.login_restoring = false;
        self.qr_running = true;
        self.sync_login_running();
        self.login_qr = None;
        self.login_error = None;
        self.login_status = "正在准备登录…".to_owned();
        if self
            .worker
            .commands
            .send(GuiCommand::Login {
                generation: self.login_generation,
                attempt: self.qr_generation,
            })
            .is_err()
        {
            self.qr_running = false;
            self.sync_login_running();
            self.login_status = "登录服务不可用".into();
            self.login_error = Some("设备后台服务已经停止".to_owned());
        }
    }

    fn cancel_login(&mut self) {
        self.phone.cancel();
        self.qr_generation = self.qr_generation.wrapping_add(1);
        self.qr_running = false;
        self.login_generation = self.login_generation.wrapping_add(1);
        let _ = self
            .worker
            .commands
            .send(GuiCommand::CancelLogin(self.login_generation));
        self.login_restoring = false;
        self.login_running = false;
        self.login_qr = None;
        self.login_error = None;
        self.login_status.clear();
    }

    fn sync_login_running(&mut self) {
        self.login_running = self.qr_running || self.phone.sending || self.phone.submitting;
    }

    fn cancel_qr_login(&mut self) {
        self.qr_generation = self.qr_generation.wrapping_add(1);
        self.qr_running = false;
        self.login_qr = None;
        self.login_error = None;
        self.login_status.clear();
        self.sync_login_running();
        let _ = self
            .worker
            .commands
            .send(GuiCommand::CancelQr(self.login_generation));
    }

    fn login_restore_error(&mut self, message: &str) {
        if self.devices.is_none() && self.catalog.is_none() && !self.login_running {
            self.login_restoring = false;
            self.login_status = "连接失败".into();
            self.login_error = Some(message.to_owned());
        }
    }

    fn logout(&mut self) {
        if self.mutation_pending || (self.assist.busy && !self.assist.querying) {
            self.status = StatusMessage::warning("请等待设备操作完成再退出账号");
            return;
        }
        self.logout_confirmation = true;
    }

    fn normalize_selection(&mut self) {
        if self.selected_device_id.as_ref().is_some_and(|id| {
            self.catalog
                .as_ref()
                .is_some_and(|c| c.groups.entries().any(|(_, d)| &d.device_id == id))
        }) {
            return;
        }
        let Some(devices) = self.devices.as_ref() else {
            self.selected_device_id = None;
            return;
        };
        let selected_exists = self.selected_device_id.as_ref().is_some_and(|selected| {
            all_devices(devices).any(|(_, device)| device.device_id == *selected)
        });
        if !selected_exists {
            self.selected_device_id = all_devices(devices)
                .next()
                .map(|(_, device)| device.device_id.clone());
        }
    }

    fn selected_device(&self) -> Option<&DeviceInfo> {
        let selected = self.selected_device_id.as_deref()?;
        self.devices
            .as_ref()
            .and_then(|devices| {
                all_devices(devices).find(|(_, device)| device.device_id == selected)
            })
            .map(|(_, device)| device)
            .or_else(|| {
                self.catalog
                    .as_ref()?
                    .groups
                    .entries()
                    .find(|(_, d)| d.device_id == selected)
                    .map(|(_, d)| d)
            })
    }

    fn clear_catalog(&mut self) {
        self.assist = AssistUi::default();
        self.extra_details.clear();
        self.detail_pending = None;
        self.queued_mutation = None;
        self.mutation_pending = false;
        self.catalog = None;
        self.catalog_error = None;
        self.account_name.clear();
        self.center_ui.close_details();
    }

    fn is_viewing_target(&self, id: &str) -> bool {
        self.devices.as_ref().is_some_and(|list| {
            list.current_device.device_id != id
                && all_devices(list).any(|(_, d)| d.device_id == id && matches!(d.platform, 1 | 4))
        }) && !self.catalog.as_ref().is_some_and(|c| {
            c.is_virtual(id)
                || c.groups
                    .mobile_devices
                    .iter()
                    .chain(&c.groups.tv_devices)
                    .any(|d| d.device_id == id)
        })
    }

    fn show_in_watching_list(&self, device: &DeviceInfo) -> bool {
        matches!(device.platform, 1 | 4)
            && self
                .catalog
                .as_ref()
                .is_none_or(|c| !c.is_virtual(&device.device_id))
    }

    fn open_details(&mut self, id: String) {
        self.selected_device_id = Some(id.clone());
        self.center_ui.open_details();
        if self
            .catalog
            .as_ref()
            .is_none_or(|c| !c.details.contains_key(&id))
            && !self.extra_details.contains_key(&id)
            && self.detail_pending.as_deref() != Some(id.as_str())
        {
            self.detail_pending = Some(id.clone());
            if self.worker.commands.send(GuiCommand::Detail(id)).is_err() {
                self.detail_pending = None;
            }
        }
    }

    fn queue_mutation(&mut self, change: DeviceMutation) {
        if self.mutation_pending || self.logout_pending {
            return;
        }
        self.mutation_pending = true;
        if matches!(&change, DeviceMutation::Remove { id, .. } if self.active_session.as_ref().is_some_and(|s| s.device_id.as_ref().is_none_or(|target| target == id)))
        {
            self.stop_viewer();
            self.status = StatusMessage::info("正在正常结束观看，随后移除该设备");
            self.queued_mutation = Some(change);
        } else {
            self.send_mutation(change);
        }
    }

    fn send_mutation(&mut self, change: DeviceMutation) {
        if self
            .worker
            .commands
            .send(GuiCommand::Mutate(change))
            .is_err()
        {
            self.mutation_pending = false;
            self.status = StatusMessage::error("后台服务已停止，未发送设备操作");
        } else {
            self.status = StatusMessage::info("正在处理设备操作…");
        }
    }

    fn request_refresh(&mut self) {
        self.extra_details.clear();
        if self.worker.commands.send(GuiCommand::Refresh).is_ok() {
            self.refresh_pending = true;
            self.status = StatusMessage::info("正在刷新设备状态");
        } else {
            self.status = StatusMessage::error("设备后台服务已经停止");
        }
    }

    fn start_viewer(&mut self) {
        if self.logout_pending || self.mutation_pending {
            return;
        }
        let Some(device) = self.selected_device().cloned() else {
            self.status = StatusMessage::warning("请先选择设备");
            return;
        };
        if !self.is_viewing_target(&device.device_id) {
            self.status = StatusMessage::warning("此设备仅用于账号管理，不能观看");
            return;
        }
        if let Err(message) = connectability_error(&device) {
            self.status = StatusMessage::warning(message);
            return;
        }
        if self.active_session.is_some() {
            self.status = StatusMessage::warning("已有观看窗口正在运行");
            return;
        }

        self.spawn_viewer(
            display_alias(&device).to_owned(),
            Some(device.device_id.clone()),
            None,
        );
    }

    fn spawn_viewer(
        &mut self,
        alias: String,
        device_id: Option<String>,
        assist_request: Option<crate::assist::AssistRequest>,
    ) {
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.status = StatusMessage::error(format!("无法定位当前程序：{error}"));
                return;
            }
        };
        let mut command = Command::new(executable);
        let owner = match ViewerOwner::new() {
            Ok(owner) => owner,
            Err(error) => {
                self.status = StatusMessage::error(format!("无法创建观看生命周期通道：{error:#}"));
                return;
            }
        };
        crate::logging::configure_child(&mut command);
        command.arg("connect").arg(&alias);
        if let Some(id) = &device_id
            && assist_request.is_none()
        {
            command.arg("--device-id").arg(id);
        }
        if assist_request.is_some() {
            command.arg("--assist-stdin");
        }
        command
            .arg("--owner-control")
            .arg(owner.descriptor())
            .arg("--fps")
            .arg(frame_rate_argument(self.media.frame_rate))
            .arg("--codec")
            .arg(codec_argument(self.media.codec))
            .arg("--hardware-decode")
            .arg(self.media.hardware_decode.to_string())
            .arg("--transport")
            .arg(transport_argument(self.media.transport));
        if self.media.muted {
            command.arg("--mute");
        }
        command
            .stdin(if assist_request.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        tracing::debug!(
            executable = ?command.get_program(),
            "launching viewer"
        );
        match command.spawn() {
            Ok(mut child) => {
                if let Some(request) = assist_request {
                    use std::io::Write;
                    let result = (|| -> Result<()> {
                        let bytes = serde_json::to_vec(&request)?;
                        child
                            .stdin
                            .take()
                            .context("子进程输入通道不可用")?
                            .write_all(&bytes)?;
                        Ok(())
                    })();
                    if let Err(error) = result {
                        owner.request_close();
                        let _ = child.kill();
                        let _ = child.wait();
                        self.status = StatusMessage::error(format!("无法传递连接参数：{error}"));
                        return;
                    }
                }
                self.status = StatusMessage::success(format!("正在打开 {}", alias.as_str()));
                self.active_session = Some(ChildSession {
                    device_id,
                    alias: alias.clone(),
                    child,
                    owner,
                });
                self.closing_session = false;
            }
            Err(error) => {
                self.status = StatusMessage::error(format!("无法启动观看窗口：{error}"));
            }
        }
    }

    fn stop_viewer(&mut self) {
        if let Some(session) = &self.active_session {
            session.owner.request_close();
            self.closing_session = true;
        }
    }
}

impl crate::ui::App for DeviceCenterApp {
    fn on_focus_changed(&mut self, focused: bool) {
        self.worker.focus.send_if_modified(|current| {
            if *current == focused {
                return false;
            }
            *current = focused;
            tracing::debug!(focused, "device center refresh focus changed");
            true
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        self.draw_center(ui);
        self.draw_dialogs(&ctx);
        ui.ctx().request_repaint_after(WORKER_TICK);
    }

    fn on_exit(&mut self) {
        if let Some(session) = &self.active_session {
            session.owner.request_close();
        }
        if self.logout_pending && !self.logout_sent {
            let _ = self.worker.commands.send(GuiCommand::Logout);
        }
        let _ = self.worker.commands.send(GuiCommand::Shutdown);
    }
}

struct GuiWorker {
    commands: mpsc::UnboundedSender<GuiCommand>,
    focus: watch::Sender<bool>,
    events: Receiver<GuiEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GuiWorker {
    fn spawn(refresh_interval: Duration) -> Self {
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (focus, foreground) = watch::channel(false);
        let (event_sender, events) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = event_sender
                        .send(GuiEvent::Error(format!("无法启动 GUI 后台服务：{error}")));
                    return;
                }
            };
            runtime.block_on(gui_worker_loop(
                refresh_interval,
                command_receiver,
                event_sender,
                foreground,
            ));
        });
        Self {
            commands,
            focus,
            events,
            thread: Some(thread),
        }
    }
}

impl Drop for GuiWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(GuiCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum GuiCommand {
    Refresh,
    Login {
        generation: u64,
        attempt: u64,
    },
    CancelQr(u64),
    CancelSms(u64),
    RequestSms {
        generation: u64,
        attempt: u64,
        phone: login::sms::PhoneNumber,
        agreed: bool,
    },
    LoginSms {
        generation: u64,
        attempt: u64,
        phone: login::sms::PhoneNumber,
        code: String,
        agreed: bool,
    },
    CancelLogin(u64),
    Logout,
    Shutdown,
    Mutate(DeviceMutation),
    Detail(String),
    RefreshAssist,
    CancelAssistCheck,
    AssistOperation {
        generation: u64,
        sequence: u64,
        operation: AssistOperation,
    },
}

enum DeviceMutation {
    Rename { id: String, alias: String },
    Remove { id: String },
}

enum GuiEvent {
    Presence(PresenceState),
    AssistLists(u64, std::result::Result<crate::assist::SavedLists, String>),
    AssistOperation(u64, u64, std::result::Result<AssistResult, String>),
    Devices(u64, DeviceList),
    Catalog(u64, std::result::Result<catalog::Catalog, String>, String),
    MutationFinished(std::result::Result<String, String>),
    Detail(
        u64,
        String,
        std::result::Result<crate::api::DeviceDetail, String>,
    ),
    Working(String),
    Warning(String),
    Error(String),
    SessionUnavailable(String),
    SignedOut,
    AccountEnded(String),
    LoginProgress(LoginMethod, u64, u64, LoginProgress),
    LoginFinished(LoginMethod, u64, u64, std::result::Result<(), String>),
    SmsCooldown(Option<Instant>),
    SmsCodeFinished {
        generation: u64,
        attempt: u64,
        phone: login::sms::PhoneNumber,
        dispatched: bool,
        result: std::result::Result<(), String>,
    },
    LoggedOut(crate::client::LogoutOutcome),
}

struct ActiveLogin {
    generation: u64,
    attempt: u64,
    task: JoinHandle<Result<login::PreparedLogin>>,
    progress: Receiver<LoginProgress>,
}

struct ActiveSmsCode {
    generation: u64,
    attempt: u64,
    phone: login::sms::PhoneNumber,
    task: JoinHandle<login::sms::CodeOutcome>,
}

async fn cancel_sms_task(task: &mut Option<ActiveSmsCode>) {
    if let Some(active) = task.take() {
        active.task.abort();
        let _ = active.task.await;
    }
}

enum GuiOperation {
    Devices(Box<DeviceList>),
}

async fn cancel_login_task(task: &mut Option<ActiveLogin>) {
    if let Some(login) = task.take() {
        login.task.abort();
        let _ = login.task.await;
        // PreparedLogin is not committed by the network worker. Both late
        // progress and a completed-but-cancelled result die with this owner.
    }
}

async fn cancel_operation<T>(task: &mut Option<JoinHandle<T>>) {
    if let Some(task) = task.take() {
        task.abort();
        let _ = task.await;
    }
}

async fn gui_worker_loop(
    refresh_interval: Duration,
    mut commands: mpsc::UnboundedReceiver<GuiCommand>,
    events: Sender<GuiEvent>,
    mut foreground: watch::Receiver<bool>,
) {
    let device_runtime = match crate::device_session::DeviceRuntime::start() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(GuiEvent::SessionUnavailable(format!(
                "无法打开虚拟设备身份：{error:#}"
            )));
            return;
        }
    };
    let mut client: Option<Arc<AuthenticatedClient>> = None;
    let mut allow_load = true;
    let mut next_refresh = Instant::now();
    let mut login_task: Option<ActiveLogin> = None;
    let mut sms_login_task: Option<ActiveLogin> = None;
    let mut commit_gate = login::LoginCommitGate::default();
    let mut sms_task: Option<ActiveSmsCode> = None;
    let mut sms_gate = login::sms::SmsGate::default();
    let mut api_task: Option<JoinHandle<Result<GuiOperation>>> = None;
    let mut catalog_task: Option<JoinHandle<(Result<catalog::Catalog>, String)>> = None;
    let mut catalog_cache = None;
    let mut next_catalog = Instant::now();
    let mut force_catalog = false;
    let mut catalog_generation = 0;
    let mut mutation_task: Option<JoinHandle<Result<String>>> = None;
    let mut detail_task: Option<JoinHandle<(String, Result<crate::api::DeviceDetail>)>> = None;
    let mut logout_task: Option<JoinHandle<crate::client::LogoutOutcome>> = None;
    let mut assist_lists_task: Option<JoinHandle<(u64, Result<crate::assist::SavedLists>)>> = None;
    let mut assist_operation_task: Option<JoinHandle<(u64, u64, Result<AssistResult>)>> = None;
    let mut assist_operation_is_query = false;
    let mut assist_operation_context = (0_u64, 0_u64);
    let mut assist_lists_generation = 0_u64;
    let mut next_assist_refresh = Instant::now();
    let mut tick = tokio::time::interval(WORKER_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut host_signal: Option<ActivePresence> = None;
    let mut presence_stopped = false;

    loop {
        let command = tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(command) => Some(command),
                None => break,
            },
            changed = foreground.changed() => {
                if changed.is_err() { break; }
                None
            },
            _ = tick.tick() => None,
        };
        if let Some(command) = command {
            match command {
                GuiCommand::RefreshAssist => {
                    if assist_lists_task.is_none() {
                        next_assist_refresh = Instant::now();
                    }
                }
                GuiCommand::CancelAssistCheck => {
                    if assist_operation_is_query {
                        cancel_operation(&mut assist_operation_task).await;
                    }
                }
                GuiCommand::AssistOperation {
                    generation,
                    sequence,
                    operation,
                } => {
                    if generation != catalog_generation
                        || logout_task.is_some()
                        || assist_operation_task.is_some()
                        || client.is_none()
                    {
                        let _ = events.send(GuiEvent::AssistOperation(
                            generation,
                            sequence,
                            Err("当前无法处理远程协助操作".into()),
                        ));
                    } else if let Some(client) = &client {
                        let client = Arc::clone(client);
                        assist_operation_is_query = operation.is_query();
                        if !assist_operation_is_query {
                            cancel_operation(&mut assist_lists_task).await;
                        }
                        assist_operation_context = (generation, sequence);
                        assist_operation_task = Some(tokio::spawn(async move {
                            (
                                generation,
                                sequence,
                                assist::execute(client, operation).await,
                            )
                        }));
                    }
                }
                GuiCommand::RequestSms {
                    generation,
                    attempt,
                    phone,
                    agreed,
                } => {
                    if generation != catalog_generation
                        || logout_task.is_some()
                        || sms_login_task.is_some()
                        || sms_task.is_some()
                    {
                        let _ = events.send(GuiEvent::SmsCooldown(sms_gate.resend_at()));
                        let _ = events.send(GuiEvent::SmsCodeFinished {
                            generation,
                            attempt,
                            phone,
                            dispatched: false,
                            result: Err("请等待当前操作完成".into()),
                        });
                        continue;
                    }
                    let deadline = match sms_gate.begin_request(Instant::now(), agreed) {
                        Ok(deadline) => deadline,
                        Err(error) => {
                            let _ = events.send(GuiEvent::SmsCooldown(sms_gate.resend_at()));
                            let _ = events.send(GuiEvent::SmsCodeFinished {
                                generation,
                                attempt,
                                phone,
                                dispatched: false,
                                result: Err(error.to_string()),
                            });
                            continue;
                        }
                    };
                    catalog_generation = generation;
                    cancel_operation(&mut detail_task).await;
                    cancel_operation(&mut catalog_task).await;
                    catalog_cache = None;
                    cancel_operation(&mut api_task).await;
                    stop_active_signal(&mut host_signal).await;
                    client = None;
                    allow_load = false;
                    let _ = events.send(GuiEvent::SmsCooldown(Some(deadline)));
                    sms_task = Some(ActiveSmsCode {
                        generation,
                        attempt,
                        phone: phone.clone(),
                        task: tokio::spawn(login::sms::request_code(
                            device_runtime.handle(),
                            phone,
                        )),
                    });
                }
                GuiCommand::LoginSms {
                    generation,
                    attempt,
                    phone,
                    code,
                    agreed,
                } => {
                    let ready = if generation != catalog_generation
                        || logout_task.is_some()
                        || sms_login_task.is_some()
                        || sms_task.is_some()
                    {
                        Err(anyhow!("登录操作已改变，请重新获取验证码"))
                    } else {
                        sms_gate.validate_submission(&phone, &code, agreed)
                    };
                    if let Err(error) = ready {
                        let _ = events.send(GuiEvent::LoginFinished(
                            LoginMethod::Phone,
                            generation,
                            attempt,
                            Err(error.to_string()),
                        ));
                        continue;
                    }
                    let device = device_runtime.handle();
                    let (progress, receiver) = std::sync::mpsc::channel();
                    let gate = commit_gate.clone();
                    sms_login_task = Some(ActiveLogin {
                        generation,
                        attempt,
                        progress: receiver,
                        task: tokio::spawn(async move {
                            let result = login::sms::prepare_login(
                                device,
                                gate,
                                phone.clone(),
                                code.clone(),
                                move |event| {
                                    let _ = progress.send(event);
                                },
                            )
                            .await;
                            result.map_err(|error| {
                                anyhow!(login::sms::error_message(
                                    &error,
                                    Some(&phone),
                                    Some(&code)
                                ))
                            })
                        }),
                    });
                }
                GuiCommand::Detail(id) => {
                    cancel_operation(&mut detail_task).await;
                    if let Some(client) = &client {
                        let client = Arc::clone(client);
                        detail_task = Some(tokio::spawn(async move {
                            let result = client.device_detail(&id).await;
                            (id, result)
                        }));
                    }
                }
                GuiCommand::Mutate(change) => {
                    if mutation_task.is_some() || logout_task.is_some() || client.is_none() {
                        let _ = events.send(GuiEvent::MutationFinished(Err(
                            "当前无法执行设备操作，未发送请求".into(),
                        )));
                    } else if let Some(client) = &client {
                        let client = Arc::clone(client);
                        mutation_task = Some(tokio::spawn(async move {
                            match change {
                                DeviceMutation::Rename { id, alias } => {
                                    client.rename_owned_device(&id, &alias).await
                                }
                                DeviceMutation::Remove { id } => {
                                    client.remove_account_device(&id).await
                                }
                            }
                        }));
                    }
                }
                GuiCommand::Shutdown => break,
                GuiCommand::Refresh => {
                    next_catalog = Instant::now();
                    force_catalog = true;
                    allow_load = true;
                    next_refresh = Instant::now();
                }
                GuiCommand::Login {
                    generation,
                    attempt,
                } if logout_task.is_none() && generation == catalog_generation => {
                    cancel_operation(&mut assist_lists_task).await;
                    cancel_operation(&mut assist_operation_task).await;
                    cancel_operation(&mut detail_task).await;
                    catalog_generation = generation;
                    cancel_operation(&mut catalog_task).await;
                    catalog_cache = None;
                    next_catalog = Instant::now();
                    cancel_login_task(&mut login_task).await;
                    cancel_operation(&mut api_task).await;
                    stop_active_signal(&mut host_signal).await;
                    client = None;
                    allow_load = false;
                    let (progress, receiver) = std::sync::mpsc::channel();
                    login_task = Some(ActiveLogin {
                        generation,
                        attempt,
                        progress: receiver,
                        task: tokio::spawn(login::prepare_login_with_gate(
                            device_runtime.handle(),
                            commit_gate.clone(),
                            move |event| {
                                let _ = progress.send(event);
                            },
                        )),
                    });
                }
                GuiCommand::Login { .. } => {}
                GuiCommand::CancelQr(generation) => {
                    if generation == catalog_generation {
                        cancel_login_task(&mut login_task).await;
                    }
                }
                GuiCommand::CancelSms(generation) => {
                    if generation == catalog_generation {
                        cancel_sms_task(&mut sms_task).await;
                        cancel_login_task(&mut sms_login_task).await;
                        sms_gate.cancel();
                    }
                }
                GuiCommand::CancelLogin(generation) => {
                    cancel_sms_task(&mut sms_task).await;
                    sms_gate.cancel();
                    catalog_generation = generation;
                    cancel_login_task(&mut login_task).await;
                    cancel_login_task(&mut sms_login_task).await;
                    commit_gate = login::LoginCommitGate::default();
                }
                GuiCommand::Logout if logout_task.is_none() => {
                    cancel_operation(&mut assist_lists_task).await;
                    cancel_operation(&mut assist_operation_task).await;
                    cancel_sms_task(&mut sms_task).await;
                    sms_gate.cancel();
                    cancel_operation(&mut detail_task).await;
                    cancel_operation(&mut catalog_task).await;
                    catalog_cache = None;
                    cancel_login_task(&mut login_task).await;
                    cancel_login_task(&mut sms_login_task).await;
                    commit_gate = login::LoginCommitGate::default();
                    cancel_operation(&mut api_task).await;
                    allow_load = false;
                    if let Some(client) = client.take() {
                        logout_task = Some(tokio::spawn(async move { client.logout().await }));
                    } else {
                        let _ = events.send(GuiEvent::LoggedOut(crate::client::LogoutOutcome {
                            remote_error: Some("没有可用账号会话，未发送服务端退出请求".into()),
                            local_error: None,
                        }));
                    }
                    stop_active_signal(&mut host_signal).await;
                }
                GuiCommand::Logout => {}
            }
        }

        if sms_task.as_ref().is_some_and(|s| s.task.is_finished()) {
            let active = sms_task.take().expect("finished SMS request");
            let outcome = active
                .task
                .await
                .unwrap_or_else(|error| login::sms::CodeOutcome {
                    dispatched: false,
                    result: Err(error.into()),
                });
            if active.generation == catalog_generation {
                if outcome.dispatched {
                    sms_gate.dispatched(active.phone.clone());
                }
                let result = outcome
                    .result
                    .map_err(|error| login::sms::error_message(&error, Some(&active.phone), None));
                tracing::info!(
                    success = result.is_ok(),
                    dispatched = outcome.dispatched,
                    "SMS code request completed"
                );
                let _ = events.send(GuiEvent::SmsCodeFinished {
                    generation: active.generation,
                    attempt: active.attempt,
                    phone: active.phone,
                    dispatched: outcome.dispatched,
                    result,
                });
            }
        }

        for method in [LoginMethod::Qr, LoginMethod::Phone] {
            let task = match method {
                LoginMethod::Qr => &mut login_task,
                LoginMethod::Phone => &mut sms_login_task,
            };
            if let Some(login) = task.as_ref() {
                while let Ok(progress) = login.progress.try_recv() {
                    let _ = events.send(GuiEvent::LoginProgress(
                        method,
                        login.generation,
                        login.attempt,
                        progress,
                    ));
                }
            }
            if !task.as_ref().is_some_and(|login| login.task.is_finished()) {
                continue;
            }
            let login = task.take().expect("finished login");
            if login.generation != catalog_generation {
                login.task.abort();
                let _ = login.task.await;
                continue;
            }
            let result = match login.task.await {
                Ok(Ok(prepared)) => prepared.commit().map(|_| ()),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
            };
            if result.is_ok() {
                // A single accepted result closes this login epoch. Errors
                // leave the other method alive; queued old events cannot win.
                cancel_login_task(&mut login_task).await;
                cancel_login_task(&mut sms_login_task).await;
                cancel_sms_task(&mut sms_task).await;
                sms_gate.cancel();
                catalog_generation = catalog_generation.wrapping_add(1);
                commit_gate = login::LoginCommitGate::default();
                allow_load = true;
                next_refresh = Instant::now();
            }
            let _ = events.send(GuiEvent::LoginFinished(
                method,
                login.generation,
                login.attempt,
                result.map_err(|error| format!("{error:#}")),
            ));
        }

        if assist_operation_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let query = assist_operation_is_query;
            match assist_operation_task
                .take()
                .expect("finished assist operation")
                .await
            {
                Ok((generation, sequence, result)) => {
                    if !query {
                        cancel_operation(&mut assist_lists_task).await;
                        next_assist_refresh = Instant::now();
                    }
                    let _ = events.send(GuiEvent::AssistOperation(
                        generation,
                        sequence,
                        result.map_err(assist::operation_error),
                    ));
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::AssistOperation(
                        assist_operation_context.0,
                        assist_operation_context.1,
                        Err(format!("远程协助操作中断：{error}")),
                    ));
                }
            }
        }
        if assist_lists_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            match assist_lists_task
                .take()
                .expect("finished assist list")
                .await
            {
                Ok((generation, result)) => {
                    let _ = events.send(GuiEvent::AssistLists(
                        generation,
                        result.map_err(assist::operation_error),
                    ));
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::AssistLists(
                        assist_lists_generation,
                        Err(format!("记录读取中断：{error}")),
                    ));
                }
            }
        }
        if logout_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = logout_task.take().expect("finished logout").await;
            match result {
                Ok(outcome) => {
                    let _ = events.send(GuiEvent::LoggedOut(outcome));
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::LoggedOut(crate::client::LogoutOutcome {
                        remote_error: Some(format!("退出后台任务异常：{error}")),
                        local_error: Some("后台退出未完成，凭据删除结果未知".into()),
                    }));
                }
            }
        }

        if let Some(signal) = &host_signal {
            while let Ok(event) = signal.events.try_recv() {
                match event {
                    PresenceEvent::State(state) => {
                        let _ = events.send(GuiEvent::Presence(state));
                    }
                    PresenceEvent::Warning(message) => {
                        let _ = events.send(GuiEvent::Warning(message));
                    }
                    PresenceEvent::DeviceChanged => {
                        next_refresh = Instant::now();
                        next_catalog = Instant::now();
                    }
                    PresenceEvent::AccountEnded => {}
                }
            }
        }
        if client.as_ref().is_some_and(|client| !client.is_active()) {
            cancel_operation(&mut assist_lists_task).await;
            cancel_operation(&mut assist_operation_task).await;
            cancel_sms_task(&mut sms_task).await;
            cancel_login_task(&mut login_task).await;
            cancel_login_task(&mut sms_login_task).await;
            commit_gate = login::LoginCommitGate::default();
            sms_gate.cancel();
            cancel_operation(&mut detail_task).await;
            cancel_operation(&mut mutation_task).await;
            cancel_operation(&mut catalog_task).await;
            catalog_cache = None;
            cancel_operation(&mut api_task).await;
            client = None;
            allow_load = false;
            let _ = events.send(GuiEvent::AccountEnded(
                "本虚拟设备的账号会话已结束，正在关闭观看连接；请重新登录".into(),
            ));
            stop_active_signal(&mut host_signal).await;
        }

        if detail_task.as_ref().is_some_and(JoinHandle::is_finished)
            && let Ok((id, result)) = detail_task.take().expect("finished detail").await
        {
            let _ = events.send(GuiEvent::Detail(
                catalog_generation,
                id,
                result.map_err(|e| format!("{e:#}")),
            ));
        }

        if mutation_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = mutation_task.take().expect("finished mutation").await;
            let result = result
                .unwrap_or_else(|e| Err(e.into()))
                .map_err(|e| format!("{e:#}"));
            // Discard a metadata query submitted before the write; refresh both
            // lists even on an unknown result. Do not repeat the write.
            cancel_operation(&mut api_task).await;
            cancel_operation(&mut catalog_task).await;
            cancel_operation(&mut detail_task).await;
            next_refresh = Instant::now();
            next_catalog = Instant::now();
            force_catalog = true;
            let _ = events.send(GuiEvent::MutationFinished(result));
        }

        if catalog_task.as_ref().is_some_and(JoinHandle::is_finished) {
            match catalog_task.take().expect("finished catalog").await {
                Ok((result, name)) => {
                    if let Ok(catalog) = &result {
                        catalog_cache = Some(catalog.clone());
                    }
                    let _ = events.send(GuiEvent::Catalog(
                        catalog_generation,
                        result.map_err(|e| format!("{e:#}")),
                        name,
                    ));
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::Catalog(
                        catalog_generation,
                        Err(format!("{error}")),
                        String::new(),
                    ));
                }
            }
        }

        if api_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = api_task.take().expect("finished API operation").await;
            match result {
                Ok(Ok(GuiOperation::Devices(devices))) => {
                    let _ = events.send(GuiEvent::Devices(catalog_generation, *devices));
                }
                Ok(Err(error)) => {
                    if let Some(active) = &client
                        && active.restoration_failed().await
                    {
                        allow_load = false;
                        client = None;
                        let _ = events.send(GuiEvent::SessionUnavailable(format!(
                            "登录恢复未完成，凭据已保留，可点击登录重试：{error:#}"
                        )));
                    } else {
                        let _ = events.send(GuiEvent::Warning(format!("设备请求失败：{error:#}")));
                    }
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::Error(format!("设备请求任务异常：{error}")));
                }
            }
        }

        if let Some(signal) = host_signal.as_ref()
            && signal.task.is_finished()
        {
            let signal = host_signal.take().expect("finished presence");
            presence_stopped = true;
            if let Err(error) = signal.task.await.unwrap_or_else(|error| Err(error.into())) {
                let _ = events.send(GuiEvent::Warning(format!("本机在线会话已停止：{error:#}")));
            }
            let _ = events.send(GuiEvent::Presence(PresenceState::Offline));
        }

        if client.is_none()
            && allow_load
            && login_task.is_none()
            && sms_login_task.is_none()
            && sms_task.is_none()
            && logout_task.is_none()
            && Instant::now() >= next_refresh
        {
            match AuthenticatedClient::from_saved_session_with_device(device_runtime.handle()) {
                Ok(loaded) => {
                    client = Some(Arc::new(loaded));
                    presence_stopped = false;
                }
                Err(error) => {
                    allow_load = false;
                    let event = if error
                        .downcast_ref::<crate::client::NoSavedSession>()
                        .is_some()
                    {
                        GuiEvent::SignedOut
                    } else {
                        GuiEvent::SessionUnavailable(format!("无法打开账号会话：{error:#}"))
                    };
                    let _ = events.send(event);
                }
            }
        }
        if let Some(active_client) = &client {
            let refresh_active = *foreground.borrow();
            if refresh_active
                && assist_lists_task.is_none()
                && Instant::now() >= next_assist_refresh
                && logout_task.is_none()
                && (assist_operation_task.is_none() || assist_operation_is_query)
            {
                let client = Arc::clone(active_client);
                let generation = catalog_generation;
                assist_lists_generation = generation;
                assist_lists_task = Some(tokio::spawn(async move {
                    (generation, client.assist_lists().await)
                }));
                next_assist_refresh = Instant::now() + Duration::from_secs(30);
            }
            if refresh_active && catalog_task.is_none() && Instant::now() >= next_catalog {
                let client = Arc::clone(active_client);
                let previous = catalog_cache.clone();
                let force = std::mem::take(&mut force_catalog);
                let progress = events.clone();
                let generation = catalog_generation;
                let foreground = foreground.clone();
                catalog_task = Some(tokio::spawn(async move {
                    let result = catalog::Catalog::load(
                        Arc::clone(&client),
                        previous,
                        force,
                        foreground,
                        |catalog| {
                            let _ = progress.send(GuiEvent::Catalog(
                                generation,
                                Ok(catalog.clone()),
                                client.account_name(),
                            ));
                        },
                    )
                    .await;
                    (result, client.account_name())
                }));
                next_catalog = Instant::now() + Duration::from_secs(30);
            }
            if refresh_active && api_task.is_none() && Instant::now() >= next_refresh {
                let client = Arc::clone(active_client);
                let _ = events.send(GuiEvent::Working("正在刷新设备状态".into()));
                api_task = Some(tokio::spawn(async move {
                    client
                        .list_devices()
                        .await
                        .map(Box::new)
                        .map(GuiOperation::Devices)
                }));
                next_refresh = Instant::now() + refresh_interval;
            }
            if host_signal.is_none() && !presence_stopped {
                host_signal = Some(ActivePresence::start(Arc::clone(active_client)));
            }
        }
    }
    cancel_login_task(&mut login_task).await;
    cancel_login_task(&mut sms_login_task).await;
    cancel_operation(&mut assist_lists_task).await;
    cancel_operation(&mut assist_operation_task).await;
    cancel_sms_task(&mut sms_task).await;
    cancel_operation(&mut api_task).await;
    cancel_operation(&mut catalog_task).await;
    cancel_operation(&mut detail_task).await;
    // A user-confirmed mutation may already have reached the service. Let its
    // acknowledgement/reconciliation and own-name persistence finish on exit.
    if let Some(task) = mutation_task {
        let _ = task.await;
    }
    // Once explicitly requested, logout still commits its local cleanup even
    // if the device-center window closes while the HTTP response is pending.
    if let Some(task) = logout_task {
        let _ = task.await;
    }
    stop_active_signal(&mut host_signal).await;
    device_runtime.close().await;
}

async fn stop_active_signal(active_signal: &mut Option<ActivePresence>) {
    if let Some(signal) = active_signal.take() {
        signal.close().await;
    }
}

struct ChildSession {
    device_id: Option<String>,
    alias: String,
    child: Child,
    owner: ViewerOwner,
}

struct StatusMessage {
    text: String,
    kind: StatusKind,
}

#[derive(Clone, Copy)]
enum StatusKind {
    Info,
    Success,
    Warning,
    Error,
}

impl StatusKind {
    fn is_alert(self) -> bool {
        matches!(self, Self::Warning | Self::Error)
    }
}

impl StatusMessage {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Info,
        }
    }

    fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Success,
        }
    }

    fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Warning,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Error,
        }
    }
}

fn all_devices(devices: &DeviceList) -> impl Iterator<Item = (&'static str, &DeviceInfo)> {
    devices
        .my_binded_devices
        .iter()
        .map(|device| ("我的设备", device))
}

fn connectability_error(device: &DeviceInfo) -> std::result::Result<(), String> {
    if !device.is_connected() {
        return Err(format!("{} 当前离线", display_alias(device)));
    }
    if !device.controlled_support || !device.controllable {
        return Err(format!("{} 当前没有开放远程控制", display_alias(device)));
    }
    if device.participant_count() != 0 {
        return Err(format!(
            "{} 已有 {} 名参与者，程序不会强制抢占",
            display_alias(device),
            device.participant_count()
        ));
    }
    Ok(())
}

fn display_alias(device: &DeviceInfo) -> &str {
    if device.alias.is_empty() {
        "未命名设备"
    } else {
        &device.alias
    }
}

fn frame_rate_argument(choice: FrameRateChoice) -> &'static str {
    match choice {
        FrameRateChoice::Auto => "auto",
        FrameRateChoice::Fps144 => "144",
        FrameRateChoice::Fps90 => "90",
        FrameRateChoice::Fps60 => "60",
        FrameRateChoice::Fps30 => "30",
    }
}

fn codec_argument(choice: CodecPreference) -> &'static str {
    match choice {
        CodecPreference::Auto => "auto",
        CodecPreference::H264 => "h264",
        CodecPreference::H265 => "h265",
    }
}

fn transport_argument(choice: TransportChoice) -> &'static str {
    match choice {
        TransportChoice::Auto => "auto",
        TransportChoice::P2p => "p2p",
        TransportChoice::Relay => "relay",
    }
}

fn qr_texture(ctx: &egui::Context, content: &str) -> Result<egui::TextureHandle> {
    let code = qrcode::QrCode::with_error_correction_level(content.as_bytes(), qrcode::EcLevel::L)
        .context("encode QR payload")?;
    let module_width = code.width();
    let quiet_zone = 4usize;
    let image_width = module_width + quiet_zone * 2;
    let mut pixels = vec![egui::Color32::WHITE; image_width * image_width];
    for (index, color) in code.to_colors().into_iter().enumerate() {
        if color == qrcode::Color::Dark {
            let source_x = index % module_width;
            let source_y = index / module_width;
            let target = (source_y + quiet_zone) * image_width + source_x + quiet_zone;
            pixels[target] = egui::Color32::BLACK;
        }
    }
    let image = egui::ColorImage::new([image_width, image_width], pixels);
    Ok(ctx.load_texture("uu-login-qr", image, egui::TextureOptions::NEAREST))
}
