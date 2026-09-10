//! One authenticated account generation. Room owners decide when it ends;
//! a generic REST error is not a global logout notification.

use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

use crate::{
    api::{ApiEnvelope, ApiFailure, DeviceList, NrdApi, RoomSession},
    auth::{KeyringIdentityStore, KeyringSessionStore, LoginSession, NativeIdentity, SessionStore},
    device_session::{DeviceHandle, DeviceRuntime},
    session_restore::{self, RestoreTrigger},
};

#[derive(Debug)]
pub(crate) struct NoSavedSession;

impl std::fmt::Display for NoSavedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no saved login session; run `OpenUUYC login` first")
    }
}
impl std::error::Error for NoSavedSession {}

mod assist;

pub struct AuthenticatedClient {
    api: Mutex<Option<NrdApi>>,
    session: LoginSession,
    device: AccountDevice,
    owned_device: tokio::sync::Mutex<Option<DeviceRuntime>>,
    session_store: KeyringSessionStore,
    ended: CancellationToken,
    validated: tokio::sync::Mutex<RestoreState>,
    restore_trigger: RestoreTrigger,
    account_name: Mutex<String>,
}

#[derive(Default)]
struct RestoreState {
    ready: bool,
    failure: Option<ApiFailure>,
}

enum AccountDevice {
    Managed(DeviceHandle),
    // A GUI-launched viewer inherits the device/account validated by its
    // authenticated owner IPC. Like the native viewer, it is not another
    // DeviceInitializer and must not register/reset that identity independently.
    Inherited(Box<NativeIdentity>),
}

impl AccountDevice {
    fn identity(&self) -> NativeIdentity {
        match self {
            Self::Managed(device) => device.identity(),
            Self::Inherited(identity) => identity.as_ref().clone(),
        }
    }
}

pub struct LogoutOutcome {
    pub remote_error: Option<String>,
    pub local_error: Option<String>,
}

impl AuthenticatedClient {
    pub fn from_saved_session() -> Result<Self> {
        let runtime = DeviceRuntime::start()?;
        Self::load(AccountDevice::Managed(runtime.handle()), Some(runtime))
    }

    pub(crate) fn from_saved_session_with_device(device: DeviceHandle) -> Result<Self> {
        Self::load(AccountDevice::Managed(device), None)
    }

    pub(crate) fn from_parent_session() -> Result<Self> {
        let identity = KeyringIdentityStore::new()?.load_or_create()?;
        if identity.client_identity()?.device_id.is_empty() {
            bail!("parent has not initialized the virtual device");
        }
        Self::load(AccountDevice::Inherited(Box::new(identity)), None)
    }

    fn load(device: AccountDevice, owned_device: Option<DeviceRuntime>) -> Result<Self> {
        let session_store = KeyringSessionStore::new()?;
        let session = session_store.load()?.ok_or(NoSavedSession)?;
        let identity = device.identity().client_identity()?;
        let mut api = NrdApi::new(identity)?;
        api.set_user_id(Some(session.user_id()))?;
        api.set_bearer_token(Some(session.token()))?;
        let ended = CancellationToken::new();
        let inherited = matches!(&device, AccountDevice::Inherited(_));
        if let AccountDevice::Managed(device) = &device {
            device.watch_account(ended.clone())?;
        }
        let restore_trigger = if owned_device.is_some() {
            RestoreTrigger::DeviceStartup
        } else {
            RestoreTrigger::ExplicitLogin
        };
        let account_name = Mutex::new(session.nickname().to_owned());
        Ok(Self {
            api: Mutex::new(Some(api)),
            session,
            device,
            owned_device: tokio::sync::Mutex::new(owned_device),
            session_store,
            ended,
            validated: tokio::sync::Mutex::new(RestoreState {
                ready: inherited,
                failure: None,
            }),
            restore_trigger,
            account_name,
        })
    }

    pub fn device_id(&self) -> String {
        self.device
            .identity()
            .client_identity()
            .expect("validated native identity")
            .device_id
    }
    pub(crate) fn viewing_settings_store(
        &self,
        publisher_id: &str,
    ) -> Result<crate::viewing_settings::ViewingSettingsStore> {
        crate::viewing_settings::ViewingSettingsStore::new(self.session.user_id(), publisher_id)
    }
    pub fn ended(&self) -> CancellationToken {
        self.ended.clone()
    }
    pub fn is_active(&self) -> bool {
        !self.ended.is_cancelled()
    }

