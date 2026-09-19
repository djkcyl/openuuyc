//! Desktop GUI ownership and shared application controls.
use anyhow::Result;

pub(crate) mod chrome;
#[cfg(windows)]
pub(crate) mod d3d11;
#[cfg(windows)]
mod d3d11_device;
pub(crate) mod gfx;
#[cfg(not(windows))]
mod wgpu_backend;
#[cfg(not(windows))]
pub(crate) mod wgpu_video;

mod shell;
pub(crate) mod window_manager;

mod app;
pub(crate) mod branding;
pub(crate) mod controls;
pub(crate) mod theme;
use app::AppFactory;

use app::AppSession;
pub(crate) use app::{App, WindowConfig};

pub(crate) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    shell::run(config, factory)
}
