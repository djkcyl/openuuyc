//! One control center per desktop login session, independent of the EXE name.
use anyhow::Result;

#[cfg(windows)]
mod platform {
    use anyhow::{Context, Result};
    use windows::{
        Win32::{
            Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM},
            System::Threading::CreateMutexW,
            UI::WindowsAndMessaging::{
                EnumWindows, GetPropW, IsIconic, MB_ICONERROR, MB_OK, MB_SETFOREGROUND,
                MessageBoxW, SW_RESTORE, SetForegroundWindow, SetPropW, ShowWindowAsync,
            },
        },
        core::{BOOL, PCWSTR, w},
    };

    pub struct Instance(HANDLE);

    impl Drop for Instance {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn acquire_named(name: PCWSTR) -> Result<Option<Instance>> {
        // Object lifetime is the reservation; no thread ownership or abandoned lock remains.
        let handle = unsafe { CreateMutexW(None, false, name) }.context("创建程序实例保护失败")?;
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let instance = Instance(handle);
        if exists {
            drop(instance);
            Ok(None)
        } else {
            Ok(Some(instance))
        }
    }

    pub fn acquire() -> Result<Option<Instance>> {
        let result = acquire_named(w!("Local\\OpenUUYC.ControlCenter.v1"));
        let message = match &result {
            Ok(Some(_)) => return result,
            Ok(None) => "OpenUUYC 已在运行，请勿重复启动。".to_owned(),
            Err(error) => format!("无法启动 OpenUUYC：{error:#}"),
        };
        let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(message.as_ptr()),
                w!("OpenUUYC"),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            );
        }
        if matches!(&result, Ok(None)) {
            activate_existing();
        }
        result
    }

    pub(crate) fn register_window(window: &winit::window::Window) -> Result<()> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
            anyhow::bail!("控制中心窗口不是 Win32 窗口");
        };
        let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
        unsafe {
            SetPropW(
                hwnd,
                w!("OpenUUYC.ControlCenter.Window.v1"),
                Some(HANDLE(hwnd.0)),
            )
        }
        .context("标记控制中心窗口失败")
    }

    fn activate_existing() {
        unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
            if !unsafe { GetPropW(hwnd, w!("OpenUUYC.ControlCenter.Window.v1")) }.is_invalid() {
                unsafe {
                    *(data.0 as *mut Option<HWND>) = Some(hwnd);
                }
                return false.into();
            }
            true.into()
        }
        let mut window: Option<HWND> = None;
        unsafe {
            let _ = EnumWindows(Some(find), LPARAM(&mut window as *mut _ as isize));
            if let Some(hwnd) = window {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindowAsync(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use anyhow::{Context, Result};
    use std::fs::{File, OpenOptions};
    use std::path::PathBuf;

    /// The advisory lock lives as long as this handle; the file itself stays behind.
    pub struct Instance(#[allow(dead_code)] File);

    fn lock_path() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        // Per-user, so a shared /tmp fallback cannot collide across accounts.
        base.join(format!("openuuyc-control-center-{}.lock", unsafe {
            libc::getuid()
        }))
    }

    pub fn acquire() -> Result<Option<Instance>> {
        let path = lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("创建程序实例保护失败：{}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Instance(file))),
            Err(std::fs::TryLockError::WouldBlock) => {
                eprintln!("OpenUUYC 已在运行，请勿重复启动。");
                Ok(None)
            }
            Err(std::fs::TryLockError::Error(error)) => {
                eprintln!("无法启动 OpenUUYC：{error}");
                Err(anyhow::Error::new(error).context("锁定程序实例保护失败"))
            }
        }
    }

    /// X11 and Wayland have no portable window-property handshake; the lock file
    /// is the whole reservation and the running window is left untouched.
    pub(crate) fn register_window(_window: &winit::window::Window) -> Result<()> {
        Ok(())
    }
}

pub use platform::Instance;
pub(crate) use platform::register_window;

pub fn acquire() -> Result<Option<Instance>> {
    platform::acquire()
}
