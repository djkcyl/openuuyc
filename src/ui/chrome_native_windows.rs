//! Win32 window manipulation behind the shared borderless chrome: DWM styling,
//! the erase-background subclass, and pointer-captured move/resize.
use super::{title_bar_height_pixels, window_hwnd};
use anyhow::Result;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUNDSMALL, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    DC_BRUSH, FillRect, GetDC, GetStockObject, HBRUSH, HDC, ReleaseDC, SetDCBrushColor,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, WM_ERASEBKGND, WM_NCDESTROY};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::window::{ResizeDirection, Window};

const BACKDROP_SUBCLASS: usize = 0x4f555542;

pub(crate) fn cancel_pointer_operation(
    moving: &mut WindowMoveState,
    resizing: &mut WindowResizeState,
) {
    if moving.start.take().is_some() | resizing.start.take().is_some() {
        let _ = unsafe { ReleaseCapture() };
    }
}

pub(crate) fn configure_dwm_window(window: &Window) {
    if let Err(error) = prepare_window_background(window) {
        tracing::warn!(%error, "prepare window background");
    }
    let Ok(hwnd) = window_hwnd(window) else {
        return;
    };
    let dark_mode = 1_i32;
    let fullscreen = window.fullscreen().is_some();
    let color = crate::ui::theme::WINDOW_BORDER;
    let border_color = if fullscreen {
        DWMWA_COLOR_NONE
    } else {
        u32::from(color.r()) | (u32::from(color.g()) << 8) | (u32::from(color.b()) << 16)
    };
    let corner = if fullscreen {
        DWMWCP_DONOTROUND
    } else {
        DWMWCP_ROUNDSMALL
    };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark_mode).cast(),
            std::mem::size_of_val(&dark_mode) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&raw const border_color).cast(),
            std::mem::size_of_val(&border_color) as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            std::mem::size_of_val(&corner) as u32,
        );
    }
}

fn prepare_window_background(window: &Window) -> Result<()> {
    anyhow::ensure!(
        unsafe {
            SetWindowSubclass(
                window_hwnd(window)?,
                Some(backdrop_proc),
                BACKDROP_SUBCLASS,
                0,
            )
            .as_bool()
        },
        "install window background"
    );
    unsafe {
        let hwnd = window_hwnd(window)?;
        let dc = GetDC(Some(hwnd));
        paint_backdrop(hwnd, dc);
        ReleaseDC(Some(hwnd), dc);
    }
    Ok(())
}

