//! The unattended host service, as far as Linux has one: not at all yet.
//!
//! Windows can install a system service that keeps the account online and
//! owns controlled sessions without a signed-in desktop, with its credentials
//! in a machine vault. Linux runs the host only inside the signed-in desktop
//! session (the portable mode), so every question below has the answer that
//! mode gives: no vault applies, no resident owner exists, and the client owns
//! its presence itself. A systemd-managed service would replace these answers.
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

const NO_SERVICE: &str = "Linux 尚未提供后台被控服务";

/// Whether an install, update or uninstall is replacing the service now.
pub(crate) fn maintaining() -> bool {
    false
}

pub(crate) mod install {
    use super::*;

    /// No service is ever installed, so the portable client does its own
    /// background upkeep.
    pub(crate) fn running() -> Result<bool> {
        Ok(false)
    }
}

pub(crate) mod vault {
    use super::*;

    /// Credentials stay in the user's Secret Service keyring.
    pub(crate) fn applies() -> Result<bool> {
        Ok(false)
    }
    pub(crate) fn root() -> Result<PathBuf> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn owner() -> Result<Option<String>> {
        Ok(None)
    }
    pub(crate) fn sid(_pid: u32) -> Result<String> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn key(service: &str, account: &str) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}.secret",
            Sha256::digest(format!("{service}\0{account}"))
        )
    }
    pub(crate) fn read(_key: &str) -> Result<Option<Vec<u8>>> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn write(_key: &str, _value: Option<&[u8]>) -> Result<()> {
        bail!(NO_SERVICE)
    }
}

pub(crate) mod resident {
    use super::*;
    pub(crate) use crate::session::resident::{Reply, Request, Snapshot};

    /// This process is never the service's resident account owner.
    pub(crate) fn is_owner() -> bool {
        false
    }
    /// The desktop client always owns the account itself.
    pub(crate) fn managed() -> bool {
        false
    }
    pub(crate) async fn request(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn call(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    /// Only asked of a managed client; with no service there is nothing to retire.
    pub(crate) async fn pause_for_exit() -> Result<()> {
        Ok(())
    }
}

/// Work the resident owner waits for before it reports itself paused. With no
/// service there is no owner waiting, so nothing is counted.
pub(crate) mod activity {
    pub(crate) struct Work;
    impl Work {
        pub fn new() -> Self {
            Self
        }
    }
}

/// Login startup through an XDG autostart entry, the counterpart of the
/// Windows per-user Run key.
pub(crate) mod startup {
    use super::*;

    fn entry() -> Result<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context("无法确定用户配置目录")?;
        Ok(config.join("autostart/openuuyc.desktop"))
    }

    pub(crate) fn set_image(enabled: bool, path: &std::path::Path) -> Result<()> {
        let entry = entry()?;
        if !enabled {
            return match std::fs::remove_file(&entry) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error).context("无法删除本程序自启动设置")
                }
                _ => Ok(()),
            };
        }
        anyhow::ensure!(path.is_file(), "尚未部署自启动程序");
        let image = path.to_str().context("程序路径不是 UTF-8")?;
        anyhow::ensure!(!image.contains(['\n', '\r']), "程序路径含换行");
        // Desktop Entry quoting: reserved characters get a backslash inside the
        // quoted argument, and the file's string escaping doubles every backslash.
        let mut quoted = String::new();
        for c in image.chars() {
            match c {
                '\\' => quoted.push_str(r"\\\\"),
                '"' | '`' | '$' => {
                    quoted.push_str(r"\\");
                    quoted.push(c);
                }
                '%' => quoted.push_str("%%"),
                _ => quoted.push(c),
            }
        }
        if let Some(directory) = entry.parent() {
            std::fs::create_dir_all(directory)?;
        }
        std::fs::write(
            &entry,
            format!(
                "[Desktop Entry]\nType=Application\nName=OpenUUYC\n\
                 Exec=\"{quoted}\" gui --background\nX-GNOME-Autostart-enabled=true\n"
            ),
        )
        .with_context(|| format!("无法写入自启动设置 {}", entry.display()))
    }
}

/// Holds the per-user lock that lets only one process keep a device's
/// presence room; two owners would keep kicking each other off the server.
pub(crate) struct PresenceReservation(#[allow(dead_code)] std::fs::File);

/// Reserve the device's presence room, or `None` while another process of
/// this user holds it.
pub(crate) fn reserve_presence(device: &str) -> Result<Option<PresenceReservation>> {
    anyhow::ensure!(
        !device.is_empty()
            && device
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "设备标识无效"
    );
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(crate::platform::paths::require_local_app_data, Ok)?;
    let directory = base.join("openuuyc");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("presence.{device}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("无法打开在线状态锁 {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(PresenceReservation(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Refuse a path that is a symbolic link, the Linux counterpart of the
/// Windows reparse-point check on service-owned directories.
pub(crate) fn reject_reparse(path: &std::path::Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(!metadata.file_type().is_symlink(), "组件目录不可为符号链接");
    }
    Ok(())
}
