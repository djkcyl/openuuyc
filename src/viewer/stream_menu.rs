//! Viewer-only stream settings UI. Protocol and budget decisions stay in stream_control.
use egui::{Align, Color32, FontId, RichText, Sense, Stroke, vec2};

use crate::media::FrameRateChoice;
use crate::stream_control::{
    AdaptiveBitrateSnapshot, BudgetPhase, MAX_CUSTOM_BITRATE_MBPS, StreamControlHandle,
    StreamControlSettings, StreamControlSnapshot, StreamQuality,
};

const WIDTH: f32 = 280.0;
const TEXT: Color32 = Color32::from_rgb(222, 225, 231);
const MUTED: Color32 = Color32::from_rgb(142, 150, 164);
const ACCENT: Color32 = Color32::from_rgb(79, 139, 230);
const LINE: Color32 = Color32::from_rgb(45, 51, 61);
const HOVER: Color32 = Color32::from_rgb(37, 43, 53);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Quality,
    Custom,
    Adaptive,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Quality => "画质",
            Self::Custom => "自定义码率",
            Self::Adaptive => "自适应码率",
        }
    }
}

#[derive(Default)]
pub(super) struct StreamControlUi {
    pub(super) open: bool,
    pub(super) budget_notice: Option<String>,
    page: Page,
    settings: Option<StreamControlSettings>,
    dirty: bool,
    local_error: Option<String>,
}

enum Action {
    Apply,
    Adopt,
    Reassess,
}

#[derive(Clone, Copy)]
enum Icon {
    Back,
    Close,
    Info,
    Retry,
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, hint: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    if response.hovered() && ui.is_enabled() {
        ui.painter().rect_filled(rect, 4.0, HOVER);
    }
    let center = rect.center();
    let stroke = Stroke::new(1.3, if response.hovered() { TEXT } else { MUTED });
    let point = |x, y| center + vec2(x, y);
    match icon {
        Icon::Back => {
            ui.painter()
                .line_segment([point(2.5, -5.0), point(-2.5, 0.0)], stroke);
            ui.painter()
                .line_segment([point(-2.5, 0.0), point(2.5, 5.0)], stroke);
        }
        Icon::Close => {
            ui.painter()
                .line_segment([point(-3.5, -3.5), point(3.5, 3.5)], stroke);
            ui.painter()
                .line_segment([point(3.5, -3.5), point(-3.5, 3.5)], stroke);
        }
        Icon::Info => {
            ui.painter().circle_stroke(center, 6.0, stroke);
            ui.painter()
                .circle_filled(point(0.0, -2.5), 0.8, stroke.color);
            ui.painter()
                .line_segment([point(0.0, 0.0), point(0.0, 3.0)], stroke);
        }
        Icon::Retry => {
            let points = (0..=20)
                .map(|step| {
                    let angle = 0.25 + step as f32 / 20.0 * 5.0;
                    center + vec2(angle.cos(), angle.sin()) * 5.0
                })
                .collect::<Vec<_>>();
            ui.painter().add(egui::Shape::line(points, stroke));
            ui.painter()
                .line_segment([point(4.8, -2.4), point(4.8, 1.3)], stroke);
            ui.painter()
                .line_segment([point(1.2, 1.3), point(4.8, 1.3)], stroke);
        }
    }
    response.on_hover_text(hint)
}

fn separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0, LINE),
    );
}

