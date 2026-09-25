//! Native graphical device center and host-presence lifecycle.

use crate::account::api::{DeviceInfo, DeviceList};
use crate::account::client::AuthenticatedClient;
use crate::account::login::{self, LoginProgress};
use crate::media::{
    CodecPreference, ConnectionMediaOptions, FrameRateChoice, LocalDisplayInfo, TransportChoice,
    detect_local_display,
};
use crate::session::presence::PresenceState;
use anyhow::{Context, Result};
use assist::{AssistOperation, AssistUi};
use messages::{DeviceMutation, GuiCommand, GuiEvent, MutationOutcome};
use phone::{LoginMethod, PhoneForm};
use runtime::GuiWorker;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use view::{CenterUi, configure_visuals};

mod assist;
mod catalog;
pub(crate) mod device_status;
mod device_sync;
mod diagnostics;

pub mod instance;
mod phone;
mod power;
mod updates;
mod view;

const WORKER_TICK: Duration = Duration::from_millis(250);

pub struct GuiOptions {
    pub media: ConnectionMediaOptions,
}

pub fn run(options: GuiOptions) -> Result<()> {
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

    let viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} · 控制中心", crate::APP_NAME))
        .with_icon(crate::ui::branding::icon())
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([1024.0, 720.0]);
    crate::ui::run(
        crate::ui::WindowConfig {
            viewport,
            centered: true,
        },
        Box::new(move |ctx, graphics| {
            crate::application::viewer::install_system_cjk_font(ctx);
            configure_visuals(ctx);
            ctx.request_repaint();
            let mut app = DeviceCenterApp::new(ctx, local_display, options.media, display_warning);
            if let Some(graphics) = graphics {
                app.diagnostics.graphics.push(graphics);
            }
            Box::new(app)
        }),
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StartupStage {
    #[default]
    Identity,
    Credentials,
    Media,
    Device,
    Account,
    Devices,
}
impl StartupStage {
    const ALL: [Self; 6] = [
        Self::Identity,
        Self::Credentials,
        Self::Media,
        Self::Device,
        Self::Account,
        Self::Devices,
    ];
    fn index(self) -> usize {
        match self {
            Self::Identity => 0,
            Self::Credentials => 1,
            Self::Media => 2,
            Self::Device => 3,
            Self::Account => 4,
            Self::Devices => 5,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Identity => "读取本机身份",
            Self::Credentials => "读取登录凭据",
            Self::Media => "检查本机媒体能力",
            Self::Device => "初始化设备会话",
            Self::Account => "校验账号登录状态",
            Self::Devices => "获取设备清单",
        }
    }
    fn detail(self) -> &'static str {
        match self {
            Self::Identity => "正在读取系统凭据库中的本机身份并启动会话服务。",
            Self::Credentials => "正在读取已保存的账号凭据，准备恢复上次登录。",
            Self::Media => "正在检查显示器和视频编码能力，完成后连接将复用检查结果。",
            Self::Device => "正在与 UU 服务同步本机身份，建立设备会话。",
            Self::Account => "正在向 UU 服务验证已保存的登录凭据并读取账号信息。",
            Self::Devices => "正在获取账号设备列表和在线状态，首批设备就绪后进入主界面。",
        }
    }
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
    extra_details: std::collections::HashMap<
        String,
        std::result::Result<crate::account::api::DeviceDetail, String>,
    >,
    detail_pending: Option<String>,
    account_name: String,
    diagnostics: diagnostics::LocalDiagnostics,
    mutation_pending: bool,
    queued_mutation: Option<DeviceMutation>,
    power_progress: std::collections::BTreeMap<String, power::PowerProgress>,
    pending_power: Option<power::PendingPower>,
    selected_device_id: Option<String>,
    local_display: LocalDisplayInfo,
    media: ConnectionMediaOptions,
    presence: PresenceState,
    host: Option<crate::features::host::Handle>,
    status: StatusMessage,
    refreshed_at: Option<Instant>,
    refresh_pending: bool,
    active_session: Option<ViewingSession>,
    opening_viewer: bool,
    closing_session: bool,
    close_confirmation: bool,
    close_confirmed: bool,
    logout_confirmation: bool,
    takeover_confirmation: Option<(u64, DeviceInfo)>,
    logout_pending: bool,
    logout_sent: bool,
    login_generation: u64,
    qr_generation: u64,
    qr_running: bool,
    phone: PhoneForm,
    login_restoring: bool,
    startup_stage: StartupStage,
    startup_stage_since: Instant,
    login_running: bool,
    login_status: String,
    login_qr: Option<egui::TextureHandle>,
    login_error: Option<String>,
}

