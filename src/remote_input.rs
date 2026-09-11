//! Typed keyboard/mouse events on UU CONTROL, independent of GUI frame cadence.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use anyhow::{Result, bail};
use tokio::sync::Notify;

const MAX_EVENTS: usize = 512;
pub(crate) const BUTTONS: [u32; 5] = [1, 2, 16, 32, 64];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    #[default]
    View,
    Smart,
    Local,
    Remote,
}

#[derive(Clone, Debug)]
pub(crate) enum InputEvent {
    Absolute {
        screen: i32,
        x: f64,
        y: f64,
    },
    Relative {
        x: i32,
        y: i32,
    },
    Button {
        button: u32,
        down: bool,
    },
    Wheel {
        delta: i32,
        horizontal: bool,
    },
    Key {
        key: u16,
        down: bool,
        lock: Option<u8>,
        interrupt: bool,
    },
    Heartbeat,
}

impl InputEvent {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let value = match *self {
            Self::Absolute { screen, x, y } => serde_json::json!({
                "action":"mouse_move_absolute", "screen_id":screen, "abs_x":x, "abs_y":y }),
            Self::Relative { x, y } => serde_json::json!({
                "action":"mouse_move_relative", "delta_x":x, "delta_y":y, "mousetype":2 }),
            Self::Button { button, down } => serde_json::json!({
                "action":if down { "mouse_press" } else { "mouse_release" }, "button":button }),
            Self::Wheel { delta, horizontal } => serde_json::json!({
                "action":"mouse_scroll", "delta_x":if horizontal {delta} else {0},
                "delta_y":if horizontal {0} else {delta} }),
            Self::Key {
                key,
                down,
                lock,
                interrupt,
            } => {
                let mut value = serde_json::json!({
                    "action": if down { "kbd_press" } else { "kbd_release" }, "key": key
                });
                if interrupt {
                    value["interrept"] = "i".into();
                }
                if let Some(status) = lock {
                    value[if key == 144 {
                        "lockkeysstatus"
                    } else {
                        "status_key_value"
                    }] = status.into();
                }
                value
            }
            Self::Heartbeat => serde_json::json!({"heartbeat":"kbd_heartbeat"}),
        };
        serde_json::to_vec(&value).expect("finite mouse events serialize")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedInputEvent {
    pub epoch: u64,
    pub event: InputEvent,
}

#[derive(Default)]
struct State {
    ready: bool,
    stopping: bool,
    mode: MouseMode,
    relative: bool,
    owner: Option<u64>,
    held: [bool; 5],
    // Marked before handing DOWN to transport. A concurrent stop must release
    // even an in-flight press, not just messages whose send() already returned.
    remote_held: [bool; 5],
    keys: BTreeMap<u16, Option<u8>>,
    remote_keys: BTreeMap<u16, Option<u8>>,
    // Transport acceptance of UP is not acknowledgement of host injection.
    // Keep keys retired at an ownership boundary for session reconciliation.
    release_checkpoint: BTreeSet<u16>,
    keyboard_generation: u64,
    keyboard_platform: i32,
    queue: VecDeque<InputEvent>,
    in_flight: bool,
    epoch: u64,
    error: Option<String>,
    recovering: bool,
    listeners: Vec<Weak<dyn Fn() + Send + Sync>>,
}

#[derive(Clone, Default)]
pub(crate) struct RemoteInput {
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    drained: Arc<Notify>,
    epoch_changed: Arc<Notify>,
}

impl RemoteInput {
    pub fn set_keyboard_platform(&self, platform: i32) {
        self.lock().keyboard_platform = platform;
    }

    pub fn keyboard_generation(&self) -> u64 {
        self.lock().keyboard_generation
    }

    pub fn keyboard_supported(&self) -> bool {
        self.lock().keyboard_platform == 1
    }

