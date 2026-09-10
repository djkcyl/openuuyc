//! Logged-in device presence and account push ownership, shared by GUI and CLI.
//! Network retry policy is independent of account/device revocation.
//!
//! 2026-09-09 注记：本机"在线房间"保留（设备在线状态基础），但不开放被控：
//! 被控权限开关（device/controllable）已从界面移除，接口与逆向记录留档。

use crate::{
    api::ApiFailure,
    client::AuthenticatedClient,
    signal::{SignalFailure, SignalPushHandler, SignalRole, SignalSession, SocketState},
};
use anyhow::Result;
use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) enum PresenceState {
    Connecting,
    Online,
    Reconnecting,
    Offline,
}

pub(crate) enum PresenceEvent {
    State(PresenceState),
    Warning(String),
    DeviceChanged,
    AccountEnded,
}

pub(crate) struct ActivePresence {
    cancel: CancellationToken,
    pub(crate) task: JoinHandle<Result<()>>,
    pub(crate) events: Receiver<PresenceEvent>,
}

impl ActivePresence {
    pub(crate) fn start(client: Arc<AuthenticatedClient>) -> Self {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (events, receiver) = mpsc::channel();
        let task = tokio::spawn(async move {
            let ended = client.ended();
            let run = run_presence(Arc::clone(&client), events.clone(), task_cancel.clone());
            tokio::pin!(run);
            tokio::select! {
                biased;
                _ = ended.cancelled() => {
                    task_cancel.cancel();
                    if let Err(error) = client.clear_saved_generation() {
                        let _ = events.send(PresenceEvent::Warning(format!("清理旧账号凭据失败：{error:#}")));
                    }
                    let _ = events.send(PresenceEvent::AccountEnded);
                    run.await
                },
                result = &mut run => result,
            }
        });
        Self {
            cancel,
            task,
            events: receiver,
        }
    }

    pub(crate) async fn close(self) {
        self.cancel.cancel();
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "host presence task ended unexpectedly");
        }
    }
}

