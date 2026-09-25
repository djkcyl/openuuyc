use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    api::{ApiFailure, RoomSession},
    client::AuthenticatedClient,
    media::{
        ConnectionMediaOptions, ConnectionMediaProfile, LocalDisplayInfo, VideoCodec,
        detect_local_display,
    },
    rtc::{ForwardedTrack, IceServer, MediaKind, NativePeer, RtpForwardConfig, RtpForwarder},
    signal::{NegotiationEvent, SignalFailure, SignalRole, SignalSession},
    stream_control::StreamControlHandle,
    viewer::{
        ConnectionProgress, NativeViewerSession, ViewerDisplayHandle, ViewerLaunchConfig,
        ViewerWindowEvent, run_connecting_viewer_window,
    },
};

pub type ConnectionProgressReporter = Arc<dyn Fn(ConnectionProgress) + Send + Sync>;
pub(crate) fn has_gui_connection(controller: &str, target: &str) -> bool {
    shared::get(&shared::key(controller, target)).is_some()
}
pub(crate) struct LocalConnectionActivity {
    pub viewing: bool,
    pub controlling: bool,
}
pub(crate) fn gui_connection_activity(
    controller: &str,
    target: &str,
) -> Option<LocalConnectionActivity> {
    shared::get(&shared::key(controller, target)).map(|session| session.activity())
}
mod assist;
mod shared;
pub(crate) mod takeover;
pub(crate) mod windows;

fn report_progress(
    reporter: Option<&ConnectionProgressReporter>,
    step: u8,
    title: impl Into<String>,
    detail: impl Into<String>,
) {
    if let Some(reporter) = reporter {
        reporter(ConnectionProgress::working(step, title, detail));
    }
}

pub struct ControllerConnection {
    peer: Arc<NativePeer>,
    forwarder: shared::ForwarderLease,
    profile: ConnectionMediaProfile,
    preference_writer: Option<crate::viewing_settings::PreferenceWriter>,
    audio_preference_writer: Option<crate::viewing_settings::PreferenceWriter>,
}

pub struct ConnectionSummary {
    pub alias: String,
    pub stream_fps: u32,
    pub codec: &'static str,
    pub display_detection_warning: Option<String>,
}

pub struct PlaybackSummary {
    pub track_id: String,
    pub codec: &'static str,
    pub payload_type: u8,
    pub requested_keyframe: bool,
    pub player: &'static str,
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow!("connection cancelled")),
        result = future => result,
    }
}

fn retry_session_failure(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<SignalFailure>(), Some(failure) if !matches!(failure, SignalFailure::Kicked))
}

fn room_released(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<SignalFailure>(),
        Some(SignalFailure::Kicked)
    )
}

async fn await_media_startup<T>(
    ended: impl std::future::Future<Output = Result<()>>,
    startup: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        result = ended => Err(result.err().unwrap_or_else(|| anyhow!("设备连接已结束"))),
        result = startup => result,
    }
}

struct ResolvedConnection {
    client: Arc<AuthenticatedClient>,
    target_device_id: String,
    controller_device_id: String,
    profile: ConnectionMediaProfile,
    transport: crate::media::TransportChoice,
    summary: ConnectionSummary,
    assist: Option<assist::AssistConnection>,
    preferences: Option<crate::stream_control::StreamControlPreferences>,
    audio_preferences: Option<crate::audio::AudioSettings>,
    target_platform: i32,
    target_version: String,
    refresh_after_upgrade: bool,
    background: Option<crate::wallpaper::Source>,
    takeover: Option<takeover::Approval>,
}

