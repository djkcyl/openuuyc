//! Foreground keyboard routing. The hook never waits for transport or logs keys.
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock, mpsc};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, KBDLLHOOKSTRUCT, MSG,
    PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::remote_input::{MouseMode, RemoteInput};

/// Winit registers mouse AND keyboard raw devices at event-loop creation.
/// This client uses only RAWMOUSE; keyboard uses window messages and a narrow hook.
/// Remove just the unused keyboard class before showing/focusing a window.
pub(super) fn remove_unused_raw_keyboard() -> Result<()> {
    use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
    let keyboard = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 6,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    };
    unsafe { RegisterRawInputDevices(&[keyboard], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
        .context("移除未使用的 Raw 键盘注册失败")?;
    tracing::debug!("unused raw keyboard registration removed; raw mouse retained");
    Ok(())
}

struct Target {
    owner: u64,
    input: RemoteInput,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Local,
    Window(u64),
    Hook(u64),
}

#[derive(Clone, Copy)]
struct KeyEdge {
    key: u16,
    scan: u32,
    down: bool,
    injected: bool,
}

struct PendingKey {
    edge: KeyEdge,
    route: Route,
    owner: u64,
    ready: bool,
}

const MAX_PENDING_KEYS: usize = 512;

struct Router {
    installed: bool,
    shortcut: Option<(u64, super::windows_presenter::ViewerShortcut)>,
    target: Option<Target>,
    physical: [bool; 256],
    blocked: [bool; 256],
    consumed: [bool; 256],
    lock_releases: Vec<(u64, u16, u64)>,
    diagnostic_seen: u64,
    observed: [bool; 256],
    routes: [Route; 256],
    legacy_owned: [Option<u64>; 256],
    pending: VecDeque<PendingKey>,
    windows: BTreeMap<u64, crate::stream_control::StreamControlHandle>,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            installed: false,
            shortcut: None,
            target: None,
            physical: [false; 256],
            blocked: [false; 256],
            consumed: [false; 256],
            lock_releases: Vec::new(),
            diagnostic_seen: 0,
            observed: [false; 256],
            routes: [Route::Local; 256],
            legacy_owned: [None; 256],
            pending: VecDeque::new(),
            windows: BTreeMap::new(),
        }
    }
}

