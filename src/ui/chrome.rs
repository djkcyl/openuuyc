//! Shared borderless window chrome and native fallback background.
use super::controls::{ViewerCaptionIcon as TitleIcon, viewer_caption_button as title_icon_button};
#[cfg(windows)]
use super::gfx::window_hwnd;
use winit::window::{ResizeDirection, Window};

#[cfg(windows)]
#[path = "chrome_native_windows.rs"]
mod native;
#[cfg(not(windows))]
#[path = "chrome_native_linux.rs"]
mod native;

pub(crate) use native::{
    WindowMoveState, WindowResizeState, cancel_pointer_operation, configure_dwm_window,
    update_nonmodal_window_move, update_nonmodal_window_resize,
};

fn title_bar_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(crate::ui::theme::SIDEBAR)
        .inner_margin(crate::ui::theme::WINDOW_TITLE_MARGIN)
        .stroke(egui::Stroke::new(
            crate::ui::theme::WINDOW_TITLE_STROKE,
            crate::ui::theme::LINE,
        ))
}

pub(crate) fn title_bar_height() -> f32 {
    crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT + title_bar_frame().total_margin().sum().y
}

pub(crate) fn title_bar_panel<R>(
    ui: &mut egui::Ui,
    id: &'static str,
    height: f32,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let clip = ui.clip_rect();
    let bottom = ui.available_rect_before_wrap().top() + height;
    // The native video child starts exactly at this boundary. Frame-edge
    // antialiasing must not escape into its first row at fractional DPI.
    ui.set_clip_rect(clip.intersect(egui::Rect::from_min_max(
        clip.min,
        egui::pos2(clip.right(), bottom),
    )));
    let result = egui::Panel::top(id)
        .frame(title_bar_frame())
        .exact_size(height)
        .show(ui, contents);
    ui.set_clip_rect(clip);
    result
}

