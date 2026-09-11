//! Consume RAWMOUSE transitions directly, without winit's per-packet final-state
//! reduction. Wheel input uses the window path, including precision touchpads.
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RAWMOUSE, RID_HEADER, RID_INPUT,
    RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, MSG, PostMessageW, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WM_NULL,
};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Cursor, CursorGrabMode, CursorIcon, CustomCursor, Window};

use super::windows_presenter::{fit_rect, title_bar_height_pixels};
type VideoSize = Arc<Mutex<Option<(u32, u32, u16)>>>;
use crate::remote_input::{BUTTONS, MouseMode, RemoteInput};
use crate::stream_control::StreamControlHandle;

#[derive(Clone)]
struct Target {
    hwnd: isize,
    control: StreamControlHandle,
    context: egui::Context,
    track: i32,
    video_size: VideoSize,
    output: [u32; 2],
    top: u32,
    resize_edges: bool,
}

impl Target {
    fn owner(&self) -> u64 {
        self.hwnd as u64
    }
    fn focused(&self) -> bool {
        unsafe { GetForegroundWindow().0 as isize == self.hwnd }
    }

    fn position(&self, mut point: POINT, dragging: bool) -> Option<(i32, f64, f64)> {
        let (width, height, rotation) =
            (*self.video_size.lock().unwrap_or_else(|p| p.into_inner()))?;
        let (width, height) = if matches!(rotation, 90 | 270) {
            (height, width)
        } else {
            (width, height)
        };
        if !unsafe { ScreenToClient(HWND(self.hwnd as _), &mut point).as_bool() } {
            return None;
        }
        let ppp = self.context.pixels_per_point();
        let logical = egui::pos2(point.x as f32 / ppp, point.y as f32 / ppp);
        if !dragging && self.resize_edges {
            let size = egui::vec2(self.output[0] as f32 / ppp, self.output[1] as f32 / ppp);
            if logical.x < 6.0
                || logical.x >= size.x - 6.0
                || logical.y >= size.y - 6.0
                || (logical.y >= size.y - 12.0 && (logical.x < 12.0 || logical.x >= size.x - 12.0))
            {
                return None;
            }
        }
        if !dragging
            && self
                .context
                .layer_id_at(logical)
                .is_some_and(|layer| layer.order != egui::Order::Background)
        {
            return None;
        }
        let rect = fit_rect(
            width,
            height,
            self.output[0],
            self.output[1].saturating_sub(self.top).max(1),
        );
        let x = point.x - rect.left;
        let y = point.y - rect.top - self.top as i32;
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w <= 0 || h <= 0 || (!dragging && (x < 0 || y < 0 || x >= w || y >= h)) {
            return None;
        }
        let (screen, remote_w, remote_h) = self.control.mouse_screen(self.track)?;
        // The host rounds ratio*screen_size, so stop at its last physical pixel.
        let x = (f64::from(x.clamp(0, w - 1)) / f64::from(w))
            .min(1.0 - 1.0 / f64::from(remote_w.max(1)));
        let y = (f64::from(y.clamp(0, h - 1)) / f64::from(h))
            .min(1.0 - 1.0 / f64::from(remote_h.max(1)));
        Some((screen, x, y))
    }
}

#[derive(Default)]
pub(super) struct RawRouter(Mutex<Option<Target>>);

pub(super) fn router() -> &'static Arc<RawRouter> {
    static ROUTER: OnceLock<Arc<RawRouter>> = OnceLock::new();
    ROUTER.get_or_init(|| Arc::new(RawRouter::default()))
}

