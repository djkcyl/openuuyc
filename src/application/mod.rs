//! Application ownership and module boundaries.

pub mod app;
pub mod viewer;
pub(crate) mod viewer_shortcuts;
pub(crate) mod wallpaper;
static INSTALLED_HANDOFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn schedule_installed_handoff() {
    INSTALLED_HANDOFF.store(true, std::sync::atomic::Ordering::Release);
}
pub fn take_installed_handoff() -> bool {
    INSTALLED_HANDOFF.swap(false, std::sync::atomic::Ordering::AcqRel)
}
#[cfg(windows)]
pub fn launch_installed(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> anyhow::Result<()> {
    crate::platform::windows::components::application::start_installed(arguments)
}
/// Hand the GUI to the installed copy when one exists. Linux has no
/// installed copy: the built binary is the application.
pub fn route_installed_gui() -> anyhow::Result<bool> {
    #[cfg(windows)]
    {
        app::maintenance::route_gui().inspect_err(app::maintenance::report_error)
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}
#[cfg(windows)]
pub fn uninstall_application(parent: Option<u32>) -> anyhow::Result<()> {
    app::maintenance::uninstall(parent).inspect_err(app::maintenance::report_error)
}

// The Windows service, its resident owner and its session agents.
#[cfg(windows)]
pub fn host_service() -> anyhow::Result<()> {
    crate::platform::windows::host_service::service::run()
}
#[cfg(windows)]
pub fn host_resident(parent: u32) -> anyhow::Result<()> {
    crate::platform::windows::host_service::resident::run(parent)
}
#[cfg(windows)]
pub fn display_agent(parent: u32) -> anyhow::Result<()> {
    crate::platform::windows::host_service::displays::run(parent)
}
#[cfg(windows)]
pub fn component_error_code(error: &anyhow::Error) -> Option<i32> {
    if error.is::<crate::platform::windows::host_service::install::ActiveSession>() {
        Some(170)
    } else if error.is::<crate::platform::windows::display::install::DriverInUse>() {
        Some(2404)
    } else {
        None
    }
}
#[cfg(windows)]
pub fn input_agent(pipe: &str, parent: u32) -> anyhow::Result<()> {
    crate::features::host::input::broker::agent(pipe, parent)
}
#[cfg(windows)]
pub fn capture_agent(pipe: &str, parent: u32) -> anyhow::Result<()> {
    crate::platform::windows::capture_service::agent(pipe, parent)
}
#[cfg(windows)]
pub use crate::platform::windows::components::{
    Kind as ComponentKind, Operation as ComponentOperation, RemovalOptions,
};
#[cfg(windows)]
pub fn component_operation(
    kind: ComponentKind,
    operation: ComponentOperation,
    allow_sas: bool,
    owner: Option<&str>,
    removal: RemovalOptions,
) -> anyhow::Result<bool> {
    crate::platform::windows::components::execute(kind, operation, allow_sas, owner, removal)
}
#[cfg(windows)]
pub fn purge_machine_data() -> anyhow::Result<()> {
    crate::platform::windows::components::application::purge_machine_data()
}

/// Internal crash-recovery role of the same executable.
pub fn display_recovery(token: &str) -> anyhow::Result<()> {
    crate::features::host::displays::recovery::watch(token)
}
