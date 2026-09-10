//! Cross-platform persistence for the login session and virtual device.
//!
//! Both records are kept in the operating system credential service. There is
//! deliberately no plaintext fallback: Windows uses Credential Manager, macOS
//! uses Keychain, and Linux uses Secret Service through the `keyring` crate.

use std::fmt;

use anyhow::{Context, Result, bail};
use keyring::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{ClientIdentity, WindowsDeviceInitRequest};

mod assist;
pub(crate) use assist::AssistCodeStore;

// Stable credential namespace; independent of product and executable names.
const SERVICE: &str = "com.openuuyc.session";
const SESSION_ACCOUNT: &str = "default-nrd-login";
const IDENTITY_ACCOUNT: &str = "native-device-identity";
const SESSION_SCHEMA: u8 = 1;
const IDENTITY_SCHEMA: u8 = 1;
const PROFILE_SCHEMA: u8 = 1;

#[derive(Clone, Serialize, Deserialize)]
pub struct LoginSession {
    schema: u8,
    token: String,
    user_id: String,
    nickname: String,
}

impl LoginSession {
    pub fn new(
        token: impl Into<String>,
        user_id: impl Into<String>,
        nickname: impl Into<String>,
    ) -> Result<Self> {
        let session = Self {
            schema: SESSION_SCHEMA,
            token: token.into(),
            user_id: user_id.into(),
            nickname: nickname.into(),
        };
        session.validate_schema()?;
        Ok(session)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn nickname(&self) -> &str {
        &self.nickname
    }

    fn validate_schema(&self) -> Result<()> {
        if self.schema != SESSION_SCHEMA {
            bail!("unsupported saved login-session schema: {}", self.schema);
        }
        if self.token.is_empty() || self.user_id.is_empty() {
            bail!("saved login session is incomplete");
        }
        Ok(())
    }
}

impl fmt::Debug for LoginSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginSession")
            .field("schema", &self.schema)
            .field("token", &"***REDACTED***")
            .field("user_id", &"***REDACTED***")
            .field("nickname", &"***REDACTED***")
            .finish()
    }
}

pub trait SessionStore {
    fn load(&self) -> Result<Option<LoginSession>>;
    fn save(&self, session: &LoginSession) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct VirtualDeviceProfile {
    schema: u8,
    name: String,
    machine_guid: String,
    os: String,
    base_board: String,
    cpu: String,
    video: Vec<String>,
    mac: String,
    memory: i64,
    screen: String,
    platform: i32,
    controllable: bool,
}

impl VirtualDeviceProfile {
    fn generate() -> Self {
        let mut mac = *Uuid::new_v4().as_bytes();
        mac[0] = (mac[0] | 0x02) & 0xfe;
        let machine_guid = Uuid::new_v4().to_string();
        Self {
            schema: PROFILE_SCHEMA,
            name: short_virtual_name(&machine_guid),
            machine_guid,
            os: "Microsoft Windows 11 Pro".into(),
            base_board: crate::virtual_hardware::BOARD.into(),
            cpu: crate::virtual_hardware::CPU.into(),
            video: vec![crate::virtual_hardware::VIDEO.into()],
            mac: mac[..6]
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(":"),
            memory: 16_384,
            screen: "1920x1080".into(),
            platform: 1,
            controllable: false,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema != PROFILE_SCHEMA {
            bail!("unsupported virtual-device profile schema: {}", self.schema);
        }
        if [
            &self.name,
            &self.machine_guid,
            &self.os,
            &self.base_board,
            &self.cpu,
            &self.mac,
            &self.screen,
        ]
        .into_iter()
        .any(|value| value.is_empty())
            || self.video.is_empty()
            || self.video.iter().any(String::is_empty)
            || self.memory < 0
        {
            bail!("saved virtual-device profile is incomplete");
        }
        Uuid::parse_str(&self.machine_guid).context("saved virtual machine_guid is not a UUID")?;
        Ok(())
    }
}

impl Default for VirtualDeviceProfile {
    fn default() -> Self {
        Self::generate()
    }
}

impl fmt::Debug for VirtualDeviceProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualDeviceProfile")
            .field("schema", &self.schema)
            .field("name", &self.name)
            .field("hardware", &"***REDACTED***")
            .field("platform", &self.platform)
            .field("controllable", &self.controllable)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeIdentity {
    schema: u8,
    client_id: String,
    device_id: String,
    system_id: String,
    #[serde(default)]
    virtual_profile: VirtualDeviceProfile,
}

impl NativeIdentity {
    pub fn generate() -> Self {
        Self {
            schema: IDENTITY_SCHEMA,
            client_id: Uuid::new_v4().to_string(),
            device_id: String::new(),
            system_id: Uuid::new_v4().to_string(),
            virtual_profile: VirtualDeviceProfile::generate(),
        }
    }

