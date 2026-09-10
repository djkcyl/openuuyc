use super::*;
use crate::assist::{
    AssistRequest, ControlMode, SavedDevice, SavedKind, SavedLists, normalize_connect_id,
};

#[derive(Default)]
pub(super) struct AssistUi {
    pub connect_id: String,
    pub direct_code: String,
    pub connect_error: Option<(String, Instant)>,
    pub code: String,
    pub password_prompt: Option<PendingAssist>,
    pub lists: Option<SavedLists>,
    pub loading: bool,
    pub busy: bool,
    pub querying: bool,
    pub sequence: u64,
    pub message: String,
    pub message_until: Option<Instant>,
    pub error_dialog: Option<String>,
    pub last_list_error: Option<String>,
    pub favorite_editor: Option<FavoriteEditor>,
    pub delete_prompt: Option<DeletePrompt>,
}
pub(super) struct PendingAssist {
    pub id: String,
    pub mode: String,
    pub focus: bool,
    pub publisher_id: Option<String>,
}
pub(super) struct FavoriteEditor {
    pub id: String,
    pub remark: String,
    pub editing: bool,
    pub code: String,
    pub code_changed: bool,
}
pub(super) enum DeletePrompt {
    Device(SavedKind, SavedDevice),
    Recent,
}

pub(super) enum AssistOperation {
    QueryMode {
        id: String,
        code: String,
        publisher_id: Option<String>,
    },
    Save {
        id: String,
        remark: String,
        code: Option<String>,
    },
    Delete {
        kind: SavedKind,
        publisher_id: String,
    },
    ClearRecent,
}
impl AssistOperation {
    pub fn is_query(&self) -> bool {
        matches!(self, Self::QueryMode { .. })
    }
}
pub(super) enum AssistResult {
    Mode {
        id: String,
        mode: ControlMode,
        code: String,
        publisher_id: Option<String>,
    },
    Updated(&'static str),
}

pub(super) fn operation_error(error: anyhow::Error) -> String {
    if let Some(failure) = error.downcast_ref::<crate::api::ApiFailure>() {
        let message = if failure.message.trim().is_empty() {
            "请求未成功，请稍后重试"
        } else {
            failure.message.trim()
        };
        format!("{message}（{}）", failure.code)
    } else {
        error.to_string()
    }
}

impl AssistUi {
    pub fn fail(&mut self, message: impl Into<String>) {
        self.message.clear();
        self.message_until = None;
        self.error_dialog = Some(message.into());
    }