impl RawRouter {
    pub(super) fn wake_keyboard(&self, owner: u64) {
        if let Some(target) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
            && target.owner() == owner
        {
            // Wake the GUI without locking egui from the input hook thread.
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(owner as _)),
                    WM_NULL,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
            };
        }
    }
    pub(super) fn keyboard_allowed(&self, owner: u64) -> bool {
        let Some(target) = self.0.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
            return false;
        };
        if target.owner() != owner || !target.focused() {
            return false;
        }
        if target.top == 0 || target.control.mouse().relative_mode() {
            return true;
        }
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point) }.is_err()
            || !unsafe { ScreenToClient(HWND(owner as _), &mut point).as_bool() }
        {
            return false;
        }
        let Some((width, height, rotation)) =
            *target.video_size.lock().unwrap_or_else(|p| p.into_inner())
        else {
            return false;
        };
        let (width, height) = if matches!(rotation, 90 | 270) {
            (height, width)
        } else {
            (width, height)
        };
        let rect = fit_rect(
            width,
            height,
            target.output[0],
            target.output[1].saturating_sub(target.top).max(1),
        );
        point.y -= target.top as i32;
        point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
    }
    fn wheel(&self, owner: u64, delta: winit::event::MouseScrollDelta) {
        let Some(target) = self.0.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
            return;
        };
        if target.owner() != owner || !target.focused() {
            return;
        }
        // Windows winit emits LineDelta from signed WM_MOUSE[H]WHEEL / 120,
        // and reverses horizontal direction. Recover native units, including 1.
        let Some([x, y]) = wheel_units(delta) else {
            return;
        };
        let input = target.control.mouse();
        if input.mode() == MouseMode::View {
            return;
        }
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point) }.is_err() {
            return;
        }
        if !input.relative_mode() {
            let Some((screen, x, y)) = target.position(point, input.owner_holds_buttons(owner))
            else {
                return;
            };
            input.absolute(owner, screen, x, y);
        }
        // Two messages because the server chooses horizontal when both are set.
        input.wheel(owner, x, true);
        input.wheel(owner, y, false);
    }

    fn install(&self, target: Target) {
        let previous = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(target.clone());
        if let Some(previous) = previous
            && previous.hwnd != target.hwnd
        {
            previous.control.mouse().pause_owner(previous.owner());
        }
    }

    pub fn clear(&self, owner: u64) {
        let old = {
            let mut target = self.0.lock().unwrap_or_else(|p| p.into_inner());
            if target
                .as_ref()
                .is_some_and(|target| target.owner() == owner)
            {
                target.take()
            } else {
                None
            }
        };
        if let Some(target) = old {
            target.control.mouse().pause_owner(owner);
        }
    }

    /// Observe but never consume WM_INPUT: winit must still call DefWindowProc
    /// for native input cleanup and maintain its own local UI event behavior.
    pub fn message(&self, pointer: *const std::ffi::c_void) -> bool {
        if pointer.is_null() {
            return false;
        }
        let msg = unsafe { &*pointer.cast::<MSG>() };
        if msg.message != WM_INPUT && msg.message != WM_INPUT_DEVICE_CHANGE {
            return false;
        }
        let Some(target) = self.0.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
            return false;
        };
        if !target.focused() || msg.message == WM_INPUT_DEVICE_CHANGE {
            target.control.mouse().pause_owner(target.owner());
            return false;
        }
        let handle = HRAWINPUT(msg.lParam.0 as _);
        let mut header = RAWINPUTHEADER::default();
        let mut header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        let read = unsafe {
            GetRawInputData(
                handle,
                RID_HEADER,
                Some((&mut header as *mut RAWINPUTHEADER).cast()),
                &mut header_size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX || header.dwType != RIM_TYPEMOUSE.0 {
            return false;
        }
        let mut raw = RAWINPUT::default();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let read = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                Some((&mut raw as *mut RAWINPUT).cast()),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX
            || read
                < (std::mem::size_of::<RAWINPUTHEADER>() + std::mem::size_of::<RAWMOUSE>()) as u32
        {
            target
                .control
                .mouse()
                .fail("读取原始鼠标输入失败，控制已停止".into());
            return false;
        }
        let mouse = unsafe { raw.data.mouse };
        let flags = unsafe { mouse.Anonymous.Anonymous.usButtonFlags };
        let input = target.control.mouse();
        let mode = input.mode();
        if mode == MouseMode::View {
            return false;
        }
        let held = input.owner_holds_buttons(target.owner());
        let relative = input.relative_mode();
        let position = target.position(msg.pt, held);
        let accepted = relative || position.is_some();
        if !relative {
            if let Some((screen, x, y)) = position {
                input.absolute(target.owner(), screen, x, y);
            }
        } else if mouse.usFlags.0 & 1 == 0 {
            input.relative(target.owner(), mouse.lLastX, mouse.lLastY);
        }
        for (button, down) in button_edges(flags) {
            // UP is routed even outside the video, but the shared owner ledger
            // rejects releases for keys this window did not press.
            if !down || accepted {
                input.button(target.owner(), button, down);
            }
        }
        // Do not also forward raw wheel flags: WM_MOUSE[H]WHEEL is the sole
        // wheel source, as in the official window-event route.
        false
    }
}

