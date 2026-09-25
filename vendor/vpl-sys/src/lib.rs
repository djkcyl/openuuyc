//! Intel oneVPL public C ABI and static dispatcher; no software codec runtime.
#![allow(warnings)]
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
compile_error!("OpenUUYC oneVPL bindings require Windows x64");
include!("bindings.rs");
