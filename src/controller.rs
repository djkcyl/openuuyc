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
mod assist;

#[cfg(all(test, windows))]
mod decoder_survey;
#[cfg(all(test, windows))]
mod multi_screen_audit;

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
    signal_shutdown: Option<oneshot::Sender<()>>,
    signal_task: tokio::task::JoinHandle<Result<()>>,
    peer: Arc<NativePeer>,
    forwarder: RtpForwarder,
    profile: ConnectionMediaProfile,
    preference_writer: Option<crate::viewing_settings::PreferenceWriter>,
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

struct ResolvedConnection {
    client: Arc<AuthenticatedClient>,
    target_device_id: String,
    controller_device_id: String,
    profile: ConnectionMediaProfile,
    transport: crate::media::TransportChoice,
    summary: ConnectionSummary,
    assist: Option<assist::AssistConnection>,
    preferences: Option<crate::stream_control::StreamControlPreferences>,
}

impl ResolvedConnection {
    async fn connect(
        &mut self,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        retries: &mut u32,
    ) -> Result<ControllerConnection> {
        if self.target_device_id == self.controller_device_id {
            bail!("cannot connect the virtual device to itself");
        }
        report_progress(reporter, 3, "创建远程会话", "正在创建会话并获取信令凭据");
        let room = if let Some(assist) = &mut self.assist {
            let reply = assist
                .join(&self.client, &self.controller_device_id, reporter, cancel)
                .await?;
            self.target_device_id = reply.publisher_device_id.clone();
            if !reply.device_name.is_empty() {
                self.summary.alias = reply.device_name.clone();
            }
            RoomSession::from_assist(&reply)
        } else {
            loop {
                match cancellable(
                    cancel,
                    self.client.join_device(&self.target_device_id, false),
                )
                .await
                {
                    Ok(room) => break room,
                    Err(error) => {
                        // F890D0: only retry the API outcomes classified by the
                        // official device-join owner; never force another viewer out.
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
                            self.profile.local_display,
                        ))
                }
                Ok(None) => {}
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => persistence_error = Some(error.to_string()),
            }
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
            self.transport,
            self.preferences,
            reporter,
            cancel,
        )
        .await?;
        let handle = connection.stream_control_handle();
        handle.set_persistence_error(persistence_error);
        connection.preference_writer = store.map(|store| store.bind(handle));
        Ok(connection)
    }
}

pub async fn run_saved_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    owner: Option<String>,
    target_id: Option<String>,
) -> Result<()> {
    run_viewer_window(alias, options, owner, target_id, None).await
}

pub async fn run_assist_viewer_window(
    alias: String,
    request: crate::assist::AssistRequest,
    options: ConnectionMediaOptions,
    owner: Option<String>,
) -> Result<()> {
    request.validate()?;
    run_viewer_window(alias, options, owner, None, Some(request)).await
}