fn wheel_units(delta: winit::event::MouseScrollDelta) -> Option<[i32; 2]> {
    let winit::event::MouseScrollDelta::LineDelta(x, y) = delta else {
        return None;
    };
    (x.is_finite() && y.is_finite())
        .then(|| [(-x * 120.0).round() as i32, (y * 120.0).round() as i32])
}

fn button_edges(flags: u16) -> impl Iterator<Item = (u32, bool)> {
    BUTTONS
        .into_iter()
        .enumerate()
        .flat_map(move |(index, button)| {
            let down = flags & (1 << (2 * index)) != 0;
            let up = flags & (2 << (2 * index)) != 0;
            [(button, true, down), (button, false, up)]
                .into_iter()
                .filter(|(_, _, present)| *present)
                .map(|(button, down, _)| (button, down))
        })
}

pub(super) struct WindowMouse {
    owner: u64,
    input: RemoteInput,
    grabbed: bool,
    capture_center: Option<POINT>,
    restore_position: Option<winit::dpi::PhysicalPosition<f64>>,
    shape: Option<Arc<crate::remote_cursor::CursorImage>>,
    decoded_shape: Option<image::RgbaImage>,
    system_source: Option<super::windows_cursor::SystemCursorSource>,
    system_cursor: Option<super::windows_cursor::SystemCursor>,
    cursor_size: [u32; 2],
    cursor: Cursor,
    using_cursor: bool,
    _repaint: Arc<dyn Fn() + Send + Sync>,
}

impl WindowMouse {
    pub fn new(window: &Window, control: &StreamControlHandle, context: &egui::Context) -> Self {
        let owner = crate::ui::d3d11::window_hwnd(window).map_or(0, |hwnd| hwnd.0 as u64);
        let ctx = context.clone();
        Self {
            owner,
            input: control.mouse().clone(),
            grabbed: false,
            capture_center: None,
            restore_position: None,
            shape: None,
            decoded_shape: None,
            system_source: None,
            system_cursor: None,
            cursor_size: [0; 2],
            cursor: CursorIcon::Default.into(),
            using_cursor: false,
            _repaint: control.mouse().subscribe(move || ctx.request_repaint()),
        }
    }

    pub fn release(&mut self, window: &Window) {
        super::windows_keyboard::clear(self.owner);
        if let Some(cursor) = &self.system_cursor {
            cursor.set_active(false);
        }
        router().clear(self.owner);
        self.input.pause_owner(self.owner);
        self.release_capture(window);
        if self.using_cursor {
            window.set_cursor_visible(true);
            window.set_cursor(CursorIcon::Default);
            self.using_cursor = false;
        }
    }

    pub fn wheel(&self, delta: winit::event::MouseScrollDelta) {
        router().wheel(self.owner, delta);
    }

