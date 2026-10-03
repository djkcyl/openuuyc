//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    capture, cursor_shape, decoder, device_profile, display, display_hdr, encoder, graphics,
    host_service, input::deadline, loopback, notifications, surface, swapchain, transfer,
    video_shader, virtual_audio,
};
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{
    capture, cursor_shape, deadline, decoder, device_profile, display, display_hdr, encoder,
    graphics, host_service, loopback, notifications, surface, transfer, virtual_audio,
};
pub(crate) mod paths;
pub(crate) mod wallpaper;