    /// Retire this generation before asynchronous room teardown. Late responses
    /// cannot restore its headers or deliver authenticated results.
    pub fn retire(&self) {
        let mut api = self.api.lock().unwrap_or_else(|error| error.into_inner());
        self.ended.cancel();
        api.take();
    }

    pub fn clear_saved_generation(&self) -> Result<()> {
        self.session_store.clear_if_matches(&self.session)?;
        Ok(())
    }

    async fn validate_saved_session(&self) -> Result<()> {
        let mut validated = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            validated = self.validated.lock() => validated,
        };
        if validated.ready {
            return Ok(());
        }
        if let Some(failure) = &validated.failure {
            return Err(failure.clone().into());
        }
        let AccountDevice::Managed(device) = &self.device else {
            return Ok(());
        };
        let identity = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            result = session_restore::initialize(device, self.restore_trigger) => result,
        };
        let identity = match identity {
            Ok(identity) => identity,
            Err(error) => {
                let failure = error
                    .downcast::<ApiFailure>()
                    .unwrap_or_else(|error| ApiFailure {
                        code: -1,
                        message: format!("{error:#}"),
                    });
                validated.failure = Some(failure.clone());
                return Err(failure.into());
            }
        };
        let mut api = self
            .api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("account session has ended")?;
        api.set_identity(identity.client_identity()?);
        let response = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            response = session_restore::restore_user(&api, self.restore_trigger) => response,
        };
        match response {
            Ok(info) => {
                if let Some(name) = info.get("nickname").and_then(serde_json::Value::as_str) {
                    *self.account_name.lock().unwrap_or_else(|e| e.into_inner()) = name.to_owned();
                }
            }
            Err(failure) => {
                if failure.invalid_saved_credentials() {
                    self.retire();
                    self.clear_saved_generation()?;
                }
                validated.failure = Some(failure.clone());
                return Err(failure.into());
            }
        }
        let mut active_api = self.api.lock().unwrap_or_else(|error| error.into_inner());
        if !self.is_active() {
            bail!("account session has ended");
        }
        *active_api = Some(api);
        validated.ready = true;
        Ok(())
    }

    pub(crate) async fn close_device_owner(&self) {
        if let Some(runtime) = self.owned_device.lock().await.take() {
            runtime.close().await;
        }
    }

    /// Finish process-owned initialization work without logging out the account.
    pub async fn close(&self) {
        self.close_device_owner().await;
    }

    pub(crate) async fn restoration_failed(&self) -> bool {
        self.validated.lock().await.failure.is_some()
    }

    pub async fn logout(&self) -> LogoutOutcome {
        let api = self
            .api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.ended.cancel();
        let remote_error = match api {
            Some(api) => api.logout().await.err().map(|error| format!("{error:#}")),
            None => None,
        };
        // Server 3E46B0 clears local credentials on either API outcome.
        let local_error = self
            .clear_saved_generation()
            .err()
            .map(|error| format!("{error:#}"));
        self.close_device_owner().await;
        LogoutOutcome {
            remote_error,
            local_error,
        }
    }

    async fn request<T, F>(&self, request: impl FnOnce(NrdApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        self.request_envelope(request).await?.into_data()
    }

    async fn request_envelope<T, F>(
        &self,
        request: impl FnOnce(NrdApi) -> F,
    ) -> Result<ApiEnvelope<T>>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        self.validate_saved_session().await?;
        let api = self
            .api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("account session has ended")?;
        let response = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            response = request(api) => response?,
        };
        if !self.is_active() {
            bail!("account session has ended");
        }
        Ok(response)
    }

    pub async fn list_devices(&self) -> Result<DeviceList> {
        self.request(|api| async move { api.list_devices().await })
            .await
    }

    pub async fn device_groups(&self) -> Result<crate::api::DeviceGroups> {
        self.management_request(|api| async move { api.device_groups().await })
            .await
    }

    pub async fn device_detail(&self, id: &str) -> Result<crate::api::DeviceDetail> {
        self.management_request(|api| async move { api.device_detail(id).await })
            .await
    }

    pub async fn account_info(&self) -> Result<serde_json::Value> {
        self.request(|api| async move { api.get_user_info().await })
            .await
    }

    pub(crate) fn account_name(&self) -> String {
        self.account_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn management_request<T, F>(&self, request: impl FnOnce(NrdApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        let result = self.request(request).await;
        // These specific official management callbacks route 1120 to account
        // invalidation. Do not broaden every room/API failure into a logout.
        if result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<ApiFailure>())
            .is_some_and(|f| f.code == 1120)
        {
            self.retire();
        }
        result
    }

    pub(crate) fn suggested_device_name(&self) -> String {
        self.device.identity().suggested_name()
    }

    pub(crate) async fn rename_owned_device(&self, id: &str, alias: &str) -> Result<String> {
        let groups = self.device_groups().await?;
        if !groups.entries().any(|(_, d)| d.device_id == id) {
            bail!("设备已不在本账号绑定列表中，未发送改名请求");
        }
        let result = self
            .management_request(|api| async move { api.rename_device(id, alias).await })
            .await;
        let actual = match result {
            Ok(response)
                if response.device_id == id
                    && !response.alias.trim().is_empty()
                    && !response.alias.chars().any(char::is_control) =>
            {
                response.alias
            }
            Err(error) if error.downcast_ref::<ApiFailure>().is_some() => {
                return Err(error.context("设备改名被服务端拒绝"));
            }
            result => {
                // A timeout/response mismatch is not evidence that a write did
                // not happen. Read back; never replay a management mutation.
                let query = self.device_groups().await;
                if query.as_ref().is_ok_and(|g| {
                    g.entries()
                        .any(|(_, d)| d.device_id == id && d.alias == alias)
                }) {
                    alias.to_owned()
                } else {
                    let error = result
                        .err()
                        .map(|e| format!("{e:#}"))
                        .unwrap_or_else(|| "返回的设备 ID 不匹配".into());
                    bail!("改名结果未确认，未自动重试：{error}。请刷新检查当前名称");
                }
            }
        };
        if id == self.device_id()
            && let AccountDevice::Managed(device) = &self.device
            && let Err(error) = device.set_name(id.into(), actual).await
        {
            return Ok(format!("服务端已改名，但本地注册名称保存失败：{error:#}"));
        }
        Ok("设备名称已更新".into())
    }

    pub(crate) async fn remove_account_device(&self, id: &str) -> Result<String> {
        if id == self.device_id() {
            bail!("移除本机观看身份请使用退出登录，以完成本地会话清理");
        }
        let present = self
            .device_groups()
            .await?
            .entries()
            .any(|(_, d)| d.device_id == id);
        if !present {
            bail!("设备或所有权已变化，未发送移除请求，请刷新列表");
        }
        let result = self
            .management_request(|api| async move { api.unbind_device(id).await })
            .await;
        if let Err(error) = result {
            if error.downcast_ref::<ApiFailure>().is_some() {
                return Err(error.context("设备移除被服务端拒绝"));
            }
            let absent = self
                .device_groups()
                .await
                .map(|g| !g.entries().any(|(_, d)| d.device_id == id));
            if !matches!(absent, Ok(true)) {
                bail!("移除结果未确认，未自动重试：{error:#}。请刷新列表检查");
            }
        }
        Ok("设备已移除".into())
    }

    pub async fn join_device(&self, device_id: &str, force_join: bool) -> Result<RoomSession> {
        let room: RoomSession = self
            .request(|api| async move { api.join_by_device(device_id, force_join).await })
            .await?;
        room.validate()?;
        Ok(room)
    }

    pub async fn create_host_room(&self, last_controlled_interval: i64) -> Result<RoomSession> {
        let room: RoomSession = self
            .request(|api| async move { api.create_room(last_controlled_interval).await })
            .await?;
        room.validate()?;
        Ok(room)
    }

    pub async fn set_controllable(&self, controllable: bool) -> Result<()> {
        let AccountDevice::Managed(device) = &self.device else {
            bail!("only the device owner can change its permissions");
        };
        self.request(|api| async move { api.set_controllable(controllable).await })
            .await?;
        device.set_controllable(controllable).await?;
        Ok(())
    }
}

impl Drop for AuthenticatedClient {
    fn drop(&mut self) {
        self.ended.cancel();
    }
}
