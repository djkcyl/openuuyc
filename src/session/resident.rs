//! Requests between the desktop client and the resident host owner that an
//! installed service runs. Plain data: the transport is platform-specific.
use crate::{account::auth::NativeIdentity, features::host, session::presence::PresenceState};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Resume,
    Pause,
    PrepareUpdate,
    CancelUpdate,
    Snapshot {
        ui: bool,
        #[serde(default)]
        device_cursor: Option<crate::account::device_change::relay::Cursor>,
    },
    Assist {
        account: String,
        action: host::assist::Action,
    },
    RefreshPublication,
    Initialize {
        force: bool,
    },
    Controllable(bool),
    Name {
        device: String,
        value: String,
    },
    Settings {
        account: String,
        allowed: bool,
        encoding: host::EncodingSettings,
        audio_device: Option<host::audio::Device>,
        audio_defaults: host::audio::DefaultDevices,
        audio_quality: crate::media::audio::encoder::Quality,
        assistance: host::assist::Settings,
    },
    Disconnect {
        account: String,
    },
    Retry {
        account: String,
    },
    Retire {
        account: String,
    },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(default)]
    pub device_changes: Option<crate::account::device_change::relay::Batch>,
    pub assistance: host::assist::Snapshot,
    pub publication: crate::account::reporting::Snapshot,
    pub account: String,
    pub online: PresenceState,
    pub allowed: bool,
    pub encoding: host::EncodingSettings,
    #[serde(default)]
    pub audio_device: Option<host::audio::Device>,
    #[serde(default)]
    pub audio_defaults: host::audio::DefaultDevices,
    #[serde(default)]
    pub audio_quality: crate::media::audio::encoder::Quality,
    pub status: host::Status,
    pub capabilities: Option<host::desktop::Capabilities>,
}
#[derive(Serialize, Deserialize)]
pub(crate) enum Reply {
    Done,
    Snapshot(Box<Snapshot>),
    Identity(Box<NativeIdentity>),
    Error(String),
    ApiError { code: i32, message: String },
}
impl Reply {
    pub(crate) fn from_error(error: anyhow::Error) -> Self {
        match error.downcast::<crate::account::api::ApiFailure>() {
            Ok(error) => Self::ApiError {
                code: error.code,
                message: error.message,
            },
            Err(error) => Self::Error(format!("{error:#}")),
        }
    }
    pub fn checked(self) -> Result<Self> {
        match self {
            Self::Error(e) => anyhow::bail!(e),
            Self::ApiError { code, message } => {
                Err(crate::account::api::ApiFailure { code, message }.into())
            }
            other => Ok(other),
        }
    }
}
