//! Viewing-window attachment, reconnect ownership and cancellation.
use super::connection::resolve_connection_with_client;
use super::{
    ConnectionProgressReporter, ControllerConnection, assist, await_media_startup, cancellable,
    retry_session_failure, room_released, takeover, windows,
};
use crate::account::client::AuthenticatedClient;
use crate::application::viewer::{
    ConnectionProgress, NativeViewerSession, ViewerDisplayHandle, ViewerWindowEvent,
    run_connecting_viewer_window,
};
use crate::media::ConnectionMediaOptions;
use anyhow::{Context as _, Result, anyhow, bail};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub async fn run_saved_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
) -> Result<()> {
    run_viewer_window(alias, options, target_id, None, None).await
}

pub async fn run_assist_viewer_window(
    alias: String,
    request: crate::account::assist::AssistRequest,
    options: ConnectionMediaOptions,
) -> Result<()> {
    request.validate()?;
    run_viewer_window(alias, options, None, Some(request), None).await
}

pub(super) async fn run_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
    assist: Option<crate::account::assist::AssistRequest>,
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
            crate::application::viewer::windows_presenter::ConnectingWindowsRunConfig {
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

pub(super) struct ViewerConnectionWindow {
    pub(super) client: Option<Arc<AuthenticatedClient>>,
    pub(super) sender: std::sync::mpsc::Sender<ViewerWindowEvent>,
    pub(super) display: oneshot::Receiver<ViewerDisplayHandle>,
    pub(super) owns_presence: bool,
    pub(super) monitor:
        tokio::sync::watch::Sender<Option<crate::diagnostics::performance::PerformanceMonitor>>,
    pub(super) target: tokio::sync::watch::Sender<Option<windows::ViewerTarget>>,
    pub(super) target_id: Option<String>,
    pub(super) assist: Option<crate::account::assist::AssistRequest>,
    pub(super) takeover: Option<takeover::Approval>,
}

pub(super) async fn run_viewer_connection_owner(
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
                let presence = crate::session::presence::ActivePresence::start(client);
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
                                if let crate::session::presence::PresenceEvent::Warning(message) = event {
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
        let (switch_sender, mut switch_receiver) = tokio::sync::mpsc::channel::<crate::application::viewer::device_switch::SwitchRequest>(1);
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
            let switcher = crate::application::viewer::device_switch::DeviceSwitcher::new(
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
