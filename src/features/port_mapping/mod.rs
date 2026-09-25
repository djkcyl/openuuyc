//! Controller-side TCP tunnels over UU's FILE channel, with binary protobuf payloads.
//! No incoming SYN is executed: this client never exposes a forwarding server.
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use prost::Message;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};

pub(crate) mod service;
pub(crate) mod store;
pub(crate) mod ui;

const CHUNK: usize = 524_160;
const BUFFER: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const WIRE_LIMIT: usize = 524_288;

#[derive(Clone, PartialEq, Message)]
struct Frame {
    #[prost(uint64, tag = "1")]
    session_id: u64,
    #[prost(uint64, tag = "2")]
    rule_id: u64,
    #[prost(uint64, tag = "3")]
    stream_id: u64,
    #[prost(int32, tag = "4")]
    kind: i32,
    #[prost(bytes = "bytes", tag = "5")]
    payload: Bytes,
}

struct Incoming {
    frame: Frame,
    _permit: OwnedSemaphorePermit,
}
struct Route {
    sender: mpsc::Sender<Incoming>,
    budget: Arc<Semaphore>,
    stop: CancellationToken,
    remote_fin: Arc<std::sync::atomic::AtomicBool>,
    fault: Arc<Mutex<Option<String>>>,
}

#[derive(Default)]
pub(crate) struct Transport {
    channel: Mutex<Weak<RTCDataChannel>>,
    routes: Mutex<HashMap<(u64, u64), Route>>,
    changed: Notify,
    serial: AtomicU64,
    send_lock: tokio::sync::Mutex<()>,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Transport {
    pub(crate) fn bind(&self, channel: &Arc<RTCDataChannel>) {
        *lock(&self.channel) = Arc::downgrade(channel);
        self.changed.notify_waiters();
    }
    pub(crate) fn wake(&self) {
        self.changed.notify_waiters();
    }
    pub(crate) fn close(&self) {
        *lock(&self.channel) = Weak::new();
        for (_, route) in lock(&self.routes).drain() {
            route.stop.cancel();
        }
        self.changed.notify_waiters();
    }
    pub(crate) fn ready(&self) -> bool {
        lock(&self.channel)
            .upgrade()
            .is_some_and(|c| c.ready_state() == RTCDataChannelState::Open)
    }
    pub(crate) async fn wait_ready(&self) -> Result<()> {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let wake = self.changed.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.ready() {
                    return;
                }
                wake.await;
            }
        })
        .await
        .context("端口转发数据通道未就绪")
    }
    pub(crate) fn receive(&self, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() < WIRE_LIMIT, "端口转发消息过大");
        let Some(frame) = crate::features::stream_control::decode_port_mapping(bytes)? else {
            return Ok(());
        };
        let frame = Frame::decode(frame.as_slice()).context("无效端口转发消息")?;
        // Unknown oneof, unknown streams and SYN must never create remote side effects.
        if !matches!(frame.kind, 1..=4) {
            return Ok(());
        }
        let routes = lock(&self.routes);
        let Some(route) = routes.get(&(frame.rule_id, frame.stream_id)) else {
            return Ok(());
        };
        if frame.kind == 1 {
            route.remote_fin.store(true, Ordering::Release);
        }
        let size = frame.payload.len().max(1);
        let Ok(permit) = Arc::clone(&route.budget).try_acquire_many_owned(size as u32) else {
            *lock(&route.fault) = Some("端口转发接收缓存超过 8 MiB".into());
            route.stop.cancel();
            return Ok(());
        };
        if route
            .sender
            .try_send(Incoming {
                frame,
                _permit: permit,
            })
            .is_err()
        {
            *lock(&route.fault) = Some("端口转发接收队列已满".into());
            route.stop.cancel();
        }
        Ok(())
    }
    async fn send(&self, rule: u64, stream: u64, kind: i32, payload: Bytes) -> Result<()> {
        let frame = Frame {
            session_id: 1,
            rule_id: rule,
            stream_id: stream,
            kind,
            payload,
        };
        let bytes = Bytes::from(crate::features::stream_control::encode_port_mapping(
            frame.encode_to_vec(),
        ));
        ensure!(bytes.len() <= WIRE_LIMIT, "端口转发消息过大");
        let channel = lock(&self.channel)
            .upgrade()
            .context("端口转发连接已关闭")?;
        // Serialize the capacity check with submission; the underlying writer
        // otherwise accepts into an unbounded queue. Stop reading TCP while full.
        tokio::time::timeout(TIMEOUT, async {
            let _guard = self.send_lock.lock().await;
            loop {
                let wake = self.changed.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                ensure!(
                    channel.ready_state() == RTCDataChannelState::Open,
                    "端口转发连接已关闭"
                );
                if channel.buffered_amount().await + bytes.len() <= BUFFER {
                    break;
                }
                wake.await;
            }
            channel.send(&bytes).await?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("端口转发发送超时")??;
        Ok(())
    }
    async fn handshake(
        &self,
        rule: &Rule,
        stream: u64,
        incoming: &mut mpsc::Receiver<Incoming>,
    ) -> Result<()> {
        let payload = serde_json::to_vec(
            &serde_json::json!({"target_host":rule.target,"target_port":rule.remote_port,"version":1}),
        )?;
        self.send(rule.id, stream, 0, payload.into()).await?;
        let response = tokio::time::timeout(TIMEOUT, incoming.recv())
            .await
            .context("目标连接握手超时")?
            .context("端口转发已关闭")?;
        ensure!(response.frame.kind == 3, "端口转发握手顺序错误");
        #[derive(serde::Deserialize)]
        struct Reply {
            ok: bool,
            #[serde(default = "protocol_version")]
            version: i32,
            #[serde(default)]
            error: String,
        }
        ensure!(response.frame.payload.len() <= 16_384, "目标回复过大");
        let reply: Reply =
            serde_json::from_slice(&response.frame.payload).context("目标握手回复无效")?;
        ensure!(reply.ok, "目标连接失败：{}", reply.error);
        ensure!(reply.version == 1, "目标端口转发协议版本不支持");
        drop(response);
        self.send(rule.id, stream, 4, Bytes::new()).await?;
        Ok(())
    }

    async fn probe(&self, rule: &Rule, parent: &CancellationToken) -> ProbeStatus {
        let stream = self.serial.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if stream == 0 {
            return ProbeStatus::Unknown;
        }
        let cancel = parent.child_token();
        let (sender, mut incoming) = mpsc::channel(32);
        let remote_fin = Arc::new(std::sync::atomic::AtomicBool::new(false));
        lock(&self.routes).insert(
            (rule.id, stream),
            Route {
                sender,
                budget: Arc::new(Semaphore::new(128 * 1024)),
                stop: cancel.clone(),
                remote_fin: Arc::clone(&remote_fin),
                fault: Arc::new(Mutex::new(None)),
            },
        );
        let started = std::time::Instant::now();
        let connected = tokio::select! {
            biased;
            _=cancel.cancelled()=>Err(anyhow::anyhow!("探测已取消")),
            result=tokio::time::timeout(Duration::from_secs(5),self.handshake(rule,stream,&mut incoming))=>result.context("TCP 探测超时").and_then(|r|r),
        };
        let outcome = match connected {
            Err(error) => ProbeStatus::Unreachable(error.to_string()),
            Ok(()) => {
                let millis = started.elapsed().as_secs_f64() * 1000.0;
                let host = SocketAddr::new(rule.target, rule.remote_port);
                let request = Bytes::from(format!(
                    "HEAD / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
                ));
                let http = tokio::select! {
                    biased;
                    _=cancel.cancelled()=>false,
                    result=tokio::time::timeout(Duration::from_secs(3),async {
                        self.send(rule.id,stream,2,request).await?;
                        let mut prefix=Vec::new();
                        while let Some(message)=incoming.recv().await {
                            if message.frame.kind==1 {break;}
                            if message.frame.kind!=2 {continue;}
                            let remaining=1024-prefix.len();
                            prefix.extend_from_slice(&message.frame.payload[..remaining.min(message.frame.payload.len())]);
                            if prefix.contains(&b'\n') || prefix.len()==1024 {break;}
                        }
                        Ok::<bool,anyhow::Error>(prefix.len()>=12
                            && (prefix.starts_with(b"HTTP/1.0 ") || prefix.starts_with(b"HTTP/1.1 "))
                            && prefix[9..12].iter().all(u8::is_ascii_digit))
                    })=>matches!(result,Ok(Ok(true))),
                };
                ProbeStatus::Reachable { millis, http }
            }
        };
        lock(&self.routes).remove(&(rule.id, stream));
        if !remote_fin.load(Ordering::Acquire) {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                self.send(rule.id, stream, 1, Bytes::new()),
            )
            .await;
        }
        outcome
    }
    async fn stream(
        self: Arc<Self>,
        rule: Rule,
        socket: TcpStream,
        parent: CancellationToken,
        stats: Arc<Mutex<RuleStatus>>,
    ) {
        let stream = self.serial.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if stream == 0 {
            return;
        }
        let stop = parent.child_token();
        let remote_fin = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fault = Arc::new(Mutex::new(None));
        let (sender, mut incoming) = mpsc::channel(1024);
        lock(&self.routes).insert(
            (rule.id, stream),
            Route {
                sender,
                budget: Arc::new(Semaphore::new(BUFFER)),
                stop: stop.clone(),
                remote_fin: Arc::clone(&remote_fin),
                fault: Arc::clone(&fault),
            },
        );
        lock(&stats).connections += 1;
        let result = tokio::select! {
            biased;
            _=stop.cancelled()=>Ok(()),
            result=async {
                socket.set_nodelay(true)?;
                self.handshake(&rule,stream,&mut incoming).await?;
                lock(&stats).error=None;
                let (mut reader,mut writer)=socket.into_split();
                let read=async {
                    let mut bytes=vec![0;CHUNK];
                    loop {
                        let count=reader.read(&mut bytes).await?;
                        if count==0 {return Ok::<_,anyhow::Error>(());}
                        self.send(rule.id,stream,2,Bytes::copy_from_slice(&bytes[..count])).await?;
                        lock(&stats).sent+=count as u64;
                    }
                };
                let write=async {
                    while let Some(message)=incoming.recv().await {
                        if message.frame.kind==1 {return Ok(());}
                        ensure!(message.frame.kind==2 || message.frame.kind==4,"端口转发收到重复握手");
                        if message.frame.kind==2 {
                            writer.write_all(&message.frame.payload).await?;
                            lock(&stats).received+=message.frame.payload.len() as u64;
                        }
                    }
                    Ok::<_,anyhow::Error>(())
                };
                tokio::select!{result=read=>result,result=write=>result}
            }=>result
        };
        lock(&self.routes).remove(&(rule.id, stream));
        // Best effort FIN is bounded independently so stopping a rule always joins.
        if !remote_fin.load(Ordering::Acquire) {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                self.send(rule.id, stream, 1, Bytes::new()),
            )
            .await;
        }
        let mut state = lock(&stats);
        state.connections = state.connections.saturating_sub(1);
        if !parent.is_cancelled()
            && let Err(error) = result
        {
            state.error = Some(error.to_string());
        }
        if let Some(error) = lock(&fault).take() {
            state.error = Some(error);
        }
    }
}
fn protocol_version() -> i32 {
    1
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct Rule {
    pub id: u64,
    pub name: String,
    pub local_addr: IpAddr,
    pub local_port: u16,
    pub target: IpAddr,
    pub remote_port: u16,
    pub enabled: bool,
}
impl Rule {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.id != 0, "规则编号无效");
        ensure!(
            !self.name.trim().is_empty() && self.name.encode_utf16().count() <= 32,
            "名称应为 1–32 个字符"
        );
        ensure!(
            self.local_port != 0 && self.remote_port != 0,
            "端口应为 1–65535"
        );
        Ok(())
    }
}
#[derive(Clone, Default)]
pub(crate) enum ProbeStatus {
    #[default]
    Unknown,
    Reachable {
        millis: f64,
        http: bool,
    },
    Unreachable(String),
}
#[derive(Clone, Default)]
pub(crate) struct RuleStatus {
    pub listening: bool,
    pub connections: usize,
    pub sent: u64,
    pub received: u64,
    pub error: Option<String>,
    pub probing: bool,
    pub probe: ProbeStatus,
    pub total_sent: u64,
    pub total_received: u64,
    pub send_rate: f64,
    pub receive_rate: f64,
}