    pub fn client_identity(&self) -> Result<ClientIdentity> {
        self.validate_schema()?;
        ClientIdentity::new(&self.client_id, &self.device_id, &self.system_id)
    }

    pub(crate) fn reset_client_uuid(&mut self) {
        // Server 17E2C0 resets General/uuid only. Hardware, system_id and the
        // previous device_id remain until a successful init response replaces it.
        self.client_id = Uuid::new_v4().to_string();
    }

    pub(crate) fn set_controllable(&mut self, value: bool) {
        self.virtual_profile.controllable = value;
    }

    pub(crate) fn set_device_name(&mut self, value: String) {
        self.virtual_profile.name = value;
    }

    pub(crate) fn suggested_name(&self) -> String {
        short_virtual_name(&self.virtual_profile.machine_guid)
    }

    pub fn device_init_request(&self) -> Result<WindowsDeviceInitRequest> {
        self.validate_schema()?;
        Ok(WindowsDeviceInitRequest {
            name: self.virtual_profile.name.clone(),
            client_id: self.client_id.clone(),
            system_id: self.system_id.clone(),
            machine_guid: self.virtual_profile.machine_guid.clone(),
            os: self.virtual_profile.os.clone(),
            base_board: self.virtual_profile.base_board.clone(),
            cpu: self.virtual_profile.cpu.clone(),
            video: self.virtual_profile.video.clone(),
            mac: self.virtual_profile.mac.clone(),
            memory: self.virtual_profile.memory,
            screen: self.virtual_profile.screen.clone(),
            platform: self.virtual_profile.platform,
            controllable: self.virtual_profile.controllable,
        })
    }

    pub fn complete_registration(&mut self, device_id: impl Into<String>) -> Result<()> {
        let device_id = device_id.into();
        let valid = device_id.len() == 16
            && device_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        if !valid {
            bail!("device initialization returned an invalid device_id");
        }
        self.device_id = device_id;
        self.validate_schema()
    }

    fn validate_schema(&self) -> Result<()> {
        if self.schema != IDENTITY_SCHEMA {
            bail!("unsupported native-identity schema: {}", self.schema);
        }
        Uuid::parse_str(&self.client_id).context("saved client_id is not a UUID")?;
        if self.system_id.is_empty() {
            bail!("saved native identity has an empty system_id");
        }
        if self.device_id.is_empty() {
            Uuid::parse_str(&self.system_id)
                .context("unregistered virtual device has a non-UUID system_id")?;
        } else {
            let valid_device_id = self.device_id.len() == 16
                && self
                    .device_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
            if !valid_device_id {
                bail!("saved device_id has an invalid format");
            }
        }
        self.virtual_profile.validate()
    }
}

fn short_virtual_name(identity: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(identity.as_bytes());
    // Stable installation identity; unrelated to account, MAC or build hash.
    format!(
        "OU-{:02X}{:02X}{:02X}{:02X}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

impl fmt::Debug for NativeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeIdentity")
            .field("schema", &self.schema)
            .field("client_id", &"***REDACTED***")
            .field("device_id", &"***REDACTED***")
            .field("system_id", &"***REDACTED***")
            .field("virtual_profile", &self.virtual_profile)
            .finish()
    }
}

pub struct KeyringSessionStore {
    entry: Entry,
}

pub struct KeyringIdentityStore {
    entry: Entry,
}

impl KeyringIdentityStore {
    pub fn new() -> Result<Self> {
        let entry = Entry::new(SERVICE, IDENTITY_ACCOUNT)
            .context("native secure credential store is unavailable")?;
        Ok(Self { entry })
    }

    pub fn load_or_create(&self) -> Result<NativeIdentity> {
        let _lock = credential_store_lock("identity.lock")?;
        self.load_or_create_unlocked()
    }

    fn load_or_create_unlocked(&self) -> Result<NativeIdentity> {
        match self.entry.get_secret() {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .context("saved native identity is not valid JSON")?;
                let missing_profile = value.get("virtual_profile").is_none();
                let identity: NativeIdentity = serde_json::from_value(value)
                    .context("saved native identity has an invalid shape")?;
                identity.validate_schema()?;
                if missing_profile {
                    self.save_unlocked(&identity)?;
                }
                Ok(identity)
            }
            Err(KeyringError::NoEntry) => {
                let identity = NativeIdentity::generate();
                self.save_unlocked(&identity)?;
                Ok(identity)
            }
            Err(error) => Err(error).context("failed to read native identity"),
        }
    }

