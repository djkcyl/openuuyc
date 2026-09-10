use super::*;
use crate::assist::{
    ControlMode, FavoriteItem, JoinReply, SavedDevice, SavedKind, SavedLists, normalize_connect_id,
};

impl AuthenticatedClient {
    async fn assist_response<T, F>(
        &self,
        request: impl FnOnce(NrdApi) -> F,
    ) -> Result<ApiEnvelope<T>>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        let response = self.request_envelope(request).await?;
        if response.code == 1120 {
            self.retire();
        }
        Ok(response)
    }
    pub(crate) async fn assist_mode(&self, id: &str) -> Result<ControlMode> {
        self.assist_response(|api| async move { api.assist_mode(id).await })
            .await?
            .into_data()
    }
    pub(crate) async fn assist_join(&self, id: &str, code: &str) -> Result<ApiEnvelope<JoinReply>> {
        let response = self
            .assist_response(|api| async move { api.assist_join(id, code).await })
            .await?;
        if response.code == 1131 {
            let store = crate::auth::AssistCodeStore::new(self.session.user_id())?;
            let id = id.to_owned();
            let code = code.to_owned();
            let cleared = tokio::task::spawn_blocking(move || store.reject_code(&id, &code)).await;
            if !matches!(cleared, Ok(Ok(()))) {
                tracing::warn!("could not clear rejected assistance code");
            }
        }
        Ok(response)
    }
    pub(crate) async fn assist_confirm(
        &self,
        id: &str,
        control_id: &str,
    ) -> Result<ApiEnvelope<JoinReply>> {
        self.assist_response(|api| async move { api.assist_confirm(id, control_id).await })
            .await
    }
    pub(crate) async fn assist_refetch(&self, token: &str) -> Result<JoinReply> {
        self.assist_response(|api| async move { api.assist_refetch(token).await })
            .await?
            .into_data()
    }
    pub(crate) async fn assist_cancel(&self, id: &str) -> Result<()> {
        self.assist_response(|api| async move { api.assist_cancel(id).await })
            .await?
            .into_data()?;
        Ok(())
    }
    pub(crate) async fn assist_saved(&self, kind: SavedKind) -> Result<Vec<SavedDevice>> {
        Ok(self
            .request(|api| async move { api.assist_saved(kind).await })
            .await?
            .saved)
    }
    pub(crate) async fn assist_lists(&self) -> Result<SavedLists> {
        let (recent, favorites) = tokio::try_join!(
            self.assist_saved(SavedKind::Recent),
            self.assist_saved(SavedKind::Favorites)
        )?;
        let store = crate::auth::AssistCodeStore::new(self.session.user_id())?;
        tokio::task::spawn_blocking(move || {
            let mut lists = SavedLists {
                recent,
                favorites,
                code_error: None,
            };
            for device in lists.recent.iter_mut().chain(&mut lists.favorites) {
                match store.load(&device.connect_id, Some(&device.publisher_device_id)) {
                    Ok(Some(saved)) => device.saved_code = saved.code,
                    Ok(None) => {}
                    Err(_) => {
                        lists.code_error = Some("无法读取本机保存的验证码，可手动输入后连接".into())
                    }
                }
            }
            lists
        })
        .await
        .context("读取本机验证码任务中断")
    }

    pub(crate) async fn resolve_assist_code(
        &self,
        id: String,
        code: String,
        publisher_id: Option<String>,
    ) -> Result<(String, Option<String>)> {
        crate::assist::validate_connect_code(&code)?;
        if !code.is_empty() {
            return Ok((code, publisher_id));
        }
        let store = crate::auth::AssistCodeStore::new(self.session.user_id())?;
        tokio::task::spawn_blocking(move || match store.load(&id, publisher_id.as_deref())? {
            Some(saved) => Ok((saved.code, Some(saved.publisher_id))),
            None => Ok((String::new(), publisher_id)),
        })
        .await
        .context("读取本机验证码任务中断")?
    }

    pub(crate) async fn remember_assist_code(
        &self,
        id: String,
        publisher_id: String,
        code: String,
    ) -> Result<()> {
        if !self.is_active() {
            bail!("账号会话已结束");
        }
        let store = crate::auth::AssistCodeStore::new(self.session.user_id())?;
        let ended = self.ended();
        tokio::task::spawn_blocking(move || {
            if ended.is_cancelled() {
                bail!("账号会话已结束");
            }
            store.save(&id, &publisher_id, code)
        })
        .await
        .context("保存本机验证码任务中断")?
    }

    pub(crate) async fn save_assist_favorite(
        &self,
        id: &str,
        remark: &str,
        code: Option<String>,
    ) -> Result<()> {
        if let Some(code) = &code {
            crate::assist::validate_connect_code(code)?;
        }
        let saved = self.upsert_assist_favorite(id, remark).await?;
        if let Some(code) = code {
            self.remember_assist_code(saved.connect_id, saved.publisher_device_id, code)
                .await
                .context("收藏已更新，但本机验证码未保存")?;
        }
        Ok(())
    }

    async fn upsert_assist_favorite(&self, id: &str, remark: &str) -> Result<SavedDevice> {
        let id = normalize_connect_id(id)?;
        let remark = remark.trim();
        if remark.chars().any(char::is_control) {
            bail!("备注不能包含控制字符");
        }
        let current = self.assist_saved(SavedKind::Favorites).await?;
        let time = current
            .iter()
            .find(|d| d.connect_id == id)
            .map(|d| d.favorited_at)
            .unwrap_or_else(|| chrono::Utc::now().timestamp());
        let item = FavoriteItem {
            connect_id: &id,
            remark,
            favorited_at: time,
        };
        let result = self
            .request(|api| async move { api.assist_save_favorite(&item).await })
            .await;
        let desired =
            |d: &SavedDevice| d.connect_id == id && (remark.is_empty() || d.remark == remark);
        match result {
            Ok(reply) => {
                if let Some(saved) = reply.saved.into_iter().find(desired) {
                    return Ok(saved);
                }
            }
            Err(error) if error.downcast_ref::<ApiFailure>().is_some() => return Err(error),
            _ => {}
        }
        if let Some(saved) = self
            .assist_saved(SavedKind::Favorites)
            .await?
            .into_iter()
            .find(desired)
        {
            return Ok(saved);
        }
        bail!("收藏未确认保存，请刷新后检查设备 ID")
    }
    pub(crate) async fn delete_assist_saved(
        &self,
        kind: SavedKind,
        publisher_id: &str,
    ) -> Result<()> {
        crate::api::validate_device_id(publisher_id)?;
        let current = self.assist_saved(kind).await?;
        if !current
            .iter()
            .any(|d| d.publisher_device_id == publisher_id)
        {
            return Ok(());
        }
        let removed: Vec<_> = current
            .into_iter()
            .filter(|d| d.publisher_device_id == publisher_id)
            .collect();
        let result = self
            .request(|api| async move { api.assist_delete_saved(kind, publisher_id).await })
            .await;
        if let Err(error) = result {
            if error.downcast_ref::<ApiFailure>().is_some() {
                return Err(error);
            }
            if self
                .assist_saved(kind)
                .await?
                .iter()
                .any(|d| d.publisher_device_id == publisher_id)
            {
                bail!("删除结果未确认，请刷新检查；未自动重复提交");
            }
        }
        self.remove_unused_assist_codes(removed).await
    }
    pub(crate) async fn clear_assist_recent(&self) -> Result<()> {
        let previous = self.assist_saved(SavedKind::Recent).await?;
        let result = self
            .request(|api| async move { api.assist_clear_recent().await })
            .await;
        if let Err(error) = result {
            if error.downcast_ref::<ApiFailure>().is_some() {
                return Err(error);
            }
            if !self.assist_saved(SavedKind::Recent).await?.is_empty() {
                bail!("清空结果未确认，请刷新检查；未自动重复提交");
            }
        }
        self.remove_unused_assist_codes(previous).await
    }

    async fn remove_unused_assist_codes(&self, candidates: Vec<SavedDevice>) -> Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }
        let (recent, favorites) = tokio::try_join!(
            self.assist_saved(SavedKind::Recent),
            self.assist_saved(SavedKind::Favorites),
        )
        .context("记录已移除，但验证码清理尚未确认")?;
        let store = crate::auth::AssistCodeStore::new(self.session.user_id())?;
        tokio::task::spawn_blocking(move || {
            for device in candidates {
                if !recent
                    .iter()
                    .chain(&favorites)
                    .any(|d| d.connect_id == device.connect_id)
                {
                    store.remove_device(&device)?;
                }
            }
            Ok(())
        })
        .await
        .context("验证码清理任务中断")?
    }
}