unsafe extern "system" fn backdrop_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        match message {
            WM_ERASEBKGND => {
                paint_backdrop(hwnd, HDC(wparam.0 as _));
                return LRESULT(1);
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(backdrop_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}

unsafe fn paint_backdrop(hwnd: HWND, dc: HDC) {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_ok() {
            let color = crate::ui::theme::BG;
            let old = SetDCBrushColor(
                dc,
                COLORREF(
                    u32::from(color.r())
                        | (u32::from(color.g()) << 8)
                        | (u32::from(color.b()) << 16),
                ),
            );
            FillRect(dc, &rect, HBRUSH(GetStockObject(DC_BRUSH).0));
            SetDCBrushColor(dc, old);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowMoveState {
    start: Option<(POINT, PhysicalPosition<i32>)>,
}

pub(crate) fn update_nonmodal_window_move(
    ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    state: &mut WindowMoveState,
) {
    if window.fullscreen().is_some() {
        state.start = None;
        return;
    }
    if response.double_clicked() {
        if state.start.take().is_some() {
            let _ = unsafe { ReleaseCapture() };
        }
        window.set_maximized(!window.is_maximized());
        return;
    }
    if response.drag_started() {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok()
            && let Ok(origin) = window.outer_position()
        {
            if window.is_maximized() {
                window.set_maximized(false);
            }
            state.start = Some((cursor, origin));
            tracing::debug!(x = origin.x, y = origin.y, "local title drag started");
            if let Ok(hwnd) = window_hwnd(window) {
                unsafe { SetCapture(hwnd) };
            }
        }
    }
    let primary_down = ctx.input(|input| input.pointer.primary_down());
    if primary_down && let Some((start_cursor, origin)) = state.start {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok() {
            let started = Instant::now();
            window.set_outer_position(PhysicalPosition::new(
                origin.x.saturating_add(cursor.x - start_cursor.x),
                origin.y.saturating_add(cursor.y - start_cursor.y),
            ));
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(50) {
                tracing::warn!(
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    "moving local viewer HWND blocked"
                );
            }
        }
    }
    if response.drag_stopped() || !primary_down {
        if state.start.is_some() {
            tracing::debug!("local title drag ended");
        }
        if state.start.take().is_some() {
            let _ = unsafe { ReleaseCapture() };
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowResizeState {
    start: Option<WindowResizeStart>,
    pub(crate) min_size: Option<PhysicalSize<u32>>,
    pub(crate) requested_render_size: Option<PhysicalSize<u32>>,
}

#[derive(Clone, Copy, Debug)]
struct WindowResizeStart {
    cursor: POINT,
    origin: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    direction: ResizeDirection,
}

pub(crate) fn update_nonmodal_window_resize(
    ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    direction: ResizeDirection,
    state: &mut WindowResizeState,
    aspect: Option<(u32, u32)>,
) {
    if response.drag_started() {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok()
            && let Ok(origin) = window.outer_position()
        {
            state.start = Some(WindowResizeStart {
                cursor,
                origin,
                size: window.inner_size(),
                direction,
            });
            if let Ok(hwnd) = window_hwnd(window) {
                unsafe { SetCapture(hwnd) };
            }
        }
    }
    let primary_down = ctx.input(|input| input.pointer.primary_down());
    if primary_down && let Some(start) = state.start {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&raw mut cursor) }.is_ok() {
            state.requested_render_size = Some(apply_absolute_resize(
                window,
                start,
                cursor.x - start.cursor.x,
                cursor.y - start.cursor.y,
                aspect,
                state.min_size.unwrap_or(PhysicalSize::new(640, 400)),
            ));
        }
    }
    if response.drag_stopped() || !primary_down {
        if state.start.take().is_some() {
            let _ = unsafe { ReleaseCapture() };
        }
    }
}

fn apply_absolute_resize(
    window: &Window,
    start: WindowResizeStart,
    dx: i32,
    dy: i32,
    aspect: Option<(u32, u32)>,
    min_size: PhysicalSize<u32>,
) -> PhysicalSize<u32> {
    let west = matches!(
        start.direction,
        ResizeDirection::West | ResizeDirection::NorthWest | ResizeDirection::SouthWest
    );
    let east = matches!(
        start.direction,
        ResizeDirection::East | ResizeDirection::NorthEast | ResizeDirection::SouthEast
    );
    let north = matches!(
        start.direction,
        ResizeDirection::North | ResizeDirection::NorthEast | ResizeDirection::NorthWest
    );
    let south = matches!(
        start.direction,
        ResizeDirection::South | ResizeDirection::SouthEast | ResizeDirection::SouthWest
    );
    let mut width = start.size.width as i32
        + if east {
            dx
        } else if west {
            -dx
        } else {
            0
        };
    let mut height = start.size.height as i32
        + if south {
            dy
        } else if north {
            -dy
        } else {
            0
        };
    let min_width = min_size.width as i32;
    let min_height = min_size.height as i32;
    width = width.max(min_width);
    height = height.max(min_height);

    if let Some((video_width, video_height)) = aspect {
        let title_height = title_bar_height_pixels(window) as i32;
        let horizontal_only = (west || east) && !north && !south;
        let vertical_only = (north || south) && !west && !east;
        let width_drives = horizontal_only
            || (!vertical_only
                && i64::from(dx.abs()) * i64::from(video_height)
                    >= i64::from(dy.abs()) * i64::from(video_width));
        if width_drives {
            height = ((i64::from(width) * i64::from(video_height) + i64::from(video_width) / 2)
                / i64::from(video_width)) as i32
                + title_height;
            height = height.max(min_height);
        } else {
            let content_height = (height - title_height).max(1);
            width = ((i64::from(content_height) * i64::from(video_width)
                + i64::from(video_height) / 2)
                / i64::from(video_height)) as i32;
            width = width.max(min_width);
        }
    }

    let left = if west {
        start.origin.x + start.size.width as i32 - width
    } else {
        start.origin.x
    };
    let top = if north {
        start.origin.y + start.size.height as i32 - height
    } else {
        start.origin.y
    };
    window.set_outer_position(PhysicalPosition::new(left, top));
    let target = PhysicalSize::new(width as u32, height as u32);
    let _ = window.request_inner_size(target);
    target
}