pub(crate) async fn run_rule(
    rule: Rule,
    transport: Arc<Transport>,
    stop: CancellationToken,
    status: Arc<Mutex<RuleStatus>>,
    probe: Arc<Notify>,
) {
    let result=async {
        rule.validate()?;
        let endpoint=SocketAddr::new(rule.local_addr,rule.local_port);
        let socket=if rule.local_addr.is_ipv4() {tokio::net::TcpSocket::new_v4()?} else {tokio::net::TcpSocket::new_v6()?};
        socket.bind(endpoint).with_context(||format!("无法监听 {endpoint}（地址不可用或端口已占用）"))?;
        let listener=socket.listen(128)?;
        lock(&status).listening=true;
        let mut streams=JoinSet::new();
        streams.spawn(probe_loop(rule.clone(),Arc::clone(&transport),stop.clone(),Arc::clone(&status),probe));
        let result=loop {
            tokio::select! {
                biased;
                _=stop.cancelled()=>break Ok(()),
                Some(result)=streams.join_next(), if !streams.is_empty()=>{if let Err(error)=result {tracing::warn!(%error,"TCP forwarding task failed");}},
                accepted=listener.accept()=>{
                    let (socket,_)=match accepted{Ok(value)=>value,Err(error)=>break Err(anyhow::Error::from(error))};
                    streams.spawn(Arc::clone(&transport).stream(rule.clone(),socket,stop.clone(),Arc::clone(&status)));
                }
            }
        };
        drop(listener);
        stop.cancel();
        while streams.join_next().await.is_some() {}
        result
    }.await;
    let mut state = lock(&status);
    state.listening = false;
    if let Err(error) = result {
        state.error = Some(format!("{error:#}"));
    }
}

async fn probe_loop(
    rule: Rule,
    transport: Arc<Transport>,
    stop: CancellationToken,
    status: Arc<Mutex<RuleStatus>>,
    requested: Arc<Notify>,
) {
    static LIMIT: std::sync::OnceLock<Semaphore> = std::sync::OnceLock::new();
    let limit = LIMIT.get_or_init(|| Semaphore::new(4));
    loop {
        let permit = tokio::select! {biased;_=stop.cancelled()=>break,p=limit.acquire()=>p.expect("probe semaphore remains open")};
        lock(&status).probing = true;
        let outcome = transport.probe(&rule, &stop).await;
        {
            let mut status = lock(&status);
            status.probing = false;
            if !stop.is_cancelled() {
                status.probe = outcome;
            }
        }
        drop(permit);
        tokio::select! {biased;_=stop.cancelled()=>break,_=requested.notified()=>{},_=tokio::time::sleep(Duration::from_secs(30))=>{}}
    }
}
