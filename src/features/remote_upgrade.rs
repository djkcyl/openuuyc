//! User-selected update of the official software on an existing Windows target.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::RichText;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use winit::window::WindowId;

use crate::account::client::AuthenticatedClient;
use crate::account::feature_ability::{Feature, FeaturePolicy};
use crate::features::stream_control::StreamControlHandle;
use crate::ui::{controls, theme};

// The official widget displays 20..0, then reconnects on the next 1s tick.
const RECONNECT_AFTER: Duration = Duration::from_secs(21);
const PREPARED_NOTICE: Duration = Duration::from_secs(10);

#[derive(Default)]
struct State {
    prompt: Option<(WindowId, &'static str)>,
    posting: bool,
    notice: Option<(WindowId, String)>,
    started: Option<Instant>,
    prepared: Option<Instant>,
}

struct Inner {
    client: Arc<AuthenticatedClient>,
    device_id: String,
    alias: String,
    version: String,
    policy: FeaturePolicy,
    state: Mutex<State>,
    changed: Notify,
    runtime: tokio::runtime::Handle,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct RemoteUpgrade(Arc<Inner>);

impl RemoteUpgrade {
    pub(crate) fn new(
        client: Arc<AuthenticatedClient>,
        device_id: String,
        alias: String,
        version: String,
        cancel: &CancellationToken,
    ) -> Self {
        let policy = client.feature_catalog().policy(1, &version);
        Self(Arc::new(Inner {
            client,
            device_id,
            alias,
            version,
            policy,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            runtime: tokio::runtime::Handle::current(),
            cancel: cancel.child_token(),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn prompt(&self, window: WindowId, feature: &'static str) {
        let mut state = self.state();
        if !state.posting && state.started.is_none() && !self.0.cancel.is_cancelled() {
            state.prompt = Some((window, feature));
        }
    }

    pub(crate) fn retire(&self) {
        self.0.cancel.cancel();
        self.state().prompt = None;
    }

    pub(crate) fn started(&self) -> bool {
        self.state().started.is_some()
    }

    pub(crate) fn owns_input(&self, window: WindowId) -> bool {
        let state = self.state();
        state.started.is_some()
            || state.prompt.is_some_and(|(owner, _)| owner == window)
            || state
                .notice
                .as_ref()
                .is_some_and(|(owner, _)| *owner == window)
    }

    pub(crate) fn receive(&self, code: i32) {
        if self.0.cancel.is_cancelled() {
            return;
        }
        let mut state = self.state();
        match code {
            -6 => {
                if state.started.is_none() {
                    tracing::info!("remote update start received; waiting for update reconnect");
                }
                state.started.get_or_insert_with(Instant::now);
                state.prompt = None;
                state.notice = None;
                state.prepared = None;
                self.0.changed.notify_waiters();
            }
            -8 if state.started.is_none() => state.prepared = Some(Instant::now()),
            _ => {}
        }
    }

    pub(crate) async fn wait_for_restart(&self) {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let started = self.state().started;
            if let Some(started) = started {
                tokio::time::sleep(RECONNECT_AFTER.saturating_sub(started.elapsed())).await;
                return;
            }
            notified.await;
        }
    }

    fn submit(&self, ctx: &egui::Context, window: WindowId, immediate: bool) {
        {
            let mut state = self.state();
            if state.posting || state.started.is_some() || self.0.cancel.is_cancelled() {
                return;
            }
            state.posting = true;
            state.prompt = None;
        }
        let this = self.clone();
        let ctx = ctx.clone();
        self.0.runtime.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = this.0.cancel.cancelled() => return,
                result = this.0.client.update_owned_device(&this.0.device_id, immediate) => result,
            };
            let mut state = this.state();
            state.posting = false;
            if state.started.is_none() {
                match result {
                    Ok(()) => tracing::info!(immediate, "remote update request accepted"),
                    Err(error) => state.notice = Some((window, format!("{error:#}"))),
                }
            }
            // HTTP acceptance is not an installation acknowledgement. Only
            // ReportError(-6) starts the update overlay and reconnect deadline.
            ctx.request_repaint();
        });
    }