pub(crate) fn window_title_bar(
    ui: &mut egui::Ui,
    window: &Window,
    alias: &str,
    move_state: Option<&mut WindowMoveState>,
) -> bool {
    ui.set_min_height(crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT);
    let rect = egui::Rect::from_min_size(
        ui.available_rect_before_wrap().min,
        egui::vec2(
            ui.available_width(),
            crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT,
        ),
    );
    let (caption_rect, controls_rect) = title_regions(rect);
    let drag = ui.interact(
        caption_rect,
        ui.id().with("window-title-drag"),
        egui::Sense::click_and_drag(),
    );
    if let Some(state) = move_state {
        update_nonmodal_window_move(ui.ctx(), window, &drag, state);
    } else if handle_title_drag(window, &drag) {
        let _ = window.drag_window();
    }
    let mut caption = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(caption_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    caption.set_clip_rect(caption_rect);
    paint_brand_logo(&mut caption);
    caption.add(
        egui::Label::new(
            egui::RichText::new(alias)
                .size(crate::ui::theme::SMALL)
                .color(crate::ui::theme::TEXT),
        )
        .truncate(),
    );
    let mut controls = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(controls_rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    controls.spacing_mut().item_spacing.x = 4.0;
    let close = window_buttons(&mut controls, window);
    ui.allocate_rect(rect, egui::Sense::hover());
    close
}

fn title_regions(rect: egui::Rect) -> (egui::Rect, egui::Rect) {
    let controls_left = (rect.right() - crate::ui::theme::WINDOW_CONTROLS_WIDTH).max(rect.left());
    (
        egui::Rect::from_min_max(rect.min, egui::pos2(controls_left, rect.bottom())),
        egui::Rect::from_min_max(egui::pos2(controls_left, rect.top()), rect.max),
    )
}

pub(crate) fn paint_brand_logo(ui: &mut egui::Ui) {
    let key = egui::Id::new("window-brand-logo");
    let texture = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(key))
        .unwrap_or_else(|| {
            let texture = crate::ui::branding::load_texture(ui.ctx());
            ui.ctx()
                .data_mut(|data| data.insert_temp(key, texture.clone()));
            texture
        });
    let (rect, _) = ui.allocate_exact_size(
        egui::Vec2::splat(crate::ui::theme::WINDOW_LOGO_SIZE),
        egui::Sense::hover(),
    );
    crate::ui::branding::paint(ui.painter(), rect, &texture);
}

pub(crate) fn handle_title_drag(window: &Window, response: &egui::Response) -> bool {
    if window.fullscreen().is_some() {
        return false;
    }
    if response.double_clicked() {
        window.set_maximized(!window.is_maximized());
        false
    } else {
        response.drag_started()
    }
}

pub(crate) fn window_buttons(ui: &mut egui::Ui, window: &Window) -> bool {
    let expanded = window.is_maximized() || window.fullscreen().is_some();
    let minimize = title_icon_button(ui, TitleIcon::Minimize, false, "最小化");
    if minimize.clicked() {
        window.set_minimized(true);
    }
    let maximize = title_icon_button(
        ui,
        if expanded {
            TitleIcon::Restore
        } else {
            TitleIcon::Maximize
        },
        false,
        if expanded { "还原" } else { "最大化" },
    );
    if maximize.clicked() {
        if window.fullscreen().is_some() {
            window.set_fullscreen(None);
        } else {
            window.set_maximized(!window.is_maximized());
        }
    }
    title_icon_button(ui, TitleIcon::Close, false, "关闭").clicked()
}

pub(crate) fn resize_regions(
    ui: &mut egui::Ui,
    window: &Window,
    mut handle: impl FnMut(&egui::Response, ResizeDirection),
) {
    if !window.is_resizable() || window.is_maximized() || window.fullscreen().is_some() {
        return;
    }
    let rect = ui.max_rect();
    // Page panels own the background layer. Keep the narrow resize handles
    // above them so a central panel cannot swallow border gestures.
    let resize_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).layer_id(
        egui::LayerId::new(egui::Order::Foreground, ui.id().with("window-resize")),
    ));
    let edge = 6.0;
    let corner = 12.0;
    let regions = [
        (
            egui::Rect::from_min_max(
                rect.min,
                egui::pos2(rect.min.x + corner, rect.min.y + corner),
            ),
            ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - corner, rect.min.y),
                egui::pos2(rect.max.x, rect.min.y + corner),
            ),
            ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.max.y - corner),
                egui::pos2(rect.min.x + corner, rect.max.y),
            ),
            ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - corner, rect.max.y - corner),
                rect.max,
            ),
            ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x + corner, rect.min.y),
                egui::pos2(rect.max.x - corner, rect.min.y + edge),
            ),
            ResizeDirection::North,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x + corner, rect.max.y - edge),
                egui::pos2(rect.max.x - corner, rect.max.y),
            ),
            ResizeDirection::South,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.min.y + corner),
                egui::pos2(rect.min.x + edge, rect.max.y - corner),
            ),
            ResizeDirection::West,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - edge, rect.min.y + corner),
                egui::pos2(rect.max.x, rect.max.y - corner),
            ),
            ResizeDirection::East,
            egui::CursorIcon::ResizeHorizontal,
        ),
    ];
    for (index, (region, direction, cursor)) in regions.into_iter().enumerate() {
        let response = resize_ui
            .interact(region, ui.id().with(("resize", index)), egui::Sense::drag())
            .on_hover_cursor(cursor);
        handle(&response, direction);
    }
}

pub(crate) fn title_bar_height_pixels(window: &Window) -> u32 {
    if window.fullscreen().is_some() {
        0
    } else {
        // Include the egui frame's padding and stroke. Round outward so a
        // fractional-DPI border cannot paint over the first video row.
        title_bar_height_at_scale(window.scale_factor())
    }
}

fn title_bar_height_at_scale(scale: f64) -> u32 {
    (f64::from(title_bar_height()) * scale).ceil().max(1.0) as u32
}
