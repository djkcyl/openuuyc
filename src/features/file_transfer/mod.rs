//! Solicited file management and transfer over the authenticated UU session.
pub(crate) mod protocol;
mod storage;
pub(crate) use storage::Store;
pub(crate) mod service;
mod tasks;
pub(crate) mod ui;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use prost::Message;
use protocol::*;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicI32, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};

pub(crate) fn lock<T>(v: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    v.lock().unwrap_or_else(|e| e.into_inner())
}
pub(crate) const BLOCK: usize = 204_800;
const WIRE: usize = 524_288;
pub(crate) enum Payload {
    Request(Req),
    Response(Res),
}
pub(crate) struct Incoming {
    pub payload: Payload,
    _permit: OwnedSemaphorePermit,
}
#[derive(Clone)]
struct Route {
    sender: mpsc::Sender<Incoming>,
    fault: Arc<Mutex<Option<String>>>,
    stop: CancellationToken,
}
#[derive(Default)]
pub(crate) struct Transport {
    text: Mutex<Weak<RTCDataChannel>>,
    file: Mutex<Weak<RTCDataChannel>>,
    routes: Mutex<HashMap<i32, Route>>,
    budget: std::sync::OnceLock<Arc<Semaphore>>,
    capabilities: Mutex<(i32, i32)>,
    serial: AtomicI32,
    permission: AtomicI32,
    changed: Notify,
    send_lock: tokio::sync::Mutex<()>,
}
pub(crate) struct RouteGuard {
    id: i32,
    owner: Arc<Transport>,
}
impl Drop for RouteGuard {
    fn drop(&mut self) {
        lock(&self.owner.routes).remove(&self.id);
    }
}
impl Transport {
    pub(crate) fn allowed(&self) -> bool {
        self.permission.load(Ordering::Acquire) != 1
    }
    pub(crate) fn metrics(&self, bytes: &[u8]) -> Result<()> {
        #[derive(prost::Message)]
        struct Metrics {
            #[prost(message, optional, tag = "4")]
            setting: Option<Setting>,
        }
        #[derive(prost::Message)]
        struct Setting {
            #[prost(int32, tag = "10")]
            control_allowed: i32,
        }
        if let Some(setting) = Metrics::decode(bytes)?.setting
            && matches!(setting.control_allowed, 1 | 2)
        {
            self.permission
                .store(setting.control_allowed, Ordering::Release);
            if setting.control_allowed == 1 {
                for r in lock(&self.routes).values() {
                    *lock(&r.fault) = Some("被控端已关闭连接权限".into());
                    r.stop.cancel();
                }
            }
            self.wake();
        }
        Ok(())
    }
    pub(crate) fn bind(&self, c: &Arc<RTCDataChannel>) {
        match c.label() {
            "TEXT_DATA_CHANNEL" => *lock(&self.text) = Arc::downgrade(c),
            "FILE_DATA_CHANNEL" => *lock(&self.file) = Arc::downgrade(c),
            _ => {}
        }
    }
    pub(crate) fn capabilities(&self, basic: i32, speedy: i32) {
        *lock(&self.capabilities) = (basic, speedy);
        self.changed.notify_waiters();
    }
    pub(crate) fn supported(&self) -> bool {
        let (a, b) = *lock(&self.capabilities);
        a >= 2 && b >= 2
    }
    pub(crate) fn wake(&self) {
        self.changed.notify_waiters();
    }
    pub(crate) fn close(&self) {
        self.permission.store(0, Ordering::Release);
        *lock(&self.capabilities) = (0, 0);
        for (_, r) in lock(&self.routes).drain() {
            *lock(&r.fault) = Some("设备连接已断开，任务已暂停".into());
            r.stop.cancel();
        }
        self.wake();
    }
    pub(crate) fn register(
        self: &Arc<Self>,
        stop: CancellationToken,
        fault: Arc<Mutex<Option<String>>>,
    ) -> Result<(i32, mpsc::Receiver<Incoming>, RouteGuard)> {
        ensure!(self.supported(), "被控端尚未就绪或不支持当前文件传输协议");
        ensure!(self.allowed(), "被控端已关闭连接权限");
        let mut routes = lock(&self.routes);
        ensure!(routes.len() < 64, "文件操作过多，请等待当前任务完成");
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        ensure!(
            (0..0x0fff_ffff).contains(&n),
            "文件任务编号已用尽，请重新连接"
        );
        let id = 0x2000_0000 + n;
        let (sender, rx) = mpsc::channel(128);
        routes.insert(
            id,
            Route {
                sender,
                fault,
                stop,
            },
        );
        Ok((
            id,
            rx,
            RouteGuard {
                id,
                owner: Arc::clone(self),
            },
        ))
    }
    pub(crate) async fn receive(&self, bytes: &Bytes) -> Result<bool> {
        ensure!(bytes.len() < WIRE, "文件通道消息过大");
        let envelope = Envelope::decode(bytes.clone())?;
        let (id, payload) = match envelope.which {
            Some(EnvelopeKind::Request(r)) => match r.which {
                Some(RequestKind::File(f)) => {
                    (r.header.map_or(0, |h| h.id), f.which.map(Payload::Request))
                }
                _ => return Ok(false),
            },
            Some(EnvelopeKind::Response(r)) => match r.which {
                Some(ResponseKind::File(f)) => {
                    (r.header.map_or(0, |h| h.id), f.which.map(Payload::Response))
                }
                _ => return Ok(false),
            },
            _ => return Ok(false),
        };
        // Never create tasks, browse local paths or write files from unsolicited messages.
        let route = i32::try_from(id)
            .ok()
            .and_then(|id| lock(&self.routes).get(&id).cloned());
        let Some(r) = route else {
            return Ok(true);
        };
        let Some(payload) = payload else {
            return Ok(true);
        };
        if matches!(&payload, Payload::Response(Res::Result(_))) && r.stop.is_cancelled() {
            let budget = self
                .budget
                .get_or_init(|| Arc::new(Semaphore::new(32 * 1024 * 1024)));
            if let Ok(permit) = Arc::clone(budget).try_acquire_many_owned(bytes.len().max(1) as u32)
            {
                let _ = r.sender.try_send(Incoming {
                    payload,
                    _permit: permit,
                });
            }
            return Ok(true);
        }
        let budget = self
            .budget
            .get_or_init(|| Arc::new(Semaphore::new(32 * 1024 * 1024)));
        let permit = tokio::select! {biased;_=r.stop.cancelled()=>return Ok(true),v=Arc::clone(budget).acquire_many_owned(bytes.len().max(1)as u32)=>v?};
        tokio::select! {biased;_=r.stop.cancelled()=>{},_=r.sender.send(Incoming{payload,_permit:permit})=>{}}
        Ok(true)
    }
    pub(crate) async fn request(&self, id: i32, which: Req, file: bool) -> Result<()> {
        self.send(
            Envelope {
                which: Some(EnvelopeKind::Request(Request {
                    header: Some(Header { id: id.into() }),
                    which: Some(RequestKind::File(FileTransferFtpRequest {
                        which: Some(which),
                    })),
                })),
            },
            file,
        )
        .await
    }
    pub(crate) async fn response(&self, id: i32, which: Res) -> Result<()> {
        self.send(
            Envelope {
                which: Some(EnvelopeKind::Response(Response {
                    header: Some(Header { id: id.into() }),
                    which: Some(ResponseKind::File(FileTransferFtpResponse {
                        which: Some(which),
                    })),
                })),
            },
            false,
        )
        .await
    }
    async fn send(&self, message: Envelope, file: bool) -> Result<()> {
        let bytes = Bytes::from(message.encode_to_vec());
        ensure!(bytes.len() < WIRE, "文件清单过大，无法发送");
        let channel = lock(if file { &self.file } else { &self.text })
            .upgrade()
            .context("文件通道未连接")?;
        let _gate = tokio::time::timeout(Duration::from_secs(30), self.send_lock.lock())
            .await
            .context("文件发送队列等待超时")?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let wake = self.changed.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                ensure!(
                    self.supported() && channel.ready_state() == RTCDataChannelState::Open,
                    "文件通道已断开"
                );
                if channel.buffered_amount().await + bytes.len() <= 4 * 1024 * 1024 {
                    break;
                }
                // TEXT's existing writer owns its low-water callback. Poll only while backpressured.
                tokio::select! { _=wake=>{}, _=tokio::time::sleep(Duration::from_millis(10))=>{} }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("文件发送超时，未自动重发")??;
        // Capacity is reserved before SCTP assigns SSN; a timed-out admission
        // is cancellable, while an admitted message is always complete.
        tokio::time::timeout(Duration::from_secs(30), async {
            if file {
                channel.send(&bytes).await
            } else {
                channel.send_text_bytes(&bytes).await
            }
        })
        .await
        .context("文件发送超时，未自动重发")??;
        Ok(())
    }
}
pub(crate) fn error(code: i32) -> String {
    match code {
        1 => "成功",
        2 => "远端处理失败",
        3 => "文件协议错误",
        4 => "没有文件访问权限",
        5 => "磁盘空间不足",
        6 => "文件读写失败",
        7 => "已取消",
        8 => "被控端未登录",
        9 => "文件或目录不存在",
        10 => "已暂停",
        11 => "文件已变化或无法续传",
        12 => "等待被控端授权",
        _ => return format!("文件操作失败（{code}）"),
    }
    .into()
}
pub(crate) fn success(code: i32) -> Result<()> {
    ensure!(code == 1, "{}", error(code));
    Ok(())
}