impl ResolvedConnection {
    async fn connect(
        &mut self,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        retries: &mut u32,
    ) -> Result<ControllerConnection> {
        let key = shared::key(&self.controller_device_id, &self.target_device_id);
        let _gate = shared::acquire_connection(&key, cancel).await?;
        if self.assist.is_none()
            && let Some(session) = shared::get(&key)
        {
            self.takeover = None;
            let mut connection = ControllerConnection::from_shared(session, self.profile, true)?;
            let session = Arc::clone(&connection.forwarder.session);
            let setup = async {
                let handle = connection.stream_control_handle();
                let store = self.client.viewing_settings_store(&self.target_device_id)?;
                if let Some(saved) = store.load().await? {
                    let preferences = crate::stream_control::StreamControlPreferences::from_saved(
                        saved,
                        self.profile,
                    );
                    if !handle.snapshot().ready {
                        let _ = handle.restore_preferences(preferences);
                    }
                }
                let mut audio = store
                    .load_audio()
                    .await?
                    .unwrap_or(crate::audio::AudioSettings {
                        volume: 100,
                        muted: false,
                    });
                audio.muted |= self.profile.muted;
                handle.audio().set_settings(audio);
                handle.set_feature_policy(
                    self.client
                        .feature_catalog()
                        .policy(self.target_platform, &self.target_version),
                );
                if self.target_platform == 1 {
                    handle.set_remote_upgrade(crate::remote_upgrade::RemoteUpgrade::new(
                        Arc::clone(&self.client),
                        self.target_device_id.clone(),
                        self.summary.alias.clone(),
                        self.target_version.clone(),
                        cancel,
                    ));
                }
                connection.preference_writer = Some(store.clone().bind(handle.clone()));
                connection.audio_preference_writer = Some(store.bind_audio(handle));
                connection.activate_viewing().await
            };
            if let Err(error) =
                cancellable(cancel, await_media_startup(session.ended(), setup)).await
            {
                drop(session);
                return Err(connection.close_after_startup_error(error).await);
            }
            return Ok(connection);
        }
        self.client.schedule_feature_refresh(true);
        if self.target_device_id == self.controller_device_id {
            bail!("cannot connect the virtual device to itself");
        }
        report_progress(reporter, 3, "创建远程会话", "正在创建会话并获取信令凭据");
        let room = if let Some(assist) = &mut self.assist {
            let reply = assist
                .join(&self.client, &self.controller_device_id, reporter, cancel)
                .await?;
            self.target_device_id = reply.publisher_device_id.clone();
            self.target_platform = reply.publisher_platform;
            self.target_version = reply.publisher_version_name.clone();
            if !reply.device_name.is_empty() {
                self.summary.alias = reply.device_name.clone();
            }
            RoomSession::from_assist(&reply)
        } else {
            // Consume before dispatch. Neither API failure nor a later room
            // reconnect may reuse this user confirmation.
            let force_join = if let Some(approval) = self.takeover.take() {
                cancellable(
                    cancel,
                    approval.verify(&self.client, &self.target_device_id),
                )
                .await?
            } else {
                false
            };
            loop {
                match cancellable(
                    cancel,
                    self.client.join_device(&self.target_device_id, force_join),
                )
                .await
                {
                    Ok(room) => break room,
                    Err(error) => {
                        if force_join {
                            return Err(error).context(
                                "接管请求未确认成功，未自动重试；请刷新设备状态后重新确认",
                            );
                        }
                        // F890D0: only retry the API outcomes classified by the
                        // official device-join owner; retries never force a takeover.
                        let retryable = error.downcast_ref::<ApiFailure>().is_some_and(|failure| {
                            !matches!(failure.code, -1 | 1120 | 2002 | 2006 | 2007 | 4042)
                        });
                        if cancel.is_cancelled() || !retryable || *retries >= 5 {
                            return Err(error);
                        }
                        *retries += 1;
                        report_progress(
                            reporter,
                            3,
                            "重新请求房间",
                            format!("{error}；3 秒后重试（{retries}/5）"),
                        );
                        cancellable(cancel, async {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            Ok(())
                        })
                        .await?;
                    }
                }
            }
        };
        if self.refresh_after_upgrade {
            // Rejoining the room establishes that the target is back online.
            // Refresh its real version before rebuilding feature policy.
            let devices = cancellable(cancel, self.client.list_devices()).await?;
            let device = devices
                .my_binded_devices
                .iter()
                .find(|device| device.device_id == self.target_device_id)
                .context("更新后的设备已不在当前账号中")?;
            anyhow::ensure!(device.platform == 1, "更新后的设备类型已变化");
            self.target_version = device.version_name.clone();
            self.refresh_after_upgrade = false;
        }
        let bitrate_limit = if room.international_connect {
            match cancellable(cancel, self.client.international_bitrate_limit()).await {
                Ok(limit) => limit,
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => {
                    tracing::warn!(%error,"international bitrate configuration unavailable");
                    None
                }
            }
        } else {
            None
        };
        let mut persistence_error = None;
        let store = match self.client.viewing_settings_store(&self.target_device_id) {
            Ok(store) => Some(store),
            Err(error) => {
                persistence_error = Some(error.to_string());
                None
            }
        };
        if self.preferences.is_none()
            && let Some(store) = &store
        {
            match cancellable(cancel, store.load()).await {
                Ok(Some(settings)) => {
                    self.preferences =
                        Some(crate::stream_control::StreamControlPreferences::from_saved(
                            settings,
                            self.profile,
                        ))
                }
                Ok(None) => {}
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => persistence_error = Some(error.to_string()),
            }
        }
        if let Some(preferences) = self.preferences.as_mut() {
            preferences.custom_bitrate_limit = bitrate_limit;
        }
        let mut audio_persistence_error = None;
        if self.audio_preferences.is_none() {
            let mut audio = crate::audio::AudioSettings {
                volume: 100,
                muted: false,
            };
            if let Some(store) = &store {
                match cancellable(cancel, store.load_audio()).await {
                    Ok(Some(saved)) => {
                        tracing::debug!(?saved, "loaded audio settings for this device");
                        audio = saved;
                    }
                    Ok(None) => {}
                    Err(error) if cancel.is_cancelled() => return Err(error),
                    Err(error) => audio_persistence_error = Some(error.to_string()),
                }
            }
            // Startup --mute is temporary; only subsequent user edits persist.
            audio.muted |= self.profile.muted;
            self.audio_preferences = Some(audio);
        }
        if let Some(preferences) = self.preferences {
            self.profile.stream_fps = preferences
                .settings
                .frame_rate
                .value(self.profile.local_display);
            self.profile.decoder_fps_cap = self
                .profile
                .local_display
                .refresh_hz
                .max(self.profile.stream_fps);
            self.summary.stream_fps = self.profile.stream_fps;
        }

        let mut connection = ControllerConnection::establish(
            room,
            &self.controller_device_id,
            self.profile,
            self.client
                .feature_catalog()
                .policy(self.target_platform, &self.target_version),
            if self.assist.is_some() {
                crate::control::ControlConnectType::Assistance
            } else {
                crate::control::ControlConnectType::Normal
            },
            self.transport,
            self.preferences,
            self.audio_preferences.expect("resolved audio settings"),
            reporter,
            cancel,
            crate::control::ControlPurpose::Viewing,
        )
        .await?;
        let handle = connection.stream_control_handle();
        if self.assist.is_none() {
            shared::register(key, &connection.forwarder.session, self.client.ended());
        }
        if self.assist.is_none() && self.target_platform == 1 {
            handle.set_remote_upgrade(crate::remote_upgrade::RemoteUpgrade::new(
                Arc::clone(&self.client),
                self.target_device_id.clone(),
                self.summary.alias.clone(),
                self.target_version.clone(),
                cancel,
            ));
        }
        handle.set_custom_bitrate_limit(bitrate_limit);
        handle.mouse().set_keyboard_platform(self.target_platform);
        handle.set_persistence_error(persistence_error);
        handle.set_audio_persistence_error(audio_persistence_error);
        connection.preference_writer = store.clone().map(|store| store.bind(handle.clone()));
        connection.audio_preference_writer = store.map(|store| store.bind_audio(handle));
        Ok(connection)
    }
}

pub async fn run_saved_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
) -> Result<()> {
    run_viewer_window(alias, options, target_id, None, None).await
}

pub async fn run_assist_viewer_window(
    alias: String,
    request: crate::assist::AssistRequest,
    options: ConnectionMediaOptions,
) -> Result<()> {
    request.validate()?;
    run_viewer_window(alias, options, None, Some(request), None).await
}

