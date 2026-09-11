use super::*;
use crate::assist::{AssistRequest, JoinReply};

pub(super) struct AssistConnection {
    request: AssistRequest,
    room: Option<JoinReply>,
}
impl AssistConnection {
    pub(super) async fn join(
        &mut self,
        client: &AuthenticatedClient,
        controller_id: &str,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
    ) -> Result<JoinReply> {
        let mut reply = if let Some(previous) = &self.room {
            report_progress(
                reporter,
                3,
                "恢复远程协助会话",
                "正在更新已授权房间的连接凭据",
            );
            let mut reply = cancellable(cancel, client.assist_refetch(&previous.token)).await?;
            if !reply.publisher_device_id.is_empty()
                && !previous.publisher_device_id.is_empty()
                && reply.publisher_device_id != previous.publisher_device_id
            {
                bail!("恢复会话时目标设备发生变化，已停止连接");
            }
            if !reply.share_id.is_empty()
                && !previous.share_id.is_empty()
                && reply.share_id != previous.share_id
            {
                bail!("恢复会话时远程协助身份发生变化，已停止连接");
            }
            if reply.publisher_device_id.is_empty() {
                reply.publisher_device_id = previous.publisher_device_id.clone();
            }
            if reply.device_name.is_empty() {
                reply.device_name = previous.device_name.clone();
            }
            if reply.share_id.is_empty() {
                reply.share_id = previous.share_id.clone();
            }
            reply
        } else {
            authorize(client, &self.request, reporter, cancel).await?
        };
        reply.validate(controller_id)?;
        if self
            .request
            .expected_publisher_id
            .as_ref()
            .is_some_and(|id| id != &reply.publisher_device_id)
        {
            bail!("该记录对应的设备身份发生变化，请刷新记录后重试");
        }
        if reply.device_name.trim().is_empty() {
            reply.device_name = format!("远程协助 {}", self.request.connect_id);
        }
        report_progress(
            reporter,
            3,
            "远程协助已授权",
            format!("{} · {}", reply.device_name, reply.publisher_version_name),
        );
        self.room = Some(reply.clone());
        Ok(reply)
    }

    pub(super) async fn remember_success(&mut self, client: &AuthenticatedClient) -> Result<()> {
        if !self.request.has_code() {
            return Ok(());
        }
        let code = std::mem::take(&mut self.request.connect_code);
        let room = self.room.as_ref().context("没有已连接的远程协助会话")?;
        client
            .remember_assist_code(
                self.request.connect_id.clone(),
                room.publisher_device_id.clone(),
                code,
            )
            .await
    }
}

pub(super) async fn resolve(
    client: Arc<AuthenticatedClient>,
    alias: &str,
    options: ConnectionMediaOptions,
    request: AssistRequest,
    reporter: Option<&ConnectionProgressReporter>,
) -> Result<ResolvedConnection> {
    request.validate()?;
    report_progress(reporter, 1, "验证本地会话", "正在恢复账号与本机设备身份");
    client.account_info().await?;
    let controller_device_id = client.device_id();
    crate::api::validate_device_id(&controller_device_id)?;
    let (display, warning) = match detect_local_display() {
        Ok(display) => (display, None),
        Err(error) => (
            LocalDisplayInfo::FALLBACK,
            Some(format!("本机显示器探测失败：{error}")),
        ),
    };
    let profile = options.resolve(display)?;
    Ok(ResolvedConnection {
        client,
        target_device_id: String::new(),
        controller_device_id,
        profile,
        transport: options.transport,
        summary: ConnectionSummary {
            alias: alias.into(),
            stream_fps: profile.stream_fps,
            codec: profile.codec.label(),
            display_detection_warning: warning,
        },
        assist: Some(AssistConnection {
            request,
            room: None,
        }),
        preferences: None,
        audio_preferences: None,
        target_platform: 0,
    })
}

async fn authorize(
    client: &AuthenticatedClient,
    request: &AssistRequest,
    reporter: Option<&ConnectionProgressReporter>,
    cancel: &CancellationToken,
) -> Result<JoinReply> {
    report_progress(reporter, 2, "检查远程协助", "正在确认对端验证方式");
    let mode = if let Some(mode) = &request.control_mode {
        mode.clone()
    } else {
        let mode = cancellable(cancel, client.assist_mode(&request.connect_id)).await?;
        if !mode.can_remote_control {
            bail!("对端当前不允许远程协助");
        }
        mode.control_mode
    };
    let mut dispatched = false;
    let result = async {
        let response = if mode == "by_confirmation" {
            report_progress(
                reporter,
                3,
                "等待对端确认",
                "请在对端允许本次连接；关闭此窗口可取消",
            );
            dispatched = true;
            cancellable(cancel, client.assist_confirm(&request.connect_id, "")).await?
        } else {
            if !request.has_code() {
                bail!("请输入对端的设备验证码（1133）");
            }
            report_progress(reporter, 3, "验证设备验证码", "正在请求远程协助会话");
            dispatched = true;
            let response = cancellable(
                cancel,
                client.assist_join(&request.connect_id, &request.connect_code),
            )
            .await?;
            if response.code == 1136 {
                let control_id = response
                    .data
                    .as_ref()
                    .map(|r| r.control_id.as_str())
                    .filter(|id| !id.is_empty())
                    .context("等待确认的响应缺少 control_id，已停止连接")?;
                report_progress(
                    reporter,
                    3,
                    "等待对端确认",
                    "验证码已提交，等待对端允许本次连接",
                );
                cancellable(
                    cancel,
                    client.assist_confirm(&request.connect_id, control_id),
                )
                .await?
            } else {
                response
            }
        };
        match response.code {
            1131 => bail!("设备验证码不正确（1131）"),
            1133 => bail!("对端要求重新输入设备验证码（1133）"),
            _ => response.into_data(),
        }
    }
    .await;
    if dispatched && cancel.is_cancelled() {
        // Withdraw only the request started by this viewer. Never log out or
        // modify the target's allow-assistance settings.
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            client.assist_cancel(&request.connect_id),
        )
        .await;
    }
    result
}