    /// Native adapters supply Windows VKs only for a verified Windows peer.
    /// Other target encodings are separate adapters, never a cast of local keys.
    pub fn key(&self, owner: u64, key: u16, down: bool, lock: Option<u8>) -> bool {
        if !(1..=254).contains(&key) {
            return false;
        }
        let mut s = self.lock();
        if s.keyboard_platform != 1 {
            return false;
        }
        if down {
            if !Self::claim(&mut s, owner) {
                return false;
            }
        } else if s.owner != Some(owner) || !s.keys.contains_key(&key) {
            return false;
        }
        let lock = lock.filter(|v| matches!(key, 20 | 144 | 145) && matches!(v, 0 | 128));
        let accepted = Self::push(
            &mut s,
            InputEvent::Key {
                key,
                down,
                lock,
                interrupt: true,
            },
        );
        if accepted {
            if down {
                s.keys.insert(key, lock);
            } else {
                s.keys.remove(&key);
            }
        }
        drop(s);
        self.wake.notify_one();
        accepted
    }

    pub fn heartbeat(&self) {
        let mut s = self.lock();
        if s.ready
            && !s.stopping
            && s.mode != MouseMode::View
            && !s.keys.is_empty()
            && !s.queue.iter().any(|e| matches!(e, InputEvent::Heartbeat))
        {
            Self::push(&mut s, InputEvent::Heartbeat);
            drop(s);
            self.wake.notify_one();
        }
    }

    pub fn subscribe(
        &self,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(callback);
        let mut s = self.lock();
        s.listeners.retain(|listener| listener.strong_count() != 0);
        s.listeners.push(Arc::downgrade(&callback));
        callback
    }

    pub fn repaint(&self) {
        let callbacks: Vec<_> = self
            .lock()
            .listeners
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for callback in callbacks {
            callback();
        }
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn mode(&self) -> MouseMode {
        self.lock().mode
    }
    pub fn relative_mode(&self) -> bool {
        self.lock().relative
    }
    pub fn set_relative_mode(&self, relative: bool) {
        let mut s = self.lock();
        s.relative = relative;
        drop(s);
        self.repaint();
    }
    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }
    pub fn owner_holds_buttons(&self, owner: u64) -> bool {
        let s = self.lock();
        s.owner == Some(owner) && s.held.iter().any(|held| *held)
    }

    pub fn owner_holds_key(&self, owner: u64, key: u16) -> bool {
        let s = self.lock();
        s.owner == Some(owner) && s.keys.contains_key(&key)
    }

    pub fn set_ready(&self, ready: bool) {
        let mut s = self.lock();
        let became_ready = ready && !s.ready && !s.stopping;
        if !ready {
            s.epoch = s.epoch.wrapping_add(1);
            s.mode = MouseMode::View;
            s.owner = None;
            s.held = [false; 5];
            s.keys.clear();
            s.keyboard_generation = s.keyboard_generation.wrapping_add(1);
            s.queue.clear();
            s.in_flight = false;
        } else if !s.ready && !s.stopping {
            // A transport send can have accepted DOWN before a disconnect.
            // Retire that uncertainty with UP before any newly enabled input.
            Self::release_locked(&mut s);
        }
        s.ready = ready && !s.stopping;
        drop(s);
        if became_ready {
            self.reconcile_keyboard_releases();
        }
        self.wake.notify_one();
        self.drained.notify_waiters();
        self.epoch_changed.notify_waiters();
        self.repaint();
    }

    pub fn enable(&self, mode: MouseMode, relative: bool) -> Result<()> {
        let mut s = self.lock();
        if !s.ready || s.stopping {
            bail!("控制连接尚未就绪");
        }
        Self::release_locked(&mut s);
        s.mode = mode;
        s.relative = relative;
        s.error = None;
        s.recovering = false;
        drop(s);
        self.wake.notify_one();
        self.repaint();
        Ok(())
    }

    fn release_locked(s: &mut State) {
        // Cancel unsent presses/motion. Preserve release obligations for events
        // already given to transport. Never replay an obsolete click on resume.
        s.queue.clear();
        s.keyboard_generation = s.keyboard_generation.wrapping_add(1);
        s.release_checkpoint.extend(s.remote_keys.keys().copied());
        // Release ordinary keys before modifiers; never synthesize new downs.
        for modifier in [false, true] {
            for &key in s.remote_keys.keys() {
                if matches!(key, 16..=18 | 91..=92 | 160..=165) == modifier {
                    s.queue.push_back(InputEvent::Key {
                        key,
                        down: false,
                        lock: None,
                        interrupt: false,
                    });
                }
            }
        }
        s.keys.clear();
        for (index, button) in BUTTONS.into_iter().enumerate() {
            if s.remote_held[index] {
                s.queue.push_back(InputEvent::Button {
                    button,
                    down: false,
                });
            }
        }
        s.held = [false; 5];
        s.owner = None;
    }