async fn run_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
    assist: Option<crate::assist::AssistRequest>,
    mut hosted: Option<windows::WindowContext>,
) -> Result<()> {
    let owns_presence = hosted.is_none();
    let background = hosted.as_ref().and_then(|h| h.background.clone());
    let takeover = hosted.as_mut().and_then(|h| h.takeover.take());
    let window_key = hosted.as_ref().map(|h| h.key.clone());
    let (progress_sender, progress_receiver) = std::sync::mpsc::channel();
    let (viewer_sender, viewer_receiver) = std::sync::mpsc::channel();
    let reporter: ConnectionProgressReporter = Arc::new(move |progress| {
        let _ = progress_sender.send(progress);
    });
    if let Some(background) =
        background.filter(|s| target_id.as_deref() == Some(s.device_id.as_str()))
    {
        reporter(ConnectionProgress::background(background));
    }
    let task_alias = alias.clone();
    let (display_sender, display_receiver) = oneshot::channel();
    let cancel = hosted
        .as_ref()
        .map(|h| h.cancel.clone())
        .unwrap_or_default();
    let owner_cancel = cancel.clone();
    let close_sender = viewer_sender.clone();
    let (monitor_sender, _monitor_receiver) = hosted
        .as_ref()
        .map(|h| (h.monitor.clone(), h.monitor.subscribe()))
        .unwrap_or_else(|| tokio::sync::watch::channel(None));
    let (target_sender, _target_receiver) = hosted
        .as_ref()
        .map(|h| (h.target.clone(), h.target.subscribe()))
        .unwrap_or_else(|| tokio::sync::watch::channel(None));
    let owner_task = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = owner_cancel.cancelled() => {let _=close_sender.send(ViewerWindowEvent::Close);},
            _ = async {
                if let Err(error) = tokio::signal::ctrl_c().await {
                    tracing::warn!(%error, "Ctrl+C listener unavailable");
                    std::future::pending::<()>().await;
                }
            } => {
                owner_cancel.cancel();
                let _ = close_sender.send(ViewerWindowEvent::Close);
            },
        }
    });
    let task_cancel = cancel.clone();
    let terminal_sender = viewer_sender.clone();
    let connection_task = tokio::spawn(async move {
        let mut reporter = reporter;
        let result = run_viewer_connection_owner(
            task_alias,
            options,
            ViewerConnectionWindow {
                sender: viewer_sender,
                display: display_receiver,
                owns_presence,
                monitor: monitor_sender,
                target: target_sender,
                target_id,
                assist,
                client: hosted.map(|h| h.client),
                takeover,
            },
            &task_cancel,
            &mut reporter,
        )
        .await;
        if !task_cancel.is_cancelled()
            && let Err(error) = &result
        {
            if room_released(error) || error.downcast_ref::<takeover::Required>().is_some() {
                // Return terminal leave / explicit takeover confirmation to
                // the owning UI without presenting a decoder/connection error.
                let _ = terminal_sender.send(ViewerWindowEvent::Close);
            } else {
                reporter(ConnectionProgress::failed(format!("{error:#}")));
            }
        }
        result
    });

    let window_result = if let Some(key) = window_key {
        crate::ui::window_manager::viewer(
            key,
            crate::viewer::windows_presenter::ConnectingWindowsRunConfig {
                alias,
                progress: progress_receiver,
                session: viewer_receiver,
                display_sender,
            },
        )
        .await
    } else {
        tokio::task::block_in_place(|| {
            run_connecting_viewer_window(alias, progress_receiver, viewer_receiver, display_sender)
        })
    };
    cancel.cancel();
    let _ = owner_task.await;
    // The owner observes cancellation in every network wait and joins cleanup;
    // aborting this task would discard an in-flight room/peer owner.
    let connection_result = connection_task
        .await
        .context("controller connection task stopped unexpectedly")?;
    if let Err(error) = &connection_result {
        tracing::debug!(%error, "connection owner finished");
    }
    window_result.and(connection_result)
}

struct ViewerConnectionWindow {
    client: Option<Arc<AuthenticatedClient>>,
    sender: std::sync::mpsc::Sender<ViewerWindowEvent>,
    display: oneshot::Receiver<ViewerDisplayHandle>,
    owns_presence: bool,
    monitor: tokio::sync::watch::Sender<Option<crate::performance::PerformanceMonitor>>,
    target: tokio::sync::watch::Sender<Option<windows::ViewerTarget>>,
    target_id: Option<String>,
    assist: Option<crate::assist::AssistRequest>,
    takeover: Option<takeover::Approval>,
}