    pub fn save(&self, identity: &NativeIdentity) -> Result<()> {
        let _lock = credential_store_lock("identity.lock")?;
        self.save_unlocked(identity)
    }

    pub(crate) fn replace_if_matches(
        &self,
        expected: &NativeIdentity,
        updated: &NativeIdentity,
    ) -> Result<bool> {
        let _lock = credential_store_lock("identity.lock")?;
        if self.load_or_create_unlocked()? != *expected {
            return Ok(false);
        }
        self.save_unlocked(updated)?;
        Ok(true)
    }

    fn save_unlocked(&self, identity: &NativeIdentity) -> Result<()> {
        identity.validate_schema()?;
        let bytes = serde_json::to_vec(identity).context("failed to serialize native identity")?;
        self.entry
            .set_secret(&bytes)
            .context("failed to save native identity")
    }
}

impl KeyringSessionStore {
    pub fn new() -> Result<Self> {
        let entry = Entry::new(SERVICE, SESSION_ACCOUNT)
            .context("native secure credential store is unavailable")?;
        Ok(Self { entry })
    }

    pub fn platform_store_available() -> bool {
        Entry::store_status().is_ok()
    }

    /// An old room/API completion must not erase a newer QR login. The lock
    /// serializes compare/delete with save across GUI and CLI processes; it
    /// contains no credentials and is never held across a network await.
    pub fn clear_if_matches(&self, expected: &LoginSession) -> Result<bool> {
        let _lock = session_store_lock()?;
        if self.load_unlocked()?.is_some_and(|current| {
            current.user_id == expected.user_id && current.token == expected.token
        }) {
            self.clear_unlocked()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn save_if_matches(
        &self,
        expected: Option<&LoginSession>,
        next: &LoginSession,
    ) -> Result<bool> {
        let _lock = session_store_lock()?;
        let current = self.load_unlocked()?;
        let matches = match (current.as_ref(), expected) {
            (None, None) => true,
            (Some(current), Some(expected)) => {
                current.user_id == expected.user_id && current.token == expected.token
            }
            _ => false,
        };
        if !matches {
            return Ok(false);
        }
        self.save_session_unlocked(next)?;
        Ok(true)
    }

    fn save_session_unlocked(&self, session: &LoginSession) -> Result<()> {
        session.validate_schema()?;
        let bytes = serde_json::to_vec(session).context("failed to serialize login session")?;
        self.entry
            .set_secret(&bytes)
            .context("failed to save login session in native secure credential store")
    }

    fn load_unlocked(&self) -> Result<Option<LoginSession>> {
        let bytes = match self.entry.get_secret() {
            Ok(bytes) => bytes,
            Err(KeyringError::NoEntry) => return Ok(None),
            Err(error) => {
                return Err(error).context("failed to read native secure credential store");
            }
        };
        let session: LoginSession =
            serde_json::from_slice(&bytes).context("saved login session is not valid JSON")?;
        session.validate_schema()?;
        Ok(Some(session))
    }

    fn clear_unlocked(&self) -> Result<()> {
        match self.entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => {
                Err(error).context("failed to clear login session from secure credential store")
            }
        }
    }
}

fn session_store_lock() -> Result<std::fs::File> {
    credential_store_lock("session.lock")
}

fn credential_store_lock(filename: &str) -> Result<std::fs::File> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .context("LOCALAPPDATA is required for session-store coordination")?;
    #[cfg(not(windows))]
    let base = {
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .context("HOME is required for session-store coordination")?;
        #[cfg(target_os = "macos")]
        let base = home.join("Library/Application Support");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".local/state"));
        base
    };
    let directory = base.join("openuuyc");
    std::fs::create_dir_all(&directory).context("create session coordination directory")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(directory.join(filename))
        .context("open session coordination lock")?;
    lock.lock().context("lock session credential transaction")?;
    Ok(lock)
}

impl SessionStore for KeyringSessionStore {
    fn load(&self) -> Result<Option<LoginSession>> {
        let _lock = session_store_lock()?;
        self.load_unlocked()
    }

    fn save(&self, session: &LoginSession) -> Result<()> {
        let _lock = session_store_lock()?;
        self.save_session_unlocked(session)
    }

    fn clear(&self) -> Result<()> {
        let _lock = session_store_lock()?;
        self.clear_unlocked()
    }
}