fn router() -> MutexGuard<'static, Router> {
    static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();
    ROUTER
        .get_or_init(|| Mutex::new(Router::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn with_router<T>(f: impl FnOnce(&mut Router) -> T) -> T {
    f(&mut router())
}

fn blocked_modifiers(blocked: &[bool; 256]) -> bool {
    [91, 92, 160, 161, 162, 163, 164, 165]
        .into_iter()
        .any(|key| blocked[key])
}

fn intercept_key(key: u16, scan: u32, held: &[bool; 256]) -> bool {
    let ctrl = held[162] || held[163];
    let shift = held[160] || held[161];
    let alt = held[164] || held[165];
    let win = held[91] || held[92];
    matches!(key, 91 | 92)
        || win
        || (alt && matches!(key, 9 | 27))
        || (ctrl && key == 27)
        || (ctrl && shift && alt && matches!(scan, 0x2c | 0x21 | 0x10))
}

pub(super) struct SessionNotifications {
    owner: u64,
    registered: bool,
}

impl SessionNotifications {
    pub fn new(owner: u64, control: &crate::stream_control::StreamControlHandle) -> Self {
        use windows::Win32::System::RemoteDesktop::{
            NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
        };
        with_router(|r| {
            r.windows.insert(owner, control.clone());
        });
        let registered =
            unsafe { WTSRegisterSessionNotification(HWND(owner as _), NOTIFY_FOR_THIS_SESSION) }
                .map_err(|error| tracing::warn!(%error, "session lock notification unavailable"))
                .is_ok();
        Self { owner, registered }
    }
}

impl Drop for SessionNotifications {
    fn drop(&mut self) {
        if self.registered {
            let _ = unsafe {
                windows::Win32::System::RemoteDesktop::WTSUnRegisterSessionNotification(HWND(
                    self.owner as _,
                ))
            };
        }
        with_router(|r| {
            r.windows.remove(&self.owner);
        });
    }
}

fn normalize_key(key: u32, scan: u32, extended: bool) -> u16 {
    match key {
        16 => {
            if scan == 0x36 {
                161
            } else {
                160
            }
        }
        17 => {
            if extended {
                163
            } else {
                162
            }
        }
        18 => {
            if extended {
                165
            } else {
                164
            }
        }
        _ => key as u16,
    }
}

/// Before TranslateMessage/winit/egui, on the window thread, not a render tick.
pub(super) fn message(pointer: *const std::ffi::c_void) -> bool {
    if pointer.is_null() {
        return false;
    }
    let msg = unsafe { &*pointer.cast::<MSG>() };
    let key_message = matches!(
        msg.message,
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
    );
    with_router(|r| {
        if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_WTSSESSION_CHANGE
            && matches!(msg.wParam.0, 2 | 4 | 6 | 7) // console/remote disconnect, logoff, lock
            && r.windows.contains_key(&(msg.hwnd.0 as u64))
        {
            r.suspend_desktop();
            return false;
        }
        if !key_message {
            r.drain_keys();
            return false;
        }
        if !(8..=254).contains(&msg.wParam.0) || msg.wParam.0 == 231 {
            return false;
        }
        let flags = msg.lParam.0 as u32;
        let scan = (flags >> 16) & 0xff;
        let virtual_key = if msg.wParam.0 == 229 {
            // The local IME may replace wParam with VK_PROCESSKEY. Preserve
            // the original physical key; committed text is not forwarded.
            unsafe { windows::Win32::UI::Input::Ime::ImmGetVirtualKey(msg.hwnd) }
        } else {
            msg.wParam.0 as u32
        };
        if !(8..=254).contains(&virtual_key) || virtual_key == 231 {
            return false;
        }
        let key = normalize_key(virtual_key, scan, flags & (1 << 24) != 0);
        let down = matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let count = if down { (flags & 0xffff).max(1) } else { 1 };
        if count > MAX_PENDING_KEYS as u32 {
            r.stop_ordering();
            return true;
        }
        let mut consumed = false;
        for _ in 0..count {
            consumed |= r.window_edge(
                msg.hwnd.0 as u64,
                KeyEdge {
                    key,
                    scan,
                    down,
                    injected: false,
                },
            );
        }
        consumed
    })
}

pub(super) struct KeyboardHook {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

impl KeyboardHook {
    pub fn install() -> Result<Self> {
        let (ready, receive) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("keyboard-input".into())
            .spawn(move || {
                // The installing thread must keep pumping messages. Do not tie
                // LowLevelHooksTimeout to GUI/font/device initialization or drawing.
                let mut message = MSG::default();
                unsafe {
                    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                }
                let result = (|| -> Result<_> {
                    let module =
                        unsafe { GetModuleHandleW(None) }.context("取得键盘控制模块失败")?;
                    unsafe {
                        SetWindowsHookExW(
                            WH_KEYBOARD_LL,
                            Some(keyboard_proc),
                            Some(HINSTANCE(module.0)),
                            0,
                        )
                    }
                    .context("安装键盘控制钩子失败")
                })();
                let hook = match result {
                    Ok(hook) => hook,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                router().installed = true;
                tracing::debug!("keyboard hook installed on dedicated message pump");
                let thread_id = unsafe { GetCurrentThreadId() };
                if ready.send(Ok(thread_id)).is_ok() {
                    loop {
                        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
                        if result <= 0 {
                            break;
                        }
                        unsafe {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                }
                let _ = unsafe { UnhookWindowsHookEx(hook) };
                let mut r = router();
                r.clear();
                r.installed = false;
            })
            .context("创建键盘输入线程失败")?;
        match receive
            .recv()
            .context("键盘输入线程启动失败")
            .and_then(|result| result)
        {
            Ok(thread_id) => Ok(Self {
                thread: Some(thread),
                thread_id,
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        {
            let mut r = router();
            r.clear();
            r.installed = false;
        }
        if let Some(thread) = self.thread.take() {
            if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok()
                || thread.is_finished()
            {
                let _ = thread.join();
            } else {
                tracing::warn!("could not stop keyboard message pump; routing is disabled");
            }
        }
    }
}

impl Router {
    fn suspend_desktop(&mut self) {
        // This is a control revocation, not a synthetic remote lock command.
        // Release obligations already handed to transport remain in its queue.
        for control in self.windows.values() {
            let state = control.snapshot();
            if state.mouse_mode != MouseMode::View || state.mouse_pending {
                let _ = control.set_mouse_mode(MouseMode::View);
            }
            control.mouse().reconcile_keyboard_releases();
        }
        self.clear();
        self.physical = [false; 256];
        self.observed = [false; 256];
        self.blocked = [false; 256];
        self.consumed = [false; 256];
        self.routes = [Route::Local; 256];
        self.legacy_owned = [None; 256];
        tracing::debug!("local desktop transition revoked remote input");
    }
    // One observation per stage/source/activation. Never record keys or text.
    fn diagnostic(&mut self, stage: u8, injected: bool, reason: &'static str) {
        if self.target.is_none() {
            return;
        }
        let bit = 1u64 << (stage * 2 + u8::from(injected));
        if self.diagnostic_seen & bit == 0 {
            self.diagnostic_seen |= bit;
            tracing::debug!(injected, reason, "keyboard routing observation");
        }
    }
    fn clear(&mut self) {
        self.shortcut = None;
        self.lock_releases.clear();
        self.pending.clear();
        if let Some(target) = self.target.take() {
            target.input.pause_owner(target.owner);
        }
        for (blocked, down) in self.blocked.iter_mut().zip(self.physical) {
            *blocked |= down;
        }
    }

    fn stop_ordering(&mut self) {
        if let Some(target) = &self.target {
            target
                .input
                .fail("键盘事件顺序中断，已停止控制并释放按键".into());
        }
        self.clear();
    }

    fn push_edge(&mut self, edge: KeyEdge, route: Route) {
        if self.pending.len() >= MAX_PENDING_KEYS {
            self.stop_ordering();
            return;
        }
        self.pending.push_back(PendingKey {
            edge,
            route,
            owner: match route {
                Route::Window(owner) | Route::Hook(owner) => owner,
                Route::Local => self.target.as_ref().map_or(0, |t| t.owner),
            },
            ready: matches!(route, Route::Hook(_)),
        });
    }

    fn drain_keys(&mut self) {
        while self.pending.front().is_some_and(|key| key.ready) {
            let key = self.pending.pop_front().unwrap();
            let edge = key.edge;
            if key.route == Route::Local {
                let i = usize::from(edge.key);
                self.physical[i] = edge.down;
                self.blocked[i] = edge.down;
                if !edge.down {
                    self.consumed[i] = false;
                }
            } else {
                self.event(edge.key, edge.scan, edge.down, edge.injected);
            }
        }
    }

    fn observe_hook(&mut self, edge: KeyEdge) -> bool {
        let i = usize::from(edge.key);
        let was_down = self.observed[i];
        self.observed[i] = edge.down;
        if edge.down && !was_down {
            let owner = self
                .target
                .as_ref()
                .filter(|t| {
                    self.installed
                        && unsafe { GetForegroundWindow() } == HWND(t.owner as _)
                        && t.input.mode() != MouseMode::View
                        && super::windows_mouse::router().keyboard_allowed(t.owner)
                })
                .map(|t| t.owner);
            self.routes[i] = if let Some(owner) = owner {
                if intercept_key(edge.key, edge.scan, &self.observed) {
                    Route::Hook(owner)
                } else {
                    Route::Window(owner)
                }
            } else {
                Route::Local
            };
        }
        let route = self.routes[i];
        if !edge.down {
            self.routes[i] = Route::Local;
        }
        match route {
            Route::Local => {
                // No remote ownership; keep release/quarantine bookkeeping up
                // to date without synthesizing a remote event.
                if self
                    .target
                    .as_ref()
                    .is_some_and(|t| unsafe { GetForegroundWindow() } == HWND(t.owner as _))
                {
                    self.push_edge(edge, route);
                } else if !edge.down {
                    self.physical[i] = false;
                    self.blocked[i] = false;
                    self.consumed[i] = false;
                }
                false
            }
            Route::Window(owner) | Route::Hook(owner) => {
                if matches!(route, Route::Window(_)) && edge.down {
                    self.legacy_owned[i] = Some(owner);
                }
                if self.target.as_ref().is_some_and(|t| t.owner == owner) {
                    self.push_edge(edge, route);
                    if matches!(route, Route::Hook(_)) {
                        super::windows_mouse::router().wake_keyboard(owner);
                    }
                } else if !edge.down {
                    self.physical[i] = false;
                    self.blocked[i] = false;
                    self.consumed[i] = false;
                }
                matches!(route, Route::Hook(_))
            }
        }
    }

    fn window_edge(&mut self, owner: u64, edge: KeyEdge) -> bool {
        let i = usize::from(edge.key);
        let position = self.pending.iter().position(|p| {
            p.owner == owner
                && !matches!(p.route, Route::Hook(_))
                && !p.ready
                && p.edge.key == edge.key
                && p.edge.down == edge.down
        });
        let mut owned = self.legacy_owned[i] == Some(owner);
        if let Some(position) = position {
            // A missing earlier window event cannot be bypassed by a later
            // intercepted key. Stop safely instead of replaying stale presses.
            if self.pending.iter().take(position).any(|p| !p.ready) {
                self.stop_ordering();
            } else {
                let injected = self.pending[position].edge.injected;
                self.pending[position].edge = KeyEdge { injected, ..edge };
                self.pending[position].ready = true;
                owned = matches!(self.pending[position].route, Route::Window(_));
                self.drain_keys();
            }
        } else if self.target.as_ref().is_some_and(|t| t.owner == owner) {
            // Window input continues to work even if the hook wasn't called.
            // No timer-based duplicate detection or duplicate fallback sends.
            if self.pending.iter().any(|p| !p.ready) {
                self.stop_ordering();
            } else {
                self.drain_keys();
                owned |= self.event(edge.key, edge.scan, edge.down, edge.injected);
                if edge.down
                    && self
                        .target
                        .as_ref()
                        .is_some_and(|t| t.input.owner_holds_key(owner, edge.key))
                {
                    self.routes[i] = Route::Window(owner);
                    self.observed[i] = true;
                    self.legacy_owned[i] = Some(owner);
                } else if !edge.down && self.routes[i] == Route::Window(owner) {
                    self.routes[i] = Route::Local;
                    self.observed[i] = false;
                }
            }
        }
        if !edge.down {
            self.legacy_owned[i] = None;
        }
        owned && !matches!(edge.key, 16..=18 | 20 | 144..=145 | 160..=165)
    }

    fn modifiers(&self) -> (bool, bool, bool, bool) {
        let p = &self.physical;
        (
            p[17] || p[162] || p[163],
            p[16] || p[160] || p[161],
            p[18] || p[164] || p[165],
            p[91] || p[92],
        )
    }

    fn event(&mut self, vk: u16, scan: u32, down: bool, injected: bool) -> bool {
        self.diagnostic(0, injected, "window_dispatch_received");
        let index = usize::from(vk);
        // VK_PACKET is committed Unicode input, not a physical VK event.
        if !(8..=254).contains(&vk) || vk == 231 {
            self.diagnostic(1, injected, "not_physical_key");
            return false;
        }
        let previously_down = self.physical[index];
        self.physical[index] = down;
        let was_consumed = self.consumed[index];
        if !down {
            self.consumed[index] = false;
        }
        let Some(target) = &mut self.target else {
            if !down {
                self.blocked[index] = false;
            }
            return was_consumed;
        };
        if unsafe { GetForegroundWindow() } != HWND(target.owner as _)
            || target.input.mode() == MouseMode::View
        {
            self.diagnostic(2, injected, "inactive_owner");
            self.clear();
            if !down {
                self.blocked[index] = false;
            }
            return was_consumed;
        }
        let generation = target.input.keyboard_generation();
        if generation != target.generation {
            target.generation = generation;
            for (blocked, physical) in self.blocked.iter_mut().zip(self.physical) {
                *blocked |= physical;
            }
            // This new edge belongs to the resumed owner, not to the old
            // generation. Only keys already physically held are quarantined.
            if !previously_down {
                self.blocked[index] = false;
            }
        }
        let (ctrl, shift, alt, win) = self.modifiers();
        if ctrl && shift && alt && !win && matches!(scan, 0x2c | 0x21 | 0x10) {
            // Forwarded keys are swallowed; use native modifier state rather
            // than reconstructing the chord from winit's partial key stream.
            if down {
                if let Some(t) = &mut self.target {
                    t.input.pause_owner(t.owner);
                    t.generation = t.input.keyboard_generation();
                    if !previously_down {
                        use super::windows_presenter::ViewerShortcut;
                        self.shortcut = Some((
                            t.owner,
                            match scan {
                                0x2c => ViewerShortcut::ReleaseMouse,
                                0x21 => ViewerShortcut::Fullscreen,
                                _ => ViewerShortcut::Close,
                            },
                        ));
                        super::windows_mouse::router().wake_keyboard(t.owner);
                    }
                }
                for (blocked, physical) in self.blocked.iter_mut().zip(self.physical) {
                    *blocked |= physical;
                }
                self.consumed[index] = true;
            } else {
                self.blocked[index] = false;
            }
            return true;
        }
        if self.blocked[index] {
            self.diagnostic(3, injected, "quarantined_key");
            if !down {
                self.blocked[index] = false;
            }
            return was_consumed;
        }
        let target = self.target.as_mut().expect("checked target");
        let allowed = super::windows_mouse::router().keyboard_allowed(target.owner);
        if down && (!allowed || blocked_modifiers(&self.blocked)) {
            self.diagnostic(
                if allowed { 4 } else { 5 },
                injected,
                if allowed {
                    "quarantined_modifier"
                } else {
                    "outside_input_area"
                },
            );
            self.blocked[index] = true;
            return was_consumed;
        }
        let target = self.target.as_mut().expect("checked target");
        if !down && matches!(vk, 20 | 144 | 145) {
            // Toggle state belongs to the foreground GUI input queue. The
            // dedicated hook thread must not sample its own GetKeyState table.
            self.lock_releases.retain(|(_, key, _)| *key != vk);
            self.lock_releases
                .push((target.owner, vk, target.generation));
            super::windows_mouse::router().wake_keyboard(target.owner);
            return false;
        }
        let lock = matches!(vk, 20 | 144 | 145).then_some(0);
        let accepted = target.input.key(target.owner, vk, down, lock);
        target.generation = target.input.keyboard_generation();
        self.diagnostic(
            if accepted { 6 } else { 7 },
            injected,
            if accepted { "queued" } else { "queue_rejected" },
        );
        // Let Windows maintain modifier/lock state, including AltGr's injected
        // Ctrl. Ordinary accepted input must not also reach egui/local IME.
        let consume = accepted && !matches!(vk, 16..=18 | 20 | 144..=145 | 160..=165);
        if down && consume {
            self.consumed[index] = true;
        }
        consume || was_consumed
    }
}

pub(super) fn set_target(owner: u64, input: &RemoteInput) {
    with_router(|r| {
        if !input.keyboard_supported() {
            r.clear();
            return;
        }
        if r.target.as_ref().is_some_and(|t| t.owner == owner) {
            return;
        }
        r.clear();
        r.diagnostic_seen = 0;
        // Keys held before activation belong to the previous local surface.
        for i in 8..255 {
            r.physical[i] = r.consumed[i]
                || (!matches!(i, 16..=18) && unsafe { GetAsyncKeyState(i as i32) } < 0);
            r.blocked[i] = r.physical[i];
            r.observed[i] = r.physical[i];
        }
        tracing::debug!(
            quarantined_keys = r.blocked.iter().filter(|key| **key).count(),
            held_modifiers = blocked_modifiers(&r.blocked),
            "keyboard input owner activated"
        );
        r.target = Some(Target {
            owner,
            input: input.clone(),
            generation: input.keyboard_generation(),
        });
    });
}

pub(super) fn clear(owner: u64) {
    with_router(|r| {
        if r.target.as_ref().is_some_and(|t| t.owner == owner) {
            tracing::debug!("keyboard input owner deactivated by GUI");
            r.clear();
        }
    });
}

unsafe extern "system" fn keyboard_proc(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 && data.0 != 0 {
        let down = matches!(message.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if down || matches!(message.0 as u32, WM_KEYUP | WM_SYSKEYUP) {
            let event = unsafe { &*(data.0 as *const KBDLLHOOKSTRUCT) };
            let vk = u32::from(normalize_key(
                event.vkCode,
                event.scanCode,
                event.flags.0 & 1 != 0,
            ));
            let scan = if event.scanCode == 0 {
                use windows::Win32::UI::Input::KeyboardAndMouse::{
                    MAPVK_VK_TO_VSC, MapVirtualKeyW,
                };
                unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) }
            } else {
                event.scanCode
            };
            if (8..=254).contains(&vk)
                && vk != 231
                && with_router(|r| {
                    r.observe_hook(KeyEdge {
                        key: vk as u16,
                        scan,
                        down,
                        injected: event.flags.0 & 0x10 != 0,
                    })
                })
            {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, message, data) }
}

pub(super) fn take_shortcut(owner: u64) -> Option<super::windows_presenter::ViewerShortcut> {
    with_router(|r| {
        if r.shortcut.is_some_and(|(id, _)| id == owner) {
            r.shortcut.take().map(|(_, command)| command)
        } else {
            None
        }
    })
}

/// Called by the foreground window's UI thread after native message dispatch.
pub(super) fn finish_lock_releases(owner: u64) {
    with_router(|r| {
        if unsafe { GetForegroundWindow() } != HWND(owner as _) {
            return;
        }
        let pending = std::mem::take(&mut r.lock_releases);
        for (id, key, generation) in pending {
            if id != owner {
                r.lock_releases.push((id, key, generation));
                continue;
            }
            if let Some(target) = &mut r.target
                && target.owner == id
                && target.input.keyboard_generation() == generation
            {
                let lock = ((unsafe { GetKeyState(i32::from(key)) } & 1) << 7) as u8;
                target.input.key(owner, key, false, Some(lock));
                target.generation = target.input.keyboard_generation();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_modifier_normalization_matches_native_scan_mapping() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VSC_TO_VK_EX, MapVirtualKeyW};
        for (generic, scan, extended) in [
            (16, 0x2a, false),
            (16, 0x36, false),
            (17, 0x1d, false),
            (17, 0x1d, true),
            (18, 0x38, false),
            (18, 0x38, true),
        ] {
            let native = unsafe {
                MapVirtualKeyW(scan | if extended { 0xe000 } else { 0 }, MAPVK_VSC_TO_VK_EX)
            };
            assert_eq!(u32::from(normalize_key(generic, scan, extended)), native);
        }
    }

    #[test]
    fn windows_remove_raw_keyboard_preserves_mouse_registration() {
        use windows::Win32::UI::Input::{
            GetRegisteredRawInputDevices, RAWINPUTDEVICE, RIDEV_DEVNOTIFY, RIDEV_REMOVE,
            RegisterRawInputDevices,
        };
        fn registrations() -> Vec<RAWINPUTDEVICE> {
            let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
            let mut count = 0;
            assert_ne!(
                unsafe { GetRegisteredRawInputDevices(None, &mut count, size) },
                u32::MAX
            );
            let mut devices = vec![RAWINPUTDEVICE::default(); count as usize];
            if count != 0 {
                let read = unsafe {
                    GetRegisteredRawInputDevices(Some(devices.as_mut_ptr()), &mut count, size)
                };
                assert_ne!(read, u32::MAX);
                devices.truncate(read as usize);
            }
            devices
        }
        struct Restore(Vec<RAWINPUTDEVICE>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let remove: Vec<_> = [2, 6]
                    .into_iter()
                    .map(|usage| RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: usage,
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: HWND::default(),
                    })
                    .collect();
                let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
                unsafe {
                    let _ = RegisterRawInputDevices(&remove, size);
                    if !self.0.is_empty() {
                        let _ = RegisterRawInputDevices(&self.0, size);
                    }
                }
            }
        }
        let _restore = Restore(
            registrations()
                .into_iter()
                .filter(|d| d.usUsagePage == 1 && matches!(d.usUsage, 2 | 6))
                .collect(),
        );
        let devices: Vec<_> = [2, 6]
            .into_iter()
            .map(|usage| RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: usage,
                dwFlags: RIDEV_DEVNOTIFY,
                hwndTarget: HWND::default(),
            })
            .collect();
        unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
            .unwrap();
        let before = registrations();
        assert!(before.iter().any(|d| d.usUsagePage == 1 && d.usUsage == 6));
        let previous_mouse = before
            .iter()
            .find(|d| d.usUsagePage == 1 && d.usUsage == 2)
            .unwrap();
        remove_unused_raw_keyboard().unwrap();
        let remaining = registrations();
        assert!(
            !remaining
                .iter()
                .any(|d| d.usUsagePage == 1 && d.usUsage == 6)
        );
        let mouse = remaining
            .iter()
            .find(|d| d.usUsagePage == 1 && d.usUsage == 2)
            .unwrap();
        // Compare the OS-reported registration: Windows can normalize flags
        // for a registration without a notification target.
        assert_eq!(mouse.dwFlags, previous_mouse.dwFlags);
        assert_eq!(mouse.hwndTarget, previous_mouse.hwndTarget);
    }

    #[test]
    fn windows_keyboard_hook_install_and_teardown() {
        // Dedicated thread: validate the native registration contract without
        // injecting any input or changing the user's foreground window.
        std::thread::spawn(|| {
            let hook = KeyboardHook::install().unwrap();
            assert!(hook.thread_id != unsafe { GetCurrentThreadId() });
            assert!(router().installed);
            drop(hook);
            let hook = KeyboardHook::install().unwrap();
            drop(hook);
        })
        .join()
        .unwrap();
    }
}
