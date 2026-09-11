//! Foreground keyboard routing. The hook never waits for transport or logs keys.
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
/// This client uses only RAWMOUSE; physical keyboard input uses the LL hook.
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

struct Router {
    installed: bool,
    shortcut: Option<(u64, super::windows_presenter::ViewerShortcut)>,
    target: Option<Target>,
    physical: [bool; 256],
    blocked: [bool; 256],
    consumed: [bool; 256],
    lock_releases: Vec<(u64, u16, u64)>,
    diagnostic_seen: u64,
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
        if let Some(target) = self.target.take() {
            target.input.pause_owner(target.owner);
        }
        for (blocked, down) in self.blocked.iter_mut().zip(self.physical) {
            *blocked |= down;
        }
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
        self.diagnostic(0, injected, "hook_received");
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
        if !self.installed
            || unsafe { GetForegroundWindow() } != HWND(target.owner as _)
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
        if !r.installed {
            r.clear();
            input.fail("键盘钩子不可用，控制已停止".into());
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
            let vk = match event.vkCode {
                16 => {
                    if event.scanCode == 0x36 {
                        161
                    } else {
                        160
                    }
                }
                17 => {
                    if event.flags.0 & 1 != 0 {
                        163
                    } else {
                        162
                    }
                }
                18 => {
                    if event.flags.0 & 1 != 0 {
                        165
                    } else {
                        164
                    }
                }
                key => key,
            };
            let scan = if event.scanCode == 0 {
                use windows::Win32::UI::Input::KeyboardAndMouse::{
                    MAPVK_VK_TO_VSC, MapVirtualKeyW,
                };
                unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) }
            } else {
                event.scanCode
            };
            if vk < 256
                && with_router(|r| r.event(vk as u16, scan, down, event.flags.0 & 0x10 != 0))
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
