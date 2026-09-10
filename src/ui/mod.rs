//! Platform-independent GUI ownership; rendering APIs stay in the backends.
use anyhow::Result;

#[cfg(windows)]
pub(crate) mod d3d11;
#[cfg(windows)]
mod windows;

mod app;
pub(crate) mod branding;
use app::AppFactory;
#[cfg(windows)]
use app::AppSession;
pub(crate) use app::{App, WindowConfig};

pub(crate) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    #[cfg(windows)]
    {
        windows::run(config, factory)
    }
    #[cfg(not(windows))]
    {
        let _ = (config, factory);
        ensure_supported()
    }
}

/// Reject GUI requests before starting login, room or media work on unfinished backends.
pub(crate) fn ensure_supported() -> Result<()> {
    #[cfg(windows)]
    {
        Ok(())
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("{} 原生 GUI 尚未实现", std::env::consts::OS)
    }
}
