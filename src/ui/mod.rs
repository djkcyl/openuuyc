//! Desktop GUI ownership and shared application controls.
use anyhow::Result;

pub(crate) mod chrome;
pub(crate) mod gfx;

#[cfg(windows)]
mod tray;
#[cfg(target_os = "linux")]
#[path = "tray_linux.rs"]
mod tray;
pub(crate) mod window_manager;
mod windows;

mod app;
pub(crate) mod branding;
pub(crate) mod controls;
pub(crate) mod fonts;
pub(crate) mod theme;
use app::AppFactory;

use app::AppSession;
pub(crate) use app::{App, WindowConfig};

pub(crate) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    windows::run(config, factory)
}
