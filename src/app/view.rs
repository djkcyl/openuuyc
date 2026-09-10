//! Device-center presentation. Network/account ownership remains in app.rs.
use super::*;
use egui::{Align, Color32, FontId, RichText, Sense, Stroke, vec2};
mod assist;

const BG: Color32 = Color32::from_rgb(22, 26, 33);
const SIDEBAR: Color32 = Color32::from_rgb(16, 20, 26);
const SURFACE: Color32 = Color32::from_rgb(31, 37, 47);
const LINE: Color32 = Color32::from_rgb(45, 53, 66);
const TEXT: Color32 = Color32::from_rgb(231, 235, 242);
const MUTED: Color32 = Color32::from_rgb(156, 167, 184);
const BLUE: Color32 = Color32::from_rgb(75, 136, 235);
const GREEN: Color32 = Color32::from_rgb(102, 207, 156);
const AMBER: Color32 = Color32::from_rgb(238, 190, 111);
const RED: Color32 = Color32::from_rgb(241, 125, 132);

fn singleline_input(value: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(value)
        .vertical_align(Align::Center)
        .margin(egui::Margin::symmetric(12, 8))
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Mine,
    Assist,
    Favorites,
    Management,
    Settings,
}
impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Mine => "我的设备",
            Self::Assist => "远程协助",
            Self::Favorites => "收藏设备",
            Self::Management => "全部设备",
            Self::Settings => "连接设置",
        }
    }
}

#[derive(Default)]
pub(super) struct CenterUi {
    page: Page,
    search: String,
    online_only: bool,
    details_open: bool,
    show_virtual: bool,
    edit: Option<DeviceEdit>,
}

struct DeviceEdit {
    device: DeviceInfo,
    alias: String,
    action: EditAction,
}
enum EditAction {
    Rename,
    Remove,
}

impl CenterUi {
    pub(super) fn open_details(&mut self) {
        self.details_open = true;
    }
    pub(super) fn show_virtual(&self) -> bool {
        self.show_virtual
    }
    pub(super) fn close_details(&mut self) {
        self.details_open = false;
        self.edit = None;
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Monitor,
    Virtual,
    Tablet,
    Settings,
    Refresh,
    Close,
    Info,
    Account,
    Assist,
    Star,
    Edit,
}

pub(super) fn configure_visuals(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = BG;
    style.visuals.extreme_bg_color = SIDEBAR;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.selection.bg_fill = Color32::from_rgb(40, 70, 112);
    style.visuals.selection.stroke = Stroke::new(1.0, BLUE);
    style.visuals.window_corner_radius = 8.0.into();
    for widgets in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.open,
    ] {
        widgets.bg_fill = SURFACE;
        widgets.weak_bg_fill = SURFACE;
        widgets.bg_stroke = Stroke::new(1.0, LINE);
        widgets.fg_stroke.color = TEXT;
        widgets.corner_radius = 5.0.into();
    }
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(44, 54, 69);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(44, 54, 69);
    style.visuals.widgets.hovered.fg_stroke.color = TEXT;
    style.visuals.widgets.hovered.corner_radius = 5.0.into();
    style.visuals.widgets.active.bg_fill = BLUE;
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(36, 60, 92);
    style.visuals.widgets.active.corner_radius = 5.0.into();
    style.spacing.item_spacing = vec2(8.0, 8.0);
    style.spacing.button_padding = vec2(14.0, 8.0);
    style.spacing.interact_size.y = 34.0;
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, FontId::proportional(12.0));
    style.interaction.selectable_labels = false;
    ctx.set_style_of(egui::Theme::Dark, style);
}

fn paint_icon(p: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
    let c = rect.center();
    let q = |x, y| c + vec2(x, y);
    let s = Stroke::new(1.5, color);
    match icon {
        Icon::Assist => {
            p.circle_stroke(q(-4.0, -5.0), 3.0, s);
            p.circle_stroke(q(6.0, -3.0), 2.5, s);
            p.add(egui::Shape::line(
                vec![
                    q(-10.0, 7.0),
                    q(-8.0, 1.0),
                    q(-3.0, 0.0),
                    q(2.0, 3.0),
                    q(3.0, 7.0),
                ],
                s,
            ));
            p.add(egui::Shape::line(
                vec![q(5.0, 2.0), q(9.0, 3.0), q(11.0, 7.0)],
                s,
            ));
        }
        Icon::Star => {
            let points = (0..10)
                .map(|i| {
                    let angle =
                        -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                    let radius = if i % 2 == 0 { 10.0 } else { 4.5 };
                    q(angle.cos() * radius, angle.sin() * radius)
                })
                .collect();
            p.add(egui::Shape::closed_line(points, s));
        }
        Icon::Edit => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-8.0, 8.0),
                    q(-6.0, 2.0),
                    q(5.0, -9.0),
                    q(9.0, -5.0),
                    q(-2.0, 6.0),
                ],
                s,
            ));
        }
        Icon::Virtual => {
            let top = q(0.0, -10.0);
            let left = q(-9.0, -5.0);
            let right = q(9.0, -5.0);
            let middle = q(0.0, 0.0);
            let bottom = q(0.0, 10.0);
            p.add(egui::Shape::closed_line(
                vec![top, right, q(9.0, 5.0), bottom, q(-9.0, 5.0), left],
                s,
            ));
            for edge in [[left, middle], [middle, right], [middle, bottom]] {
                p.line_segment(edge, s);
            }
        }
        Icon::Tablet => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, vec2(14.0, 20.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            p.circle_filled(q(0.0, 6.5), 1.0, color);
        }
        Icon::Monitor => {
            p.rect_stroke(
                egui::Rect::from_center_size(q(0.0, -2.0), vec2(20.0, 14.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            p.line_segment([q(0.0, 5.0), q(0.0, 9.0)], s);
            p.line_segment([q(-5.0, 9.0), q(5.0, 9.0)], s);
        }
        Icon::Settings => {
            for (y, knob) in [(-6.0, -3.0), (0.0, 4.0), (6.0, -1.0)] {
                p.line_segment([q(-9.0, y), q(9.0, y)], s);
                p.circle_filled(q(knob, y), 2.5, color);
            }
        }
        Icon::Refresh => {
            let points = (0..=22)
                .map(|i| {
                    let angle = 0.3 + i as f32 / 22.0 * 5.1;
                    c + vec2(angle.cos(), angle.sin()) * 7.0
                })
                .collect();
            p.add(egui::Shape::line(points, s));
            p.line_segment([q(6.5, -3.0), q(6.5, 2.5)], s);
            p.line_segment([q(1.0, 2.5), q(6.5, 2.5)], s);
        }
        Icon::Close => {
            p.line_segment([q(-4.5, -4.5), q(4.5, 4.5)], s);
            p.line_segment([q(4.5, -4.5), q(-4.5, 4.5)], s);
        }
        Icon::Info => {
            p.circle_stroke(c, 8.0, s);
            p.circle_filled(q(0.0, -3.5), 1.0, color);
            p.line_segment([q(0.0, 0.0), q(0.0, 4.0)], s);
        }
        Icon::Account => {
            p.circle_stroke(q(0.0, -5.0), 4.0, s);
            p.add(egui::Shape::line(
                vec![
                    q(-8.0, 8.0),
                    q(-6.0, 2.0),
                    q(0.0, 0.0),
                    q(6.0, 2.0),
                    q(8.0, 8.0),
                ],
                s,
            ));
        }
    }
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, hint: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    if response.hovered() && ui.is_enabled() {
        ui.painter().rect_filled(rect, 5.0, SURFACE);
    }
    paint_icon(
        ui.painter(),
        rect,
        icon,
        if ui.is_enabled() {
            MUTED
        } else {
            Color32::from_gray(90)
        },
    );
    response.on_hover_text(hint)
}

fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(BLUE)
        .stroke(Stroke::NONE)
        .min_size(vec2(84.0, 34.0))
}