fn menu_row(
    ui: &mut egui::Ui,
    label: &str,
    detail: &str,
    selected: bool,
    enabled: bool,
    more: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), 32.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if selected || (response.hovered() && enabled) {
        ui.painter().rect_filled(
            rect,
            4.0,
            if selected {
                Color32::from_rgb(31, 47, 67)
            } else {
                HOVER
            },
        );
    }
    let color = if enabled {
        TEXT
    } else {
        Color32::from_gray(89)
    };
    if selected {
        let origin = rect.left_center() + vec2(13.0, 0.0);
        let stroke = Stroke::new(1.5, ACCENT);
        ui.painter()
            .line_segment([origin + vec2(-3.0, 0.0), origin + vec2(-0.5, 2.5)], stroke);
        ui.painter()
            .line_segment([origin + vec2(-0.5, 2.5), origin + vec2(4.5, -3.0)], stroke);
    }
    ui.painter().text(
        rect.left_center() + vec2(28.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        color,
    );
    ui.painter().text(
        rect.right_center() - vec2(if more { 26.0 } else { 10.0 }, 0.0),
        egui::Align2::RIGHT_CENTER,
        detail,
        FontId::proportional(11.5),
        if enabled {
            MUTED
        } else {
            Color32::from_gray(78)
        },
    );
    if more {
        let center = rect.right_center() - vec2(12.0, 0.0);
        let stroke = Stroke::new(1.1, MUTED);
        ui.painter()
            .line_segment([center + vec2(-2.0, -3.5), center + vec2(1.5, 0.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(1.5, 0.0), center + vec2(-2.0, 3.5)], stroke);
    }
    response
}

fn switch(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(34.0, 20.0), Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            ui.is_enabled(),
            *value,
            "自动调整",
        )
    });
    ui.painter().rect_filled(
        rect,
        10.0,
        if *value {
            ACCENT
        } else {
            Color32::from_rgb(65, 73, 86)
        },
    );
    let center = egui::pos2(
        if *value {
            rect.right() - 10.0
        } else {
            rect.left() + 10.0
        },
        rect.center().y,
    );
    ui.painter()
        .circle_filled(center, 7.0, Color32::from_rgb(234, 237, 243));
    response
}

fn bitrate_editor(ui: &mut egui::Ui, value: &mut u32, adaptive: bool, multi_screen: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(if multi_screen && adaptive {
            "每屏上限"
        } else if multi_screen {
            "每屏码率"
        } else if adaptive {
            "视频码率上限"
        } else {
            "视频码率"
        });
        icon_button(
            ui,
            Icon::Info,
            if multi_screen {
                "同一上限分别应用到各屏幕；实际码率随内容和带宽变化，音频与重传另计。"
            } else if adaptive {
                "仅限制视频编码码率。音频、纠错、重传和协议还会占用额外带宽；不是总网络限速。"
            } else {
                "使用UU的自定义码率设置；实际流量随画面内容和网络变化。"
            },
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("Mbps").size(11.0).color(MUTED));
            changed |= ui
                .add_sized(
                    [60.0, 30.0],
                    egui::DragValue::new(value)
                        .range(1..=MAX_CUSTOM_BITRATE_MBPS)
                        .speed(1.0),
                )
                .changed();
        });
    });
    ui.add_space(6.0);
    changed |= ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            ui.spacing_mut().slider_rail_height = 3.0;
            ui.spacing_mut().interact_size.y = 18.0;
            ui.add(
                egui::Slider::new(value, 1..=MAX_CUSTOM_BITRATE_MBPS)
                    .logarithmic(true)
                    .show_value(false)
                    .trailing_fill(true)
                    .handle_shape(egui::style::HandleShape::Circle),
            )
            .changed()
        })
        .inner;
    ui.horizontal(|ui| {
        ui.label(RichText::new("1").size(10.0).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(MAX_CUSTOM_BITRATE_MBPS.to_string())
                    .size(10.0)
                    .color(MUTED),
            );
        });
    });
    changed
}

fn budget_status(
    ui: &mut egui::Ui,
    budget: &AdaptiveBitrateSnapshot,
    snapshot: &StreamControlSnapshot,
    can_apply: bool,
    action: &mut Option<Action>,
) {
    if let Some(cap) = budget.pending_mbps {
        ui.label(
            RichText::new(format!("正在调整至 {cap} Mbps…"))
                .size(12.0)
                .color(MUTED),
        );
        return;
    }
    if budget.phase == BudgetPhase::Suspended {
        ui.label(
            RichText::new("自动调整已暂停")
                .size(12.0)
                .color(super::warning_color()),
        )
        .on_hover_text(budget.message);
    } else if let Some(cap) = budget.suggested_mbps {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("网络拥堵，建议 {cap} Mbps"))
                    .size(12.0)
                    .color(super::warning_color()),
            )
            .on_hover_text(budget.reference_video_mbps.map_or_else(
                || budget.message.to_owned(),
                |video| {
                    format!(
                        "过载前有效视频约 {video:.1} Mbps，留出约10%余量。该建议不是线路测速结果。"
                    )
                },
            ));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(can_apply, egui::Button::new("采用"))
                    .clicked()
                {
                    *action = Some(Action::Adopt);
                }
            });
        });
    } else if budget.phase == BudgetPhase::Congested {
        ui.label(
            RichText::new("网络拥堵，建议降低上限")
                .size(12.0)
                .color(super::warning_color()),
        )
        .on_hover_text(budget.message);
    }
    if let Some(cap) = budget.applied_mbps
        && cap < snapshot.settings.adaptive_ceiling_mbps
    {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("当前限制  {cap} Mbps"))
                    .size(12.0)
                    .color(MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(can_apply, |ui| {
                    if icon_button(
                        ui,
                        Icon::Retry,
                        "重新评估：再次尝试你设定的上限，可能短暂卡顿",
                    )
                    .clicked()
                    {
                        *action = Some(Action::Reassess);
                    }
                });
            });
        });
    } else if budget.phase == BudgetPhase::Suspended
        && ui
            .add_enabled(can_apply, egui::Button::new("重试"))
            .clicked()
    {
        *action = Some(Action::Reassess);
    }
}

