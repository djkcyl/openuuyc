//! Application ownership and module boundaries.

pub mod app;
pub mod viewer;
pub(crate) mod viewer_shortcuts;
pub(crate) mod wallpaper;

/// Internal crash-recovery role of the same executable.
pub fn display_recovery(token: &str) -> anyhow::Result<()> {
    crate::features::host::displays::recovery::watch(token)
}

/// Elevated role reached only by an explicit driver installation action.
pub fn install_display_driver() -> anyhow::Result<bool> {
    crate::platform::display::install::install()
}

/// Elevated role reached only by an explicit driver removal action.
pub fn uninstall_display_driver() -> anyhow::Result<bool> {
    crate::platform::display::install::uninstall()
}