fn login_button(label: &str) -> egui::Button<'_> {
    // Button's AtomLayout otherwise inherits the form's left alignment.
    egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow())).gap(0.0)
}

fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(20))
}

fn nav_item(
    ui: &mut egui::Ui,
    icon: Icon,
    title: &str,
    count: Option<usize>,
    selected: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    if selected || response.hovered() {
        ui.painter().rect_filled(
            rect,
            5.0,
            if selected {
                Color32::from_rgb(33, 51, 77)
            } else {
                SURFACE
            },
        );
    }
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(rect.left_center() + vec2(20.0, 0.0), vec2(22.0, 22.0)),
        icon,
        if selected { BLUE } else { MUTED },
    );
    ui.painter().text(
        rect.left_center() + vec2(42.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.0),
        TEXT,
    );
    if let Some(count) = count {
        ui.painter().text(
            rect.right_center() - vec2(12.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            count.to_string(),
            FontId::proportional(12.0),
            MUTED,
        );
    }
    response.clicked()
}

fn presence_text(state: &PresenceState) -> (&'static str, Color32) {
    match state {
        PresenceState::Connecting => ("本机正在上线", MUTED),
        PresenceState::Online => ("本机在线", GREEN),
        PresenceState::Reconnecting => ("本机正在重连", AMBER),
        PresenceState::Offline => ("本机离线", MUTED),
    }
}

fn device_status(device: &DeviceInfo) -> (&str, Color32) {
    if !device.is_connected() {
        ("离线", MUTED)
    } else if device.participant_count() > 0 {
        ("使用中", AMBER)
    } else if !device.controllable || !device.controlled_support {
        ("未开放连接", AMBER)
    } else {
        ("在线", GREEN)
    }
}

enum RowAction {
    Details,
    Connect,
}

fn table_columns(rect: egui::Rect) -> (f32, f32, f32) {
    (
        rect.right() - 408.0,
        rect.right() - 294.0,
        rect.right() - 164.0,
    )
}

fn device_row(
    ui: &mut egui::Ui,
    group: &str,
    device: &DeviceInfo,
    selected: bool,
    own_session: bool,
    connect_issue: Option<&str>,
    viewing: bool,
) -> Option<RowAction> {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 72.0), Sense::click());
    if response.hovered() || selected {
        ui.painter().rect_filled(
            rect,
            5.0,
            if selected {
                Color32::from_rgb(30, 44, 63)
            } else {
                Color32::from_rgb(29, 35, 44)
            },
        );
    }
    let (platform_x, status_x, actions_x) = table_columns(rect);
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(rect.left_center() + vec2(24.0, 0.0), vec2(28.0, 28.0)),
        if group.starts_with("虚拟设备") {
            Icon::Virtual
        } else if matches!(device.platform, 2 | 3) {
            Icon::Tablet
        } else {
            Icon::Monitor
        },
        if device.is_connected() { TEXT } else { MUTED },
    );
    let mut name = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_max(
                rect.left_top() + vec2(54.0, 14.0),
                egui::pos2(platform_x - 18.0, rect.bottom()),
            ))
            .layout(egui::Layout::top_down(Align::Min)),
    );
    name.add(egui::Label::new(RichText::new(display_alias(device)).size(15.0).strong()).truncate())
        .on_hover_text(display_alias(device));
    name.label(RichText::new(group).size(12.0).color(MUTED));
    ui.painter().text(
        egui::pos2(platform_x, rect.top() + 25.0),
        egui::Align2::LEFT_CENTER,
        device.platform_label(),
        FontId::proportional(13.0),
        TEXT,
    );
    ui.painter().text(
        egui::pos2(platform_x, rect.top() + 47.0),
        egui::Align2::LEFT_CENTER,
        if device.version_name.is_empty() {
            "版本未知"
        } else {
            &device.version_name
        },
        FontId::proportional(11.5),
        MUTED,
    );
    let (status, color) = if !viewing {
        (
            device.status_label(),
            if device.is_connected() { GREEN } else { MUTED },
        )
    } else if own_session {
        ("窗口已打开", BLUE)
    } else {
        device_status(device)
    };
    ui.painter().text(
        egui::pos2(status_x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        status,
        FontId::proportional(13.0),
        color,
    );
    let mut action = None;
    let mut buttons = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(egui::Rect::from_min_max(
                egui::pos2(actions_x, rect.top()),
                rect.right_bottom() - vec2(8.0, 0.0),
            ))
            .layout(egui::Layout::right_to_left(Align::Center)),
    );
    if viewing {
        let connect = buttons.add_enabled(
            connect_issue.is_none(),
            egui::Button::new(if own_session { "已打开" } else { "连接" })
                .fill(if selected && connect_issue.is_none() {
                    BLUE
                } else {
                    SURFACE
                })
                .min_size(vec2(76.0, 32.0)),
        );
        if connect.clicked() {
            action = Some(RowAction::Connect);
        }
        if let Some(issue) = connect_issue {
            connect.on_hover_text(issue);
        }
    }
    if buttons
        .add(egui::Button::new("详情").frame(false))
        .clicked()
    {
        action = Some(RowAction::Details);
    }
    ui.painter().line_segment(
        [
            rect.left_bottom() + vec2(10.0, 0.0),
            rect.right_bottom() - vec2(10.0, 0.0),
        ],
        Stroke::new(1.0, LINE),
    );
    action.or_else(|| response.clicked().then_some(RowAction::Details))
}