async fn run_viewer_connection_owner(
    alias: String,
    options: ConnectionMediaOptions,
    window: ViewerConnectionWindow,
    cancel: &CancellationToken,
    reporter: &mut ConnectionProgressReporter,
) -> Result<()> {
    let ViewerConnectionWindow {
        sender: viewer_sender,
        display: display_receiver,
        owns_presence,
        monitor,
        target,
        target_id,
        assist,
        client: hosted_client,
        takeover,
    } = window;
    let presence_stop = CancellationToken::new();
    let mut presence_task = None;
    let mut account_owner = None;
    let result = async {
        let client = if let Some(client)=hosted_client.clone(){client}else{Arc::new(AuthenticatedClient::from_saved_session()?)};
        account_owner = Some(Arc::clone(&client));
        let mut resolved = if let Some(request) = assist {
            cancellable(cancel, assist::resolve(client, &alias, options, request, Some(reporter))).await?
        } else {
            cancellable(cancel, resolve_connection_with_client(client, &alias, options, Some(reporter), target_id.as_deref(), takeover)).await?
        };
        // Standalone processes keep the host presence room (设备在线状态；
        // 被控权限开关已从界面移除，不开放被控). The account-ended token
        // still closes the viewer so a revoked session cannot keep watching.
        if owns_presence {
            let client = Arc::clone(&resolved.client);
            let stop = presence_stop.clone();
            let cancel = cancel.clone();
            let sender = viewer_sender.clone();
            presence_task = Some(tokio::spawn(async move {
                let ended = client.ended();
                let presence = crate::presence::ActivePresence::start(client);
                let mut tick = tokio::time::interval(Duration::from_millis(250));
                loop {
                    tokio::select! {
                        biased;
                        _ = ended.cancelled() => {
                            cancel.cancel();
                            let _ = sender.send(ViewerWindowEvent::Close);
                            break;
                        },
                        _ = stop.cancelled() => break,
                        _ = cancel.cancelled() => break,
                        _ = tick.tick() => {
                            while let Ok(event) = presence.events.try_recv() {
                                if let crate::presence::PresenceEvent::Warning(message) = event {
                                    tracing::warn!(%message, "standalone device presence");
                                }
                            }
                        }
                    }
                }
                presence.close().await;
            }));
        }
        let mut display = cancellable(cancel, async { display_receiver.await.context("player display was not created") }).await?;
        let (switch_sender, mut switch_receiver) = tokio::sync::mpsc::channel::<crate::viewer::device_switch::SwitchRequest>(1);
        let mut retries = 0;
        loop {
            monitor.send_replace(None);
            target.send_replace(Some(windows::ViewerTarget {
                device_id: resolved.target_device_id.clone(), alias: resolved.summary.alias.clone(),
            }));
            let mut controller = match resolved.connect(Some(reporter), cancel, &mut retries).await {
                Ok(controller) => controller,
                Err(error) if !cancel.is_cancelled() && retry_session_failure(&error) && retries < 5 => {
                    retries += 1;
                    reporter(ConnectionProgress::working(4, "重建观看会话", format!("{error:#}；正在重新加入房间（{retries}/5）")));
                    continue;
                }
                Err(error) => return Err(error),
            };
            // F94230 resets the full-session retry budget on peer connected.
            monitor.send_replace(Some(controller.performance_monitor()));
            retries = 0;
            let prepared = {
                let startup_session = Arc::clone(&controller.forwarder.session);
                cancellable(cancel, await_media_startup(
                    startup_session.ended(),
                    controller.start_native_viewer_with_progress(
                        &resolved.summary.alias, Some(reporter), display,
                    ),
                )).await
            };
            let (mut viewer, playback) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => return Err(controller.close_after_startup_error(error).await),
            };
            // The decoder opened against the first frame's parameter sets after
            // the RTP forwarder started; only then is presentation known.
            let started = {
                let startup_session = Arc::clone(&controller.forwarder.session);
                cancellable(cancel, await_media_startup(
                    startup_session.ended(),
                    async {
                        tokio::time::timeout(Duration::from_secs(30), viewer.startup())
                            .await.context("first-frame decoder startup timeout")?
                    },
                )).await
            };
            if let Err(error) = started {
                return Err(controller.close_after_startup_error(error).await);
            }
            let route = controller.peer.selected_route_details().await.unwrap_or_else(|| "安全媒体通道已建立".into());
            reporter(ConnectionProgress::ready(format!("{route} · {} · {}", playback.codec, controller.performance_monitor().snapshot().decoder)));
            tracing::info!(device = %resolved.summary.alias, codec = playback.codec, track = %playback.track_id, "single-window viewer entered playback");
            let close = viewer.close_handle();
            let switcher = crate::viewer::device_switch::DeviceSwitcher::new(
                Arc::clone(&resolved.client), resolved.target_device_id.clone(), switch_sender.clone(), cancel.clone());
            viewer.set_device_switch(switcher.clone());
            if viewer_sender.send(ViewerWindowEvent::Playing(Box::new(viewer))).is_err() {
                let _ = controller.close().await;
                bail!("player window closed before playback");
            }
            if !cancel.is_cancelled() && let Some(assist) = &mut resolved.assist
                && let Err(error) = assist.remember_success(&resolved.client).await {
                tracing::warn!(%error, "connected assistance code was not saved");
                reporter(ConnectionProgress::ready(format!("{route} · 验证码未保存：{error}")));
            }
            let (stop_sender, stop_receiver) = oneshot::channel();
            let session_control = controller.stream_control_handle();
            let upgrade = session_control.remote_upgrade();
            let upgrade_deadline = async {
                if let Some(upgrade) = &upgrade {
                    upgrade.wait_for_restart().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(upgrade_deadline);
            let alive = controller.keep_alive(stop_receiver);
            tokio::pin!(alive);
            let mut next_connection = None;
            let mut upgrade_reconnect = false;
            let result = loop {
                tokio::select! {
                    result = &mut alive => break result,
                    _ = &mut upgrade_deadline => {
                        upgrade_reconnect = true;
                        let _ = stop_sender.send(());
                        break alive.await;
                    }
                    _ = cancel.cancelled() => {
                        let _ = stop_sender.send(());
                        break alive.await;
                    }
                    Some(request) = switch_receiver.recv() => {
                        if request.from != resolved.target_device_id || request.device.device_id == resolved.target_device_id { continue; }
                        // Revalidate the real ID while the old room continues playing.
                        let next = cancellable(cancel, resolve_connection_with_client(
                            Arc::clone(&resolved.client), &request.device.alias, options, None,
                            Some(&request.device.device_id), request.takeover)).await;
                        let next = match next {
                            Ok(next) => next,
                            Err(error) => {
                                if let Some(required) = error.downcast_ref::<takeover::Required>() {
                                    switcher.require_takeover(required.0.clone(), request.window);
                                } else {
                                    switcher.failed(format!("无法切换：{error}"));
                                }
                                continue;
                            }
                        };
                        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                        let (display_tx, display_rx) = oneshot::channel();
                        *reporter = Arc::new(move |progress| { let _ = progress_tx.send(progress); });
                        reporter(ConnectionProgress::working(1, "切换设备", format!("正在连接 {}", next.summary.alias)));
                        if let Some(background)=next.background.clone(){reporter(ConnectionProgress::background(background));}
                        // The UI releases input and joins every old screen/decoder,
                        // retaining the window in which the user selected the device.
                        let replacement = async {
                            viewer_sender.send(ViewerWindowEvent::Reconnect {
                                alias: next.summary.alias.clone(), window: Some(request.window),
                                progress: progress_rx, display: display_tx,
                            }).map_err(|_| anyhow!("player window closed during device switch"))?;
                            display_rx.await.context("player did not acknowledge device switch")
                        };
                        let new_display = cancellable(cancel, replacement).await;
                        let _ = stop_sender.send(());
                        let stopped = alive.await;
                        let new_display = new_display?;
                        if let Err(error) = stopped {
                            tracing::debug!(%error, "old viewing session ended during device switch");
                        }
                        next_connection = Some((next, new_display));
                        break Ok(());
                    }
                }
            };
            if cancel.is_cancelled() {
                close.close();
                // Closing the HWND can race the observed server leave. Keep
                // that reason for the main window instead of losing it here.
                return match result {
                    Err(error) if room_released(&error) => Err(error),
                    _ => Ok(()),
                };
            }
            if let Some((next, new_display)) = next_connection {
                if let Some(upgrade) = &upgrade { upgrade.retire(); }
                display = new_display;
                resolved = next;
                retries = 0;
                continue;
            }
            if upgrade.as_ref().is_some_and(|upgrade| upgrade.started()) {
                // A transport loss during installation is expected. Wait for
                // the official update countdown instead of ordinary retries.
                // The service also emits room leave/2005 here. Only a preceding
                // update-start notice permits this exception to terminal leave.
                if !upgrade_reconnect {
                    cancellable(cancel, async {
                        upgrade.as_ref().unwrap().wait_for_restart().await;
                        Ok(())
                    }).await?;
                }
                upgrade.as_ref().unwrap().retire();
                resolved.preferences = Some(session_control.preferences());
                resolved.audio_preferences = Some(session_control.audio().settings());
                resolved.refresh_after_upgrade = true;
                retries = 0;
                let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                let (display_tx, display_rx) = oneshot::channel();
                *reporter = Arc::new(move |progress| { let _ = progress_tx.send(progress); });
                reporter(ConnectionProgress::working(1, "更新后重新连接", format!("正在重新连接 {}", resolved.summary.alias)));
                if let Some(background) = resolved.background.clone() { reporter(ConnectionProgress::background(background)); }
                viewer_sender.send(ViewerWindowEvent::Reconnect {
                    alias: resolved.summary.alias.clone(), window: None,
                    progress: progress_rx, display: display_tx,
                }).map_err(|_| anyhow!("更新等待窗口已关闭"))?;
                display = cancellable(cancel, async {
                    display_rx.await.context("更新等待窗口已关闭")
                }).await?;
                continue;
            }
            if let Some(upgrade) = &upgrade { upgrade.retire(); }
            match result {
                Err(error) if retry_session_failure(&error) => {
                    resolved.preferences = Some(session_control.preferences());
                    resolved.audio_preferences = Some(session_control.audio().settings());
                    retries += 1;
                    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                    let (display_tx, display_rx) = oneshot::channel();
                    *reporter = Arc::new(move |progress| { let _ = progress_tx.send(progress); });
                    reporter(ConnectionProgress::working(4, "重建观看会话", format!("{error:#}；正在重新加入房间（{retries}/5）")));
                    if let Some(background)=resolved.background.clone(){reporter(ConnectionProgress::background(background));}
                    viewer_sender.send(ViewerWindowEvent::Reconnect { alias: resolved.summary.alias.clone(), window: None, progress: progress_rx, display: display_tx })
                        .map_err(|_| anyhow!("player window closed during reconnect"))?;
                    display = cancellable(cancel, async { display_rx.await.context("player did not acknowledge room replacement") }).await?;
                }
                result => { close.close(); return result; }
            }
        }
    }.await;
    monitor.send_replace(None);
    presence_stop.cancel();
    if let Some(task) = presence_task {
        let _ = task.await;
    }
    if hosted_client.is_none()
        && let Some(client) = account_owner
    {
        client.close().await;
    }
    if cancel.is_cancelled() && !result.as_ref().is_err_and(|error| room_released(error)) {
        Ok(())
    } else {
        result
    }
}

impl ControllerConnection {
    async fn close_after_startup_error(self, error: anyhow::Error) -> anyhow::Error {
        // Peer teardown can close RTP/decoder input before the signal task has
        // finished cleanup. Preserve the explicit leave instead of that symptom.
        match self.close().await {
            Err(ended) if room_released(&ended) => ended,
            _ => error,
        }
    }
    pub(crate) async fn connect_mapping(
        client: &AuthenticatedClient,
        device: &crate::api::DeviceInfo,
        policy: crate::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::control::ControlPurpose::PortMapping,
        )
        .await
    }
    pub(crate) async fn connect_files(
        client: &AuthenticatedClient,
        device: &crate::api::DeviceInfo,
        policy: crate::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::control::ControlPurpose::FileTransfer,
        )
        .await
    }
    async fn connect_business(
        client: &AuthenticatedClient,
        device: &crate::api::DeviceInfo,
        policy: crate::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
        purpose: crate::control::ControlPurpose,
    ) -> Result<Self> {
        let key = shared::key(&client.device_id(), &device.device_id);
        let _gate = shared::acquire_connection(&key, cancel).await?;
        let display = detect_local_display().unwrap_or(LocalDisplayInfo::FALLBACK);
        let mut profile = options.resolve(display)?;
        if let Ok(store) = client.viewing_settings_store(&device.device_id)
            && let Ok(Some(saved)) = store.load().await
            && let Some(settings) = saved.settings
        {
            profile.stream_fps = settings.frame_rate.value(display);
            profile.decoder_fps_cap = display.refresh_hz.max(profile.stream_fps);
        }
        if let Some(session) = shared::get(&key) {
            return Self::from_shared(session, profile, false);
        }
        if device.participant_count() > 0 && takeover.is_none() {
            return Err(takeover::Required(device.clone()).into());
        }
        let force_join = if let Some(approval) = takeover {
            cancellable(cancel, approval.verify(client, &device.device_id)).await?
        } else {
            false
        };
        let room = cancellable(cancel, client.join_device(&device.device_id, force_join))
            .await
            .map_err(|error| {
                if force_join {
                    error.context("接管请求未确认成功，未自动重试；请检查设备状态后重新确认")
                } else {
                    error
                }
            })?;
        let connection = Self::establish(
            room,
            &client.device_id(),
            profile,
            policy,
            crate::control::ControlConnectType::Normal,
            options.transport,
            None,
            crate::audio::AudioSettings {
                volume: 0,
                muted: true,
            },
            None,
            cancel,
            purpose,
        )
        .await?;
        shared::register(key, &connection.forwarder.session, client.ended());
        Ok(connection)
    }
    fn from_shared(
        session: Arc<shared::Session>,
        profile: ConnectionMediaProfile,
        viewing: bool,
    ) -> Result<Self> {
        Ok(Self {
            peer: Arc::clone(&session.peer),
            forwarder: session.lease(viewing)?,
            profile,
            preference_writer: None,
            audio_preference_writer: None,
        })
    }
    async fn activate_viewing(&self) -> Result<()> {
        self.wait_port_mapping_ready().await?;
        let handle = self.stream_control_handle();
        let snapshot = handle.snapshot();
        let screen = snapshot
            .screens
            .first()
            .context("被控端尚未提供显示器列表")?;
        handle.set_screen_capture(screen.id, true).await?;
        handle.set_viewing_enabled(true);
        Ok(())
    }
    pub(crate) fn port_mapping_transport(&self) -> Arc<crate::port_mapping::Transport> {
        self.peer.port_mapping()
    }
    pub(crate) async fn wait_port_mapping_ready(&self) -> Result<()> {
        let control = self.stream_control_handle();
        let changed = control.protocol_notifications();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let wake = changed.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if control.handshake_status().connected {
                    break;
                }
                wake.await;
            }
        })
        .await
        .context("端口转发协议握手未完成")?;
        self.port_mapping_transport().wait_ready().await
    }
    pub fn stream_control_handle(&self) -> StreamControlHandle {
        self.peer.stream_control_handle()
    }

    pub fn performance_monitor(&self) -> crate::performance::PerformanceMonitor {
        self.peer.performance_monitor()
    }

    async fn select_video_track(&mut self) -> Result<(ForwardedTrack, VideoCodec)> {
        let video = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let track = if let Some(track) = self.forwarder.selected_metadata() {
                    track
                } else {
                    self.forwarder
                        .next_track()
                        .await
                        .ok_or_else(|| anyhow!("remote RTP track channel closed"))?
                };
                match track.kind {
                    MediaKind::Audio => {}
                    MediaKind::Video => return Ok::<_, anyhow::Error>(track),
                }
            }
        })
        .await
        .context("remote device sent no video RTP within 12 seconds")??;

        let codec = match video.codec.to_ascii_lowercase() {
            value if value.contains("h265") || value.contains("hevc") => VideoCodec::H265,
            value if value.contains("h264") => VideoCodec::H264,
            _ => bail!(
                "remote device selected unsupported video codec {}",
                video.codec
            ),
        };
        let video_track_index = video
            .id
            .strip_prefix("video_")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(0);
        self.peer.set_stream_video_stream(codec, video_track_index);
        Ok((video, codec))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn establish(
        room: RoomSession,
        controller_device_id: &str,
        profile: ConnectionMediaProfile,
        features: crate::feature_ability::FeaturePolicy,
        connect_type: crate::control::ControlConnectType,
        transport: crate::media::TransportChoice,
        preferences: Option<crate::stream_control::StreamControlPreferences>,
        audio_settings: crate::audio::AudioSettings,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        purpose: crate::control::ControlPurpose,
    ) -> Result<Self> {
        tracing::info!(?profile, ?transport, "establishing controller connection");
        report_progress(
            reporter,
            4,
            "连接信令服务",
            "正在完成 TLS、WebSocket、Engine.IO 与 Socket.IO 房间握手",
        );
        let mut signal = SignalSession::connect_cancellable(room, SignalRole::Controller, cancel)
            .await
            .context("connect controller signaling")?;
        report_progress(
            reporter,
            4,
            "信令通道已连接",
            "Socket.IO 房间验证完成，保活与断线重连密钥已就绪",
        );
        report_progress(
            reporter,
            5,
            "控制握手",
            format!(
                "正在探测并上报解码能力；帧率上限 {} FPS，{}",
                profile.stream_fps,
                profile.codec.label()
            ),
        );
        let control = match cancellable(
            cancel,
            signal.start_control(
                controller_device_id,
                profile,
                connect_type,
                preferences,
                purpose,
            ),
        )
        .await
        {
            Ok(control) => control,
            Err(error) => {
                let _ = signal.close().await;
                return Err(error).context("complete controller handshake");
            }
        };
        report_progress(
            reporter,
            5,
            "控制握手已接受",
            format!(
                "远端返回 {} 组 ICE 服务；自动切路={}，强制 Relay={}",
                control.ice_servers.len(),
                if control.auto_switch_network {
                    "启用"
                } else {
                    "关闭"
                },
                if control.force_relay { "是" } else { "否" }
            ),
        );
        let transport = if control.force_relay {
            crate::media::TransportChoice::Relay
        } else {
            transport
        };
        report_progress(
            reporter,
            6,
            "创建安全传输栈",
            "正在初始化 ICE、DTLS-SRTP、SCTP 数据通道与 RTP/RTX/FEC 接收链",
        );
        let peer = match NativePeer::new_with_profile(
            control
                .ice_servers
                .iter()
                .map(|server| IceServer {
                    urls: vec![server.urls.clone()],
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect(),
            transport,
            profile,
        )
        .await
        {
            Ok(peer) => Arc::new(peer),
            Err(error) => {
                let _ = signal.close().await;
                return Err(error).context("create native WebRTC peer");
            }
        };
        peer.stream_control_handle().set_feature_policy(features);
        peer.stream_control_handle()
            .set_viewing_enabled(purpose == crate::control::ControlPurpose::Viewing);
        peer.stream_control_handle()
            .set_display_connection_type(connect_type);
        if let Some(preferences) = preferences
            && let Err(error) = peer
                .stream_control_handle()
                .restore_preferences(preferences)
        {
            let _ = peer.close().await;
            let _ = signal.close().await;
            return Err(error);
        }
        // Restore before media can arrive or the playback window is exposed.
        peer.stream_control_handle()
            .audio()
            .set_settings(audio_settings);
        peer.configure_network_control(transport, control.force_relay);
        report_progress(
            reporter,
            6,
            "安全传输栈已就绪",
            "本地媒体能力与控制数据通道已经创建，等待远端协商",
        );
        let forward_config = RtpForwardConfig {
            video_track_id: None,
        };
        let forwarder = match peer.install_rtp_forwarder(forward_config).await {
            Ok(forwarder) => forwarder,
            Err(error) => {
                let _ = peer.close().await;
                let _ = signal.close().await;
                return Err(error).context("install decrypted RTP forwarder");
            }
        };
        report_progress(
            reporter,
            7,
            "交换媒体能力",
            format!(
                "正在发送 SDP Offer 并协商 {}、RTP 扩展、RTX/FEC 与数据通道",
                profile.codec.label()
            ),
        );
        let answer_installed = AtomicBool::new(false);
        let negotiation_progress = |event| match event {
            NegotiationEvent::OfferCreated {
                sdp_bytes,
                compressed_bytes,
            } => report_progress(
                reporter,
                7,
                "本地媒体能力已生成",
                format!(
                    "SDP {sdp_bytes} 字节，压缩后 {compressed_bytes} 字节；包含视频、RTP 扩展、RTX/FEC 与数据通道"
                ),
            ),
            NegotiationEvent::OfferSent => report_progress(
                reporter,
                7,
                "等待远端媒体答复",
                "SDP Offer 已通过 SOAC 信令发送，正在等待 Answer 与远端候选",
            ),
            NegotiationEvent::LocalCandidateSent { count } => report_progress(
                reporter,
                if answer_installed.load(Ordering::Acquire) {
                    8
                } else {
                    7
                },
                "收集本地网络候选",
                format!("已向远端发送 {count} 个 ICE 候选，LAN/P2P/Relay 探测继续进行"),
            ),
            NegotiationEvent::AnswerInstalled { sdp_bytes } => {
                answer_installed.store(true, Ordering::Release);
                report_progress(
                    reporter,
                    8,
                    "远端媒体能力已确认",
                    format!(
                        "已安装 {sdp_bytes} 字节 SDP Answer，正在检查候选对并完成 DTLS-SRTP 握手"
                    ),
                );
            }
            NegotiationEvent::RemoteCandidateInstalled { count } => report_progress(
                reporter,
                8,
                "判断最佳连接线路",
                format!(
                    "已安装 {count} 个远端 ICE 候选，正在按优先级与连通性选择 LAN、P2P 或 Relay"
                ),
            ),
            NegotiationEvent::Connected => report_progress(
                reporter,
                8,
                "安全媒体通道已建立",
                "ICE 候选对、DTLS 握手与 SRTP 密钥协商均已完成",
            ),
        };
        if let Err(error) = cancellable(
            cancel,
            signal.negotiate(&control, &peer, Some(&negotiation_progress)),
        )
        .await
        {
            let _ = peer.close().await;
            let _ = signal.close().await;
            return Err(error).context("negotiate native WebRTC session");
        }
        let route = peer
            .selected_route_details()
            .await
            .unwrap_or_else(|| "安全媒体通道已连接，候选对详情尚未发布".to_owned());
        report_progress(reporter, 8, "连接线路已选定", route);
        peer.stream_control_handle()
            .network_control()
            .connected(true);
        let (signal_shutdown, signal_shutdown_receiver) = oneshot::channel();
        let signal_peer = Arc::clone(&peer);
        let signal_control = control.clone();
        let signal_task = tokio::spawn(signal.keep_alive(
            signal_shutdown_receiver,
            Some(signal_peer),
            Some(signal_control),
        ));
        tracing::info!("controller WebRTC negotiation complete");
        let session = shared::Session::new(peer, forwarder, signal_shutdown, signal_task);
        Self::from_shared(
            session,
            profile,
            purpose == crate::control::ControlPurpose::Viewing,
        )
    }

    pub async fn start_native_viewer(
        &mut self,
        alias: &str,
    ) -> Result<(NativeViewerSession, PlaybackSummary)> {
        let (mut viewer, summary) = self
            .start_native_viewer_with_progress(alias, None, ViewerDisplayHandle::default())
            .await?;
        // The progress-window entry awaits startup separately. This direct
        // API must consume it before run() selects the native presenter.
        tokio::time::timeout(Duration::from_secs(30), viewer.startup())
            .await
            .context("first-frame decoder startup timeout")??;
        Ok((viewer, summary))
    }

    pub(crate) async fn start_native_viewer_with_progress(
        &mut self,
        alias: &str,
        reporter: Option<&ConnectionProgressReporter>,
        display: ViewerDisplayHandle,
    ) -> Result<(NativeViewerSession, PlaybackSummary)> {
        let hardware_decode = self.profile.hardware_decode;
        report_progress(
            reporter,
            9,
            "等待视频轨道",
            "安全媒体通道已建立，正在等待远端发布桌面视频轨道",
        );
        let (video, codec) = self.select_video_track().await?;
        report_progress(
            reporter,
            10,
            "视频轨道已协商",
            format!(
                "{} · RTP PT {} · SSRC {} · 轨道 {}",
                match codec {
                    VideoCodec::H264 => "H.264/AVC",
                    VideoCodec::H265 => "H.265/HEVC",
                },
                video.payload_type,
                video.ssrc,
                video.id
            ),
        );
        let profile = self.profile;
        let track_index = video
            .id
            .strip_prefix("video_")
            .and_then(|id| id.parse().ok())
            .unwrap_or(0);
        let performance = self
            .peer
            .performance_monitor()
            .for_video_track(track_index as u64);
        performance.set_video_codec(match codec {
            VideoCodec::H264 => format!("H.264/AVC · RTP PT {}", video.payload_type),
            VideoCodec::H265 => format!("H.265/HEVC · RTP PT {}", video.payload_type),
        });
        let stream_control = self.peer.stream_control_handle();
        let receiver_feedback = self.forwarder.video_receiver_feedback();
        let title = format!("{}{alias}", crate::VIEWER_TITLE_PREFIX);
        report_progress(
            reporter,
            11,
            "初始化视频解码器",
            format!(
                "正在打开 {} {} 解码路径，实际画面尺寸以收到的码流为准",
                if hardware_decode {
                    "平台硬件优先"
                } else {
                    "软件"
                },
                match codec {
                    VideoCodec::H264 => "H.264",
                    VideoCodec::H265 => "H.265",
                },
            ),
        );
        let mut viewer = NativeViewerSession::launch(ViewerLaunchConfig {
            codec,
            hardware_decode,
            title,
            initial_width: profile.local_display.width,
            initial_height: profile.local_display.height,
            frame_rate: profile.stream_fps,
            receiver_feedback,
            performance: performance.clone(),
            stream_control,
            display,
        })
        .await?;
        viewer.attach_screen_playback(&self.peer, profile, alias, track_index);

        report_progress(
            reporter,
            11,
            "视频解码器已就绪",
            format!(
                "{} · {}",
                performance.snapshot().decoder,
                if hardware_decode {
                    "允许平台硬解，失败时由解码层报告实际后端"
                } else {
                    "已按用户设置禁用硬解优先"
                }
            ),
        );
        self.forwarder.add_video_sink(viewer.video_sink()).await;
        viewer.ensure_running()?;
        let keyframe_generation = self.forwarder.video_keyframe_generation();
        self.forwarder.start();
        if let Err(error) = self.peer.stream_control_handle().audio().start() {
            tracing::warn!(%error, "native audio did not start; video remains available");
        }
        report_progress(
            reporter,
            12,
            "同步首个完整画面",
            "已启动 RTP 接收，正在发送 PLI 并等待参数集、完整关键帧与首帧解码",
        );
        self.peer.request_keyframe(video.ssrc).await?;
        let keyframe_ready = self
            .forwarder
            .wait_for_video_keyframe_after(keyframe_generation, Duration::from_secs(2))
            .await;
        viewer.ensure_running()?;
        if !keyframe_ready {
            tracing::warn!("PLI 后 2 秒内未识别到完整参数集和关键帧，已继续等待后续关键帧");
            report_progress(
                reporter,
                12,
                "继续等待关键帧",
                "远端尚未在 2 秒内返回完整关键帧；接收链保持运行并继续请求恢复",
            );
        } else {
            report_progress(
                reporter,
                12,
                "首个完整画面已到达",
                "参数集与关键帧已通过接收门禁，播放器可以开始显示",
            );
        }
        let summary = PlaybackSummary {
            track_id: video.id,
            codec: match codec {
                VideoCodec::H264 => "H.264",
                VideoCodec::H265 => "H.265",
            },
            payload_type: video.payload_type,
            requested_keyframe: true,
            player: "原生窗口",
        };

        Ok((viewer, summary))
    }

    pub async fn keep_alive(mut self, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
        let result = tokio::select! {
            result = self.forwarder.session.ended() => result,
            _ = &mut shutdown => Ok(()),
        };
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        if let Some(writer) = &mut self.audio_preference_writer {
            writer.finish().await;
        }
        let closed = self.close().await;
        result.and(closed)
    }

    pub async fn close(mut self) -> Result<()> {
        if self.forwarder.viewing {
            let handle = self.stream_control_handle();
            handle.mouse().disable();
            handle.audio().suspend();
            // Local capture and clipboard permissions end before any network
            // wait. A shared file/port session must not keep a closing viewer's
            // microphone alive while screen-stop messages are backpressured.
            handle.set_viewing_enabled(false);
            if Arc::strong_count(&self.forwarder.session) > 1 {
                for screen in handle.snapshot().screens {
                    let _ = handle.set_screen_capture(screen.id, false).await;
                }
            }
        }
        let result = if Arc::strong_count(&self.forwarder.session) == 1 {
            self.forwarder.session.request_close();
            self.forwarder.session.ended().await
        } else {
            Ok(())
        };
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        if let Some(writer) = &mut self.audio_preference_writer {
            writer.finish().await;
        }
        result
    }
}

fn flatten_signal_task(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    result.context("controller signaling task failed")?
}

pub async fn run_native_viewer_session(
    connection: ControllerConnection,
    viewer: NativeViewerSession,
) -> Result<()> {
    let close_handle = viewer.close_handle();
    let close_on_session_end = close_handle.clone();
    let close_on_interrupt = close_handle;
    let (shutdown, shutdown_rx) = oneshot::channel();
    let keep_alive = tokio::spawn(async move {
        let result = connection.keep_alive(shutdown_rx).await;
        close_on_session_end.close();
        result
    });
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            close_on_interrupt.close();
        }
    });
    let viewer_result = viewer.run();
    interrupt.abort();
    let _ = shutdown.send(());
    keep_alive
        .await
        .context("controller shutdown task failed")??;
    viewer_result
}

