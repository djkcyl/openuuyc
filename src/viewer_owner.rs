//! Local device-center/child lifecycle IPC, not a UU wire protocol.
//! Parent EOF requests normal player teardown, including during handshake.

use anyhow::{Context, Result, bail};
use std::{
    net::TcpListener,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) struct ViewerOwner {
    descriptor: String,
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
    info: Arc<Mutex<Option<ViewerInfo>>>,
}

/// Small local-only facts; no credentials, packets or per-frame statistics.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ViewerInfo {
    pub connection: String,
    pub decoder: String,
    pub video_format: String,
    pub remote_encoder: String,
    pub remote_capture: String,
}

impl ViewerOwner {
    pub(crate) fn new() -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .context("bind viewer lifecycle endpoint")?;
        listener.set_nonblocking(true)?;
        let nonce = Uuid::new_v4();
        let descriptor = format!("{}:{nonce}", listener.local_addr()?.port());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let info = Arc::new(Mutex::new(None));
        let task_info = Arc::clone(&info);
        let task = std::thread::Builder::new().name("viewer-owner".into()).spawn(move || {
            let result = runtime.block_on(async {
                let listener = tokio::net::TcpListener::from_std(listener)?;
                tokio::select! {
                    biased;
                    _ = task_stop.cancelled() => Ok(()),
                    result = async {
                        loop {
                            let (mut stream, _) = listener.accept().await?;
                            let authenticated = tokio::time::timeout(Duration::from_secs(5), async {
                                let mut received = [0; 16];
                                stream.read_exact(&mut received).await?;
                                if received != *nonce.as_bytes() { bail!("wrong viewer lifecycle nonce"); }
                                stream.write_all(&[1]).await?;
                                Ok::<_, anyhow::Error>(())
                            }).await;
                            if !matches!(authenticated, Ok(Ok(()))) { continue; }
                            tracing::debug!("viewer lifecycle child attached");
                            loop {
                                let Ok(size) = stream.read_u32().await else { return Ok::<_, anyhow::Error>(()); };
                                if size > 8192 { bail!("oversized viewer information message"); }
                                let mut bytes = vec![0; size as usize];
                                stream.read_exact(&mut bytes).await?;
                                let value: ViewerInfo = serde_json::from_slice(&bytes)?;
                                *task_info.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
                            }
                        }
                    } => result,
                }
            });
            if let Err(error) = result { tracing::warn!(%error, "viewer lifecycle endpoint ended"); }
        })?;
        Ok(Self {
            descriptor,
            stop,
            task: Some(task),
            info,
        })
    }

    pub(crate) fn descriptor(&self) -> &str {
        &self.descriptor
    }
    pub(crate) fn request_close(&self) {
        self.stop.cancel();
    }
    pub(crate) fn info(&self) -> Option<ViewerInfo> {
        self.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub(crate) async fn report_until_owner_closes(
    mut stream: TcpStream,
    monitor: tokio::sync::watch::Receiver<Option<crate::performance::PerformanceMonitor>>,
) {
    let (mut reader, mut writer) = stream.split();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut eof = [0];
    loop {
        tokio::select! {
            biased;
            _ = reader.read(&mut eof) => return,
            _ = tick.tick() => {
                let info = monitor.borrow().as_ref().map(|m| {
                    let s = m.snapshot();
                    ViewerInfo { connection: s.connection.clone(), decoder: s.decoder.clone(), video_format: s.video_format.clone(), remote_encoder: s.remote_encoder.clone(), remote_capture: s.remote_capture.clone() }
                }).unwrap_or_default();
                let Ok(bytes) = serde_json::to_vec(&info) else { continue; };
                if bytes.len() > 8192 { continue; }
                let send = async { writer.write_u32(bytes.len() as u32).await?; writer.write_all(&bytes).await };
                if !matches!(tokio::time::timeout(Duration::from_secs(2), send).await, Ok(Ok(()))) { return; }
            }
        }
    }
}

impl Drop for ViewerOwner {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

pub(crate) async fn connect(descriptor: &str) -> Result<TcpStream> {
    let (port, nonce) = descriptor
        .split_once(':')
        .context("invalid viewer owner descriptor")?;
    let port: u16 = port.parse().context("invalid viewer owner port")?;
    let nonce = Uuid::parse_str(nonce).context("invalid viewer owner nonce")?;
    tokio::time::timeout(Duration::from_secs(5), async {
        // Never allow a command-line value to redirect this lifecycle channel
        // to a remote host. No UU credentials are sent through it.
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        stream.write_all(nonce.as_bytes()).await?;
        let mut ready = [0];
        stream.read_exact(&mut ready).await?;
        if ready != [1] {
            bail!("viewer owner refused lifecycle connection");
        }
        tracing::debug!("viewer lifecycle owner attached");
        Ok(stream)
    })
    .await
    .context("viewer owner handshake timed out")?
}