fn form_row(ui: &mut egui::Ui, label: &str, hint: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let label_width = (ui.available_width() - 258.0).max(160.0);
        ui.allocate_ui_with_layout(
            vec2(label_width, 52.0),
            egui::Layout::top_down(Align::Min),
            |ui| {
                ui.label(RichText::new(label).size(14.0));
                if !hint.is_empty() {
                    ui.label(RichText::new(hint).size(12.0).color(MUTED));
                }
            },
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), content);
    });
    ui.separator();
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(20.0);
    ui.label(RichText::new(title).size(16.0).strong());
    ui.add_space(8.0);
}

fn login_scan_placeholder(p: &egui::Painter, rect: egui::Rect) {
    let c = rect.center();
    let corner = Stroke::new(2.0, Color32::from_rgb(75, 106, 147));
    for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let edge = c + vec2(x * 48.0, y * 48.0);
        p.add(egui::Shape::line(
            vec![edge - vec2(x * 15.0, 0.0), edge, edge - vec2(0.0, y * 15.0)],
            corner,
        ));
    }
    p.rect_stroke(
        egui::Rect::from_center_size(c, vec2(38.0, 64.0)),
        6.0,
        Stroke::new(2.0, BLUE),
        egui::StrokeKind::Inside,
    );
    p.line_segment(
        [c + vec2(-6.0, -23.0), c + vec2(6.0, -23.0)],
        Stroke::new(2.0, BLUE),
    );
    p.circle_filled(c + vec2(0.0, 24.0), 2.0, BLUE);
}

fn login_qr_area(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    loading: bool,
) -> egui::Rect {
    // Reserve the same square exactly once for every state. Drawing into it
    // must not advance the cursor again (Ui::put does), nor negotiate a Frame
    // width with the wider login column.
    let (rect, _) = ui.allocate_exact_size(vec2(216.0, 216.0), Sense::hover());
    if let Some(texture) = texture {
        ui.painter().rect_filled(rect, 6.0, Color32::WHITE);
        ui.painter().image(
            texture.id(),
            rect.shrink(12.0),
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        ui.painter()
            .rect_filled(rect, 8.0, Color32::from_rgb(20, 26, 35));
        ui.painter()
            .rect_stroke(rect, 8.0, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        if loading {
            egui::Spinner::new().paint_at(
                ui,
                egui::Rect::from_center_size(rect.center(), vec2(28.0, 28.0)),
            );
        } else {
            login_scan_placeholder(ui.painter(), rect);
        }
    }
    rect
}

fn login_surface(root: &mut egui::Ui, mut content: impl FnMut(&mut egui::Ui, LoginMethod)) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(BG).inner_margin(24))
        .show(root, |ui| {
            let bounds = ui.available_rect_before_wrap();
            let card = egui::Rect::from_center_size(
                bounds.center(),
                vec2(
                    800.0_f32.min(bounds.width()),
                    510.0_f32.min(bounds.height()),
                ),
            );
            let painter = ui.painter();
            painter.rect_filled(
                card.translate(vec2(0.0, 8.0)),
                14.0,
                Color32::from_black_alpha(28),
            );
            painter.rect_filled(card, 14.0, Color32::from_rgb(27, 33, 43));
            painter.rect_stroke(card, 14.0, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
            let title =
                painter.layout_no_wrap(crate::APP_NAME.into(), FontId::proportional(25.0), TEXT);
            let brand_width = 36.0 + 12.0 + title.size().x;
            let brand = egui::Rect::from_min_size(
                egui::pos2(card.center().x - brand_width * 0.5, card.top() + 34.0),
                vec2(36.0, 36.0),
            );
            painter.rect_filled(brand, 9.0, Color32::from_rgb(37, 70, 114));
            paint_icon(
                painter,
                brand,
                Icon::Monitor,
                Color32::from_rgb(139, 185, 255),
            );
            painter.galley(
                brand.right_center() + vec2(12.0, -title.size().y * 0.5),
                title,
                TEXT,
            );
            painter.vline(
                card.center().x,
                (card.top() + 106.0)..=(card.bottom() - 34.0),
                Stroke::new(1.0, LINE),
            );
            for (method, center_x, heading) in [
                (LoginMethod::Qr, card.center().x - 192.0, "扫码登录"),
                (LoginMethod::Phone, card.center().x + 192.0, "短信登录"),
            ] {
                let column = egui::Rect::from_min_max(
                    egui::pos2(center_x - 160.0, card.top() + 104.0),
                    egui::pos2(center_x + 160.0, card.bottom() - 24.0),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .id_salt(heading)
                        .max_rect(column)
                        .layout(egui::Layout::top_down(Align::Center)),
                    |ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.label(RichText::new(heading).size(18.0).strong());
                        ui.add_space(18.0);
                        content(ui, method);
                    },
                );
            }
        });
}

#[derive(Default, PartialEq, Eq)]
enum QrAction {
    #[default]
    None,
    Start,
}

fn qr_form(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    running: bool,
    enabled: bool,
    status: &str,
    error: Option<&str>,
) -> QrAction {
    login_qr_area(ui, texture, running);
    ui.add_space(12.0);
    let message = if status.is_empty() {
        "使用 UU 远程手机端扫码"
    } else {
        status
    };
    let response = ui.add_sized(
        [320.0, 20.0],
        egui::Label::new(RichText::new(message).size(13.0).color(if error.is_some() {
            AMBER
        } else {
            MUTED
        }))
        .truncate(),
    );
    if let Some(error) = error {
        response.on_hover_text(error);
    }
    ui.add_space(10.0);
    if !running && error.is_some() {
        let clicked = ui
            .add_enabled_ui(enabled, |ui| {
                ui.add_sized([216.0, 40.0], login_button("刷新二维码").frame(false))
            })
            .inner
            .clicked();
        if clicked {
            return QrAction::Start;
        }
    } else {
        ui.allocate_exact_size(vec2(216.0, 40.0), Sense::hover());
    }
    QrAction::None
}