pub async fn connect_saved_alias(
    alias: &str,
    options: ConnectionMediaOptions,
) -> Result<(ControllerConnection, ConnectionSummary)> {
    connect_saved_alias_with_progress(alias, options, None).await
}

pub async fn connect_saved_alias_with_progress(
    alias: &str,
    options: ConnectionMediaOptions,
    reporter: Option<&ConnectionProgressReporter>,
) -> Result<(ControllerConnection, ConnectionSummary)> {
    let mut resolved = resolve_saved_connection(alias, options, reporter).await?;
    let mut retries = 0;
    let connection = resolved
        .connect(reporter, &CancellationToken::new(), &mut retries)
        .await;
    resolved.client.close().await;
    Ok((connection?, resolved.summary))
}

async fn resolve_saved_connection(
    alias: &str,
    options: ConnectionMediaOptions,
    reporter: Option<&ConnectionProgressReporter>,
) -> Result<ResolvedConnection> {
    let client = Arc::new(AuthenticatedClient::from_saved_session()?);
    let result =
        resolve_connection_with_client(Arc::clone(&client), alias, options, reporter, None, None)
            .await;
    if result.is_err() {
        client.close().await;
    }
    result
}

async fn resolve_connection_with_client(
    client: Arc<AuthenticatedClient>,
    alias: &str,
    options: ConnectionMediaOptions,
    reporter: Option<&ConnectionProgressReporter>,
    target_id: Option<&str>,
    takeover: Option<takeover::Approval>,
) -> Result<ResolvedConnection> {
    report_progress(
        reporter,
        1,
        "验证本地会话",
        "正在读取登录态、虚拟设备身份与本机显示能力",
    );
    report_progress(
        reporter,
        2,
        "恢复账号会话",
        format!("正在初始化本虚拟设备、核验保存的登录态，然后检查 {alias} 的在线与可观看状态"),
    );
    let devices = client.list_devices().await?;
    if let Some(id) = target_id {
        crate::api::validate_device_id(id)?;
    }
    if target_id.map_or(devices.current_device.alias == alias, |id| {
        devices.current_device.device_id == id
    }) {
        bail!("cannot connect the current virtual device to itself");
    }

    let matches = devices
        .my_binded_devices
        .iter()
        .filter(|device| target_id.map_or(device.alias == alias, |id| device.device_id == id))
        .collect::<Vec<_>>();
    let device = match matches.as_slice() {
        [] => bail!("no device has the exact alias `{alias}`"),
        [device] => *device,
        _ => bail!("more than one device has the alias `{alias}`; rename one before connecting"),
    };
    let background = crate::wallpaper::Source::new(&device.device_id, &device.wallpaper_url);
    if let Some(reporter) = reporter {
        reporter(ConnectionProgress::background(background.clone()));
    }
    if !matches!(device.platform, 1 | 4) {
        bail!("this device type is for account management only");
    }
    if !device.is_connected() {
        bail!("device `{alias}` is offline");
    }
    if !device.controlled_support || !device.controllable {
        bail!("device `{alias}` does not currently allow control");
    }
    let _gate = shared::connection_gate(&shared::key(&client.device_id(), &device.device_id))
        .lock_owned()
        .await;
    if device.participant_count() != 0
        && shared::get(&shared::key(&client.device_id(), &device.device_id)).is_none()
        && !takeover
            .as_ref()
            .is_some_and(|a| a.permits(&device.device_id))
    {
        return Err(takeover::Required(device.clone()).into());
    }

    let (display, display_detection_warning) = match detect_local_display() {
        Ok(display) => (display, None),
        Err(error) => (
            LocalDisplayInfo::FALLBACK,
            Some(format!(
                "local display detection failed ({error:#}); using 1920×1080 @ 60 Hz"
            )),
        ),
    };
    let profile = options.resolve(display)?;
    report_progress(
        reporter,
        2,
        "目标设备可连接",
        format!(
            "本机显示 {}×{} @ {} Hz；帧率上限 {} FPS，{}",
            display.width,
            display.height,
            display.refresh_hz,
            profile.stream_fps,
            profile.codec.label()
        ),
    );
    let target_device_id = device.validated_device_id()?.to_owned();
    let controller_device_id = devices.current_device.validated_device_id()?.to_owned();
    let summary = ConnectionSummary {
        alias: device.alias.clone(),
        stream_fps: profile.stream_fps,
        codec: profile.codec.label(),
        display_detection_warning,
    };
    Ok(ResolvedConnection {
        client,
        target_device_id,
        controller_device_id,
        profile,
        transport: options.transport,
        summary,
        assist: None,
        preferences: None,
        audio_preferences: None,
        target_platform: device.platform,
        target_version: device.version_name.clone(),
        refresh_after_upgrade: false,
        background: Some(background),
        takeover,
    })
}
