use super::*;
use crate::account::assist::{SavedDevice, SavedKind, normalize_connect_id};
use crate::application::app::assist::{DeletePrompt, FavoriteEditor};

impl DeviceCenterApp {
    pub(super) fn assist_page(&mut self, ui: &mut egui::Ui, favorites: bool) {
        if let Some(deadline) = self.assist.message_until {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.assist.message.clear();
                self.assist.message_until = None;
            } else {
                ui.ctx().request_repaint_after(remaining);
            }
        }
        ui.horizontal(|ui| {
            ui.set_min_height(crate::ui::theme::CONTROL_HEIGHT);
            ui.label(
                RichText::new(if favorites {
                    "收藏设备"
                } else {
                    "远程协助"
                })
                .size(crate::ui::theme::TITLE)
                .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled_ui(!self.assist.loading, |ui| {
                        if self.assist.loading {
                            let (rect, response) =
                                ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                            egui::Spinner::new()
                                .size(18.0)
                                .paint_at(ui, rect.shrink(7.0));
                            response.on_hover_text("正在刷新记录…")
                        } else {
                            icon_button(ui, Icon::Refresh, "刷新记录")
                        }
                    })
                    .inner
                    .clicked()
                {
                    self.request_assist_refresh();
                }
                if favorites
                    && ui
                        .add_enabled(!self.assist.busy, login_button("添加收藏"))
                        .clicked()
                {
                    self.assist.favorite_editor = Some(FavoriteEditor {
                        id: String::new(),
                        remark: String::new(),
                        editing: false,
                        code: String::new(),
                        code_changed: false,
                    });
                }
                self.active_view(ui);
            });
        });
        ui.add_space(18.0);
        let available = !self.assist.busy && !self.logout_pending && !self.mutation_pending;
        let mut connect = None;
        if !favorites {
            egui::Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, LINE))
                .corner_radius(crate::ui::theme::PANEL_RADIUS)
                .inner_margin(20)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("伙伴的设备 ID").color(MUTED));
                    ui.add_space(8.0);
                    ui.add_enabled_ui(available && self.active_session.is_none(), |ui| {
                        ui.horizontal(|ui| {
                            let width = ((ui.available_width() - 120.0) * 0.5).clamp(160.0, 280.0);
                            let input = ui.add_sized(
                                [width, 40.0],
                                singleline_input(&mut self.assist.connect_id)
                                    .hint_text("输入9位设备 ID")
                                    .char_limit(24),
                            );
                            if input.changed() {
                                self.assist.connect_error = None;
                                self.assist.direct_code.clear();
                            }
                            let code = ui.add_sized(
                                [width, 40.0],
                                singleline_input(&mut self.assist.direct_code)
                                    .hint_text("设备验证码（可选）")
                                    .char_limit(256),
                            );
                            let enter = (input.has_focus() || code.has_focus())
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let clicked = ui
                                .add_sized(
                                    [104.0, 40.0],
                                    login_button("连接").fill(BLUE).stroke(Stroke::NONE),
                                )
                                .clicked();
                            if enter || clicked {
                                connect = Some((
                                    self.assist.connect_id.clone(),
                                    self.assist.direct_code.clone(),
                                ));
                            }
                        });
                    });
                });
        }
        if let Some((_, deadline)) = &self.assist.connect_error {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.assist.connect_error = None;
            } else {
                ui.ctx().request_repaint_after(remaining);
            }
        }
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "assist-connect-error",
            "远程协助",
            crate::ui::controls::DialogIcon::Warning,
            self.assist
                .connect_error
                .as_ref()
                .map(|(message, _)| message.as_str())
                .filter(|_| !favorites),
        );
        if self.assist.querying {
            if crate::ui::controls::observe_notice_action(
                ui.ctx(),
                "assist-query",
                "检查对端设备",
                crate::ui::controls::DialogIcon::Waiting,
                Some(&self.assist.message),
                "取消连接",
            )
            .is_some()
            {
                self.cancel_assist_check();
            }
        } else {
            crate::ui::controls::clear_notice(ui.ctx(), "assist-query");
            if !self.assist.busy && !self.assist.message.is_empty() {
                crate::ui::controls::notice(
                    ui.ctx(),
                    "assist-result",
                    "远程协助",
                    crate::ui::controls::DialogIcon::Info,
                    std::mem::take(&mut self.assist.message),
                );
                self.assist.message_until = None;
            }
        }
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if favorites {
                    "已收藏"
                } else {
                    "最近连接"
                })
                .size(crate::ui::theme::SECTION)
                .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let has_recent = self
                    .assist
                    .lists
                    .as_ref()
                    .is_some_and(|l| !l.recent.is_empty());
                if !favorites
                    && ui
                        .add_enabled(
                            available && has_recent,
                            login_button("清空记录").frame(false),
                        )
                        .clicked()
                {
                    self.assist.delete_prompt = Some(DeletePrompt::Recent);
                }
            });
        });
        ui.add_space(10.0);
        let kind = if favorites {
            SavedKind::Favorites
        } else {
            SavedKind::Recent
        };
        let mut items = self
            .assist
            .lists
            .as_ref()
            .map(|l| {
                if favorites {
                    l.favorites.clone()
                } else {
                    l.recent.clone()
                }
            })
            .unwrap_or_default();
        items.sort_by(|a, b| {
            let time = |d: &SavedDevice| {
                if favorites {
                    d.favorited_at
                } else {
                    d.last_connected_at
                }
            };
            time(b)
                .cmp(&time(a))
                .then_with(|| a.connect_id.cmp(&b.connect_id))
        });
        if items.is_empty() {
            ui.label(
                RichText::new(if self.assist.loading && self.assist.lists.is_none() {
                    "正在读取记录…"
                } else if self.assist.lists.is_none() && self.assist.last_list_error.is_some() {
                    "记录读取失败，请点击右上角刷新重试"
                } else if favorites {
                    "暂无收藏"
                } else {
                    "暂无最近连接"
                })
                .color(MUTED),
            );
        }
        crate::ui::controls::page_scroll(("assist-saved-list", favorites)).show(ui, |ui| {
            for item in &items {
                ui.push_id(&item.publisher_device_id, |ui| {
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (icon, _) =
                                    ui.allocate_exact_size(vec2(28.0, 42.0), Sense::hover());
                                paint_icon(ui.painter(), icon, Icon::Monitor, MUTED);
                                let text_width = (ui.available_width() - 228.0).max(140.0);
                                ui.allocate_ui_with_layout(
                                    vec2(text_width, 44.0),
                                    egui::Layout::top_down(Align::Min),
                                    |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(item.title())
                                                    .size(crate::ui::theme::BODY),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(item.title());
                                        let time = item
                                            .last_connected_at
                                            .filter(|&t| !favorites && t > 0)
                                            .and_then(|t| chrono::DateTime::from_timestamp(t, 0));
                                        let detail = if let Some(time) = time {
                                            let time = time
                                                .with_timezone(&chrono::Local)
                                                .format("%m-%d %H:%M");
                                            format!("{}  ·  {}", item.connect_id, time)
                                        } else {
                                            item.connect_id.clone()
                                        };
                                        ui.label(
                                            RichText::new(detail)
                                                .size(crate::ui::theme::SMALL)
                                                .color(MUTED),
                                        );
                                    },
                                );
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    let valid = item.validate().is_ok();
                                    if ui
                                        .add_enabled_ui(
                                            available && valid && self.active_session.is_none(),
                                            |ui| ui.add_sized([78.0, 34.0], login_button("连接")),
                                        )
                                        .inner
                                        .clicked()
                                    {
                                        connect = Some((item.connect_id.clone(), String::new()));
                                    }
                                    ui.add_enabled_ui(available && valid, |ui| {
                                        if icon_button(
                                            ui,
                                            Icon::Close,
                                            if favorites {
                                                "取消收藏"
                                            } else {
                                                "移除记录"
                                            },
                                        )
                                        .clicked()
                                        {
                                            self.assist.delete_prompt =
                                                Some(DeletePrompt::Device(kind, item.clone()));
                                        }
                                        if favorites
                                            && icon_button(ui, Icon::Edit, "编辑收藏").clicked()
                                        {
                                            self.assist.favorite_editor = Some(FavoriteEditor {
                                                id: item.connect_id.clone(),
                                                remark: item.remark.clone(),
                                                editing: true,
                                                code: item.saved_code.clone(),
                                                code_changed: false,
                                            });
                                        }
                                        if !favorites {
                                            let response = icon_button(
                                                ui,
                                                Icon::Star,
                                                if item.is_favorite {
                                                    "已收藏"
                                                } else {
                                                    "收藏"
                                                },
                                            );
                                            if item.is_favorite {
                                                paint_icon(
                                                    ui.painter(),
                                                    response.rect,
                                                    Icon::Star,
                                                    AMBER,
                                                );
                                            }
                                            if response.clicked() {
                                                if item.is_favorite {
                                                    self.assist.delete_prompt =
                                                        Some(DeletePrompt::Device(
                                                            SavedKind::Favorites,
                                                            item.clone(),
                                                        ));
                                                } else {
                                                    self.assist.favorite_editor =
                                                        Some(FavoriteEditor {
                                                            id: item.connect_id.clone(),
                                                            remark: item.remark.clone(),
                                                            editing: false,
                                                            code: item.saved_code.clone(),
                                                            code_changed: false,
                                                        });
                                                }
                                            }
                                        }
                                    });
                                });
                            });
                        });
                    ui.separator();
                });
            }
        });
        if let Some((id, code)) = connect {
            self.request_assist_connect(id, code);
        }
    }

    pub(super) fn assist_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(message) = &self.assist.error_dialog {
            let mut close = false;
            let response = egui::Modal::new(egui::Id::new("assist-error"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(400.0);
                    close = crate::ui::controls::dialog_header(
                        ui,
                        "操作未完成",
                        crate::ui::controls::DialogIcon::Error,
                        true,
                    );
                    ui.add(egui::Label::new(message).wrap());
                    close |= crate::ui::controls::dialog_actions(
                        ui,
                        Some(crate::ui::controls::DialogAction::new("知道了")),
                        None,
                    )
                    .0;
                });
            if close || response.should_close() {
                self.assist.error_dialog = None;
            }
            return;
        }
        if let Some(mut pending) = self.assist.password_prompt.take() {
            let mut submit = false;
            let mut cancel = false;
            let response = egui::Modal::new(egui::Id::new("assist-password"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(360.0);
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        "设备验证码",
                        crate::ui::controls::DialogIcon::Info,
                        true,
                    );
                    ui.label(RichText::new(&pending.id).color(MUTED));
                    ui.add_space(18.0);
                    let input = ui.add_sized(
                        [360.0, theme::CONTROL_HEIGHT],
                        singleline_input(&mut self.assist.code)
                            .hint_text("输入对端设备验证码")
                            .char_limit(256),
                    );
                    if pending.focus {
                        input.request_focus();
                        pending.focus = false;
                    }
                    if input.has_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !self.assist.code.is_empty()
                    {
                        submit = true;
                    }
                    let (accept, dismiss) = crate::ui::controls::dialog_actions(
                        ui,
                        Some(
                            crate::ui::controls::DialogAction::new("连接")
                                .enabled(!self.assist.code.is_empty()),
                        ),
                        Some("取消"),
                    );
                    submit |= accept;
                    cancel |= dismiss;
                });
            if submit {
                let code = std::mem::take(&mut self.assist.code);
                self.launch_assist(pending, code);
            } else if cancel || response.should_close() {
                self.assist.code.clear();
            } else {
                self.assist.password_prompt = Some(pending);
            }
        }
        if let Some(mut edit) = self.assist.favorite_editor.take() {
            let mut submit = false;
            let mut cancel = false;
            let response = egui::Modal::new(egui::Id::new("assist-favorite-editor"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(380.0);
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        if edit.editing {
                            "编辑收藏"
                        } else {
                            "添加收藏"
                        },
                        crate::ui::controls::DialogIcon::Edit,
                        true,
                    );
                    ui.label(RichText::new("设备 ID").color(MUTED));
                    ui.add_enabled_ui(!edit.editing, |ui| {
                        if ui
                            .add_sized(
                                [380.0, theme::CONTROL_HEIGHT],
                                singleline_input(&mut edit.id).char_limit(24),
                            )
                            .changed()
                        {
                            edit.code.clear();
                            edit.code_changed = false;
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new("备注").color(MUTED));
                    ui.add_sized(
                        [380.0, theme::CONTROL_HEIGHT],
                        singleline_input(&mut edit.remark)
                            .hint_text("可选")
                            .char_limit(128),
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new("设备验证码").color(MUTED));
                    edit.code_changed |= ui
                        .add_sized(
                            [380.0, theme::CONTROL_HEIGHT],
                            singleline_input(&mut edit.code)
                                .hint_text("可选，仅保存在本机")
                                .char_limit(256),
                        )
                        .changed();
                    let (accept, dismiss) =
                        crate::ui::controls::dialog_actions(
                            ui,
                            Some(crate::ui::controls::DialogAction::new("保存").enabled(
                                normalize_connect_id(&edit.id).is_ok() && !self.assist.busy,
                            )),
                            Some("取消"),
                        );
                    submit = accept;
                    cancel |= dismiss;
                });
            if submit {
                self.request_assist_operation(AssistOperation::Save {
                    id: edit.id,
                    remark: edit.remark,
                    code: edit.code_changed.then_some(edit.code),
                });
            } else if !cancel && !response.should_close() {
                self.assist.favorite_editor = Some(edit);
            }
        }
        if let Some(prompt) = self.assist.delete_prompt.take() {
            let mut submit = false;
            let mut cancel = false;
            let response = egui::Modal::new(egui::Id::new("assist-delete-record"))
                .frame(dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(380.0);
                    let title = match &prompt {
                        DeletePrompt::Recent => "清空最近连接？",
                        DeletePrompt::Device(SavedKind::Recent, _) => "移除这条记录？",
                        DeletePrompt::Device(SavedKind::Favorites, _) => "取消收藏？",
                    };
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        title,
                        crate::ui::controls::DialogIcon::Warning,
                        true,
                    );
                    if let DeletePrompt::Device(_, item) = &prompt {
                        ui.label(item.title());
                        ui.label(RichText::new(&item.connect_id).color(MUTED));
                    }
                    let (accept, dismiss) = crate::ui::controls::dialog_actions(
                        ui,
                        Some(
                            crate::ui::controls::DialogAction::new("确认")
                                .enabled(!self.assist.busy)
                                .danger(true),
                        ),
                        Some("取消"),
                    );
                    submit = accept;
                    cancel |= dismiss;
                });
            if submit {
                let operation = match prompt {
                    DeletePrompt::Recent => AssistOperation::ClearRecent,
                    DeletePrompt::Device(kind, item) => AssistOperation::Delete {
                        kind,
                        publisher_id: item.publisher_device_id,
                    },
                };
                self.request_assist_operation(operation);
            } else if !cancel && !response.should_close() {
                self.assist.delete_prompt = Some(prompt);
            }
        }
    }
}