impl DeviceCenterApp {
    fn needs_login(&self) -> bool {
        self.devices.is_none() && self.catalog.is_none()
    }

    pub(super) fn draw_center(&mut self, ui: &mut egui::Ui) {
        if self.needs_login() {
            self.login_page(ui);
            return;
        }
        self.draw_navigation(ui);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(28, 24)),
            )
            .show(ui, |ui| {
                if self.center_ui.page == Page::Settings {
                    self.settings_page(ui);
                } else if self.center_ui.page == Page::Management {
                    self.management_page(ui);
                } else if matches!(self.center_ui.page, Page::Assist | Page::Favorites) {
                    self.assist_page(ui, self.center_ui.page == Page::Favorites);
                } else {
                    self.devices_page(ui);
                }
            });
    }

    fn draw_navigation(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("center-navigation")
            .resizable(false)
            .default_size(188.0)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(12, 20)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(28.0, 30.0), Sense::hover());
                    paint_icon(ui.painter(), rect, Icon::Monitor, BLUE);
                    ui.label(RichText::new(crate::APP_NAME).size(17.0).strong());
                });
                ui.add_space(28.0);
                let count = self
                    .devices
                    .as_ref()
                    .map(|list| {
                        list.my_binded_devices
                            .iter()
                            .filter(|d| self.show_in_watching_list(d))
                            .count()
                    })
                    .unwrap_or_default();
                if nav_item(
                    ui,
                    Icon::Monitor,
                    "我的设备",
                    Some(count),
                    self.center_ui.page == Page::Mine,
                ) {
                    self.center_ui.page = Page::Mine;
                }
                if nav_item(
                    ui,
                    Icon::Monitor,
                    "全部设备",
                    self.catalog.as_ref().map(|c| c.groups.entries().count()),
                    self.center_ui.page == Page::Management,
                ) {
                    self.center_ui.page = Page::Management;
                }
                ui.add_space(18.0);
                ui.label(RichText::new("远程协助").small().color(MUTED));
                if nav_item(
                    ui,
                    Icon::Assist,
                    "开始协助",
                    None,
                    self.center_ui.page == Page::Assist,
                ) {
                    self.center_ui.page = Page::Assist;
                    self.request_assist_refresh();
                }
                if nav_item(
                    ui,
                    Icon::Star,
                    "收藏设备",
                    self.assist.lists.as_ref().map(|l| l.favorites.len()),
                    self.center_ui.page == Page::Favorites,
                ) {
                    self.center_ui.page = Page::Favorites;
                    self.request_assist_refresh();
                }
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(10.0);
                if nav_item(
                    ui,
                    Icon::Settings,
                    "连接设置",
                    None,
                    self.center_ui.page == Page::Settings,
                ) {
                    self.center_ui.page = Page::Settings;
                }
                ui.with_layout(egui::Layout::bottom_up(Align::Min), |ui| {
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .size(11.0)
                            .color(MUTED),
                    );
                    let (presence, color) = presence_text(&self.presence);
                    ui.label(RichText::new(presence).size(12.0).color(color));
                    ui.add_space(8.0);
                    if self.logout_pending {
                        ui.label("正在退出账号…");
                    } else {
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(vec2(24.0, 26.0), Sense::hover());
                            paint_icon(ui.painter(), rect, Icon::Account, MUTED);
                            ui.add_sized(
                                [82.0, 24.0],
                                egui::Label::new(if self.account_name.trim().is_empty() {
                                    "已登录"
                                } else {
                                    &self.account_name
                                })
                                .truncate(),
                            )
                            .on_hover_text(&self.account_name);
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                if ui.add(egui::Button::new("退出").frame(false)).clicked() {
                                    self.logout();
                                }
                            });
                        });
                    }
                    ui.separator();
                });
            });
    }

    fn alert(&mut self, ui: &mut egui::Ui) {
        if !self.status.kind.is_alert() {
            return;
        }
        let color = if matches!(self.status.kind, StatusKind::Error) {
            RED
        } else {
            AMBER
        };
        let mut dismiss = false;
        egui::Frame::new()
            .fill(Color32::from_rgb(42, 35, 33))
            .corner_radius(5.0)
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 24.0), Sense::hover());
                    paint_icon(ui.painter(), rect, Icon::Info, color);
                    ui.add_sized(
                        [ui.available_width() - 40.0, 26.0],
                        egui::Label::new(RichText::new(&self.status.text).size(13.0).color(TEXT))
                            .truncate(),
                    )
                    .on_hover_text(&self.status.text);
                    dismiss = icon_button(ui, Icon::Close, "关闭提示").clicked();
                });
            });
        if dismiss {
            self.status = StatusMessage::info("");
        }
        ui.add_space(12.0);
    }

    fn active_view(&mut self, ui: &mut egui::Ui) {
        let Some(session) = &self.active_session else {
            return;
        };
        let alias = session.alias.clone();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if self.closing_session {
                    format!("正在关闭  {alias}")
                } else {
                    format!("观看窗口已打开  ·  {alias}")
                })
                .color(TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(!self.closing_session, egui::Button::new("结束观看"))
                    .clicked()
                {
                    self.stop_viewer();
                }
            });
        });
        ui.add_space(10.0);
    }

    fn devices_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(self.center_ui.page.title())
                    .size(25.0)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!self.refresh_pending && !self.logout_pending, |ui| {
                    let hint = self.refreshed_at.map_or_else(
                        || "刷新设备".to_owned(),
                        |at| format!("刷新设备 · 上次更新 {} 秒前", at.elapsed().as_secs()),
                    );
                    if icon_button(ui, Icon::Refresh, &hint).clicked() {
                        self.request_refresh();
                    }
                });
                ui.add_sized(
                    [248.0, 34.0],
                    singleline_input(&mut self.center_ui.search)
                        .hint_text("搜索设备名称或系统")
                        .desired_width(248.0),
                );
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let count = self
                .devices
                .as_ref()
                .map(|list| {
                    all_devices(list)
                        .filter(|(_, d)| self.show_in_watching_list(d))
                        .count()
                })
                .unwrap_or(0);
            ui.label(RichText::new(format!("共 {count} 台设备")).color(MUTED));
            if self.refresh_pending {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.checkbox(&mut self.center_ui.online_only, "仅在线");
                ui.checkbox(&mut self.center_ui.show_virtual, "显示虚拟设备");
            });
        });
        ui.add_space(18.0);
        self.alert(ui);
        self.active_view(ui);
        let Some(list) = &self.devices else {
            self.empty_state(
                ui,
                if self.refresh_pending {
                    "正在读取设备…"
                } else {
                    "暂时无法加载设备"
                },
                if self.refresh_pending {
                    "正在恢复账号并获取设备列表"
                } else {
                    "点击刷新重试"
                },
            );
            return;
        };
        let query = self.center_ui.search.trim().to_lowercase();
        let mut rows = all_devices(list)
            .filter(|(_, device)| {
                self.show_in_watching_list(device)
                    && (!self.center_ui.online_only || device.is_connected())
                    && (query.is_empty()
                        || display_alias(device).to_lowercase().contains(&query)
                        || device.platform_label().to_lowercase().contains(&query))
            })
            .collect::<Vec<_>>();
        rows.sort_by(|(_, a), (_, b)| {
            b.is_connected()
                .cmp(&a.is_connected())
                .then_with(|| a.alias.to_lowercase().cmp(&b.alias.to_lowercase()))
                .then_with(|| a.device_id.cmp(&b.device_id))
        });
        if rows.is_empty() {
            self.empty_state(
                ui,
                if !query.is_empty() {
                    "没有找到匹配的设备"
                } else if self.center_ui.online_only {
                    "暂无在线设备"
                } else {
                    "暂无设备"
                },
                if !query.is_empty() {
                    "换一个名称试试，或清空搜索条件"
                } else {
                    "设备上线后会自动显示在这里"
                },
            );
            if (!query.is_empty() || self.center_ui.online_only) && ui.button("清除筛选").clicked()
            {
                self.center_ui.search.clear();
                self.center_ui.online_only = false;
            }
            return;
        }
        let mut picked = None;
        egui::ScrollArea::vertical()
            .id_salt("center-devices-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (header, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
                let (platform, status, action) = table_columns(header);
                for (x, label) in [
                    (header.left() + 54.0, "设备名称"),
                    (platform, "系统 / 版本"),
                    (status, "状态"),
                    (action + 80.0, "操作"),
                ] {
                    ui.painter().text(
                        egui::pos2(x, header.center().y),
                        egui::Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(12.0),
                        MUTED,
                    );
                }
                for (group, device) in rows {
                    let note = if self
                        .catalog
                        .as_ref()
                        .is_some_and(|c| c.is_virtual(&device.device_id))
                    {
                        "虚拟设备"
                    } else {
                        group
                    };
                    let own = self.active_session.as_ref().is_some_and(|session| {
                        session.device_id.as_deref() == Some(device.device_id.as_str())
                    });
                    let issue = if self.logout_pending {
                        Some("正在退出账号".to_owned())
                    } else if self.active_session.is_some() {
                        Some("请先关闭当前观看窗口".to_owned())
                    } else {
                        connectability_error(device).err()
                    };
                    let action = ui
                        .push_id(&device.device_id, |ui| {
                            device_row(
                                ui,
                                note,
                                device,
                                self.selected_device_id.as_deref() == Some(&device.device_id),
                                own,
                                issue.as_deref(),
                                self.is_viewing_target(&device.device_id),
                            )
                        })
                        .inner;
                    if let Some(action) = action {
                        picked = Some((device.device_id.clone(), action));
                    }
                }
            });
        if let Some((id, action)) = picked {
            self.selected_device_id = Some(id);
            match action {
                RowAction::Details => {
                    self.open_details(self.selected_device_id.clone().expect("selected row"))
                }
                RowAction::Connect => self.start_viewer(),
            }
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, title: &str, detail: &str) {
        ui.add_space(56.0);
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
            paint_icon(ui.painter(), rect, Icon::Monitor, MUTED);
            ui.add_space(10.0);
            ui.label(RichText::new(title).size(20.0).strong());
            ui.label(RichText::new(detail).size(14.0).color(MUTED));
            if self.devices.is_none() && !self.refresh_pending {
                ui.add_space(16.0);
                if ui
                    .add_enabled(
                        !self.login_running
                            && !self.logout_pending
                            && self.active_session.is_none(),
                        primary("刷新"),
                    )
                    .clicked()
                {
                    self.request_refresh();
                }
            }
        });
    }

    fn management_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("全部设备").size(25.0).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, Icon::Refresh, "刷新列表及硬件详情").clicked() {
                    self.request_refresh();
                }
                ui.add_sized(
                    [248.0, 34.0],
                    singleline_input(&mut self.center_ui.search).hint_text("搜索设备名称或系统"),
                );
            });
        });
        ui.add_space(18.0);
        self.alert(ui);
        if let Some(error) = &self.catalog_error {
            ui.colored_label(AMBER, "完整设备清单读取失败，点击刷新重试")
                .on_hover_text(error);
        }
        let Some(catalog) = &self.catalog else {
            self.empty_state(
                ui,
                if self.devices.is_some() {
                    "正在读取完整清单…"
                } else {
                    "登录后查看全部设备"
                },
                "",
            );
            return;
        };
        let query = self.center_ui.search.trim().to_lowercase();
        let mut picked = None;
        let (mut desktops, mut virtuals, mut pending) = (Vec::new(), Vec::new(), Vec::new());
        for device in &catalog.groups.desktop_devices {
            match catalog.virtual_status(&device.device_id) {
                Some(true) => virtuals.push(device),
                Some(false) => desktops.push(device),
                None => pending.push(device),
            }
        }
        egui::ScrollArea::vertical()
            .id_salt("account-catalog")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (group, devices) in [
                    ("电脑", desktops),
                    ("虚拟设备", virtuals),
                    (
                        "手机 / 平板",
                        catalog.groups.mobile_devices.iter().collect(),
                    ),
                    ("电视", catalog.groups.tv_devices.iter().collect()),
                    ("待识别", pending),
                ] {
                    if devices.is_empty() {
                        continue;
                    }
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(format!("{group}  {}", devices.len()))
                            .size(16.0)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    for device in devices {
                        if !query.is_empty()
                            && !device.alias.to_lowercase().contains(&query)
                            && !device.platform_label().to_lowercase().contains(&query)
                        {
                            continue;
                        }
                        let note = if device.device_id == catalog.groups.current_device_id {
                            format!("{group} · 本机")
                        } else {
                            group.to_owned()
                        };
                        if ui
                            .push_id(&device.device_id, |ui| {
                                device_row(
                                    ui,
                                    &note,
                                    device,
                                    self.selected_device_id.as_deref()
                                        == Some(device.device_id.as_str()),
                                    false,
                                    None,
                                    false,
                                )
                            })
                            .inner
                            .is_some()
                        {
                            picked = Some(device.device_id.clone());
                        }
                    }
                }
            });
        if let Some(id) = picked {
            self.open_details(id);
        }
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("连接设置").size(25.0).strong());
        ui.add_space(18.0);
        self.alert(ui);
        egui::ScrollArea::vertical()
            .id_salt("center-settings-scroll")
            .show(ui, |ui| {
                ui.set_max_width(740.0);
                section(ui, "画面与连接");
                form_row(ui, "串流帧率", "以远端实际刷新率为准", |ui| {
                    egui::ComboBox::from_id_salt("center-fps")
                        .width(238.0)
                        .selected_text(self.media.frame_rate.label(self.local_display))
                        .show_ui(ui, |ui| {
                            for choice in FrameRateChoice::available(self.local_display) {
                                ui.selectable_value(
                                    &mut self.media.frame_rate,
                                    choice,
                                    choice.label(self.local_display),
                                );
                            }
                        });
                });
                form_row(
                    ui,
                    "视频编码",
                    "自动选择双方支持的编码",
                    |ui| {
                        egui::ComboBox::from_id_salt("center-codec")
                            .width(238.0)
                            .selected_text(self.media.codec.label())
                            .show_ui(ui, |ui| {
                                for choice in [
                                    CodecPreference::Auto,
                                    CodecPreference::H265,
                                    CodecPreference::H264,
                                ] {
                                    ui.selectable_value(
                                        &mut self.media.codec,
                                        choice,
                                        choice.label(),
                                    );
                                }
                            });
                    },
                );
                form_row(ui, "解码方式", "仅影响本机播放", |ui| {
                    egui::ComboBox::from_id_salt("center-decoder")
                        .width(238.0)
                        .selected_text(if self.media.hardware_decode {
                            "优先硬件解码"
                        } else {
                            "软件解码"
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.media.hardware_decode,
                                true,
                                "优先硬件解码",
                            );
                            ui.selectable_value(&mut self.media.hardware_decode, false, "软件解码");
                        });
                });
                form_row(
                    ui,
                    "连接线路",
                    "自动模式支持直连与中转切换",
                    |ui| {
                        egui::ComboBox::from_id_salt("center-route")
                            .width(238.0)
                            .selected_text(self.media.transport.label())
                            .show_ui(ui, |ui| {
                                for choice in [
                                    TransportChoice::Auto,
                                    TransportChoice::P2p,
                                    TransportChoice::Relay,
                                ] {
                                    ui.selectable_value(
                                        &mut self.media.transport,
                                        choice,
                                        choice.label(),
                                    );
                                }
                            });
                    },
                );
                section(ui, "本机");
                ui.label(format!(
                    "显示器  {} × {}  ·  {} Hz",
                    self.local_display.width,
                    self.local_display.height,
                    self.local_display.refresh_hz
                ));
                ui.add_space(8.0);
                egui::CollapsingHeader::new("账号中的虚拟设备").show(ui, |ui| {
                    if let Some(device) = self
                        .devices
                        .as_ref()
                        .map(|list| list.current_device.clone())
                    {
                        ui.label(display_alias(&device));
                        ui.label(RichText::new(&device.device_id).monospace().color(MUTED));
                        ui.label(
                            RichText::new("已关闭本机被控接入（仅内部使用）")
                                .small()
                                .color(MUTED),
                        );
                    } else {
                        ui.label(RichText::new("登录后可查看").color(MUTED));
                    }
                });
                egui::CollapsingHeader::new("诊断信息").show(ui, |ui| {
                    for (label, value) in &self.diagnostics.rows {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(label).color(MUTED));
                            ui.label(value);
                        });
                    }
                    for value in &self.diagnostics.graphics {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("渲染设备").color(MUTED));
                            ui.label(value);
                        });
                    }
                    ui.add_space(8.0);
                    ui.label("解码器配置检测");
                    if ui
                        .add_enabled(
                            !self.diagnostics.busy() && self.active_session.is_none(),
                            egui::Button::new(if self.diagnostics.busy() {
                                "正在探测…"
                            } else {
                                "检测本机解码器"
                            }),
                        )
                        .clicked()
                    {
                        self.diagnostics.probe(self.local_display, self.media);
                    }
                    if let Some(rows) = &self.diagnostics.probe {
                        for (codec, value) in rows {
                            ui.label(format!("{codec}   {value}"));
                        }
                    }
                    if let Some(info) = self.active_session.as_ref().and_then(|s| s.owner.info()) {
                        ui.add_space(8.0);
                        ui.label("当前观看");
                        for (label, value) in [
                            ("本机解码器", info.decoder),
                            ("接收码流", info.video_format),
                            ("连接线路", info.connection),
                            ("远端编码器", info.remote_encoder),
                            ("远端采集", info.remote_capture),
                        ] {
                            ui.label(format!(
                                "{label}   {}",
                                if value.is_empty() {
                                    "等待会话建立"
                                } else {
                                    &value
                                }
                            ));
                        }
                    }
                    ui.separator();
                    let path = std::path::absolute(&self.log_file)
                        .unwrap_or_else(|_| self.log_file.clone());
                    ui.label("日志文件");
                    ui.add(
                        egui::Label::new(
                            RichText::new(path.display().to_string())
                                .monospace()
                                .color(MUTED),
                        )
                        .wrap(),
                    );
                });
            });
    }

    pub(super) fn draw_dialogs(&mut self, ctx: &egui::Context) {
        if self.needs_login() {
            self.logout_confirmation = false;
            return;
        }
        if self.center_ui.edit.is_some() {
            self.device_edit_dialog(ctx);
        } else if self.center_ui.details_open {
            self.device_details(ctx);
        }
        if self.logout_confirmation {
            self.logout_dialog(ctx);
        }
        if self.center_ui.edit.is_none()
            && !self.center_ui.details_open
            && !self.logout_confirmation
        {
            self.assist_dialogs(ctx);
        }
    }

    fn device_details(&mut self, ctx: &egui::Context) {
        let Some(device) = self.selected_device().cloned() else {
            self.center_ui.details_open = false;
            return;
        };
        let mut close = false;
        let mut connect = false;
        let mut edit = None;
        let mut exit_account = false;
        let current = self
            .devices
            .as_ref()
            .is_some_and(|g| g.current_device.device_id == device.device_id);
        let owned = self.catalog.as_ref().is_some_and(|c| {
            c.groups
                .entries()
                .any(|(_, d)| d.device_id == device.device_id)
        });
        let response = egui::Modal::new(egui::Id::new("center-device-details"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(500.0_f32.min(ctx.content_rect().width() - 60.0));
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [ui.available_width() - 40.0, 32.0],
                        egui::Label::new(RichText::new(display_alias(&device)).size(21.0).strong())
                            .truncate(),
                    )
                    .on_hover_text(display_alias(&device));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        close = icon_button(ui, Icon::Close, "关闭").clicked();
                    });
                });
                ui.add_space(16.0);
                egui::Grid::new("center-device-detail-grid")
                    .num_columns(2)
                    .min_row_height(24.0)
                    .spacing([30.0, 10.0])
                    .show(ui, |ui| {
                        for (label, value) in [
                            ("状态", device.status_label().to_owned()),
                            ("系统", device.platform_label()),
                            (
                                "版本",
                                if device.version_name.is_empty() {
                                    "未知".into()
                                } else {
                                    device.version_name.clone()
                                },
                            ),
                            ("设备 ID", device.device_id.clone()),
                        ] {
                            ui.label(RichText::new(label).color(MUTED));
                            ui.label(value);
                            ui.end_row();
                        }
                    });
                ui.add_space(20.0);
                ui.separator();
                ui.label(RichText::new("硬件").color(MUTED));
                egui::ScrollArea::vertical()
                    .id_salt("hardware-detail-scroll")
                    .max_height((ctx.content_rect().height() - 390.0).clamp(100.0, 340.0))
                    .show(ui, |ui| {
                        match self.extra_details.get(&device.device_id).or_else(|| {
                            self.catalog
                                .as_ref()
                                .and_then(|c| c.details.get(&device.device_id))
                                .map(|d| &d.value)
                        }) {
                            Some(value) => match value {
                                Ok(detail) if !detail.details.is_empty() => {
                                    for (label, value) in &detail.details {
                                        if label == "名称" && value == &device.alias {
                                            continue;
                                        }
                                        ui.label(RichText::new(label).small().color(MUTED));
                                        ui.add(
                                            egui::Label::new(if value.trim().is_empty() {
                                                "未提供"
                                            } else {
                                                value
                                            })
                                            .wrap(),
                                        );
                                        ui.add_space(4.0);
                                    }
                                }
                                Ok(_) => {
                                    ui.label("暂无信息");
                                }
                                Err(error) => {
                                    ui.colored_label(AMBER, "详情读取失败，可刷新重试")
                                        .on_hover_text(error);
                                }
                            },
                            None => {
                                ui.label("正在读取硬件详情…");
                            }
                        }
                    });
                ui.add_space(12.0);
                ui.add_enabled_ui(!self.mutation_pending && !self.logout_pending, |ui| {
                    ui.horizontal(|ui| {
                        if owned && ui.button("重命名").clicked() {
                            edit = Some(EditAction::Rename);
                        }
                        if current {
                            if ui.button("退出本机账号").clicked() {
                                exit_account = true;
                            }
                        } else if owned
                            && ui.button(RichText::new("从账号移除").color(RED)).clicked()
                        {
                            edit = Some(EditAction::Remove);
                        }
                    });
                });
                if self.mutation_pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在处理设备操作…");
                    });
                }
                let issue = if self.active_session.is_some() {
                    Some("请先关闭当前观看窗口".into())
                } else {
                    connectability_error(&device).err()
                };
                if self.is_viewing_target(&device.device_id)
                    && let Some(issue) = &issue
                {
                    ui.label(RichText::new(issue).color(MUTED));
                }
                if self.is_viewing_target(&device.device_id) {
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        connect = ui
                            .add_enabled(!self.logout_pending && issue.is_none(), primary("连接"))
                            .clicked();
                    });
                }
            });
        if connect {
            self.start_viewer();
            self.center_ui.details_open = false;
        } else if close || response.should_close() {
            self.center_ui.details_open = false;
        }
        if let Some(action) = edit {
            self.center_ui.edit = Some(DeviceEdit {
                alias: device.alias.clone(),
                device,
                action,
            });
        }
        if exit_account {
            self.center_ui.close_details();
            self.logout();
        }
    }

    fn device_edit_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut edit) = self.center_ui.edit.take() else {
            return;
        };
        let mut commit = false;
        let mut cancel = false;
        let rename = matches!(edit.action, EditAction::Rename);
        let response = egui::Modal::new(egui::Id::new("edit-account-device"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(450.0);
                ui.label(
                    RichText::new(if rename {
                        "重命名设备"
                    } else {
                        "从账号移除设备？"
                    })
                    .size(21.0)
                    .strong(),
                );
                ui.add_space(12.0);
                ui.label(display_alias(&edit.device));
                ui.label(
                    RichText::new(&edit.device.device_id)
                        .monospace()
                        .small()
                        .color(MUTED),
                );
                ui.add_space(12.0);
                if rename {
                    ui.add_sized(
                        [ui.available_width(), 36.0],
                        singleline_input(&mut edit.alias).hint_text("设备名称"),
                    );
                    if let Some(catalog) = &self.catalog
                        && edit.device.device_id == catalog.groups.current_device_id
                        && ui
                            .button(format!("使用短名  {}", catalog.suggested_name))
                            .clicked()
                    {
                        edit.alias = catalog.suggested_name.clone();
                    }
                } else {
                    if self.active_session.as_ref().is_some_and(|s| {
                        s.device_id
                            .as_ref()
                            .is_none_or(|id| id == &edit.device.device_id)
                    }) {
                        ui.colored_label(AMBER, "当前观看将结束。");
                    }
                }
                ui.add_space(18.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    commit = ui
                        .add_enabled(
                            !self.mutation_pending
                                && (!rename
                                    || (!edit.alias.trim().is_empty()
                                        && edit.alias != edit.device.alias
                                        && !edit.alias.chars().any(char::is_control))),
                            if rename {
                                primary("保存名称")
                            } else {
                                egui::Button::new(RichText::new("确认移除").color(Color32::WHITE))
                                    .fill(Color32::from_rgb(161, 56, 67))
                            },
                        )
                        .clicked();
                    cancel = ui.button("取消").clicked();
                });
            });
        if commit {
            let change = match edit.action {
                EditAction::Rename => DeviceMutation::Rename {
                    id: edit.device.device_id,
                    alias: edit.alias.trim().to_owned(),
                },
                EditAction::Remove => DeviceMutation::Remove {
                    id: edit.device.device_id,
                },
            };
            self.center_ui.details_open = false;
            self.queue_mutation(change);
        } else if !cancel && !response.should_close() {
            self.center_ui.edit = Some(edit);
        }
    }

    fn login_page(&mut self, root: &mut egui::Ui) {
        let locked = self.login_restoring
            || self.logout_pending
            || self.active_session.is_some()
            || self.mutation_pending;
        // First restore saved credentials. Once the login page is idle,
        // request QR automatically; failures require an explicit refresh.
        if !locked && !self.qr_running && self.login_qr.is_none() && self.login_error.is_none() {
            self.begin_login();
        }
        let mut qr_action = QrAction::None;
        let mut phone_action = PhoneAction::default();
        login_surface(root, |ui, method| match method {
            LoginMethod::Qr => {
                qr_action = qr_form(
                    ui,
                    self.login_qr.as_ref(),
                    self.login_restoring || self.qr_running,
                    !locked,
                    &self.login_status,
                    self.login_error.as_deref(),
                );
            }
            LoginMethod::Phone => {
                let running = self.phone.sending || self.phone.submitting;
                phone_action =
                    phone_form(ui, &mut self.phone, locked || running, running && !locked);
            }
        });
        if phone_action.cancel {
            self.cancel_phone_login();
        } else if qr_action == QrAction::Start {
            self.begin_login();
        } else if phone_action.send {
            self.request_sms_code();
        } else if phone_action.submit {
            self.submit_sms_login();
        }
    }
}

