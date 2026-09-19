//! OpenUUYC: native UU Remote interoperability building blocks.

pub const APP_NAME: &str = "OpenUUYC";
pub(crate) const VIEWER_TITLE_PREFIX: &str = "OpenUUYC — ";

pub mod api;
pub mod app;
pub mod assist;
mod audio;
pub mod auth;
mod capability;
pub mod client;
mod clipboard;
mod codec_parameters;
mod control;
pub mod controller;
mod decoder;
mod decoder_pool;
mod decoder_result;
mod device_change;
mod device_session;
#[cfg(windows)]
mod display_hdr;
#[cfg(not(windows))]
#[path = "display_hdr_linux.rs"]
mod display_hdr;
mod feature_ability;
mod file_transfer;
mod flexfec;
pub mod logging;
pub mod login;
pub mod media;
mod network_control;
mod nrd_http;
pub mod official_receiver;
mod official_version;
mod paths;
pub mod performance;

pub mod plugins;
mod port_mapping;
mod power;
mod presence;
mod remote_cursor;
mod remote_input;
mod remote_upgrade;
mod rsfec;
pub mod rtc;
mod rtcp_timing;
pub mod rtp_capture;
mod session_restore;
pub mod signal;
pub mod stream_control;
mod timing;
mod ui;
mod ulpfec;
mod uu_kcp;
mod video_color;
mod video_format;
pub mod viewer;
mod viewer_shortcuts;
mod viewing_settings;
mod virtual_hardware;
#[cfg(not(windows))]
mod virtual_keys;
mod wallpaper;
mod xor_fec;