async fn run_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    owner: Option<String>,
    target_id: Option<String>,
    assist: Option<crate::assist::AssistRequest>,
) -> Result<()> {
    let owns_presence = owner.is_none();
    let owner = match owner {
        Some(descriptor) => Some(crate::viewer_owner::connect(&descriptor).await?),
        None => None,
    };
    let (progress_sender, progress_receiver) = std::sync::mpsc::channel();
    let (viewer_sender, viewer_receiver) = std::sync::mpsc::channel();
    let reporter: ConnectionProgressReporter = Arc::new(move |progress| {
        let _ = progress_sender.send(progress);
    });
    let task_alias = alias.clone();
    let (display_sender, display_receiver) = oneshot::channel();
    let cancel = CancellationToken::new();
    let owner_cancel = cancel.clone();
    let close_sender = viewer_sender.clone();
    let (monitor_sender, monitor_receiver) = tokio::sync::watch::channel(None);
    let owner_task = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = owner_cancel.cancelled() => {},
            _ = async {
                if let Some(owner) = owner {
                    crate::viewer_owner::report_until_owner_closes(owner, monitor_receiver).await;
                } else { std::future::pending::<()>().await; }
            } => {
                tracing::info!("device-center owner ended; closing viewer gracefully");
                owner_cancel.cancel();
                let _ = close_sender.send(ViewerWindowEvent::Close);
            },
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
                target_id,
                assist,
            },
            &task_cancel,
            &mut reporter,
        )
        .await;
        if !task_cancel.is_cancelled()
            && let Err(error) = &result
        {
            reporter(ConnectionProgress::failed(format!("{error:#}")));
        }
        result
    });

    let window_result = tokio::task::block_in_place(|| {
        run_connecting_viewer_window(alias, progress_receiver, viewer_receiver, display_sender)
    });
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
    sender: std::sync::mpsc::Sender<ViewerWindowEvent>,
    display: oneshot::Receiver<ViewerDisplayHandle>,
    owns_presence: bool,
    monitor: tokio::sync::watch::Sender<Option<crate::performance::PerformanceMonitor>>,
    target_id: Option<String>,
    assist: Option<crate::assist::AssistRequest>,
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
        target_id,
        assist,
    } = window;
    let presence_stop = CancellationToken::new();
    let mut presence_task = None;
    let mut account_owner = None;
    let result = async {
        let client = Arc::new(if owns_presence {
            AuthenticatedClient::from_saved_session()?
        } else {
            AuthenticatedClient::from_parent_session()?
        });
        account_owner = Some(Arc::clone(&client));
        let mut resolved = if let Some(request) = assist {
            cancellable(cancel, assist::resolve(client, &alias, options, request, Some(reporter))).await?
        } else {
            cancellable(cancel, resolve_connection_with_client(client, &alias, options, Some(reporter), target_id.as_deref())).await?
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
        let mut retries = 0;
        loop {
            monitor.send_replace(None);
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
            let prepared = cancellable(cancel, async {
                controller.start_native_viewer_with_progress(
                    &resolved.summary.alias, Some(reporter), display,
                ).await
            }).await;
            let (mut viewer, playback) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => { let _ = controller.close().await; return Err(error); }
            };
            // The decoder opened against the first frame's parameter sets after
            // the RTP forwarder started; only then is presentation known.
            if let Err(error) = cancellable(cancel, async {
                tokio::time::timeout(Duration::from_secs(30), viewer.startup())
                    .await.context("first-frame decoder startup timeout")?
            }).await {
                let _ = controller.close().await;
                return Err(error);
            }
            let route = controller.peer.selected_route_details().await.unwrap_or_else(|| "安全媒体通道已建立".into());
            reporter(ConnectionProgress::ready(format!("{route} · {} · {}", playback.codec, controller.performance_monitor().snapshot().decoder)));
            tracing::info!(device = %resolved.summary.alias, codec = playback.codec, track = %playback.track_id, "single-window viewer entered playback");
            let close = viewer.close_handle();
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
            let alive = controller.keep_alive(stop_receiver);
            tokio::pin!(alive);
            let result = tokio::select! {
                result = &mut alive => result,
                _ = cancel.cancelled() => {
                    let _ = stop_sender.send(());
                    alive.await
                }
            };
            if cancel.is_cancelled() {
                close.close();
                return Ok(());
            }
            match result {
                Err(error) if retry_session_failure(&error) => {
                    resolved.preferences = Some(session_control.preferences());
                    retries += 1;
                    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                    let (display_tx, display_rx) = oneshot::channel();
                    *reporter = Arc::new(move |progress| { let _ = progress_tx.send(progress); });
                    reporter(ConnectionProgress::working(4, "重建观看会话", format!("{error:#}；正在重新加入房间（{retries}/5）")));
                    viewer_sender.send(ViewerWindowEvent::Reconnect { progress: progress_rx, display: display_tx })
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
    if let Some(client) = account_owner {
        client.close().await;
    }
    if cancel.is_cancelled() {
        Ok(())
    } else {
        result
    }
}

impl ControllerConnection {
    pub fn stream_control_handle(&self) -> StreamControlHandle {
        self.peer.stream_control_handle()
    }

    pub fn performance_monitor(&self) -> crate::performance::PerformanceMonitor {
        self.peer.performance_monitor()
    }

    async fn select_video_track(&mut self) -> Result<(ForwardedTrack, VideoCodec)> {
        let video = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let track = self
                    .forwarder
                    .next_track()
                    .await
                    .ok_or_else(|| anyhow!("remote RTP track channel closed"))?;
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

    pub(crate) async fn establish(
        room: RoomSession,
        controller_device_id: &str,
        profile: ConnectionMediaProfile,
        transport: crate::media::TransportChoice,
        preferences: Option<crate::stream_control::StreamControlPreferences>,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
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
        let control =
            match cancellable(cancel, signal.start_control(controller_device_id, profile)).await {
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
        if let Some(preferences) = preferences
            && let Err(error) = peer
                .stream_control_handle()
                .restore_preferences(preferences)
        {
            let _ = peer.close().await;
            let _ = signal.close().await;
            return Err(error);
        }
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
        Ok(Self {
            signal_shutdown: Some(signal_shutdown),
            signal_task,
            peer,
            forwarder,
            profile,
            preference_writer: None,
        })
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
            eprintln!("警告：PLI 后 2 秒内未识别到完整参数集和关键帧，已继续等待后续关键帧");
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
            result = &mut self.signal_task => {
                self.signal_shutdown.take();
                flatten_signal_task(result)
            }
            _ = &mut shutdown => {
                if let Some(signal_shutdown) = self.signal_shutdown.take() {
                    let _ = signal_shutdown.send(());
                }
                flatten_signal_task((&mut self.signal_task).await)
            }
        };
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        result
    }

    pub async fn close(mut self) -> Result<()> {
        if let Some(signal_shutdown) = self.signal_shutdown.take() {
            let _ = signal_shutdown.send(());
        }
        let result = flatten_signal_task((&mut self.signal_task).await);
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        result
    }
}

impl Drop for ControllerConnection {
    fn drop(&mut self) {
        if let Some(shutdown) = self.signal_shutdown.take() {
            let _ = shutdown.send(());
        }
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
        resolve_connection_with_client(Arc::clone(&client), alias, options, reporter, None).await;
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
    if !matches!(device.platform, 1 | 4) {
        bail!("this device type is for account management only");
    }
    if !device.is_connected() {
        bail!("device `{alias}` is offline");
    }
    if !device.controlled_support || !device.controllable {
        bail!("device `{alias}` does not currently allow control");
    }
    if device.participant_count() != 0 {
        bail!(
            "device `{alias}` already has {} participant(s); refusing to force takeover",
            device.participant_count()
        );
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
    })
}