#[derive(Default)]
struct PhoneAction {
    send: bool,
    submit: bool,
    cancel: bool,
}

fn phone_form(
    ui: &mut egui::Ui,
    phone: &mut PhoneForm,
    busy: bool,
    can_cancel: bool,
) -> PhoneAction {
    let before = phone.contact().ok();
    let mut action = PhoneAction::default();
    ui.allocate_ui_with_layout(vec2(320.0, 0.0), egui::Layout::top_down(Align::Min), |ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
        ui.label(RichText::new("手机号").color(MUTED));
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [64.0, 40.0],
                    singleline_input(&mut phone.country)
                        .hint_text("+86")
                        .char_limit(6)
                        .horizontal_align(Align::Center)
                        .margin(egui::Margin::symmetric(8, 8)),
                );
                ui.add_sized(
                    [248.0, 40.0],
                    singleline_input(&mut phone.mobile)
                        .hint_text("请输入手机号")
                        .char_limit(24),
                );
            });
        });
        if phone.contact().ok() != before {
            phone.code.clear();
            phone.status.clear();
            phone.error = None;
        }
        ui.add_space(14.0);
        ui.label(RichText::new("验证码").color(MUTED));
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                let response = ui.add_sized(
                    [184.0, 40.0],
                    singleline_input(&mut phone.code)
                        .hint_text("6位短信验证码")
                        .char_limit(6),
                );
                if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    action.submit = phone.can_submit();
                }
            });
            let remaining = phone.remaining();
            let label = if phone.sending {
                "发送中…".into()
            } else if remaining > 0 {
                format!("{remaining}秒后重发")
            } else {
                "获取验证码".into()
            };
            action.send = ui
                .add_enabled(
                    !busy && remaining == 0 && phone.agreed && phone.contact().is_ok(),
                    login_button(&label).min_size(vec2(128.0, 40.0)),
                )
                .clicked();
        });
        ui.add_space(12.0);
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.spacing_mut().interact_size.y = 20.0;
                ui.checkbox(
                    &mut phone.agreed,
                    RichText::new("我已阅读并同意").size(12.0),
                );
                ui.hyperlink_to(RichText::new("用户协议").size(12.0), login::sms::TERMS_URL);
                ui.label(RichText::new("和").size(12.0));
                ui.hyperlink_to(
                    RichText::new("隐私政策").size(12.0),
                    login::sms::PRIVACY_URL,
                );
            });
        });
        ui.add_space(18.0);
        action.submit |= ui
            .add_enabled_ui(!busy && phone.can_submit(), |ui| {
                ui.add_sized(
                    [320.0, 42.0],
                    login_button(if busy && !phone.sending {
                        "正在登录…"
                    } else {
                        "登录"
                    })
                    .fill(BLUE)
                    .stroke(Stroke::NONE),
                )
            })
            .inner
            .clicked();
        ui.add_space(6.0);
        let message = if phone.status.is_empty() {
            " "
        } else {
            phone.status.as_str()
        };
        let response = ui.add(
            egui::Label::new(
                RichText::new(message)
                    .size(13.0)
                    .color(if phone.error.is_some() { AMBER } else { MUTED }),
            )
            .truncate(),
        );
        if let Some(error) = &phone.error {
            response.on_hover_text(error.as_str());
        }
        if can_cancel {
            ui.add_space(8.0);
            action.cancel = ui
                .add_sized([320.0, 28.0], login_button("取消").frame(false))
                .clicked();
        }
    });
    action
}

impl DeviceCenterApp {
    fn logout_dialog(&mut self, ctx: &egui::Context) {
        let mut confirm = false;
        let mut cancel = false;
        let response = egui::Modal::new(egui::Id::new("center-logout"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(410.0);
                ui.label(RichText::new("退出登录？").size(21.0).strong());
                ui.add_space(14.0);
                ui.label("当前观看将结束，本虚拟设备也会从账号中移除。");
                ui.add_space(24.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    confirm = ui
                        .add(
                            egui::Button::new(RichText::new("退出登录").color(Color32::WHITE))
                                .fill(Color32::from_rgb(170, 62, 72))
                                .min_size(vec2(100.0, 34.0)),
                        )
                        .clicked();
                    cancel = ui.button("取消").clicked();
                });
            });
        if confirm {
            self.logout_confirmation = false;
            self.logout_pending = true;
            self.cancel_login();
            self.stop_viewer();
            self.status = StatusMessage::info("正在结束观看并退出账号…");
        } else if cancel || response.should_close() {
            self.logout_confirmation = false;
        }
    }
}