fn menu_style(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    style.override_text_style = Some(egui::TextStyle::Body);
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(13.0));
    style.spacing.item_spacing = vec2(6.0, 3.0);
    style.spacing.button_padding = vec2(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.selection.bg_fill = ACCENT;
    style.visuals.widgets.inactive.bg_fill = HOVER;
    style.visuals.widgets.inactive.weak_bg_fill = HOVER;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(47, 56, 70);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(47, 56, 70);
    style.visuals.widgets.active.bg_fill = ACCENT;
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(31, 47, 67);
    style.visuals.widgets.inactive.corner_radius = 4.0.into();
    style.visuals.widgets.hovered.corner_radius = 4.0.into();
    style.visuals.widgets.active.corner_radius = 4.0.into();
    style.interaction.selectable_labels = false;
}

pub(super) fn show_stream_control_window(
    ctx: &egui::Context,
    handle: &StreamControlHandle,
    state: &mut StreamControlUi,
) {
    let snapshot = handle.snapshot();
    let multi_screen = snapshot.screens.len() > 1;
    state.budget_notice = snapshot.adaptive.as_ref().and_then(|budget| {
        if budget.phase == BudgetPhase::Suspended {
            Some("码率自动调整已暂停，打开设置重试".into())
        } else if let Some(cap) = budget.suggested_mbps {
            Some(format!("网络拥堵，建议 {cap} Mbps"))
        } else {
            budget
                .applied_mbps
                .filter(|cap| *cap < snapshot.settings.adaptive_ceiling_mbps)
                .map(|cap| format!("当前视频限制为 {cap} Mbps"))
        }
    });
    if !state.open {
        state.page = Page::Quality;
        state.settings = None;
        state.dirty = false;
        state.local_error = None;
        return;
    }
    if !state.dirty && state.local_error.is_none() {
        state.settings = Some(snapshot.settings);
    }
    let mut settings = state.settings.unwrap_or(snapshot.settings);
    let mut open = state.open;
    let mut action = None;
    let mut back = false;
    let mut close = false;
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        open = false;
    }
    egui::Window::new("串流画质")
        .id(egui::Id::new("runtime-stream-settings-window"))
        .open(&mut open)
        .collapsible(false)
        .auto_sized()
        .title_bar(false)
        .anchor(egui::Align2::RIGHT_TOP, [-114.0, 52.0])
        .default_width(WIDTH + 24.0)
        .frame(
            egui::Frame::new()
                .fill(Color32::from_rgb(24, 28, 35))
                .stroke(Stroke::new(1.0, LINE))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::same(12)),
        )
        .show(ctx, |ui| {
            menu_style(ui);
            ui.set_width(WIDTH);
            ui.horizontal(|ui| {
                if state.page != Page::Quality {
                    back =
                        icon_button(ui, Icon::Back, "返回画质菜单；未应用的修改会取消").clicked();
                }
                let title = if multi_screen {
                    format!("{} · 全部屏幕", state.page.title())
                } else {
                    state.page.title().to_owned()
                };
                ui.label(RichText::new(title).size(13.0).strong());
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    close = icon_button(ui, Icon::Close, "关闭").clicked();
                });
            });
            ui.add_space(8.0);
            ui.scope(|ui| {
                ui.set_width(WIDTH);
                match state.page {
                    Page::Quality => {
                        for (quality, label, detail) in [
                            (StreamQuality::Auto, "自动（原画）", String::new()),
                            (StreamQuality::Original, "原画", "20M".into()),
                            (StreamQuality::High, "高清", "8M".into()),
                            (StreamQuality::Clear, "清晰", "2M".into()),
                            (
                                StreamQuality::Custom,
                                "自定义码率",
                                format!("{} Mbps", settings.custom_bitrate_mbps),
                            ),
                            (
                                StreamQuality::Adaptive,
                                "自适应码率",
                                format!("≤ {} Mbps", settings.adaptive_ceiling_mbps),
                            ),
                        ] {
                            let more =
                                matches!(quality, StreamQuality::Custom | StreamQuality::Adaptive);
                            let enabled = snapshot.ready
                                && (!more || snapshot.protocol.supports_custom_bitrate());
                            let response = menu_row(
                                ui,
                                label,
                                &detail,
                                settings.quality == quality,
                                enabled,
                                more,
                            );
                            if response.clicked() {
                                if more {
                                    state.page = if quality == StreamQuality::Custom {
                                        Page::Custom
                                    } else {
                                        Page::Adaptive
                                    };
                                }
                                if settings.quality != quality
                                    || state.local_error.is_some()
                                    || snapshot.last_error.is_some()
                                {
                                    settings.quality = quality;
                                    action = Some(Action::Apply);
                                }
                            }
                            if more && !snapshot.protocol.supports_custom_bitrate() {
                                response.on_hover_text("此被控端不支持自定义码率");
                            }
                        }
                        ui.add_space(11.0);
                        separator(ui);
                        ui.add_space(9.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("帧率").size(11.0).color(MUTED))
                                .on_hover_text(
                                    snapshot
                                        .last_notice
                                        .as_deref()
                                        .unwrap_or("实际帧率受被控端刷新率和画面内容影响"),
                                );
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new("FPS").size(10.0).color(MUTED));
                            });
                        });
                        ui.add_space(4.0);
                        let choices = FrameRateChoice::available(snapshot.local_display)
                            .into_iter()
                            .filter(|choice| *choice != FrameRateChoice::Auto)
                            .collect::<Vec<_>>();
                        let width = (WIDTH - 6.0 * (choices.len().saturating_sub(1)) as f32)
                            / choices.len().max(1) as f32;
                        ui.add_enabled_ui(snapshot.ready, |ui| {
                            ui.horizontal(|ui| {
                                for choice in choices {
                                    if ui
                                        .add_sized(
                                            [width, 30.0],
                                            egui::Button::new(
                                                choice.value(snapshot.local_display).to_string(),
                                            )
                                            .selected(settings.frame_rate == choice),
                                        )
                                        .clicked()
                                        && settings.frame_rate != choice
                                    {
                                        settings.frame_rate = choice;
                                        action = Some(Action::Apply);
                                    }
                                }
                            });
                        });
                        ui.add_space(11.0);
                        separator(ui);
                        ui.add_space(9.0);
                        ui.horizontal(|ui| {
                            ui.label("强制中转");
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                let mut relay = snapshot.network.relay_enabled;
                                let response = ui
                                    .add_enabled_ui(snapshot.network.available, |ui| {
                                        switch(ui, &mut relay)
                                    })
                                    .inner;
                                if response.changed() {
                                    state.local_error = handle
                                        .set_relay_enabled(relay)
                                        .err()
                                        .map(|e| e.to_string());
                                }
                                response.on_hover_text(
                                    snapshot.network.unavailable_reason.unwrap_or(
                                        "仅本次连接生效；关闭后恢复自动选路，不保证一定直连",
                                    ),
                                );
                            });
                        });
                        ui.add_space(9.0);
                        separator(ui);
                        ui.add_space(9.0);
                        let audio = handle.audio();
                        let mut audio_settings = audio.settings();
                        let audio_status = audio.snapshot();
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut audio_settings.muted, "静音");
                            ui.add(
                                egui::Slider::new(&mut audio_settings.volume, 0..=100)
                                    .suffix("%")
                                    .show_value(true),
                            )
                            .on_hover_text(
                                if audio_status.device.is_empty() {
                                    if audio_status.receiving {
                                        "Opus · 48 kHz · 双声道"
                                    } else {
                                        "尚未收到音频"
                                    }
                                } else {
                                    &audio_status.device
                                },
                            );
                        });
                        audio.set_settings(audio_settings);
                        if let Some(error) = audio_status.error {
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(Color32::from_rgb(224, 174, 99), error);
                                if ui.small_button("重试").clicked() {
                                    audio.retry();
                                }
                            });
                        }
                        if snapshot.network.pending {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("正在切换线路…").size(11.0).color(MUTED));
                            });
                        } else if let Some(notice) = snapshot.network.notice {
                            ui.label(RichText::new(notice).size(11.0).color(MUTED));
                        }
                    }
                    Page::Custom | Page::Adaptive => {
                        let adaptive = state.page == Page::Adaptive;
                        ui.add_enabled_ui(snapshot.ready, |ui| {
                            let value = if adaptive {
                                &mut settings.adaptive_ceiling_mbps
                            } else {
                                &mut settings.custom_bitrate_mbps
                            };
                            state.dirty |= bitrate_editor(ui, value, adaptive, multi_screen);
                            if adaptive {
                                ui.add_space(14.0);
                                separator(ui);
                                ui.add_space(9.0);
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label("自动调整");
                                        ui.label(
                                            RichText::new(if settings.stability_priority {
                                                "网络拥堵时降低码率"
                                            } else {
                                                "仅提醒，不自动调整"
                                            })
                                            .size(11.0)
                                            .color(MUTED),
                                        );
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(Align::Center),
                                        |ui| {
                                            state.dirty |=
                                                switch(ui, &mut settings.stability_priority)
                                                    .changed();
                                        },
                                    );
                                });
                            }
                            ui.add_space(16.0);
                            let can_apply = state.dirty
                                || state.local_error.is_some()
                                || snapshot.last_error.is_some();
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(if state.dirty { "尚未应用" } else { "" })
                                        .size(11.0)
                                        .color(MUTED),
                                );
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    if ui
                                        .add_enabled(
                                            can_apply,
                                            egui::Button::new("应用")
                                                .fill(if can_apply { ACCENT } else { HOVER })
                                                .stroke(Stroke::NONE)
                                                .min_size(vec2(76.0, 30.0)),
                                        )
                                        .clicked()
                                    {
                                        action = Some(Action::Apply);
                                    }
                                });
                            });
                            // Keep the Apply target stationary when live
                            // network advice appears or an ACK arrives.
                            if adaptive && let Some(budget) = &snapshot.adaptive {
                                ui.add_space(8.0);
                                budget_status(
                                    ui,
                                    budget,
                                    &snapshot,
                                    !state.dirty
                                        && action.is_none()
                                        && snapshot.pending_sequence.is_none(),
                                    &mut action,
                                );
                            }
                        });
                    }
                }
                if let Some(error) = state
                    .local_error
                    .as_ref()
                    .or(snapshot.last_error.as_ref())
                    .or(snapshot.persistence_error.as_ref())
                    .or(snapshot.network.error.as_ref())
                {
                    ui.add_space(9.0);
                    ui.add(egui::Label::new(
                        RichText::new("设置未生效")
                            .size(11.0)
                            .color(super::bad_color()),
                    ))
                    .on_hover_text(error);
                } else if let Some(waiting) = snapshot.waiting_for {
                    ui.add_space(9.0);
                    ui.label(RichText::new("等待串流就绪…").size(11.0).color(MUTED))
                        .on_hover_text(waiting);
                } else if snapshot.pending_sequence.is_some() && state.page != Page::Adaptive {
                    ui.add_space(9.0);
                    ui.label(RichText::new("正在应用…").size(11.0).color(MUTED));
                }
            });
        });
    if back {
        state.page = Page::Quality;
        state.dirty = false;
        state.local_error = None;
        settings = snapshot.settings;
    } else if let Some(action) = action {
        let result = match action {
            Action::Apply => handle.apply(settings),
            Action::Adopt => handle.adopt_budget_suggestion(),
            Action::Reassess => handle.reassess_budget(),
        };
        match result {
            Ok(sequence) => {
                state.local_error = None;
                state.dirty = false;
                tracing::info!(
                    sequence,
                    ?settings,
                    "runtime stream settings requested from GUI"
                );
            }
            Err(error) => state.local_error = Some(error.to_string()),
        }
    }
    state.settings = Some(settings);
    state.open = open && !close;
}
