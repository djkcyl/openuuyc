//! UI composition stays on the window thread and never submits a video frame.
//! UU hands a separate native video window to streamer. DirectComposition gives
//! our egui UI the equivalent independent layer above that child window.

use anyhow::{Context, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;
use winit::dpi::PhysicalSize;
use winit::window::Window;

use super::windows_presenter::title_bar_height_pixels;
use crate::ui::d3d11::window_hwnd;
pub(super) use crate::ui::d3d11::{UiPresenter, UiTimingAudit};

pub(super) struct VideoWindow {
    hwnd: HWND,
}

impl VideoWindow {
    pub(super) fn new(parent: &Window) -> Result<Self> {
        // A disabled child is skipped by hit testing, so the parent continues
        // to receive all local UI input, including menus drawn above the video.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("UU Video"),
                WS_CHILD | WS_VISIBLE | WS_DISABLED | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                Some(window_hwnd(parent)?),
                None,
                None,
                None,
            )
        }
        .context("create native video child window")?;
        let child = Self { hwnd };
        child.resize(parent, parent.inner_size())?;
        tracing::info!(
            parent = window_hwnd(parent)?.0 as usize,
            video = hwnd.0 as usize,
            "created independent UI and video presentation targets"
        );
        Ok(child)
    }

    pub(super) fn handle(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub(super) fn resize(
        &self,
        parent: &Window,
        size: PhysicalSize<u32>,
    ) -> Result<PhysicalSize<u32>> {
        let top = title_bar_height_pixels(parent);
        let video_size =
            PhysicalSize::new(size.width.max(1), size.height.saturating_sub(top).max(1));
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                top as i32,
                video_size.width as i32,
                video_size.height as i32,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .context("resize native video child window")?;
        Ok(video_size)
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        // The owner drops/join()s Video Render before destroying this HWND.
        if let Err(error) = unsafe { DestroyWindow(self.hwnd) } {
            tracing::debug!(%error, "destroy video child window");
        }
    }
}