    pub fn pause_owner(&self, owner: u64) {
        let mut s = self.lock();
        if s.owner != Some(owner) {
            return;
        }
        Self::release_locked(&mut s);
        drop(s);
        self.wake.notify_one();
    }

    /// Reconcile release-only state across an OS input-desktop transition.
    /// This also covers Focused(false) retiring keys before WTS_SESSION_LOCK.
    /// Never replay a press or restore a stale Caps/Num/Scroll lock preference.
    pub fn reconcile_keyboard_releases(&self) {
        let mut s = self.lock();
        let retired = std::mem::take(&mut s.release_checkpoint);
        if s.ready {
            for key in retired {
                if !s.queue.iter().any(|event| matches!(event, InputEvent::Key { key: queued, down: false, .. } if *queued == key)) {
                    s.remote_keys.entry(key).or_insert(None);
                    s.queue.push_back(InputEvent::Key { key, down: false, lock: None, interrupt: false });
                }
            }
        } else {
            s.release_checkpoint = retired;
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn disable(&self) {
        let mut s = self.lock();
        s.mode = MouseMode::View;
        Self::release_locked(&mut s);
        if !s.ready {
            s.queue.clear();
        }
        drop(s);
        self.wake.notify_one();
        self.repaint();
    }

    pub fn fail(&self, error: String) {
        self.disable();
        self.lock().error = Some(error);
        self.repaint();
    }

    fn claim(s: &mut State, owner: u64) -> bool {
        if !s.ready || s.stopping || s.mode == MouseMode::View {
            return false;
        }
        if s.owner != Some(owner) {
            Self::release_locked(s);
            s.owner = Some(owner);
        }
        true
    }

    fn push(s: &mut State, event: InputEvent) -> bool {
        if s.queue.len() >= MAX_EVENTS {
            Self::release_locked(s);
            s.mode = MouseMode::View;
            s.error = Some("键鼠输入积压，已停止控制并释放按键，请重新开启".into());
            return false;
        }
        s.queue.push_back(event);
        true
    }

    pub fn absolute(&self, owner: u64, screen: i32, x: f64, y: f64) {
        if screen < 0
            || !x.is_finite()
            || !y.is_finite()
            || !(0.0..1.0).contains(&x)
            || !(0.0..1.0).contains(&y)
        {
            return;
        }
        let mut s = self.lock();
        if s.relative || !Self::claim(&mut s, owner) {
            return;
        }
        if let Some(InputEvent::Absolute {
            screen: old_screen,
            x: old_x,
            y: old_y,
        }) = s.queue.back_mut()
            && *old_screen == screen
        {
            *old_x = x;
            *old_y = y;
        } else {
            Self::push(&mut s, InputEvent::Absolute { screen, x, y });
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn relative(&self, owner: u64, x: i32, y: i32) {
        if x == 0 && y == 0 {
            return;
        }
        let mut s = self.lock();
        if !s.relative || !Self::claim(&mut s, owner) {
            return;
        }
        if let Some(InputEvent::Relative { x: old_x, y: old_y }) = s.queue.back_mut()
            && let (Some(x), Some(y)) = (old_x.checked_add(x), old_y.checked_add(y))
        {
            *old_x = x;
            *old_y = y;
        } else {
            Self::push(&mut s, InputEvent::Relative { x, y });
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn button(&self, owner: u64, button: u32, down: bool) {
        let Some(index) = BUTTONS.iter().position(|b| *b == button) else {
            return;
        };
        let mut s = self.lock();
        if down {
            if !Self::claim(&mut s, owner) {
                return;
            }
        } else if s.owner != Some(owner) || !s.held[index] {
            return;
        }
        // Do not collapse rapid DOWN/UP transitions to a frame's final state.
        if Self::push(&mut s, InputEvent::Button { button, down }) {
            s.held[index] = down;
            tracing::trace!(button, down, "mouse button edge queued");
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn wheel(&self, owner: u64, delta: i32, horizontal: bool) {
        if delta == 0 {
            return;
        }
        let mut s = self.lock();
        if !Self::claim(&mut s, owner) {
            return;
        }
        Self::push(&mut s, InputEvent::Wheel { delta, horizontal });
        drop(s);
        self.wake.notify_one();
    }

    pub async fn next(&self) -> QueuedInputEvent {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            {
                let mut s = self.lock();
                if let Some(event) = s.queue.pop_front() {
                    s.in_flight = true;
                    if let InputEvent::Button { button, down: true } = event
                        && let Some(index) = BUTTONS.iter().position(|b| *b == button)
                    {
                        s.remote_held[index] = true;
                    }
                    if let InputEvent::Key {
                        key,
                        down: true,
                        lock,
                        ..
                    } = event
                    {
                        s.remote_keys.insert(key, lock);
                        s.release_checkpoint.remove(&key);
                    }
                    return QueuedInputEvent {
                        epoch: s.epoch,
                        event,
                    };
                }
            }
            wake.await;
        }
    }

    pub fn is_current(&self, event: &QueuedInputEvent) -> bool {
        let s = self.lock();
        s.ready && event.epoch == s.epoch
    }

    pub fn discard(&self, event: &QueuedInputEvent) {
        let mut s = self.lock();
        if event.epoch == s.epoch {
            s.in_flight = false;
        }
        drop(s);
        self.drained.notify_waiters();
    }

    pub async fn epoch_cancelled(&self, epoch: u64) {
        loop {
            let changed = self.epoch_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.lock().epoch != epoch {
                return;
            }
            changed.await;
        }
    }

    pub fn complete(&self, event: &QueuedInputEvent, result: Result<()>) {
        let mut s = self.lock();
        if event.epoch != s.epoch {
            return;
        }
        s.in_flight = false;
        match result {
            Ok(()) => {
                if let InputEvent::Key {
                    key, down: false, ..
                } = event.event
                {
                    s.remote_keys.remove(&key);
                }
                if let InputEvent::Button { button, down } = event.event {
                    tracing::trace!(button, down, "mouse button edge submitted to transport");
                }
                if let InputEvent::Button {
                    button,
                    down: false,
                } = event.event
                    && let Some(index) = BUTTONS.iter().position(|b| *b == button)
                {
                    s.remote_held[index] = false;
                }
            }
            Err(error) => {
                s.mode = MouseMode::View;
                s.owner = None;
                s.held = [false; 5];
                if !s.recovering {
                    Self::release_locked(&mut s);
                    if let InputEvent::Button {
                        button,
                        down: false,
                    } = event.event
                    {
                        s.queue.retain(|event| !matches!(event, InputEvent::Button { button: queued, .. } if *queued == button));
                    }
                    s.recovering = true;
                    if let InputEvent::Key {
                        key, down: false, ..
                    } = event.event
                    {
                        s.queue.retain(
                            |e| !matches!(e, InputEvent::Key { key: queued, .. } if *queued == key),
                        );
                    }
                }
                s.error = Some(format!("键鼠发送失败，已停止控制：{error}"));
                // One best-effort release pass. Never retry presses/clicks, or
                // loop forever if the release itself cannot be sent.
            }
        }
        drop(s);
        self.drained.notify_waiters();
        if self.mode() == MouseMode::View {
            self.repaint();
        }
    }

    pub async fn close(&self) {
        {
            let mut s = self.lock();
            s.stopping = true;
            s.mode = MouseMode::View;
            Self::release_locked(&mut s);
            if !s.ready {
                s.queue.clear();
                s.in_flight = false;
            }
        }
        self.wake.notify_one();
        let drain = async {
            loop {
                let changed = self.drained.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let s = self.lock();
                    if s.queue.is_empty() && !s.in_flight {
                        break;
                    }
                }
                changed.await;
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(1), drain)
            .await
            .is_err()
        {
            tracing::warn!("mouse release drain timed out; remote receipt is unknown");
        }
        self.set_ready(false);
    }
}