    pub(crate) fn show(
        &self,
        ctx: &egui::Context,
        window: WindowId,
        control: &StreamControlHandle,
    ) {
        let (prompt, notice, started, prepared) = {
            let state = self.state();
            (
                state.prompt,
                state.notice.clone(),
                state.started,
                state.prepared,
            )
        };
        if let Some((owner, feature)) = prompt.filter(|(owner, _)| *owner == window) {
            let supported = self.0.policy.supports(Feature::ControlledUpdate);
            let (chosen, dismiss) =
                version_prompt(ctx, &self.0.alias, &self.0.version, feature, supported);
            if let Some(immediate) = chosen {
                control.mouse().disable();
                self.submit(ctx, owner, immediate);
            } else if dismiss {
                self.state().prompt = None;
            }
        } else if let Some((_, message)) = notice.filter(|(owner, _)| *owner == window) {
            if result_prompt(ctx, &message) {
                self.state().notice = None;
            }
        }
        if let Some(started) = started {
            if progress_prompt(ctx, started) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        } else if prepared.is_some_and(|at| at.elapsed() < PREPARED_NOTICE) {
            if prepared_notice(ctx) {
                self.state().prepared = None;
                let control = control.clone();
                let this = self.clone();
                let ctx = ctx.clone();
                self.0.runtime.spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = this.0.cancel.cancelled() => return,
                        result = control.stop_acquire_update() => result,
                    };
                    if let Err(error) = result {
                        this.state().notice =
                            Some((window, format!("延后安装结果未确认：{error:#}")));
                    }
                    ctx.request_repaint();
                });
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

fn version_prompt(
    ctx: &egui::Context,
    alias: &str,
    version: &str,
    feature: &str,
    supported: bool,
) -> (Option<bool>, bool) {
    let mut chosen = None;
    let mut dismiss = false;
    let response = egui::Modal::new(egui::Id::new("remote-version-too-low"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::REMOTE_UPGRADE_WIDTH.min((ctx.content_rect().width() - 80.0).max(260.0)),
            );
            dismiss =
                controls::dialog_header(ui, "被控端需要更新", controls::DialogIcon::Required, true);
            ui.label(format!("当前被控端不支持{feature}，请先更新。"));
            ui.add_space(12.0);
            controls::update_device_row(ui, alias, version);
            if supported {
                let (now, later) = controls::update_actions(
                    ui,
                    Some("立即更新"),
                    Some(("稍后再说", "通知被控端延后更新，不立即安装")),
                );
                if now {
                    chosen = Some(true);
                } else if later {
                    chosen = Some(false);
                }
            } else {
                ui.add_space(12.0);
                ui.label(
                    RichText::new("请在被控端手动更新 UU 远程后重试。")
                        .size(theme::COMPACT_TEXT)
                        .color(theme::MUTED),
                );
                dismiss |= controls::update_actions(ui, Some("知道了"), None).0;
            }
        });
    (chosen, dismiss || response.should_close())
}

fn result_prompt(ctx: &egui::Context, message: &str) -> bool {
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("remote-update-request-result"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::REMOTE_UPGRADE_WIDTH.min((ctx.content_rect().width() - 80.0).max(260.0)),
            );
            close =
                controls::dialog_header(ui, "未能确认更新结果", controls::DialogIcon::Error, true);
            egui::ScrollArea::vertical()
                .id_salt(("remote-update-error-message", message))
                .max_height(theme::UPDATE_MESSAGE_HEIGHT)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(message).wrap().selectable(true));
                });
            close |= controls::update_actions(ui, Some("知道了"), None).0;
        });
    close || response.should_close()
}

fn progress_prompt(ctx: &egui::Context, started: Instant) -> bool {
    let mut close = false;
    egui::Modal::new(egui::Id::new("remote-upgrade-progress"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::REMOTE_UPGRADE_WIDTH.min((ctx.content_rect().width() - 80.0).max(260.0)),
            );
            controls::dialog_header(ui, "正在更新被控端", controls::DialogIcon::Waiting, false);
            ui.label(RichText::new("连接会暂时中断，请稍候。").color(theme::MUTED));
            ui.add_space(16.0);
            controls::update_countdown(ui, 20_u64.saturating_sub(started.elapsed().as_secs()));
            close = controls::update_actions(
                ui,
                None,
                Some(("关闭观看", "仅关闭观看窗口，被控端会继续更新")),
            )
            .1;
        });
    close
}

fn prepared_notice(ctx: &egui::Context) -> bool {
    controls::update_prepared_notice(ctx)
}
