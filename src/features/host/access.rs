//! Device access policy, account lifetime and individual connection leases.
use super::{capture, lock, settings};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Default)]
pub(crate) struct Status {
    pub ready: bool,
    pub connected: bool,
    pub session_active: bool,
    pub message: String,
    pub error: Option<String>,
    pub settings_error: Option<String>,
    pub saving: bool,
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
    pub streams: std::collections::BTreeMap<usize, StreamStatus>,
}

#[derive(Clone, Default)]
pub(crate) struct StreamStatus {
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
}

fn update_media_summary(status: &mut Status) {
    let current = status
        .streams
        .values()
        .find(|s| s.video.is_some())
        .or_else(|| status.streams.values().next());
    status.encoder = current.and_then(|s| s.encoder);
    status.capture = current.and_then(|s| s.capture.clone());
    status.screen = current.and_then(|s| s.screen.clone());
    status.video = current.and_then(|s| s.video);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActiveEncoding {
    pub backend: super::format::Backend,
    pub adapter: u64,
    pub format: super::format::Format,
    pub size: (u32, u32),
    pub fps: u32,
    pub target_bps: u32,
}

#[derive(Clone)]
pub(crate) struct AccessRequest {
    generation: u64,
}

struct Ownership {
    permission: u64,
    settings_revision: u64,
    encoding: super::EncodingSettings,
    media: u64,
    stream_generations: std::collections::BTreeMap<usize, u64>,
    allowed: bool,
    active: bool,
    dirty: bool,
    last_controlled: Option<std::time::Instant>,
    status: Status,
}

#[derive(Clone)]
pub(crate) struct Handle {
    desired: watch::Sender<Option<AccessRequest>>,
    ownership: Arc<Mutex<Ownership>>,
    store: Option<settings::Store>,
    saving: Arc<tokio::sync::Mutex<()>>,
    media: Arc<super::desktop::Cache>,
    scope: String,
}

impl Default for Handle {
    fn default() -> Self {
        Self {
            desired: watch::channel(None).0,
            ownership: Arc::new(Mutex::new(Ownership {
                permission: 0,
                settings_revision: 0,
                encoding: Default::default(),
                media: 0,
                stream_generations: Default::default(),
                allowed: false,
                active: true,
                dirty: false,
                last_controlled: None,
                status: Status::default(),
            })),
            store: None,
            saving: Arc::new(tokio::sync::Mutex::new(())),
            media: Arc::default(),
            scope: String::new(),
        }
    }
}

fn finish_session(state: &mut Ownership) {
    if state.status.connected {
        state.last_controlled = Some(std::time::Instant::now());
    }
    state.status.connected = false;
    state.status.session_active = false;
    state.status.encoder = None;
    state.status.capture = None;
    state.status.screen = None;
    state.status.video = None;
    state.status.streams.clear();
    state.stream_generations.clear();
}

impl Handle {
    pub(crate) async fn prepare_startup(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        tokio::task::spawn_blocking(super::displays::recovery::recover_abandoned).await??;
        let screens = capture::screens()?;
        let Some(screen) = screens
            .iter()
            .find(|s| s.primary)
            .or(screens.first())
            .cloned()
        else {
            return Ok(());
        };
        let owner = self.clone();
        self.media
            .prepare(screen, move || {
                !cancel.is_cancelled() && lock(&owner.ownership).active
            })
            .await?;
        Ok(())
    }
    pub(crate) async fn invalidate_media(&self) {
        self.media.invalidate().await;
    }
    pub(crate) fn load(account: &str, device: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut handle = Self::default();
        handle.scope = format!("{:x}", Sha256::digest(format!("{account}\0{device}")));
        let result = settings::Store::new(account, device).and_then(|store| {
            handle.store = Some(store.clone());
            store.load()
        });
        match result {
            Ok((allowed, encoding)) => {
                handle.set_allowed(allowed);
                let mut state = lock(&handle.ownership);
                state.encoding = encoding;
                state.dirty = false;
                state.status.saving = false;
            }
            Err(error) => lock(&handle.ownership).status.settings_error = Some(error.to_string()),
        }
        handle
    }
    pub(crate) fn allowed(&self) -> bool {
        lock(&self.ownership).allowed
    }
    pub(crate) fn encoding_settings(&self) -> super::EncodingSettings {
        lock(&self.ownership).encoding
    }
    pub(crate) fn set_encoding_settings(
        &self,
        encoding: super::EncodingSettings,
    ) -> anyhow::Result<()> {
        encoding.validate()?;
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.encoding != encoding {
            state.encoding = encoding;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn capabilities(&self) -> Option<Arc<super::desktop::Capabilities>> {
        self.media.snapshot()
    }
    pub(crate) fn set_allowed(&self, allowed: bool) {
        let mut state = lock(&self.ownership);
        if !state.active || state.allowed == allowed {
            return;
        }
        state.allowed = allowed;
        state.settings_revision = state.settings_revision.wrapping_add(1);
        state.permission = state.permission.wrapping_add(1);
        state.dirty = true;
        finish_session(&mut state);
        state.status.ready = false;
        state.status.error = None;
        state.status.settings_error = None;
        state.status.saving = true;
        state.status.message = if allowed {
            "正在启用被控…"
        } else {
            "已禁止被控"
        }
        .into();
        self.desired.send_replace(allowed.then_some(AccessRequest {
            generation: state.permission,
        }));
    }
    pub(crate) async fn persist_settings(&self) {
        let _saving = self.saving.lock().await;
        loop {
            let (revision, allowed, encoding) = {
                let state = lock(&self.ownership);
                if !state.dirty {
                    return;
                }
                (state.settings_revision, state.allowed, state.encoding)
            };
            let result = if let Some(store) = self.store.clone() {
                tokio::task::spawn_blocking(move || store.save(allowed, encoding))
                    .await
                    .map_err(|_| anyhow::anyhow!("被控设置保存任务中断"))
                    .and_then(|r| r)
            } else {
                Err(anyhow::anyhow!("被控设置存储不可用"))
            };
            let mut state = lock(&self.ownership);
            if state.settings_revision != revision {
                continue;
            }
            state.dirty = false;
            state.status.saving = false;
            state.status.settings_error = result.err().map(|e| e.to_string());
            return;
        }
    }
    pub(crate) fn retry(&self) {
        let mut state = lock(&self.ownership);
        if !state.active || !state.allowed {
            return;
        }
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.error = None;
        state.status.message = "正在启用被控…".into();
        self.desired.send_replace(Some(AccessRequest {
            generation: state.permission,
        }));
    }
    pub(crate) fn retire(&self) {
        let mut state = lock(&self.ownership);
        state.active = false;
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = "本机离线".into();
        self.desired.send_replace(None);
    }
    pub(crate) fn room_closed(&self) {
        let mut state = lock(&self.ownership);
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = "本机离线".into();
        self.desired
            .send_replace((state.active && state.allowed).then_some(AccessRequest {
                generation: state.permission,
            }));
    }
    pub(crate) fn disconnect(&self) {
        let mut state = lock(&self.ownership);
        if !state.status.session_active {
            return;
        }
        state.media = state.media.wrapping_add(1);
        finish_session(&mut state);
        state.status.message = "等待连接".into();
    }
    pub(crate) fn requested(&self) -> bool {
        let state = lock(&self.ownership);
        state.active && state.allowed
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<AccessRequest>> {
        self.desired.subscribe()
    }
    pub(crate) fn status(&self) -> Status {
        lock(&self.ownership).status.clone()
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        Self::update_locked(&mut lock(&self.ownership), ready, connected, message.into());
    }
    fn update_locked(state: &mut Ownership, ready: bool, connected: bool, message: String) {
        if connected || state.status.connected {
            state.last_controlled = Some(std::time::Instant::now());
        }
        state.status.ready = ready;
        state.status.connected = connected;
        state.status.message = message;
    }
    pub(crate) fn lease(&self, request: &AccessRequest) -> Lease {
        Lease {
            handle: self.clone(),
            permission: request.generation,
            media: None,
            track: 0,
            stream_generation: None,
            stop: None,
        }
    }
    pub(crate) fn last_controlled_interval(&self) -> i64 {
        lock(&self.ownership)
            .last_controlled
            .map_or(-1, |at| at.elapsed().as_secs().min(i64::MAX as u64) as i64)
    }
}

#[derive(Clone)]
pub(crate) struct Lease {
    handle: Handle,
    permission: u64,
    media: Option<u64>,
    track: usize,
    stream_generation: Option<u64>,
    stop: Option<tokio_util::sync::CancellationToken>,
}

/// Unique connection owner. Callback clones only hold the checked lease.
pub(crate) struct SessionLease(Lease);
impl std::ops::Deref for SessionLease {
    type Target = Lease;
    fn deref(&self) -> &Lease {
        &self.0
    }
}
impl Drop for SessionLease {
    fn drop(&mut self) {
        self.0.finish();
    }
}
impl Lease {
    pub(crate) fn with_cancellation(&self, stop: tokio_util::sync::CancellationToken) -> Self {
        let mut lease = self.clone();
        lease.stop = Some(stop);
        lease
    }
    pub(crate) fn display_scope(&self) -> &str {
        &self.handle.scope
    }
    pub(crate) fn begin_stream(&self, track: usize) -> anyhow::Result<Self> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        let generation = state.stream_generations.entry(track).or_default();
        *generation = generation.wrapping_add(1);
        let mut lease = self.clone();
        lease.track = track;
        lease.stream_generation = Some(*generation);
        Ok(lease)
    }
    pub(crate) fn clear_stream(&self) {
        let mut state = lock(&self.handle.ownership);
        if self.current_owner(&state) {
            state.status.streams.remove(&self.track);
            let generation = state.stream_generations.entry(self.track).or_default();
            *generation = generation.wrapping_add(1);
            update_media_summary(&mut state.status);
        }
    }
    pub(crate) fn video(&self, encoding: Option<ActiveEncoding>) {
        self.modify(|state| {
            state.status.streams.entry(self.track).or_default().video = encoding;
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) async fn prepare_media(
        &self,
        screen: capture::Screen,
    ) -> anyhow::Result<Arc<super::desktop::Capabilities>> {
        let lease = self.clone();
        self.handle
            .media
            .prepare(screen, move || lease.requested())
            .await
    }
    fn current(&self, state: &Ownership) -> bool {
        self.current_owner(state) && self.stop.as_ref().is_none_or(|stop| !stop.is_cancelled())
    }
    fn current_owner(&self, state: &Ownership) -> bool {
        state.active
            && state.allowed
            && self.permission == state.permission
            && self.media.is_none_or(|m| m == state.media)
            && self
                .stream_generation
                .is_none_or(|g| state.stream_generations.get(&self.track) == Some(&g))
    }
    fn modify(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            update(&mut state);
        }
    }
    pub(crate) fn peer_lease(&self) -> anyhow::Result<SessionLease> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        state.media = state.media.wrapping_add(1);
        finish_session(&mut state);
        state.status.session_active = true;
        state.status.error = None;
        Ok(SessionLease(Self {
            handle: self.handle.clone(),
            permission: self.permission,
            media: Some(state.media),
            track: 0,
            stream_generation: None,
            stop: None,
        }))
    }
    pub(crate) fn finish(&self) {
        let mut state = lock(&self.handle.ownership);
        if self.current_owner(&state) {
            if self.media.is_some() {
                state.media = state.media.wrapping_add(1);
            }
            finish_session(&mut state);
            state.status.message = "等待连接".into();
        }
    }
    pub(crate) fn source(&self, screen: &capture::Screen, backend: &str) {
        self.modify(|state| {
            let stream = state.status.streams.entry(self.track).or_default();
            stream.capture = Some(backend.into());
            stream.screen = Some(screen.clone());
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) fn encoder(
        &self,
        implementation: i32,
        maximum: (u32, u32),
        screen: &capture::Screen,
        capture: &str,
    ) {
        self.modify(|state| {
            let stream = state.status.streams.entry(self.track).or_default();
            stream.encoder = Some((implementation, maximum));
            stream.capture = Some(capture.into());
            stream.screen = Some(screen.clone());
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) fn requested(&self) -> bool {
        self.current(&lock(&self.handle.ownership))
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        self.modify(|state| Handle::update_locked(state, ready, connected, message.into()));
    }
    pub(crate) fn frame(&self) {
        self.modify(|state| {
            state.status.error = None;
            state.status.message = "正在被远程访问".into();
        });
    }
    pub(crate) fn recovery_failed(&self, error: impl Into<String>) {
        let mut state = lock(&self.handle.ownership);
        if self.media == Some(state.media) {
            state.status.error = Some(error.into());
        }
    }
    pub(crate) fn fail(&self, error: impl Into<String>) {
        self.modify(|state| state.status.error = Some(error.into()));
    }
}
