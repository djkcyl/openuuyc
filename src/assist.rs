//! Outgoing remote assistance and server-synchronized saved devices.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;

pub fn normalize_connect_id(value: &str) -> Result<String> {
    let id: String = value.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if id.len() != 9 || !id.bytes().all(|c| c.is_ascii_digit()) {
        bail!("请输入9位设备 ID");
    }
    Ok(id)
}

pub(crate) fn validate_connect_code(code: &str) -> Result<()> {
    if code.len() > 256 || code.chars().any(char::is_control) {
        bail!("设备验证码格式不正确");
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AssistRequest {
    pub connect_id: String,
    pub(crate) connect_code: String,
    /// Validated preflight result passed only over local child stdin.
    pub(crate) control_mode: Option<String>,
    #[serde(default)]
    pub(crate) expected_publisher_id: Option<String>,
}
impl AssistRequest {
    pub fn new(connect_id: &str, connect_code: String) -> Result<Self> {
        let request = Self {
            connect_id: normalize_connect_id(connect_id)?,
            connect_code,
            control_mode: None,
            expected_publisher_id: None,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        normalize_connect_id(&self.connect_id)?;
        if let Some(id) = &self.expected_publisher_id {
            crate::api::validate_device_id(id)?;
        }
        validate_connect_code(&self.connect_code)
    }
    pub(crate) fn has_code(&self) -> bool {
        !self.connect_code.is_empty()
    }
}
impl fmt::Debug for AssistRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistRequest")
            .field("credentials", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct ControlMode {
    pub can_remote_control: bool,
    pub control_mode: String,
}

#[derive(Clone, Default, Deserialize)]
pub(crate) struct JoinReply {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub share_id: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub signaling_list: Vec<String>,
    #[serde(default)]
    pub ws_connect_timeout_ms: i32,
    #[serde(default)]
    pub streamer_retry_delta_ms: i32,
    #[serde(default = "windows_platform")]
    pub publisher_platform: i32,
    #[serde(default)]
    pub publisher_version_name: String,
    #[serde(default)]
    pub publisher_device_id: String,
    #[serde(default)]
    pub control_id: String,
}
fn windows_platform() -> i32 {
    1
}
impl JoinReply {
    pub(crate) fn validate(&self, controller_id: &str) -> Result<()> {
        if self.token.is_empty() || self.signaling_list.is_empty() {
            bail!("远程协助未返回有效的会话凭据");
        }
        if !matches!(self.publisher_platform, 1 | 4) {
            bail!("暂不支持观看此类设备");
        }
        if !self.publisher_device_id.is_empty() {
            crate::api::validate_device_id(&self.publisher_device_id)?;
            if self.publisher_device_id == controller_id {
                bail!("不能连接本机身份");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SavedKind {
    Recent,
    Favorites,
}
impl SavedKind {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Favorites => "favorites",
        }
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct SavedDevice {
    pub publisher_device_id: String,
    pub connect_id: String,
    pub remark: String,
    #[serde(default)]
    pub is_favorite: bool,
    #[serde(default)]
    pub last_connected_at: Option<i64>,
    #[serde(default)]
    pub favorited_at: Option<i64>,
    #[serde(skip)]
    pub saved_code: String,
}
impl SavedDevice {
    pub(crate) fn title(&self) -> &str {
        if self.remark.trim().is_empty() {
            &self.connect_id
        } else {
            &self.remark
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        normalize_connect_id(&self.connect_id)?;
        crate::api::validate_device_id(&self.publisher_device_id)
    }
}
#[derive(Clone, Default, Deserialize)]
pub(crate) struct SavedReply {
    #[serde(default)]
    pub saved: Vec<SavedDevice>,
}
#[derive(Clone, Default)]
pub(crate) struct SavedLists {
    pub recent: Vec<SavedDevice>,
    pub favorites: Vec<SavedDevice>,
    pub code_error: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct FavoriteItem<'a> {
    pub connect_id: &'a str,
    pub remark: &'a str,
    pub favorited_at: i64,
}

/// Child launch data is never placed in argv, environment or a plaintext file.
pub fn read_launch_request() -> Result<AssistRequest> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(4097)
        .read_to_end(&mut bytes)
        .context("读取远程协助参数失败")?;
    if bytes.len() > 4096 {
        bail!("远程协助参数过长");
    }
    let request: AssistRequest =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("远程协助参数格式不正确"))?;
    request.validate()?;
    Ok(request)
}
