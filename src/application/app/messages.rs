//! Commands and events crossing the UI/worker boundary.
use super::assist::{AssistOperation, AssistResult};
use super::phone::LoginMethod;
use super::{StartupStage, catalog, power};
use crate::account::api::{DeviceInfo, DeviceList};
use crate::account::login::{self, LoginProgress};
use crate::media::ConnectionMediaOptions;
use crate::session::presence::PresenceState;
use std::time::Instant;

pub(super) enum GuiCommand {
    SaveHostSettings {
        generation: u64,
    },
    View {
        generation: u64,
        alias: String,
        device_id: Option<String>,
        assist: Option<crate::account::assist::AssistRequest>,
        options: ConnectionMediaOptions,
        background: Option<crate::application::wallpaper::Source>,
        takeover: Option<crate::session::controller::takeover::Approval>,
    },
    Ports {
        generation: u64,
        device: DeviceInfo,
        options: ConnectionMediaOptions,
    },
    Files {
        generation: u64,
        device: DeviceInfo,
        options: ConnectionMediaOptions,
    },
    Refresh,
    Login {
        generation: u64,
        attempt: u64,
    },
    CancelQr(u64),
    CancelSms(u64),
    RequestSms {
        generation: u64,
        attempt: u64,
        phone: login::sms::PhoneNumber,
        agreed: bool,
    },
    LoginSms {
        generation: u64,
        attempt: u64,
        phone: login::sms::PhoneNumber,
        code: String,
        agreed: bool,
    },
    CancelLogin(u64),
    Logout,
    Shutdown,
    Mutate {
        generation: u64,
        change: DeviceMutation,
    },
    Detail(String),
    RefreshAssist,
    CancelAssistCheck,
    AssistOperation {
        generation: u64,
        sequence: u64,
        operation: AssistOperation,
    },
}

pub(super) enum DeviceMutation {
    Rename {
        id: String,
        alias: String,
    },
    Remove {
        id: String,
    },
    Power {
        device: DeviceInfo,
        action: crate::account::power::PowerAction,
    },
}

pub(super) enum MutationOutcome {
    Changed {
        message: String,
        change: crate::account::device_change::DeviceChange,
    },
    Power(Box<power::AcceptedPower>),
}

pub(super) enum GuiEvent {
    Host(u64, crate::features::host::Handle),
    Viewer(
        u64,
        String,
        Option<String>,
        std::result::Result<crate::session::controller::windows::ViewerHandle, String>,
    ),
    PowerDispatched(u64, String, crate::account::power::PowerAction),
    Startup(u64, StartupStage),
    Presence(PresenceState),
    AssistLists(
        u64,
        std::result::Result<crate::account::assist::SavedLists, String>,
    ),
    AssistOperation(u64, u64, std::result::Result<AssistResult, String>),
    Devices(u64, DeviceList),
    Catalog(u64, std::result::Result<catalog::Catalog, String>, String),
    MutationFinished(u64, std::result::Result<MutationOutcome, String>),
    Detail(
        u64,
        String,
        std::result::Result<crate::account::api::DeviceDetail, String>,
    ),
    Working(String),
    Warning(String),
    Error(String),
    SessionUnavailable(String),
    SignedOut,
    AccountEnded(String),
    LoginProgress(LoginMethod, u64, u64, LoginProgress),
    LoginFinished(LoginMethod, u64, u64, std::result::Result<(), String>),
    SmsCooldown(Option<Instant>),
    SmsCodeFinished {
        generation: u64,
        attempt: u64,
        phone: login::sms::PhoneNumber,
        dispatched: bool,
        result: std::result::Result<(), String>,
    },
    LoggedOut(crate::account::client::LogoutOutcome),
}
