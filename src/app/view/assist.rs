use super::*;
use crate::app::assist::{DeletePrompt, FavoriteEditor};
use crate::assist::{SavedDevice, SavedKind, normalize_connect_id};

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
            ui.label(
                RichText::new(if favorites {
                    "收藏设备"
                } else {
                    "远程协助"
                })
                .size(25.0)
                .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled_ui(!self.assist.loading, |ui| {
                        icon_button(ui, Icon::Refresh, "刷新记录")
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
            });
        });
        ui.add_space(18.0);
        self.active_view(ui);
        let available = !self.assist.busy && !self.logout_pending && !self.mutation_pending;
        let mut connect = None;
        if !favorites {
            egui::Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, LINE))
                .corner_radius(8.0)
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
                                    .password(true)
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
        if !favorites && let Some((message, _)) = &self.assist.connect_error {
            ui.add_space(12.0);
            ui.label(RichText::new(message).color(AMBER));
        }
        if !self.assist.message.is_empty() || self.assist.loading {
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                if self.assist.busy || self.assist.loading {
                    ui.spinner();
                }
                ui.label(RichText::new(&self.assist.message).color(MUTED));
                if self.assist.querying && ui.button("取消").clicked() {
                    self.cancel_assist_check();
                }
            });
        }
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if favorites {
                    "已收藏"
                } else {
                    "最近连接"
                })
                .size(16.0)
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
                RichText::new(if self.assist.lists.is_none() {
                    "正在读取记录…"
                } else if favorites {
                    "暂无收藏"
                } else {
                    "暂无最近连接"
                })
                .color(MUTED),
            );
        }
        egui::ScrollArea::vertical()
            .id_salt("assist-saved-list")
            .show(ui, |ui| {
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
                                                    RichText::new(item.title()).size(15.0),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(item.title());
                                            let time = item
                                                .last_connected_at
                                                .filter(|&t| !favorites && t > 0)
                                                .and_then(|t| {
                                                    chrono::DateTime::from_timestamp(t, 0)
                                                });
                                            let detail = if let Some(time) = time {
                                                let time = time
                                                    .with_timezone(&chrono::Local)
                                                    .format("%m-%d %H:%M");
                                                format!("{}  ·  {}", item.connect_id, time)
                                            } else {
                                                item.connect_id.clone()
                                            };
                                            ui.label(RichText::new(detail).size(12.0).color(MUTED));
                                        },
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(Align::Center),
                                        |ui| {
                                            let valid = item.validate().is_ok();
                                            if ui
                                                .add_enabled_ui(
                                                    available
                                                        && valid
                                                        && self.active_session.is_none(),
                                                    |ui| {
                                                        ui.add_sized(
                                                            [78.0, 34.0],
                                                            login_button("连接"),
                                                        )
                                                    },
                                                )
                                                .inner
                                                .clicked()
                                            {
                                                connect =
                                                    Some((item.connect_id.clone(), String::new()));
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
                                                    self.assist.delete_prompt = Some(
                                                        DeletePrompt::Device(kind, item.clone()),
                                                    );
                                                }
                                                if favorites
                                                    && icon_button(ui, Icon::Edit, "编辑收藏")
                                                        .clicked()
                                                {
                                                    self.assist.favorite_editor =
                                                        Some(FavoriteEditor {
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
                                        },
                                    );
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
                    ui.label(RichText::new("操作未完成").size(21.0).strong());
                    ui.add_space(16.0);
                    ui.add(egui::Label::new(message).wrap());
                    ui.add_space(20.0);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        close = ui.add(login_button("知道了").fill(BLUE)).clicked();
                    });
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
                    ui.label(RichText::new("设备验证码").size(21.0).strong());
                    ui.label(RichText::new(&pending.id).color(MUTED));
                    ui.add_space(18.0);
                    let input = ui.add_sized(
                        [360.0, 40.0],
                        singleline_input(&mut self.assist.code)
                            .password(true)
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
                    ui.add_space(18.0);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        submit |= ui
                            .add_enabled(
                                !self.assist.code.is_empty(),
                                login_button("连接").fill(BLUE),
                            )
                            .clicked();
                        cancel = ui.button("取消").clicked();
                    });
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
                    ui.label(
                        RichText::new(if edit.editing {
                            "编辑收藏"
                        } else {
                            "添加收藏"
                        })
                        .size(21.0)
                        .strong(),
                    );
                    ui.add_space(16.0);
                    ui.label(RichText::new("设备 ID").color(MUTED));
                    ui.add_enabled_ui(!edit.editing, |ui| {
                        if ui
                            .add_sized([380.0, 36.0], singleline_input(&mut edit.id).char_limit(24))
                            .changed()
                        {
                            edit.code.clear();
                            edit.code_changed = false;
                        }
                    });
                    ui.add_space(10.0);
                    ui.label(RichText::new("备注").color(MUTED));
                    ui.add_sized(
                        [380.0, 36.0],
                        singleline_input(&mut edit.remark)
                            .hint_text("可选")
                            .char_limit(128),
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new("设备验证码").color(MUTED));
                    edit.code_changed |= ui
                        .add_sized(
                            [380.0, 36.0],
                            singleline_input(&mut edit.code)
                                .password(true)
                                .hint_text("可选，仅保存在本机")
                                .char_limit(256),
                        )
                        .changed();
                    ui.add_space(18.0);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        submit = ui
                            .add_enabled(
                                normalize_connect_id(&edit.id).is_ok() && !self.assist.busy,
                                login_button("保存").fill(BLUE),
                            )
                            .clicked();
                        cancel = ui.button("取消").clicked();
                    });
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
                    ui.label(RichText::new(title).size(21.0).strong());
                    ui.add_space(14.0);
                    if let DeletePrompt::Device(_, item) = &prompt {
                        ui.label(item.title());
                        ui.label(RichText::new(&item.connect_id).color(MUTED));
                    }
                    ui.add_space(18.0);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        submit = ui
                            .add_enabled(
                                !self.assist.busy,
                                login_button("确认").fill(Color32::from_rgb(161, 56, 67)),
                            )
                            .clicked();
                        cancel = ui.button("取消").clicked();
                    });
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