impl DeviceCenterApp {
    fn new(
        ctx: &egui::Context,
        local_display: LocalDisplayInfo,
        media: ConnectionMediaOptions,
        display_warning: Option<String>,
    ) -> Self {
        Self {
            brand_texture: crate::ui::branding::load_texture(ctx),
            updates: updates::UpdateCheck::start(ctx),
            center_ui: CenterUi::default(),
            opening_viewer: false,
            assist: AssistUi::default(),
            worker: GuiWorker::spawn(),
            devices: None,
            catalog: None,
            catalog_error: None,
            extra_details: Default::default(),
            detail_pending: None,
            account_name: String::new(),
            diagnostics: diagnostics::LocalDiagnostics::start(),
            mutation_pending: false,
            queued_mutation: None,
            power_progress: Default::default(),
            pending_power: None,
            selected_device_id: None,
            local_display,
            media,
            presence: PresenceState::Connecting,
            host: None,
            status: display_warning.map_or_else(
                || StatusMessage::info("正在读取设备并建立本机在线状态"),
                StatusMessage::warning,
            ),
            refreshed_at: None,
            refresh_pending: true,
            active_session: None,
            closing_session: false,
            close_confirmation: false,
            close_confirmed: false,
            logout_confirmation: false,
            takeover_confirmation: None,
            logout_pending: false,
            logout_sent: false,
            login_generation: 0,
            qr_generation: 0,
            qr_running: false,
            phone: PhoneForm::default(),
            login_restoring: true,
            startup_stage: StartupStage::Identity,
            startup_stage_since: Instant::now(),
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
                GuiEvent::Viewer(generation, alias, device_id, result) => {
                    self.opening_viewer = false;
                    if generation != self.login_generation || self.logout_pending {
                        continue;
                    }
                    match result {
                        Ok(handle) => {
                            self.active_session = Some(ViewingSession {
                                device_id,
                                alias,
                                handle,
                            });
                            self.closing_session = false;
                        }
                        Err(error) => self.status = StatusMessage::error(error),
                    }
                }
                GuiEvent::Startup(generation, stage) => {
                    if generation == self.login_generation
                        && self.login_restoring
                        && !self.logout_pending
                        && self.startup_stage != stage
                    {
                        self.startup_stage = stage;
                        self.startup_stage_since = Instant::now();
                    }
                }
                GuiEvent::Presence(presence) => self.presence = presence,
                GuiEvent::Host(generation, host) => {
                    if generation == self.login_generation {
                        self.host = Some(host);
                    }
                }
                GuiEvent::PowerDispatched(generation, id, action) => {
                    if generation == self.login_generation
                        && self.mutation_pending
                        && !self.logout_pending
                    {
                        self.pending_power = Some(power::PendingPower {
                            id,
                            action,
                            saw_offline: false,
                        });
                    }
                }
                GuiEvent::Devices(generation, devices) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.observe_power(&devices);
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
                            // The worker owns hardware freshness and has already merged deltas.
                            self.extra_details.retain(|id, _| {
                                !catalog.groups.entries().any(|(_, d)| &d.device_id == id)
                            });
                            if self.detail_pending.as_ref().is_some_and(|id| {
                                !catalog.groups.entries().any(|(_, d)| &d.device_id == id)
                            }) {
                                self.detail_pending = None;
                            }
                            self.catalog = Some(catalog);
                            self.normalize_selection();
                            self.catalog_error = None;
                            self.login_restoring = false;
                        }
                        Err(error) => self.catalog_error = Some(error),
                    }
                }
                GuiEvent::MutationFinished(generation, result) => {
                    if generation != self.login_generation
                        || self.logout_pending
                        || self.login_running
                    {
                        continue;
                    }
                    self.mutation_pending = false;
                    self.status = match result {
                        Ok(MutationOutcome::Changed { message, .. }) => {
                            StatusMessage::success(message)
                        }
                        Ok(MutationOutcome::Power(accepted)) => {
                            self.accept_power(*accepted);
                            StatusMessage::info("电源请求已受理，正在观察设备状态")
                        }
                        Err(message) => StatusMessage::warning(message),
                    };
                    self.center_ui.finish_device_operation();
                    self.pending_power = None;
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
                            self.startup_stage = StartupStage::Device;
                            self.startup_stage_since = Instant::now();
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
                        session.handle.request_close();
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

        if let Some(session) = &mut self.active_session
            && let Some(target) = session.handle.info().and_then(|info| info.target)
        {
            session.device_id = Some(target.device_id);
            session.alias = target.alias;
        }
        let finished = self.active_session.as_ref().and_then(|session| {
            session
                .handle
                .result()
                .map(|result| (session.alias.clone(), result))
        });
        if let Some((alias, result)) = finished {
            tracing::info!(device = %alias, outcome = ?result, "viewing window ended");
            self.active_session = None;
            self.closing_session = false;
            self.status = match result {
                Ok(crate::session::controller::windows::ViewerEnd::Closed) => {
                    StatusMessage::success(format!("{alias} 的观看窗口已关闭"))
                }
                Ok(crate::session::controller::windows::ViewerEnd::RoomReleased) => {
                    StatusMessage::notice(format!(
                        "与 {alias} 的连接已结束，可能已被其他设备断开或接管"
                    ))
                }
                Ok(crate::session::controller::windows::ViewerEnd::TakeoverRequired(device)) => {
                    self.takeover_confirmation = Some((self.login_generation, *device));
                    StatusMessage::info("等待确认接管")
                }
                Err(error) => StatusMessage::error(format!(
                    "{alias} 的观看窗口已停止（{error}），请查看诊断日志"
                )),
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
            all_devices(devices).any(|(_, device)| {
                device.device_id == *selected && self.show_in_watching_list(device)
            })
        });
        if !selected_exists {
            self.selected_device_id = all_devices(devices)
                .filter(|(_, device)| self.show_in_watching_list(device))
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
        self.host = None;
        self.center_ui.clear_wallpapers();
        self.assist = AssistUi::default();
        self.extra_details.clear();
        self.detail_pending = None;
        self.queued_mutation = None;
        self.power_progress.clear();
        self.pending_power = None;
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
                .is_some_and(|c| c.virtual_status(&device.device_id) == Some(false))
    }

    fn watching_list_resolution(&self) -> (usize, usize) {
        let Some(list) = &self.devices else {
            return (0, 0);
        };
        let (mut pending, mut unresolved) = (0, 0);
        for (_, device) in all_devices(list).filter(|(_, d)| matches!(d.platform, 1 | 4)) {
            if self
                .catalog
                .as_ref()
                .and_then(|c| c.virtual_status(&device.device_id))
                .is_some()
            {
                continue;
            }
            let failed = self.catalog_error.is_some()
                || self.catalog.as_ref().is_some_and(|c| {
                    c.details.contains_key(&device.device_id)
                        || !c
                            .groups
                            .desktop_devices
                            .iter()
                            .any(|d| d.device_id == device.device_id)
                });
            if failed {
                unresolved += 1;
            } else {
                pending += 1;
            }
        }
        (pending, unresolved)
    }

    fn open_details(&mut self, id: String) {
        self.selected_device_id = Some(id.clone());
        self.center_ui.open_details(id.clone());
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
        if let DeviceMutation::Power { device, action } = &change
            && let Err(error) = self.power_available(device, *action)
        {
            self.status = StatusMessage::warning(error.to_string());
            return;
        }
        self.mutation_pending = true;
        let close_target = match &change {
            DeviceMutation::Remove { id } => Some(id),
            DeviceMutation::Power { device, action }
                if *action != crate::account::power::PowerAction::Wake =>
            {
                Some(&device.device_id)
            }
            _ => None,
        };
        if close_target.is_some_and(|id| {
            self.active_session
                .as_ref()
                .is_some_and(|s| s.device_id.as_ref().is_none_or(|target| target == id))
        }) {
            self.stop_viewer();
            self.status = StatusMessage::info("正在正常结束观看，随后执行已确认的设备操作");
            self.queued_mutation = Some(change);
        } else {
            self.send_mutation(change);
        }
    }

    fn send_mutation(&mut self, change: DeviceMutation) {
        self.pending_power = None;
        if self
            .worker
            .commands
            .send(GuiCommand::Mutate {
                generation: self.login_generation,
                change,
            })
            .is_err()
        {
            self.mutation_pending = false;
            self.status = StatusMessage::error("后台服务已停止，未发送设备操作");
            self.pending_power = None;
        } else {
            self.status = StatusMessage::info("正在处理设备操作…");
        }
    }

    fn request_refresh(&mut self) {
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
        if let Err(message) = self.connection_issue(&device) {
            self.status = StatusMessage::warning(message);
            return;
        }
        if let Some(session) = &self.active_session {
            if session.device_id.as_ref() == Some(&device.device_id) {
                session.handle.focus();
                return;
            }
            self.status = StatusMessage::warning("已有观看窗口正在运行");
            return;
        }

        if self.needs_takeover(&device) {
            self.takeover_confirmation = Some((self.login_generation, device));
            return;
        }
        self.spawn_viewer(
            display_alias(&device).to_owned(),
            Some(device.device_id.clone()),
            None,
        );
    }

    fn viewer_action_issue(&self, device: &DeviceInfo) -> Option<String> {
        if self.logout_pending {
            Some("正在退出账号".into())
        } else if self.mutation_pending {
            Some("正在处理设备操作".into())
        } else if self.opening_viewer {
            Some("正在打开观看窗口".into())
        } else if self
            .active_session
            .as_ref()
            .is_some_and(|session| session.device_id.as_deref() != Some(device.device_id.as_str()))
        {
            Some("请先关闭当前观看窗口".into())
        } else {
            self.connection_issue(device).err()
        }
    }

    fn connection_issue(&self, device: &DeviceInfo) -> std::result::Result<(), String> {
        if self.devices.as_ref().is_some_and(|list| {
            crate::session::controller::has_gui_connection(
                &list.current_device.device_id,
                &device.device_id,
            )
        }) && device.is_connected()
            && device.controllable
            && device.controlled_support
        {
            return Ok(());
        }
        connectability_error(device)
    }

    fn needs_takeover(&self, device: &DeviceInfo) -> bool {
        device.participant_count() > 0
            && !self.devices.as_ref().is_some_and(|list| {
                crate::session::controller::has_gui_connection(
                    &list.current_device.device_id,
                    &device.device_id,
                )
            })
    }

    fn spawn_viewer(
        &mut self,
        alias: String,
        device_id: Option<String>,
        assist_request: Option<crate::account::assist::AssistRequest>,
    ) {
        self.spawn_viewer_with_takeover(alias, device_id, assist_request, None);
    }

    fn spawn_viewer_with_takeover(
        &mut self,
        alias: String,
        device_id: Option<String>,
        assist_request: Option<crate::account::assist::AssistRequest>,
        takeover: Option<crate::session::controller::takeover::Approval>,
    ) {
        if self.opening_viewer {
            return;
        }
        self.diagnostics.cancel_probe();
        let background = device_id.as_ref().and_then(|id| {
            self.devices.as_ref().and_then(|list| {
                list.my_binded_devices
                    .iter()
                    .find(|d| &d.device_id == id)
                    .map(|d| {
                        crate::application::wallpaper::Source::new(&d.device_id, &d.wallpaper_url)
                    })
            })
        });
        self.opening_viewer = true;
        if self
            .worker
            .commands
            .send(GuiCommand::View {
                generation: self.login_generation,
                alias,
                device_id,
                assist: assist_request,
                options: self.media,
                background,
                takeover,
            })
            .is_err()
        {
            self.opening_viewer = false;
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
    fn stop_viewer(&mut self) {
        if let Some(session) = &self.active_session {
            session.handle.request_close();
            self.closing_session = true;
        }
    }
}

impl crate::ui::App for DeviceCenterApp {
    fn on_close_requested(&mut self) -> bool {
        if self.close_confirmed {
            return true;
        }
        let has_viewer = self.opening_viewer
            || self
                .active_session
                .as_ref()
                .is_some_and(|session| session.handle.result().is_none());
        if has_viewer
            || crate::features::port_mapping::service::active_service_count() > 0
            || crate::features::file_transfer::service::active_count() > 0
        {
            self.close_confirmation = true;
            false
        } else {
            true
        }
    }

    fn on_focus_changed(&mut self, focused: bool) {
        if !focused {
            self.center_ui.shortcuts.cancel_recording();
        }
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
        crate::application::viewer_shortcuts::refresh();
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        self.tick_power();
        self.draw_center(ui);
        self.draw_dialogs(&ctx);
        self.display_driver_dialogs(&ctx);
        if !self.close_confirmation && !self.login_restoring && !self.login_running {
            self.update_dialog(&ctx);
        }
        ui.ctx().request_repaint_after(WORKER_TICK);
    }

    fn on_exit(&mut self) {
        if let Some(session) = &self.active_session {
            session.handle.request_close();
        }
        if self.logout_pending && !self.logout_sent {
            let _ = self.worker.commands.send(GuiCommand::Logout);
        }
        let _ = self.worker.commands.send(GuiCommand::Shutdown);
    }
}

struct ViewingSession {
    device_id: Option<String>,
    alias: String,
    handle: crate::session::controller::windows::ViewerHandle,
}

struct StatusMessage {
    text: String,
    kind: StatusKind,
}

#[derive(Clone, Copy)]
enum StatusKind {
    Info,
    Notice,
    Success,
    Warning,
    Error,
}

impl StatusKind {
    fn is_alert(self) -> bool {
        matches!(self, Self::Notice | Self::Warning | Self::Error)
    }
}

impl StatusMessage {
    fn notice(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Notice,
        }
    }
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
    Ok(())
}

fn display_alias(device: &DeviceInfo) -> &str {
    if device.alias.is_empty() {
        "未命名设备"
    } else {
        &device.alias
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

mod messages;
mod runtime;