    fn release_capture(&mut self, window: &Window) -> bool {
        if self.grabbed {
            if let Err(error) = window.set_cursor_grab(CursorGrabMode::None) {
                self.input.fail(format!("释放本地鼠标捕获失败：{error}"));
                return false;
            }
            self.grabbed = false;
            self.capture_center = None;
            window.set_cursor_visible(true);
            if window.has_focus()
                && let Some(position) = self.restore_position.take()
                && let Err(error) = window.set_cursor_position(position)
            {
                self.input.fail(format!("恢复鼠标位置失败：{error}"));
                return false;
            }
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn refresh(
        &mut self,
        window: &Window,
        context: &egui::Context,
        control: &StreamControlHandle,
        track: i32,
        video_size: &VideoSize,
        menu_open: bool,
        event_loop: &ActiveEventLoop,
    ) {
        let mode = self.input.mode();
        let relative = self.input.relative_mode();
        let frame_size = *video_size.lock().unwrap_or_else(|p| p.into_inner());
        if !window.has_focus()
            || window.is_minimized().unwrap_or(false)
            || mode == MouseMode::View
            || menu_open
            || context.any_popup_open()
            || context.text_edit_focused()
            || frame_size.is_none()
        {
            self.release(window);
            return;
        }
        if !relative {
            let mut remote_position = None;
            if let Some(point) = control.take_mouse_restore_point(track)
                && let Some((width, height, rotation)) = frame_size
            {
                let (width, height) = if matches!(rotation, 90 | 270) {
                    (height, width)
                } else {
                    (width, height)
                };
                let size = window.inner_size();
                let top = title_bar_height_pixels(window);
                let rect = fit_rect(
                    width,
                    height,
                    size.width,
                    size.height.saturating_sub(top).max(1),
                );
                remote_position = Some(winit::dpi::PhysicalPosition::new(
                    f64::from(rect.left)
                        + (point[0] * f64::from(rect.right - rect.left))
                            .min(f64::from((rect.right - rect.left - 1).max(0))),
                    f64::from(rect.top)
                        + f64::from(top)
                        + (point[1] * f64::from(rect.bottom - rect.top))
                            .min(f64::from((rect.bottom - rect.top - 1).max(0))),
                ));
            }
            // Smart cursor transitions change pointer presentation, not the
            // user's held-button state. Explicit pause/exit uses release().
            if remote_position.is_some() {
                self.restore_position = remote_position;
            }
            if !self.release_capture(window) {
                return;
            }
            // Capture was already released while waiting for a capture RPC.
            // Still apply the one-shot remote restore after Smart is confirmed.
            if remote_position.is_some()
                && let Some(position) = self.restore_position.take()
                && let Err(error) = window.set_cursor_position(position)
            {
                self.input.fail(format!("恢复鼠标位置失败：{error}"));
                self.release(window);
                return;
            }
        }
        let size = window.inner_size();
        let target = Target {
            hwnd: self.owner as isize,
            control: control.clone(),
            context: context.clone(),
            track,
            video_size: video_size.clone(),
            output: [size.width, size.height],
            top: title_bar_height_pixels(window),
            resize_edges: !window.is_maximized() && window.fullscreen().is_none(),
        };
        if relative && let Some((width, height, rotation)) = frame_size {
            let (width, height) = if matches!(rotation, 90 | 270) {
                (height, width)
            } else {
                (width, height)
            };
            let rect = fit_rect(
                width,
                height,
                size.width,
                size.height.saturating_sub(target.top).max(1),
            );
            let local_center = winit::dpi::PhysicalPosition::new(
                (rect.left + rect.right) / 2,
                target.top as i32 + (rect.top + rect.bottom) / 2,
            );
            let mut center = POINT {
                x: local_center.x,
                y: local_center.y,
            };
            if !unsafe { ClientToScreen(HWND(self.owner as _), &mut center).as_bool() } {
                self.input.fail("无法定位鼠标捕获区域".into());
                self.release(window);
                return;
            }
            if !self.grabbed || self.capture_center != Some(center) {
                if !self.grabbed {
                    self.restore_position = None;
                    let mut point = POINT::default();
                    if unsafe { GetCursorPos(&mut point) }.is_ok()
                        && unsafe { ScreenToClient(HWND(self.owner as _), &mut point).as_bool() }
                    {
                        self.restore_position = Some(winit::dpi::PhysicalPosition::new(
                            f64::from(point.x),
                            f64::from(point.y),
                        ));
                    }
                }
                // Winit alone owns clipping. Locked keeps the hidden cursor at
                // this video point; resize/move updates it without releasing keys.
                let result = window
                    .set_cursor_grab(CursorGrabMode::None)
                    .and_then(|()| window.set_cursor_position(local_center))
                    .and_then(|()| window.set_cursor_grab(CursorGrabMode::Locked));
                if let Err(error) = result {
                    self.input.fail(format!("捕获游戏鼠标失败：{error}"));
                    // Also retire a capture whose final native call partly failed.
                    self.grabbed = true;
                    self.release(window);
                    return;
                }
                self.grabbed = true;
                self.capture_center = Some(center);
            }
        }
        let mut point = POINT::default();
        let hovering =
            unsafe { GetCursorPos(&mut point) }.is_ok() && target.position(point, false).is_some();
        if !relative && hovering {
            if let Some(remote) = control.remote_cursor() {
                let changed = self
                    .shape
                    .as_ref()
                    .is_none_or(|shape| !Arc::ptr_eq(shape, &remote.image));
                let scale = match (frame_size, control.mouse_screen(track)) {
                    (Some((width, height, rotation)), Some((_, remote_w, remote_h)))
                        if remote_w > 0 && remote_h > 0 =>
                    {
                        let (width, height) = if matches!(rotation, 90 | 270) {
                            (height, width)
                        } else {
                            (width, height)
                        };
                        let rect = fit_rect(
                            width,
                            height,
                            size.width,
                            size.height.saturating_sub(target.top).max(1),
                        );
                        // Official VideoWidget uses physical display area / peer
                        // screen area, not encoded resolution, with a 50% floor.
                        // inner_size is already physical: do not apply DPI twice.
                        (f64::from((rect.right - rect.left).max(1))
                            * f64::from((rect.bottom - rect.top).max(1))
                            / (f64::from(remote_w) * f64::from(remote_h)))
                        .sqrt()
                        .max(0.5)
                    }
                    _ => 1.0,
                };
                if changed {
                    self.decoded_shape = decode_cursor(&remote.image).unwrap_or_else(|error| {
                        tracing::warn!(%error, "remote cursor image unavailable");
                        None
                    });
                    self.system_source = if self.decoded_shape.is_none() {
                        super::windows_cursor::SystemCursorSource::load(
                            remote.image.system_type,
                            [remote.image.width, remote.image.height],
                        )
                        .map_err(|error| tracing::warn!(%error, "system cursor source unavailable"))
                        .ok()
                    } else {
                        None
                    };
                }
                let source_size = self
                    .system_source
                    .as_ref()
                    .map_or([remote.image.width, remote.image.height], |source| {
                        source.size
                    });
                let dimensions = cursor_dimensions(source_size, scale);
                if changed || self.cursor_size != dimensions {
                    self.system_cursor = None;
                    self.cursor = make_cursor(
                        &remote.image,
                        self.decoded_shape.as_ref(),
                        dimensions,
                        event_loop,
                    )
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, "remote native cursor unavailable");
                        system_cursor(remote.image.system_type).into()
                    });
                    if let Some(source) = &self.system_source {
                        self.system_cursor = super::windows_cursor::SystemCursor::new(
                            HWND(self.owner as _),
                            source,
                            dimensions,
                        )
                        .map_err(|error| tracing::warn!(%error, "scaled system cursor unavailable"))
                        .ok();
                    }
                    self.cursor_size = dimensions;
                }
                self.shape = Some(remote.image);
            } else {
                self.shape = None;
                self.decoded_shape = None;
                self.system_cursor = None;
                self.system_source = None;
                self.cursor_size = [0; 2];
                self.cursor = CursorIcon::Default.into();
            }
            window.set_cursor(self.cursor.clone());
            if let Some(cursor) = &self.system_cursor {
                cursor.set_active(true);
            }
            self.using_cursor = true;
        } else if self.using_cursor {
            if let Some(cursor) = &self.system_cursor {
                cursor.set_active(false);
            }
            window.set_cursor(CursorIcon::Default);
            self.using_cursor = false;
        }
        // Controlled mode gets its cursor from the video. Smart-hidden and
        // forced-relative modes also hide the local pointer, as official UU.
        window.set_cursor_visible(!relative && (!hovering || !control.remote_cursor_hidden()));
        self.using_cursor |= relative;
        router().install(target);
        super::windows_keyboard::set_target(self.owner, &self.input);
    }
}

