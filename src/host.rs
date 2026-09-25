//! Local Windows publisher. Capture is owned by an explicit sharing session.
mod amf;
mod burst;
pub(crate) mod capture;
mod congestion;
mod cursor;
pub(crate) mod encoder;
mod encoder_rate;
mod fec;
pub(crate) mod format;
mod gdi;
mod gpu_conversion;
mod hevc;
mod keyframe;
pub(crate) mod network;
mod nvenc;
pub(crate) mod parameters;
pub(crate) mod peer;
mod preprocess;
mod protection;
mod qsv;
mod qsv_allocator;
mod rust_h264;
mod track;
mod transfer;
mod transport;

use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

pub(crate) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Default)]
pub(crate) struct Status {
    pub enabled: bool,
    pub connected: bool,
    pub message: String,
    pub frames: u64,
    pub error: Option<String>,
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
}

#[derive(Clone)]
pub(crate) struct ShareRequest {
    pub screen: capture::Screen,
    generation: u64,
}

#[derive(Default)]
struct Ownership {
    sharing: u64,
    media: u64,
    requested: bool,
    status: Status,
}

#[derive(Clone)]
pub(crate) struct Handle {
    last_controlled: Arc<Mutex<Option<std::time::Instant>>>,
    desired: watch::Sender<Option<ShareRequest>>,
    ownership: Arc<Mutex<Ownership>>,
}
impl Default for Handle {
    fn default() -> Self {
        Self {
            last_controlled: Arc::new(Mutex::new(None)),
            desired: watch::channel(None).0,
            ownership: Arc::new(Mutex::new(Ownership::default())),
        }
    }
}
impl Handle {
    pub(crate) fn start(&self, screen: capture::Screen) {
        let mut state = lock(&self.ownership);
        state.sharing = state.sharing.wrapping_add(1);
        state.requested = true;
        state.status.error = None;
        state.status.frames = 0;
        state.status.encoder = None;
        state.status.capture = None;
        state.status.screen = None;
        self.desired.send_replace(Some(ShareRequest {
            screen,
            generation: state.sharing,
        }));
    }
    pub(crate) fn stop(&self) {
        self.stop_locked(&mut lock(&self.ownership));
    }
    fn stop_locked(&self, state: &mut Ownership) {
        state.sharing = state.sharing.wrapping_add(1);
        state.requested = false;
        self.update_locked(state, false, false, "画面共享已关闭".into());
        self.desired.send_replace(None);
    }
    pub(crate) fn requested(&self) -> bool {
        lock(&self.ownership).requested
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<ShareRequest>> {
        self.desired.subscribe()
    }
    pub(crate) fn status(&self) -> Status {
        lock(&self.ownership).status.clone()
    }
    pub(crate) fn update(&self, enabled: bool, connected: bool, message: impl Into<String>) {
        self.update_locked(
            &mut lock(&self.ownership),
            enabled,
            connected,
            message.into(),
        );
    }
    fn update_locked(
        &self,
        state: &mut Ownership,
        enabled: bool,
        connected: bool,
        message: String,
    ) {
        if connected || state.status.connected {
            *lock(&self.last_controlled) = Some(std::time::Instant::now());
        }
        state.status.enabled = enabled;
        state.status.connected = connected;
        state.status.message = message;
    }
    pub(crate) fn lease(&self, request: &ShareRequest) -> Lease {
        // The authorization belongs to this exact source request, not whichever
        // share happens to be current after an asynchronous operation finishes.
        Lease {
            handle: self.clone(),
            sharing: request.generation,
            media: None,
        }
    }
    pub(crate) fn last_controlled_interval(&self) -> i64 {
        lock(&self.last_controlled)
            .map_or(-1, |at| at.elapsed().as_secs().min(i64::MAX as u64) as i64)
    }
}

#[derive(Clone)]
pub(crate) struct Lease {
    handle: Handle,
    sharing: u64,
    media: Option<u64>,
}
impl Lease {
    fn current(&self, state: &Ownership) -> bool {
        state.requested
            && self.sharing == state.sharing
            && self.media.is_none_or(|media| media == state.media)
    }
    fn modify(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            update(&mut state);
        }
    }
    pub(crate) fn peer_lease(&self) -> anyhow::Result<Self> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "所选屏幕的共享授权已结束");
        state.media = state.media.wrapping_add(1);
        state.status.connected = false;
        state.status.encoder = None;
        state.status.capture = None;
        state.status.screen = None;
        state.status.frames = 0;
        state.status.error = None;
        Ok(Self {
            handle: self.handle.clone(),
            sharing: self.sharing,
            media: Some(state.media),
        })
    }
    pub(crate) fn stop_with_error(&self, error: impl Into<String>) {
        self.modify(|state| {
            self.handle.stop_locked(state);
            state.status.error = Some(error.into());
        });
    }
    pub(crate) fn source(&self, screen: &capture::Screen, backend: &str) {
        self.modify(|state| {
            state.status.capture = Some(backend.into());
            state.status.screen = Some(screen.clone());
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
            state.status.encoder = Some((implementation, maximum));
            state.status.capture = Some(capture.into());
            state.status.screen = Some(screen.clone());
        });
    }
    pub(crate) fn requested(&self) -> bool {
        self.current(&lock(&self.handle.ownership))
    }
    pub(crate) fn update(&self, enabled: bool, connected: bool, message: impl Into<String>) {
        self.modify(|state| {
            self.handle
                .update_locked(state, enabled, connected, message.into())
        });
    }
    pub(crate) fn frame(&self) {
        self.modify(|state| {
            state.status.frames += 1;
            state.status.error = None;
            state.status.message = if state
                .status
                .encoder
                .is_some_and(|(implementation, _)| implementation == 5)
            {
                "正在共享画面 · 软件编码回退"
            } else {
                "正在共享画面"
            }
            .into();
        });
    }
    pub(crate) fn fail(&self, error: impl Into<String>) {
        self.modify(|state| state.status.error = Some(error.into()));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoConfig {
    pub fps: u32,
    pub requested_fps: u32,
    pub fps_limit: u32,
    pub bitrate: u32,
    pub quality: i32,
    pub auto_quality: i32,
    pub revision: u64,
    pub reported_quality: i32,
    pub format: format::Format,
    pub maximum: (u32, u32),
    pub maximum_fps: u32,
    pub maximum_quality: i32,
    pub sending: bool,
    pub capturing: bool,
    pub cursor_capture: bool,
}
impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            requested_fps: 30,
            fps_limit: 144,
            bitrate: 8_000_000,
            quality: 2,
            auto_quality: 2,
            revision: 0,
            reported_quality: 2,
            format: format::Format::AVC,
            maximum: (3840, 2160),
            maximum_fps: 144,
            maximum_quality: 4,
            sending: true,
            capturing: true,
            cursor_capture: false,
        }
    }
}

pub(crate) fn output_size(width: u32, height: u32, quality: i32) -> (u32, u32) {
    fit_size(width, height, parameters::dimensions(quality))
}

pub(crate) fn fit_size(width: u32, height: u32, maximum: (u32, u32)) -> (u32, u32) {
    if width <= maximum.0 && height <= maximum.1 {
        return ((width & !1).max(2), (height & !1).max(2));
    }
    // T CCE2C0/CD3A70: single-precision min scale, then 2*round(x/2).
    // The bounds keep their axes on portrait sources; they are not swapped.
    let scale = (maximum.0 as f32 / width as f32)
        .min(maximum.1 as f32 / height as f32)
        .min(1.0);
    (
        ((width as f32 * scale * 0.5).round() as u32 * 2).max(2),
        ((height as f32 * scale * 0.5).round() as u32 * 2).max(2),
    )
}
