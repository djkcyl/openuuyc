//! One software playback decoder across all OpenUUYC processes in this
//! Windows login session. The owner stays on its decoder worker thread.
use anyhow::{Context, Result, bail};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::w;

static OCCUPIED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
pub(crate) static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug)]
pub(crate) struct SoftwarePlaybackBusy;
impl std::fmt::Display for SoftwarePlaybackBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("已有一个软解窗口正在播放，请先关闭它或切换到硬解。整个客户端最多允许一个软解播放窗口。")
    }
}
impl std::error::Error for SoftwarePlaybackBusy {}

pub(crate) struct SoftwareSlot {
    handle: HANDLE,
    _thread_bound: PhantomData<Rc<()>>,
}
impl SoftwareSlot {
    pub(crate) fn acquire() -> Result<Rc<Self>> {
        // Also disallow Win32 mutex recursion on the same thread. Existing
        // playback explicitly shares an Rc across decoder reconfiguration.
        if OCCUPIED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SoftwarePlaybackBusy.into());
        }
        let result = Self::acquire_global();
        if result.is_err() {
            OCCUPIED.store(false, Ordering::Release);
        }
        result
    }
    fn acquire_global() -> Result<Rc<Self>> {
        let handle =
            unsafe { CreateMutexW(None, false, w!("Local\\OpenUUYC.SoftwareVideoPlayback.v1")) }
                .context("创建全客户端软解窗口限制失败")?;
        let status = unsafe { WaitForSingleObject(handle, 0) };
        if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
            return Ok(Rc::new(Self {
                handle,
                _thread_bound: PhantomData,
            }));
        }
        let _ = unsafe { CloseHandle(handle) };
        if status == WAIT_TIMEOUT {
            return Err(SoftwarePlaybackBusy.into());
        }
        bail!("获取软解窗口名额失败：{status:?}")
    }
}
impl Drop for SoftwareSlot {
    fn drop(&mut self) {
        // Rc/PhantomData keep final release on the acquiring worker. Win32
        // abandoned-mutex handling permits recovery if the owner process dies.
        let _ = unsafe { ReleaseMutex(self.handle) };
        let _ = unsafe { CloseHandle(self.handle) };
        OCCUPIED.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;

    fn child(mode: &str) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "decoder::software_slot::tests::child_probe",
                "--ignored",
            ])
            .env("OPENUUYC_TEST_SOFTWARE_SLOT", mode)
            .creation_flags(0x08000000)
            .status()
            .unwrap();
        assert!(status.success(), "software slot child failed: {status}");
    }

    #[test]
    #[ignore = "subprocess entry for the software slot lifecycle test"]
    fn child_probe() {
        let mode = std::env::var("OPENUUYC_TEST_SOFTWARE_SLOT").unwrap();
        if mode == "busy" {
            assert!(
                SoftwareSlot::acquire()
                    .err()
                    .unwrap()
                    .is::<SoftwarePlaybackBusy>()
            );
        } else {
            let _slot = SoftwareSlot::acquire().unwrap();
            if mode == "abandon" {
                std::process::exit(0);
            }
        }
    }

    #[test]
    fn process_exclusion_shared_lifetime_and_abandoned_owner() {
        let _serial = TEST_SERIAL.lock().unwrap();
        let slot = SoftwareSlot::acquire().unwrap();
        assert!(SoftwareSlot::acquire().is_err());
        let retained = Rc::clone(&slot);
        drop(slot);
        child("busy");
        drop(retained);
        child("free");
        // Keep the named kernel object alive after the child exits without
        // Rust cleanup, exercising WAIT_ABANDONED rather than a fresh object.
        let handle =
            unsafe { CreateMutexW(None, false, w!("Local\\OpenUUYC.SoftwareVideoPlayback.v1")) }
                .unwrap();
        child("abandon");
        drop(SoftwareSlot::acquire().unwrap());
        unsafe { CloseHandle(handle) }.unwrap();
    }
}