async fn run_presence(
    client: Arc<AuthenticatedClient>,
    events: Sender<PresenceEvent>,
    task_cancel: CancellationToken,
) -> Result<()> {
    let mut policy = HostRoomRetry::default();
    let mut initial_requests = 0;
    let mut backoff = false;
    loop {
        if task_cancel.is_cancelled() {
            return Ok(());
        }
        let _ = events.send(PresenceEvent::State(if backoff {
            PresenceState::Reconnecting
        } else {
            PresenceState::Connecting
        }));
        // 39C5B0: a device that has never been controlled sends -1.
        let request = tokio::select! {
            _ = task_cancel.cancelled() => return Ok(()),
            result = client.create_host_room(-1) => result,
        };
        let room = match request {
            Ok(room) => {
                policy.room_response(&room);
                room
            }
            Err(error) => {
                if error
                    .downcast_ref::<ApiFailure>()
                    .is_some_and(|failure| failure.code == 1120)
                {
                    return Err(error);
                }
                let delay = if backoff {
                    policy.failed_backoff_request()
                } else {
                    initial_requests += 1;
                    if initial_requests >= 3 {
                        return Err(error);
                    }
                    Duration::from_millis(500)
                };
                let _ = events.send(PresenceEvent::Warning(format!(
                    "在线房间请求失败：{error:#}；{} ms 后重试",
                    delay.as_millis()
                )));
                tokio::select! { _ = task_cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(delay) => {} }
                continue;
            }
        };
        let room_cancel = task_cancel.child_token();
        let push_client = Arc::clone(&client);
        let push_cancel = room_cancel.clone();
        let push_events = events.clone();
        let observer: SignalPushHandler = Arc::new(move |push| {
            if push_cancel.is_cancelled() || !push_client.is_active() {
                return;
            }
            let Some(kind) = push.get("type").and_then(serde_json::Value::as_str) else {
                tracing::warn!("discarded push without string type");
                return;
            };
            if !matches!(
                kind,
                "device_unbind" | "device_binded" | "device_info_changed"
            ) {
                return;
            }
            let Some(id) = push
                .get("data")
                .and_then(|data| data.get("device_id"))
                .and_then(serde_json::Value::as_str)
            else {
                tracing::warn!(kind, "discarded device push without string device_id");
                return;
            };
            if kind == "device_unbind" && id == push_client.device_id() {
                // Server 398A50 -> 3B5540: exact current-device match.
                // No credential I/O on the Socket.IO input worker.
                tracing::info!("current virtual device was unbound; retiring account generation");
                push_client.retire();
            } else {
                let _ = push_events.send(PresenceEvent::DeviceChanged);
            }
        });
        let session =
            SignalSession::connect_observed(room, SignalRole::Host, &room_cancel, Some(observer))
                .await;
        let result = match session {
            Ok(session) => {
                let _ = events.send(PresenceEvent::State(PresenceState::Online));
                let mut socket_state = session.socket_state();
                let mut state_open = true;
                let (shutdown, shutdown_receiver) = oneshot::channel();
                let alive = session.keep_alive(shutdown_receiver, None, None);
                tokio::pin!(alive);
                loop {
                    tokio::select! {
                        result = &mut alive => break result,
                        _ = task_cancel.cancelled() => { let _ = shutdown.send(()); break alive.await; }
                        state = socket_state.changed(), if state_open => {
                            if state.is_err() { state_open = false; continue; }
                            let presence = match *socket_state.borrow_and_update() {
                                SocketState::Connected => PresenceState::Online,
                                SocketState::Connecting | SocketState::Reconnecting => PresenceState::Reconnecting,
                                SocketState::Closed => PresenceState::Offline,
                            };
                            let _ = events.send(PresenceEvent::State(presence));
                        }
                    }
                }
            }
            Err(error) => Err(error),
        };
        room_cancel.cancel();
        if task_cancel.is_cancelled() {
            return Ok(());
        }
        if matches!(
            result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<SignalFailure>()),
            Some(SignalFailure::Kicked)
        ) {
            return result;
        }
        let delay = policy.lost_room();
        backoff = true;
        initial_requests = 0;
        if let Err(error) = result {
            let _ = events.send(PresenceEvent::Warning(format!(
                "在线房间失联：{error:#}；{} 秒后重建",
                delay.as_secs()
            )));
        }
        tokio::select! { _ = task_cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(delay) => {} }
    }
}

// GameViewerServer 2D5DE0, 3CA500..3CA680 and 3BB060/3BB490.
// HTTP-response and socket-open time are distinct: >=30s starts a random
// 1..5s backoff; repeated short failures double delay up to the server limit.
struct HostRoomRetry {
    delay_seconds: u32,
    maximum_seconds: u32,
    last_room_response: Option<Instant>,
}
impl Default for HostRoomRetry {
    fn default() -> Self {
        Self {
            delay_seconds: 0,
            maximum_seconds: 300,
            last_room_response: None,
        }
    }
}
impl HostRoomRetry {
    fn room_response(&mut self, room: &crate::api::RoomSession) {
        if room.max_reconnect_delta > 0 {
            self.maximum_seconds = room.max_reconnect_delta as u32;
        }
        self.last_room_response = Some(Instant::now());
    }
    fn lost_room(&mut self) -> Duration {
        if self
            .last_room_response
            .is_none_or(|time| time.elapsed() >= Duration::from_secs(30))
        {
            self.delay_seconds = rand::random_range(1..=5);
        } else {
            self.delay_seconds = self
                .delay_seconds
                .saturating_mul(2)
                .min(self.maximum_seconds);
        }
        self.delay_seconds = self.delay_seconds.max(1);
        Duration::from_secs(self.delay_seconds as u64)
    }
    fn failed_backoff_request(&mut self) -> Duration {
        self.delay_seconds = self
            .delay_seconds
            .saturating_mul(2)
            .min(self.maximum_seconds)
            .max(1);
        Duration::from_secs(self.delay_seconds as u64)
    }
}