impl Drop for WindowMouse {
    fn drop(&mut self) {
        super::windows_keyboard::clear(self.owner);
        router().clear(self.owner);
        self.input.pause_owner(self.owner);
        // Winit releases cursor clipping with the owning Window. Normal paths
        // explicitly call release() while that Window is still alive.
    }
}

fn system_cursor(kind: i32) -> CursorIcon {
    match kind {
        32513 => CursorIcon::Text,
        32514 => CursorIcon::Wait,
        32515 => CursorIcon::Crosshair,
        32642 => CursorIcon::NwseResize,
        32643 => CursorIcon::NeswResize,
        32644 => CursorIcon::EwResize,
        32645 => CursorIcon::NsResize,
        32646 => CursorIcon::Move,
        32648 => CursorIcon::NotAllowed,
        32649 => CursorIcon::Pointer,
        32650 => CursorIcon::Progress,
        32651 => CursorIcon::Help,
        _ => CursorIcon::Default,
    }
}

fn decode_cursor(
    shape: &crate::remote_cursor::CursorImage,
) -> anyhow::Result<Option<image::RgbaImage>> {
    if shape.png.is_empty() {
        return Ok(None);
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(&shape.png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.into_rgba8();
    anyhow::ensure!(
        image.width() == shape.width && image.height() == shape.height,
        "cursor dimensions mismatch"
    );
    Ok(Some(image))
}

fn cursor_dimensions(size: [u32; 2], scale: f64) -> [u32; 2] {
    let limit = f64::from(winit::window::MAX_CURSOR_SIZE);
    let scale = scale.min(limit / f64::from(size[0].max(size[1]).max(1)));
    size.map(|n| (f64::from(n) * scale).floor().max(1.0) as u32)
}

fn scaled_cursor_pixels(image: &image::RgbaImage, dimensions: [u32; 2]) -> Vec<u8> {
    if [image.width(), image.height()] == dimensions {
        return image.as_raw().clone();
    }
    // Filter premultiplied colors so transparent PNG borders do not produce
    // dark/colored fringes. Winit expects straight alpha at the API boundary.
    let premultiplied = image::Rgba32FImage::from_fn(image.width(), image.height(), |x, y| {
        let p = image.get_pixel(x, y);
        let a = f32::from(p[3]) / 255.0;
        image::Rgba([
            f32::from(p[0]) / 255.0 * a,
            f32::from(p[1]) / 255.0 * a,
            f32::from(p[2]) / 255.0 * a,
            a,
        ])
    });
    let resized = image::imageops::resize(
        &premultiplied,
        dimensions[0],
        dimensions[1],
        image::imageops::FilterType::Triangle,
    );
    let mut pixels = Vec::with_capacity(resized.len());
    for p in resized.pixels() {
        let alpha = (p[3] * 255.0).round().clamp(0.0, 255.0) as u8;
        for c in &p.0[..3] {
            pixels.push(if alpha == 0 {
                0
            } else {
                (c / p[3] * 255.0).round().clamp(0.0, 255.0) as u8
            });
        }
        pixels.push(alpha);
    }
    pixels
}

fn make_cursor(
    shape: &crate::remote_cursor::CursorImage,
    image: Option<&image::RgbaImage>,
    dimensions: [u32; 2],
    event_loop: &ActiveEventLoop,
) -> anyhow::Result<Cursor> {
    let Some(image) = image else {
        return Ok(system_cursor(shape.system_type).into());
    };
    // Use the rounded bitmap dimensions for the hotspot, matching CreateIconIndirect.
    let hotspot = [0, 1].map(|axis| {
        (u64::from(shape.hotspot[axis]) * u64::from(dimensions[axis])
            / u64::from([shape.width, shape.height][axis].max(1)))
        .min(u64::from(dimensions[axis] - 1)) as u16
    });
    let source = CustomCursor::from_rgba(
        scaled_cursor_pixels(image, dimensions),
        dimensions[0].try_into()?,
        dimensions[1].try_into()?,
        hotspot[0],
        hotspot[1],
    )?;
    Ok(event_loop.create_custom_cursor(source).into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_winit_wheel_units_keep_direction_and_sub_detents() {
        use winit::event::MouseScrollDelta::LineDelta;
        assert_eq!(
            super::wheel_units(LineDelta(-1.0 / 120.0, 1.0 / 120.0)),
            Some([1, 1])
        );
        assert_eq!(super::wheel_units(LineDelta(1.0, -2.0)), Some([-120, -240]));
    }

    #[test]
    fn raw_packet_keeps_both_edges_for_every_button() {
        let edges: Vec<_> = super::button_edges(0x03ff).collect();
        assert_eq!(
            edges,
            vec![
                (1, true),
                (1, false),
                (2, true),
                (2, false),
                (16, true),
                (16, false),
                (32, true),
                (32, false),
                (64, true),
                (64, false)
            ]
        );
        assert_eq!(super::button_edges(0x0c00).count(), 0);
    }
}
