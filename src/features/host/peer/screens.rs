mod actions;
// Fixed negotiated video-track pool; captures are attached/detached independently.
use super::{Published, media};
use crate::features::host::{
    Lease, VideoConfig, capture, format::Negotiated, lock, track::VideoTrack, transport::Transport,
};
use crate::platform::display::topology::{Target, Topology};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use webrtc::{
    peer_connection::RTCPeerConnection, rtp_transceiver::rtp_sender::RTCRtpSender,
    track::track_local::TrackLocal,
};

pub(crate) const TRACK_COUNT: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenInfo {
    pub screen: capture::Screen,
    pub initial: capture::Screen,
    pub target: Option<Target>,
    pub kind: i32,
    pub resolution_type: i32,
}
pub(super) struct Reports {
    pub catalog: Mutex<Vec<ScreenInfo>>,
    pub current: AtomicI32,
    pub sequence: std::sync::atomic::AtomicI64,
    pub publications: Vec<Mutex<Option<watch::Receiver<Published>>>>,
    pub changed: watch::Sender<u64>,
}
impl Reports {
    pub fn notify(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn media(&self) -> Vec<(usize, Published)> {
        self.publications
            .iter()
            .enumerate()
            .filter_map(|(index, rx)| lock(rx).as_ref().map(|rx| (index, rx.borrow().clone())))
            .collect()
    }
}

pub(crate) struct Slot {
    pub screen: Option<capture::Screen>,
    pub suspended: Option<capture::Screen>,
    pub config: Arc<Mutex<VideoConfig>>,
    pub negotiated: Arc<Negotiated>,
    pub transport: Transport,
    pub sender: Arc<RTCRtpSender>,
    track: Arc<VideoTrack>,
    clock: Arc<media::Timeline>,
    worker: Option<media::Worker>,
    observer: Option<tokio::task::JoinHandle<()>>,
    lease: Option<Lease>,
    awaiting_source: bool,
}
pub(crate) struct Screens {
    pub displays: Arc<crate::features::host::displays::Session>,
    pub slots: Vec<Slot>,
    pub registered: Vec<usize>,
    pub(super) reports: Arc<Reports>,
    connection: std::sync::Weak<RTCPeerConnection>,
    lease: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    base: VideoConfig,
    originals: BTreeMap<i32, capture::Screen>,
    source_generation: u16,
    initial: capture::Screen,
    before_super: Option<actions::Running>,
}
impl Screens {
    pub(crate) fn set_fps_limit(&mut self, limit: u32) {
        self.base.fps_limit = limit;
        self.base.fps = self
            .base
            .requested_fps
            .min(limit)
            .min(self.base.maximum_fps);
        for slot in &self.slots {
            let mut config = lock(&slot.config);
            config.fps_limit = limit;
            config.fps = config.requested_fps.min(limit).min(config.maximum_fps);
            config.revision = config.revision.wrapping_add(1);
        }
        self.reports.notify();
    }
    pub(crate) fn set_media_default(&mut self, config: VideoConfig) {
        self.base = config;
    }
    pub(crate) fn handshake_source(&self) -> capture::Screen {
        self.initial.clone()
    }
    pub(crate) fn authorization(&self) -> Lease {
        self.lease.clone()
    }
    pub(super) async fn new(
        connection: Arc<RTCPeerConnection>,
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
        initial: VideoConfig,
        negotiated: Arc<Negotiated>,
        network: Transport,
        selected: capture::Screen,
        displays: Arc<crate::features::host::displays::Session>,
    ) -> Result<Self> {
        let reports = Arc::new(Reports {
            catalog: Mutex::default(),
            current: AtomicI32::new(selected.id),
            sequence: std::sync::atomic::AtomicI64::new(1),
            publications: (0..TRACK_COUNT).map(|_| Mutex::new(None)).collect(),
            changed: watch::channel(0).0,
        });
        let mut slots = Vec::new();
        for index in 0..TRACK_COUNT {
            let negotiated = Arc::new(negotiated.for_track());
            let transport = if index == 0 {
                network.clone()
            } else {
                network.fork(index)
            };
            let track = Arc::new(VideoTrack::new(
                index,
                initial.format.codec,
                negotiated.clone(),
            ));
            let sender = connection
                .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
                .await?;
            let parameters = sender.get_parameters().await;
            for encoding in parameters.encodings {
                transport.register_ssrc(encoding.ssrc);
            }
            slots.push(Slot {
                screen: None,
                suspended: None,
                config: Arc::new(Mutex::new(initial)),
                negotiated,
                transport,
                sender,
                track,
                clock: media::Timeline::new(),
                worker: None,
                observer: None,
                lease: None,
                awaiting_source: false,
            });
        }
        let mut state = Self {
            displays,
            slots,
            registered: vec![0],
            reports,
            connection: Arc::downgrade(&connection),
            lease,
            cancel,
            connected,
            base: initial,
            originals: BTreeMap::new(),
            source_generation: 0,
            initial: selected.clone(),
            before_super: None,
        };
        state.refresh()?;
        state.start_at(0, selected, initial).await?;
        Ok(state)
    }
    pub(crate) fn refresh(&mut self) -> Result<()> {
        let targets = Topology::query(false)?.targets()?;
        let mut catalog = Vec::new();
        for mut screen in capture::screens()? {
            let mut target = targets
                .iter()
                .find(|target| {
                    screen
                        .identity
                        .as_ref()
                        .map_or(target.source == screen.device_name, |identity| {
                            *identity == target.identity
                        })
                })
                .cloned();
            let (kind, resolution_type) = self.displays.annotate(&mut screen, &mut target)?;
            let initial = self
                .originals
                .entry(screen.id)
                .or_insert_with(|| self.displays.initial_screen(&screen))
                .clone();
            catalog.push(ScreenInfo {
                screen,
                initial,
                target,
                kind,
                resolution_type,
            });
        }
        let mut old = lock(&self.reports.catalog);
        if *old != catalog {
            *old = catalog;
            drop(old);
            self.reports.notify();
        }
        Ok(())
    }
    pub(crate) fn info(&self, id: i32) -> Result<ScreenInfo> {
        lock(&self.reports.catalog)
            .iter()
            .find(|s| s.screen.id == id)
            .cloned()
            .context("显示器已不可用")
    }
    pub(crate) fn active_slot(&self, id: i32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.screen.as_ref().is_some_and(|s| s.id == id))
    }
    pub(crate) fn available_slot(&self) -> Option<usize> {
        self.registered
            .iter()
            .copied()
            .find(|&index| self.slots[index].screen.is_none())
    }
    pub(crate) async fn register(&mut self, indices: &[i32]) -> Result<()> {
        ensure!(
            indices.len() <= TRACK_COUNT
                && indices.iter().all(|i| (0..TRACK_COUNT as i32).contains(i)),
            "无效视频轨道池"
        );
        let mut registered: Vec<_> = indices.iter().map(|i| *i as usize).collect();
        registered.sort_unstable();
        registered.dedup();
        self.registered = registered;
        for (index, slot) in self.slots.iter().enumerate() {
            lock(&slot.config).sending = self.registered.contains(&index);
            if !self.registered.contains(&index) {
                slot.transport.pause();
            }
        }
        self.reports.notify();
        Ok(())
    }
    pub(crate) async fn start(&mut self, id: i32) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        self.refresh()?;
        let screen = self.info(id)?.screen;
        if let Some(index) = self.active_slot(id) {
            if self.slots[index]
                .worker
                .as_ref()
                .is_some_and(|w| !w.ended())
            {
                let mut config = lock(&self.slots[index].config);
                config.capturing = true;
                config.sending = self.registered.contains(&index);
                drop(config);
                self.reports.current.store(id, Ordering::Release);
                self.reports.notify();
                return Ok(());
            }
            let config = *lock(&self.slots[index].config);
            return self.start_at(index, screen, config).await;
        }
        let index = self
            .slots
            .iter()
            .enumerate()
            .find(|(index, s)| {
                self.registered.contains(index) && s.suspended.as_ref().is_some_and(|s| s.id == id)
            })
            .map(|(index, _)| index)
            .or_else(|| self.available_slot())
            .or_else(|| (self.registered.len() == 1).then(|| self.registered[0]))
            .context("没有空闲的已登记视频轨道")?;
        let config = if self.slots[index]
            .suspended
            .as_ref()
            .is_some_and(|s| s.id == id)
        {
            *lock(&self.slots[index].config)
        } else {
            self.base
        };
        self.start_at(index, screen, config).await
    }
    async fn start_at(
        &mut self,
        index: usize,
        screen: capture::Screen,
        mut config: VideoConfig,
    ) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        let requested_format = config.format;
        self.slots[index].negotiated.apply(
            &mut config,
            None,
            requested_format.chroma,
            requested_format.hdr(),
            (screen.width, screen.height),
        )?;
        self.stop_at(index).await;
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "被控许可已失效"
        );
        let slot = &mut self.slots[index];
        slot.suspended = None;
        slot.awaiting_source = false;
        config.capturing = true;
        config.sending = self.registered.contains(&index);
        *lock(&slot.config) = config;
        self.source_generation = self.source_generation.wrapping_add(1);
        let (tx, mut rx) = watch::channel(Published {
            screen: screen.clone(),
            capturing: false,
            visible: false,
            quality: 0,
            fps: config.fps,
            encoder: None,
            capture: String::new(),
        });
        let lease = self.lease.begin_stream(index)?;
        let cancel = self.cancel.child_token();
        let worker = media::Worker::spawn(
            index,
            self.source_generation,
            slot.clock.clone(),
            self.connection.upgrade().context("媒体连接已关闭")?,
            slot.track.clone(),
            slot.sender.clone(),
            screen.clone(),
            lease.clone(),
            cancel.clone(),
            self.connected.clone(),
            slot.config.clone(),
            slot.negotiated.clone(),
            slot.transport.clone(),
            tx,
        )?;
        *lock(&self.reports.publications[index]) = Some(rx.clone());
        let reports = self.reports.clone();
        slot.observer = Some(tokio::spawn(async move {
            loop {
                tokio::select! { _=cancel.cancelled()=>break, result=rx.changed()=>{ if result.is_err() { break; } } }
                reports.notify();
            }
            reports.notify();
        }));
        slot.lease = Some(lease);
        slot.worker = Some(worker);
        slot.screen = Some(screen.clone());
        self.reports.current.store(screen.id, Ordering::Release);
        self.reports.notify();
        Ok(())
    }
    async fn stop_at(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        if let Some(lease) = slot.lease.take() {
            lease.clear_stream();
        }
        if let Some(worker) = slot.worker.take() {
            worker.close().await;
        }
        if let Some(observer) = slot.observer.take() {
            let _ = observer.await;
        }
        slot.transport.pause();
        slot.screen = None;
        *lock(&self.reports.publications[index]) = None;
        self.reports.notify();
    }
    pub(crate) async fn stop(&mut self, screen: i32) -> Result<()> {
        ensure!(self.lease.requested(), "被控许可已失效");
        if screen == -1 {
            for slot in &mut self.slots {
                slot.awaiting_source = false;
            }
            for index in 0..self.slots.len() {
                if self.slots[index].screen.is_some() {
                    self.slots[index].suspended = self.slots[index].screen.clone();
                }
                self.stop_at(index).await;
            }
        } else if let Some(index) = self.active_slot(screen) {
            self.slots[index].awaiting_source = false;
            self.slots[index].suspended = self.slots[index].screen.clone();
            self.stop_at(index).await;
        } else {
            for slot in &mut self.slots {
                if slot.suspended.as_ref().is_some_and(|s| s.id == screen) {
                    slot.awaiting_source = false;
                }
            }
            self.info(screen)?;
        }
        Ok(())
    }
    pub(crate) async fn maintain(&mut self) -> Result<()> {
        self.refresh()?;
        let mut fatal = false;
        for index in 0..self.slots.len() {
            if self.slots[index]
                .worker
                .as_ref()
                .is_some_and(|worker| worker.ended())
            {
                if self.slots[index]
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.source_missing())
                {
                    self.slots[index].awaiting_source = true;
                    self.slots[index].suspended = self.slots[index].screen.clone();
                } else {
                    fatal = true;
                }
                self.stop_at(index).await;
            }
            if self.slots[index].awaiting_source {
                if let Some(old) = self.slots[index].suspended.clone() {
                    if let Ok(info) = self.info(old.id) {
                        // A returning target must have the same monitor identity; never
                        // attach an unrelated monitor which inherited DISPLAYn.
                        if info.screen.identity == old.identity && old.identity.is_some() {
                            let config = *lock(&self.slots[index].config);
                            self.start_at(index, info.screen, config).await?;
                        }
                    }
                }
            }
        }
        if fatal
            && self
                .slots
                .iter()
                .all(|s| s.worker.is_none() && !s.awaiting_source)
        {
            self.cancel.cancel();
        }
        Ok(())
    }
    pub(super) async fn close(&mut self) {
        for index in 0..self.slots.len() {
            self.stop_at(index).await;
        }
        if let Err(error) = self.displays.close().await {
            self.lease
                .recovery_failed(format!("显示恢复未完成：{error:#}"));
            tracing::error!(%error,"display cleanup failed");
        }
    }
}
impl Drop for Screens {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Some(lease) = slot.lease.take() {
                lease.clear_stream();
            }
            slot.worker.take();
            if let Some(observer) = slot.observer.take() {
                observer.abort();
            }
            slot.transport.pause();
        }
    }
}