    pub fn list_failure(&mut self, message: String) {
        if self.last_list_error.as_ref() != Some(&message) {
            self.error_dialog = Some(message.clone());
        }
        self.last_list_error = Some(message);
    }
}
pub(super) async fn execute(
    client: Arc<AuthenticatedClient>,
    operation: AssistOperation,
) -> Result<AssistResult> {
    Ok(match operation {
        AssistOperation::QueryMode {
            id,
            code,
            publisher_id,
        } => {
            let mode = client.assist_mode(&id).await?;
            let (code, publisher_id) =
                if mode.can_remote_control && mode.control_mode != "by_confirmation" {
                    client
                        .resolve_assist_code(id.clone(), code, publisher_id)
                        .await?
                } else {
                    (String::new(), publisher_id)
                };
            AssistResult::Mode {
                id,
                mode,
                code,
                publisher_id,
            }
        }
        AssistOperation::Save { id, remark, code } => {
            client.save_assist_favorite(&id, &remark, code).await?;
            AssistResult::Updated("收藏已更新")
        }
        AssistOperation::Delete { kind, publisher_id } => {
            client.delete_assist_saved(kind, &publisher_id).await?;
            AssistResult::Updated("记录已移除")
        }
        AssistOperation::ClearRecent => {
            client.clear_assist_recent().await?;
            AssistResult::Updated("最近连接已清空")
        }
    })
}

impl DeviceCenterApp {
    pub(super) fn request_assist_refresh(&mut self) {
        if self.logout_pending || self.assist.loading {
            return;
        }
        if self.worker.commands.send(GuiCommand::RefreshAssist).is_ok() {
            self.assist.loading = true;
        }
    }
    pub(super) fn request_assist_connect(&mut self, value: String, code: String) {
        if self.assist.password_prompt.is_some() {
            return;
        }
        self.assist.connect_error = None;
        if self.active_session.is_some() {
            self.assist.fail("请先关闭当前观看窗口");
            return;
        }
        match normalize_connect_id(&value) {
            Ok(id) => {
                self.assist.connect_id = id.clone();
                self.assist.direct_code.clear();
                self.assist.code.clear();
                let publisher_id = self
                    .assist
                    .lists
                    .as_ref()
                    .and_then(|l| {
                        l.favorites
                            .iter()
                            .chain(&l.recent)
                            .find(|d| d.connect_id == id)
                    })
                    .map(|d| d.publisher_device_id.clone());
                self.request_assist_operation(AssistOperation::QueryMode {
                    id,
                    code,
                    publisher_id,
                });
            }
            Err(error) => {
                self.assist.connect_error =
                    Some((error.to_string(), Instant::now() + Duration::from_secs(3)));
            }
        }
    }
    pub(super) fn request_assist_operation(&mut self, operation: AssistOperation) {
        if self.assist.busy || self.logout_pending || self.login_running || self.mutation_pending {
            return;
        }
        self.assist.sequence = self.assist.sequence.wrapping_add(1);
        self.assist.querying = operation.is_query();
        self.assist.message = if self.assist.querying {
            "正在检查对端验证方式…"
        } else {
            "正在更新记录…"
        }
        .into();
        self.assist.message_until = None;
        self.assist.error_dialog = None;
        self.assist.busy = self
            .worker
            .commands
            .send(GuiCommand::AssistOperation {
                generation: self.login_generation,
                sequence: self.assist.sequence,
                operation,
            })
            .is_ok();
        if !self.assist.busy {
            self.assist.fail("设备后台服务不可用");
        }
    }
    pub(super) fn cancel_assist_check(&mut self) {
        if !self.assist.querying {
            return;
        }
        self.assist.sequence = self.assist.sequence.wrapping_add(1);
        self.assist.busy = false;
        self.assist.querying = false;
        self.assist.message.clear();
        let _ = self.worker.commands.send(GuiCommand::CancelAssistCheck);
    }
    pub(super) fn finish_assist_operation(
        &mut self,
        result: std::result::Result<AssistResult, String>,
    ) {
        self.assist.busy = false;
        self.assist.querying = false;
        match result {
            Ok(AssistResult::Mode {
                id,
                mode,
                code,
                publisher_id,
            }) => {
                self.assist.message.clear();
                if !mode.can_remote_control {
                    self.assist.fail("对端当前不允许远程协助");
                    return;
                }
                let pending = PendingAssist {
                    id,
                    mode: mode.control_mode,
                    focus: true,
                    publisher_id,
                };
                if pending.mode == "by_confirmation" || !code.is_empty() {
                    self.launch_assist(pending, code);
                } else {
                    self.assist.code.clear();
                    self.assist.password_prompt = Some(pending);
                }
            }
            Ok(AssistResult::Updated(message)) => {
                self.assist.message = message.into();
                self.assist.message_until = Some(Instant::now() + Duration::from_secs(3));
                self.request_assist_refresh();
            }
            Err(message) => {
                self.assist.fail(message);
            }
        }
    }
    pub(super) fn launch_assist(&mut self, pending: PendingAssist, code: String) {
        if self.active_session.is_some() || self.logout_pending || self.mutation_pending {
            return;
        }
        let mut request = match AssistRequest::new(&pending.id, code) {
            Ok(request) => request,
            Err(error) => {
                self.assist.fail(error.to_string());
                return;
            }
        };
        request.control_mode = Some(pending.mode);
        let saved = self.assist.lists.as_ref().and_then(|lists| {
            lists
                .favorites
                .iter()
                .chain(&lists.recent)
                .find(|d| d.connect_id == pending.id)
        });
        let alias = saved
            .map(|d| d.title().to_owned())
            .unwrap_or_else(|| format!("远程协助 {}", pending.id));
        let publisher_id = pending.publisher_id;
        request.expected_publisher_id = publisher_id.clone();
        self.spawn_viewer(alias, publisher_id, Some(request));
    }
}
