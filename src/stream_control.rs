//! Official runtime stream-setting protocol.
//!
//! UU uses two different reliable DataChannels for this state machine:
//! protobuf ECHO/feature negotiation is binary data on CONTROL, while both
//! `CaptureSettingRequest` is sent as protobuf bytes with the text PPID on TEXT.
//! Capture requests are complete snapshots,
//! so no request is produced until the active remote screen's physical mode is
//! known from `ScreenSources`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use prost::Message as _;
use tokio::sync::{Notify, mpsc};

use crate::capability::{DualCapability, FrameQualityCapability};
use crate::media::{ConnectionMediaProfile, FrameRateChoice, LocalDisplayInfo, VideoCodec};
pub use crate::network_control::NetworkControlSnapshot;
use crate::performance::PerformanceMonitor;
pub use crate::remote_cursor::{CursorImage, RemoteCursor};
pub use crate::remote_input::MouseMode;

pub(crate) mod annotation;
mod display_settings;
mod display_topology;
pub use display_settings::{
    DisplayChangeRequest, DisplayChangeStatus, DisplayResolution, RemoteDisplayInfo,
    RemoteDisplayMode,
};
pub use display_topology::{DisplayTopologyAction, DisplayTopologyStatus, DisplayTopologySupport};

const VIDEO_QUALITY_FAST: i32 = 1;
const VIDEO_QUALITY_GENERAL: i32 = 2;
const VIDEO_QUALITY_HD: i32 = 3;
const VIDEO_QUALITY_BLURAY: i32 = 4;
const VIDEO_QUALITY_AUTO: i32 = 5;
const VIDEO_QUALITY_CUSTOM: i32 = 6;

const FPS_30: i32 = 1;
const FPS_60: i32 = 2;
const FPS_90: i32 = 3;
const FPS_144: i32 = 4;

const ACTION_TYPE_ECHO_REQUEST: i32 = 0;
const ACTION_TYPE_ECHO_RESPONSE: i32 = 1;
const CHROMA_420: i32 = 1;
const CHROMA_444: i32 = 3;
const RESOLUTION_DEFAULT: i32 = 1;
// Official VideoScreenState::buildCaptureConfig(-2): update existing tracks,
// not a physical monitor. Negative dimensions bypass changeResolution, and
// zero DPI bypasses SetDisplayDpi on the host. Never echo a stale monitor mode.
const EXISTING_SESSION_TRACKS: i32 = -2;
const UNCHANGED_PHYSICAL_DIMENSION: i32 = -1;
const DEFAULT_CUSTOM_BITRATE_MBPS: u32 = 30;
pub const MAX_CUSTOM_BITRATE_MBPS: u32 = 500;
const CAPTURE_RESULT_FPS_ADJUSTED: i32 = -3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamQuality {
    Auto,
    Original,
    High,
    Clear,
    Custom,
    Fast,
}

impl StreamQuality {
    const fn capability_quality(self) -> i32 {
        match self {
            Self::Auto => 0,
            Self::Fast => 1,
            Self::Clear => 2,
            Self::High => 3,
            Self::Original => 4,
            Self::Custom => 5,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Original => "原画 30M",
            Self::High => "超清 14M",
            Self::Clear => "高清 8M",
            Self::Custom => "自定义",
            Self::Fast => "低码率 1M",
        }
    }

    const fn protobuf(self) -> i32 {
        match self {
            Self::Auto => VIDEO_QUALITY_AUTO,
            Self::Original => VIDEO_QUALITY_BLURAY,
            Self::High => VIDEO_QUALITY_HD,
            Self::Clear => VIDEO_QUALITY_GENERAL,
            Self::Custom => VIDEO_QUALITY_CUSTOM,
            Self::Fast => VIDEO_QUALITY_FAST,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamControlProtocol {
    Negotiating,
    CaptureSetting,
    Unsupported,
}

impl StreamControlProtocol {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Negotiating => "协商中",
            Self::CaptureSetting => "CaptureSetting",
            Self::Unsupported => "串流协议不受支持",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamControlSettings {
    #[serde(default)]
    pub hdr: bool,
    #[serde(default)]
    pub true_color: bool,
    pub frame_rate: FrameRateChoice,
    pub quality: StreamQuality,
    pub custom_bitrate_mbps: u32,
}

pub(crate) fn default_auto_quality() -> i32 {
    VIDEO_QUALITY_GENERAL
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SavedStreamControl {
    pub settings: StreamControlSettings,
    pub auto_frame_quality: i32,
}

pub(crate) struct LoadedStreamControl {
    pub settings: Option<StreamControlSettings>,
    pub auto_frame_quality: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ViewingPreferenceUpdate {
    Settings(SavedStreamControl),
    AutoQuality(i32),
}

pub fn custom_bitrate_choices(limit: u32) -> Vec<u32> {
    let limit = limit.clamp(1, MAX_CUSTOM_BITRATE_MBPS);
    let mut choices: Vec<_> = (1..=20)
        .chain((25..=100).step_by(5))
        .chain((110..=200).step_by(10))
        .chain([250, 300, 350, 400, 500])
        .filter(|value| *value <= limit)
        .collect();
    if choices.last() != Some(&limit) {
        choices.push(limit);
    }
    choices
}

pub(crate) fn normalize_custom_bitrate(value: u32) -> u32 {
    custom_bitrate_choices(MAX_CUSTOM_BITRATE_MBPS)
        .into_iter()
        .rev()
        .find(|n| *n <= value)
        .unwrap_or(1)
}

/// User-facing state survives a room replacement, unlike PB sequence numbers,
/// screen snapshots, or the newly negotiated codec/transport.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamControlPreferences {
    pub(crate) settings: StreamControlSettings,
    pub(crate) custom_bitrate_limit: Option<u32>,
    audio: Option<crate::audio::AudioSettings>,
    auto_frame_quality: i32,
}

impl StreamControlPreferences {
    fn saved(self) -> SavedStreamControl {
        SavedStreamControl {
            settings: self.settings,
            auto_frame_quality: self.auto_frame_quality,
        }
    }
    pub(crate) fn initial_capture_quality(self) -> Result<(u64, u64, u64)> {
        let bitrate = if self.settings.quality == StreamQuality::Custom {
            if !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&self.settings.custom_bitrate_mbps) {
                bail!("自定义码率超出有效范围");
            }
            u64::from(
                self.settings
                    .custom_bitrate_mbps
                    .min(self.custom_bitrate_limit.unwrap_or(MAX_CUSTOM_BITRATE_MBPS)),
            ) * 1_000_000
        } else {
            0
        };
        Ok((
            u64::try_from(self.settings.quality.protobuf())?,
            u64::try_from(self.auto_frame_quality)?,
            bitrate,
        ))
    }

    pub(crate) fn from_saved(saved: LoadedStreamControl, profile: ConnectionMediaProfile) -> Self {
        let mut settings = saved.settings.unwrap_or(StreamControlSettings {
            true_color: false,
            hdr: false,
            quality: StreamQuality::Auto,
            frame_rate: frame_rate_choice(profile.stream_fps),
            custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
        });
        normalize_low_quality(&mut settings);
        settings.custom_bitrate_mbps = normalize_custom_bitrate(settings.custom_bitrate_mbps);
        Self {
            settings,
            custom_bitrate_limit: None,
            audio: None,
            auto_frame_quality: if settings.quality == StreamQuality::Auto {
                saved.auto_frame_quality
            } else {
                VIDEO_QUALITY_GENERAL
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteDisplayState {
    pub screen_id: i32,
    pub video_track_index: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteScreen {
    pub id: i32,
    pub name: String,
    pub primary: bool,
    pub video_track_index: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub display: RemoteDisplayInfo,
}

#[derive(Clone, Debug)]
pub struct StreamControlSnapshot {
    pub hdr_supported: bool,
    pub hdr_unavailable: Option<String>,
    pub custom_bitrate_limit: u32,
    pub auto_quality_label: String,
    pub true_color_supported: bool,
    pub true_color_unavailable: Option<String>,
    pub true_color_max_quality: Option<StreamQuality>,
    pub mouse_preference: MouseMode,
    pub mouse_mode: MouseMode,
    pub mouse_pending: bool,
    pub mouse_error: Option<String>,
    pub cursor_pending: bool,
    pub cursor_error: Option<String>,
    pub local_display: LocalDisplayInfo,
    pub settings: StreamControlSettings,
    pub remote_display: Option<RemoteDisplayState>,
    pub screens: Vec<RemoteScreen>,
    pub screens_generation: u64,
    pub topology: DisplayTopologyStatus,
    pub topology_support: DisplayTopologySupport,
    pub display_settings_supported: bool,
    pub dpi_settings_supported: bool,
    pub display_changes: std::collections::BTreeMap<i32, DisplayChangeStatus>,

    pub protocol: StreamControlProtocol,
    pub custom_bitrate_supported: bool,
    pub mouse_modes_supported: bool,
    pub control_channel_open: bool,
    pub text_channel_open: bool,
    pub pb_connected: bool,
    pub ready: bool,
    pub waiting_for: Option<&'static str>,
    pub pending_sequence: Option<i64>,
    pub pending_count: usize,
    pub last_applied_sequence: Option<i64>,
    pub last_error: Option<String>,
    pub last_notice: Option<String>,
    pub remote_notice: Option<&'static str>,
    pub persistence_error: Option<String>,
    pub network: NetworkControlSnapshot,
}

#[derive(Clone)]
pub struct StreamControlHandle {
    clipboard: crate::clipboard::Clipboard,
    files: Arc<crate::file_transfer::Transport>,
    mouse: crate::remote_input::RemoteInput,
    cursor: crate::remote_cursor::RemoteCursorState,
    audio: crate::audio::AudioPlayback,
    network: crate::network_control::NetworkControl,
    shared: Arc<Mutex<StreamControlState>>,
    outgoing: mpsc::UnboundedSender<OutgoingControlMessage>,
    echo_responses: mpsc::UnboundedSender<Vec<u8>>,
    protocol_changed: Arc<Notify>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PbMessageSource {
    Control,
    Text,
    Signal,
}

#[derive(Clone, Copy)]
pub(crate) struct PbHandshakeStatus {
    pub generation: u64,
    pub open: bool,
    pub connected: bool,
}

pub(crate) struct OutgoingControlMessage {
    pub annotation_generation: Option<u64>,
    pub sequence: i64,
    pub payload: Vec<u8>,
    pub protocol: StreamControlProtocol,
    pub completion: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
}

#[derive(Clone, Copy)]
struct CaptureSettingBaseline {
    requested_fps: u32,
    fps_count: u32,
    frame_quality: i32,
    auto_frame_quality: i32,
    cursor_capture: bool,
    chroma_format: i32,
    max_custom_bitrate: u32,
    enable_hdr: bool,
    codec_type: i32,
    max_scale_width: u32,
    max_scale_height: u32,
}

#[derive(Clone)]
struct ScreenBaseline {
    id: i32,
    name: String,
    primary: bool,
    video_track_index: i32,
    fps: u32,
    width: u32,
    height: u32,
    pixel_width: u32,
    pixel_height: u32,
    dpi_scale: u32,
    resolution_type: i32,
    display: RemoteDisplayInfo,
}

struct PendingCapturePreferences {
    sequence: i64,
    preferences: StreamControlPreferences,
    cursor_capture: bool,
    persist: bool,
}

struct StreamControlState {
    peer_clipboard: i32,
    clipboard_files_allowed: bool,
    /// Last reported value of the clipboard gate, so the diagnostic only fires
    /// when it changes.
    clipboard_ready_reported: bool,
    remote_upgrade: Option<crate::remote_upgrade::RemoteUpgrade>,
    annotation: annotation::Annotation,
    custom_bitrate_limit: u32,
    features: Option<crate::feature_ability::FeaturePolicy>,
    remote_notice: Option<(Instant, &'static str)>,
    preferred_mouse_mode: MouseMode,
    /// Take control automatically once the control channel is usable.
    auto_mouse_control: bool,
    /// Set when the viewer explicitly gives control back, so an automatic
    /// hand-over does not fight that choice on the next reconnect.
    auto_mouse_declined: bool,
    /// Keeps the "still waiting" diagnostic to one line per session.
    auto_mouse_reported: bool,
    remote_cursor: crate::remote_cursor::RemoteCursorState,
    peer_mouse_relative: Option<bool>,
    cursor_sync_needed: bool,
    cursor_desired_capture: bool,
    mouse_restore_point: Option<(i32, [f64; 2])>,
    mouse: crate::remote_input::RemoteInput,
    mouse_transport_connected: bool,
    cursor_pending: Option<(i64, bool, Instant)>,
    cursor_error: Option<String>,
    available_video_tracks: Vec<i32>,
    registered_video_tracks: Vec<i32>,
    track_registration: Option<(i64, Vec<i32>)>,
    track_registration_error: Option<String>,
    local_display: LocalDisplayInfo,
    active_video_track_index: i32,
    current_screen_id: i32,
    screens: Vec<ScreenBaseline>,
    screens_generation: u64,
    display_changes: display_settings::DisplayChanges,
    topology: display_topology::DisplayTopology,
    assistance: bool,

    settings: StreamControlSettings,
    baseline: CaptureSettingBaseline,
    capability: Option<DualCapability>,
    remote_display: Option<RemoteDisplayState>,
    control_channel_open: bool,
    text_channel_open: bool,
    protocol_generation: u64,
    pb_connected: bool,
    peer_capture_setting: u32,
    initial_capture_sync_sent: bool,
    viewing_enabled: bool,
    next_sequence: i64,
    pending_sequences: VecDeque<i64>,
    last_applied_sequence: Option<i64>,
    latest_requested_sequence: Option<i64>,
    last_error: Option<String>,
    last_notice: Option<String>,
    preference_updates: tokio::sync::watch::Sender<Option<ViewingPreferenceUpdate>>,
    confirmed_preferences: StreamControlPreferences,
    pending_capture_preferences: VecDeque<PendingCapturePreferences>,
    user_settings_requested: bool,
    persistence_error: Option<String>,
    audio_persistence_error: Option<String>,
    performance: PerformanceMonitor,
}

impl StreamControlHandle {
    pub(crate) fn new(
        profile: ConnectionMediaProfile,
        performance: PerformanceMonitor,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<OutgoingControlMessage>,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let (outgoing, receiver) = mpsc::unbounded_channel();
        let (echo_responses, echo_receiver) = mpsc::unbounded_channel();
        let frame_rate = frame_rate_choice(profile.stream_fps);
        let fps_count = profile
            .local_display
            .refresh_hz
            .clamp(1, profile.stream_fps.max(1));
        let mouse = crate::remote_input::RemoteInput::default();
        let cursor = crate::remote_cursor::RemoteCursorState::default();
        tracing::debug!(
            auto_mouse_control = profile.auto_mouse_control,
            "stream control created"
        );
        let state = StreamControlState {
            peer_clipboard: 0,
            clipboard_files_allowed: true,
            clipboard_ready_reported: false,
            remote_upgrade: None,
            annotation: Default::default(),
            custom_bitrate_limit: MAX_CUSTOM_BITRATE_MBPS,
            features: None,
            preferred_mouse_mode: MouseMode::Smart,
            auto_mouse_control: profile.auto_mouse_control,
            auto_mouse_declined: false,
            auto_mouse_reported: false,
            remote_notice: None,
            remote_cursor: cursor.clone(),
            peer_mouse_relative: None,
            cursor_sync_needed: false,
            cursor_desired_capture: true,
            mouse_restore_point: None,
            mouse: mouse.clone(),
            mouse_transport_connected: false,
            cursor_pending: None,
            cursor_error: None,
            available_video_tracks: Vec::new(),
            registered_video_tracks: Vec::new(),
            track_registration: None,
            track_registration_error: None,
            local_display: profile.local_display,
            active_video_track_index: 0,
            current_screen_id: 0,
            screens: Vec::new(),
            screens_generation: 0,
            display_changes: Default::default(),
            topology: Default::default(),
            assistance: false,

            settings: StreamControlSettings {
                true_color: false,
                hdr: false,
                frame_rate,
                quality: StreamQuality::Auto,
                custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
            },
            baseline: CaptureSettingBaseline {
                requested_fps: profile.stream_fps,
                fps_count,
                frame_quality: VIDEO_QUALITY_AUTO,
                auto_frame_quality: VIDEO_QUALITY_GENERAL,
                // Independent CursorShape coordinates are only sampled at
                // shape changes. Watching needs capture-side cursor motion.
                cursor_capture: true,
                chroma_format: CHROMA_420,
                max_custom_bitrate: 0,
                enable_hdr: false,
                codec_type: 0,
                // UU VideoScreenState ctor B9EDA0. B743A0/BA41F0 replace
                // these when negotiation data is available; without it the
                // original retains these defaults. Not hardware capability
                // or a user-selected output-resolution ceiling.
                max_scale_width: 1920,
                max_scale_height: 1080,
            },
            capability: None,
            remote_display: None,
            control_channel_open: false,
            text_channel_open: false,
            protocol_generation: 0,
            pb_connected: false,
            peer_capture_setting: 0,
            initial_capture_sync_sent: false,
            viewing_enabled: true,
            next_sequence: 1,
            pending_sequences: VecDeque::new(),
            last_applied_sequence: None,
            latest_requested_sequence: None,
            last_error: None,
            last_notice: None,
            preference_updates: tokio::sync::watch::channel(None).0,
            confirmed_preferences: StreamControlPreferences {
                custom_bitrate_limit: None,
                settings: StreamControlSettings {
                    true_color: false,
                    hdr: false,
                    frame_rate,
                    quality: StreamQuality::Auto,
                    custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
                },
                audio: None,
                auto_frame_quality: VIDEO_QUALITY_GENERAL,
            },
            pending_capture_preferences: VecDeque::new(),
            user_settings_requested: false,
            persistence_error: None,
            audio_persistence_error: None,
            performance,
        };
        state.performance.set_quality(viewing_quality_label(&state));
        let audio = crate::audio::AudioPlayback::new();
        audio.set_settings(crate::audio::AudioSettings {
            volume: 100,
            muted: profile.muted,
        });
        let clipboard = crate::clipboard::Clipboard::new();
        // The connection setting decides where the per-session 文件复制 switch
        // starts; the player can still turn it on or off afterwards.
        clipboard.set_files(profile.clipboard_files);
        (
            Self {
                clipboard,
                files: Arc::new(crate::file_transfer::Transport::default()),
                mouse,
                cursor,
                audio,
                network: crate::network_control::NetworkControl::new(),
                shared: Arc::new(Mutex::new(state)),
                outgoing,
                echo_responses,
                protocol_changed: Arc::new(Notify::new()),
            },
            receiver,
            echo_receiver,
        )
    }

    /// Last independent shape notification for a future local-input presenter.
    /// Its position is sampled at shape change, not continuous motion tracking.
    /// Watching uses capture-side composition instead of a stale local overlay.
    pub(crate) fn mouse(&self) -> &crate::remote_input::RemoteInput {
        &self.mouse
    }

    pub(crate) fn clipboard(&self) -> &crate::clipboard::Clipboard {
        &self.clipboard
    }

    pub(crate) fn file_transfer(&self) -> &Arc<crate::file_transfer::Transport> {
        &self.files
    }

    pub(crate) fn set_mouse_transport_ready(&self, connected: bool) {
        let mut state = lock(&self.shared);
        state.mouse_transport_connected = connected;
        if !connected {
            self.clipboard.suspend();
            state.annotation.disconnect();
            state.peer_mouse_relative = None;
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            // Reconnect always starts in View, even after a failed mode change.
            state.baseline.cursor_capture = true;
            state.initial_capture_sync_sent = false;
            if let Some((sequence, _, _)) = state.cursor_pending {
                fail_cursor_request(&mut state, sequence, "鼠标连接已中断".into());
                state
                    .pending_sequences
                    .retain(|pending| *pending != sequence);
                state
                    .pending_capture_preferences
                    .retain(|pending| pending.sequence != sequence);
            }
            state.mouse.set_ready(false);
        } else {
            state.mouse.set_ready(
                protocol(&state) == StreamControlProtocol::CaptureSetting
                    && state.control_channel_open
                    && state.text_channel_open,
            );
            self.maybe_auto_take_mouse(&mut state);
            self.maybe_send_initial_capture_sync(&mut state);
        }
    }

    pub(crate) fn mouse_screen(&self, track: i32) -> Option<(i32, u32, u32)> {
        let s = lock(&self.shared);
        let screen = s
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track && screen.id >= 0)?;
        Some((screen.id, screen.width, screen.height))
    }

    pub(crate) fn take_mouse_restore_point(&self, track: i32) -> Option<[f64; 2]> {
        let mut s = lock(&self.shared);
        let screen = s
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track)?
            .id;
        if s.mouse.mode() == MouseMode::Smart
            && s.mouse_restore_point.is_some_and(|(id, _)| id == screen)
        {
            s.mouse_restore_point.take().map(|(_, point)| point)
        } else {
            None
        }
    }

    pub fn remote_cursor(&self) -> Option<RemoteCursor> {
        self.cursor.snapshot()
    }
    pub(crate) fn remote_cursor_hidden(&self) -> bool {
        self.cursor.hidden()
    }

    /// Whether the host is drawing its own pointer into the captured frames.
    /// While it is not, nothing but this client can put a pointer on screen.
    pub(crate) fn remote_cursor_captured(&self) -> bool {
        lock(&self.shared).baseline.cursor_capture
    }

    pub fn snapshot(&self) -> StreamControlSnapshot {
        let mut state = lock(&self.shared);
        expire_cursor_request(&mut state);
        let active_protocol = protocol(&state);
        let mut network = self.network.snapshot();
        if !feature_supported(&state, crate::feature_ability::Feature::ManualTransfer) {
            network.available = false;
            network.unavailable_reason = Some("官方当前能力配置未开放手动中转");
        }
        let waiting_for = if !state.control_channel_open {
            Some("CONTROL 通道")
        } else if !state.text_channel_open {
            Some("TEXT 通道")
        } else if !state.pb_connected {
            Some("PB 特性协商")
        } else if state.remote_display.is_none() {
            Some("活动屏幕基线")
        } else if state.baseline.codec_type == 0 {
            Some("视频编码协商")
        } else {
            None
        };
        StreamControlSnapshot {
            hdr_supported: state.peer_capture_setting >= 6 && state.capability.is_some(),
            hdr_unavailable: format_proposal(&state, None, Some(true))
                .err()
                .map(|e| e.to_string()),
            custom_bitrate_limit: state.custom_bitrate_limit,
            auto_quality_label: format!(
                "自动（{}）",
                quality_name(state.baseline.auto_frame_quality)
            ),
            true_color_supported: color_supported(&state),
            true_color_unavailable: validate_color(&state, true)
                .err()
                .map(|error| error.to_string()),
            true_color_max_quality: state
                .capability
                .as_ref()
                .map(|cap| cap.select(3, state.settings.hdr, 0))
                .filter(|cap| cap.result == 0)
                .and_then(|cap| quality_from_capability(cap.max_frame_quality)),
            mouse_preference: state.preferred_mouse_mode,
            mouse_mode: state.mouse.mode(),
            mouse_pending: state.mouse.waiting_for_neutral(),
            mouse_error: state.mouse.error(),
            cursor_pending: state.cursor_pending.is_some(),
            cursor_error: state.cursor_error.clone(),
            local_display: state.local_display,
            settings: state.settings,
            remote_display: state.remote_display,
            screens_generation: state.screens_generation,
            display_settings_supported: display_settings::supported(&state),
            dpi_settings_supported: display_settings::supported(&state)
                && state.peer_capture_setting >= 5,
            display_changes: state.display_changes.status.clone(),
            topology: state.topology.status.clone(),
            topology_support: display_topology::support(&state),

            screens: state
                .screens
                .iter()
                .map(|screen| RemoteScreen {
                    id: screen.id,
                    name: screen.name.clone(),
                    primary: screen.primary,
                    video_track_index: screen.video_track_index,
                    width: screen.width,
                    height: screen.height,
                    refresh_hz: screen.fps,
                    display: screen.display.clone(),
                })
                .collect(),
            protocol: active_protocol,
            custom_bitrate_supported: custom_bitrate_supported(&state)
                && feature_supported(&state, crate::feature_ability::Feature::CustomBitrate),
            mouse_modes_supported: feature_supported(
                &state,
                crate::feature_ability::Feature::SmartMouse,
            ),
            control_channel_open: state.control_channel_open,
            text_channel_open: state.text_channel_open,
            pb_connected: state.pb_connected,
            ready: waiting_for.is_none()
                && active_protocol == StreamControlProtocol::CaptureSetting,
            waiting_for,
            pending_sequence: state.pending_sequences.back().copied(),
            pending_count: state.pending_sequences.len(),
            last_applied_sequence: state.last_applied_sequence,
            last_error: state.last_error.clone(),
            last_notice: state.last_notice.clone(),
            remote_notice: state
                .remote_notice
                .filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(3))
                .map(|(_, notice)| notice),
            persistence_error: state
                .persistence_error
                .clone()
                .or_else(|| state.audio_persistence_error.clone()),
            network,
        }
    }

    pub(crate) fn audio(&self) -> crate::audio::AudioPlayback {
        self.audio.clone()
    }

    pub(crate) fn preferences(&self) -> StreamControlPreferences {
        let state = lock(&self.shared);
        StreamControlPreferences {
            settings: state.confirmed_preferences.settings,
            custom_bitrate_limit: (state.custom_bitrate_limit < MAX_CUSTOM_BITRATE_MBPS)
                .then_some(state.custom_bitrate_limit),
            audio: Some(self.audio.settings()),
            auto_frame_quality: state.confirmed_preferences.auto_frame_quality,
        }
    }
    pub(crate) fn network_control(&self) -> crate::network_control::NetworkControl {
        self.network.clone()
    }

    pub fn set_relay_enabled(&self, enabled: bool) -> Result<()> {
        if !feature_supported(
            &lock(&self.shared),
            crate::feature_ability::Feature::ManualTransfer,
        ) {
            bail!("官方当前能力配置未开放手动中转");
        }
        self.network.request(enabled)
    }

    pub(crate) fn set_feature_policy(&self, policy: crate::feature_ability::FeaturePolicy) {
        self.clipboard
            .platform(if policy.is_windows() { 1 } else { 4 });
        lock(&self.shared).features = Some(policy);
    }

    pub(crate) fn set_remote_upgrade(&self, upgrade: crate::remote_upgrade::RemoteUpgrade) {
        lock(&self.shared).remote_upgrade = Some(upgrade);
    }

    pub(crate) fn remote_upgrade(&self) -> Option<crate::remote_upgrade::RemoteUpgrade> {
        lock(&self.shared).remote_upgrade.clone()
    }

    pub(crate) async fn stop_acquire_update(&self) -> Result<()> {
        let (complete, done) = tokio::sync::oneshot::channel();
        {
            let mut state = lock(&self.shared);
            ensure_ready(&state)?;
            if state.remote_upgrade.is_none() {
                bail!("当前会话不支持被控端更新");
            }
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            self.outgoing
                .send(OutgoingControlMessage {
                    annotation_generation: None,
                    sequence,
                    payload: PbControlMessage {
                        seq: 0,
                        timestamp: 0,
                        payload: Some(PbPayload::SimpleAction(PbSimpleAction {
                            action: 20,
                            args: String::new(),
                            params: None,
                        })),
                    }
                    .encode_to_vec(),
                    protocol: protocol(&state),
                    completion: Some(complete),
                })
                .map_err(|_| anyhow!("观看连接已关闭"))?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), done)
            .await
            .map_err(|_| anyhow!("延后安装请求发送超时"))?
            .map_err(|_| anyhow!("观看连接已关闭"))?
            .map_err(anyhow::Error::msg)
    }

    /// Ordinary desktop capture only. Negative IDs include all-screen actions
    /// and are never accepted from a single monitor/window operation.
    pub async fn set_screen_capture(&self, screen_id: i32, active: bool) -> Result<()> {
        if active {
            let state = lock(&self.shared);
            if screen_id != state.current_screen_id
                && !feature_supported(&state, crate::feature_ability::Feature::MultiScreen)
            {
                bail!("官方当前能力配置未开放多屏观看");
            }
        }
        if screen_id < 0
            || !self
                .snapshot()
                .screens
                .iter()
                .any(|screen| screen.id == screen_id)
        {
            bail!("显示器已不可用");
        }
        if active {
            self.ensure_video_tracks_registered().await?;
        }
        let (complete, done) = tokio::sync::oneshot::channel();
        {
            let mut state = lock(&self.shared);
            ensure_business_ready(&state)?;
            if screen_id < 0 || !state.screens.iter().any(|screen| screen.id == screen_id) {
                bail!("显示器已不可用");
            }
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            let payload = PbControlMessage {
                seq: sequence,
                timestamp: 0,
                payload: Some(PbPayload::SimpleAction(PbSimpleAction {
                    action: if active { 8 } else { 7 },
                    args: serde_json::json!({"screen_id":screen_id}).to_string(),
                    params: None,
                })),
            }
            .encode_to_vec();
            self.outgoing
                .send(OutgoingControlMessage {
                    annotation_generation: None,
                    sequence,
                    payload,
                    protocol: protocol(&state),
                    completion: Some(complete),
                })
                .map_err(|_| anyhow!("观看连接已关闭"))?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), done)
            .await
            .map_err(|_| anyhow!("屏幕采集请求发送超时"))?
            .map_err(|_| anyhow!("观看连接已关闭"))?
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn set_available_video_tracks(&self, mut tracks: Vec<i32>) {
        tracks.retain(|index| *index >= 0);
        tracks.sort_unstable();
        tracks.dedup();
        let mut state = lock(&self.shared);
        if state.available_video_tracks != tracks {
            tracing::info!(
                ?tracks,
                "negotiated remote video tracks available for capture registration"
            );
            state.available_video_tracks = tracks;
            state.track_registration_error = None;
        }
        self.maybe_register_video_tracks(&mut state);
    }

    async fn ensure_video_tracks_registered(&self) -> Result<()> {
        {
            let mut state = lock(&self.shared);
            ensure_business_ready(&state)?;
            if state.available_video_tracks.is_empty() {
                bail!("被控端未协商可用的视频轨道");
            }
            state.track_registration_error = None;
            self.maybe_register_video_tracks(&mut state);
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let state = lock(&self.shared);
                    if let Some(error) = &state.track_registration_error {
                        bail!("{error}");
                    }
                    if state.available_video_tracks == state.registered_video_tracks {
                        return Ok(());
                    }
                    if !state.pb_connected {
                        bail!("观看连接已断开");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| {
            let mut state = lock(&self.shared);
            state.track_registration = None;
            state.track_registration_error = Some("视频轨道注册未收到确认".into());
            anyhow!("视频轨道注册未收到确认")
        })?
    }

    fn maybe_register_video_tracks(&self, state: &mut StreamControlState) {
        if !state.pb_connected
            || !state.text_channel_open
            || state.available_video_tracks.is_empty()
            || state.available_video_tracks == state.registered_video_tracks
            || state.track_registration.is_some()
            || state.track_registration_error.is_some()
        {
            return;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        let tracks = state.available_video_tracks.clone();
        let request = PbRpcRequest {
            request_header: Some(PbRequestHeader {
                request_id: sequence,
            }),
            capture_setting: None,
            send_video_track: Some(PbSendVideoTrackRequest {
                video_track_index: tracks.clone(),
            }),
            ..Default::default()
        };
        let payload = encode_envelope(sequence, PbPayload::RpcRequest(request.encode_to_vec()));
        if self
            .outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: protocol(state),
                completion: None,
            })
            .is_ok()
        {
            state.track_registration = Some((sequence, tracks));
        } else {
            state.track_registration_error = Some("视频轨道注册发送失败".into());
        }
    }

    pub(crate) fn preference_updates(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<ViewingPreferenceUpdate>> {
        lock(&self.shared).preference_updates.subscribe()
    }

    pub(crate) fn set_persistence_error(&self, error: Option<String>) {
        lock(&self.shared).persistence_error = error;
    }

    pub(crate) fn set_audio_persistence_error(&self, error: Option<String>) {
        lock(&self.shared).audio_persistence_error = error;
    }

    pub(crate) fn restore_preferences(&self, preferences: StreamControlPreferences) -> Result<()> {
        let mut state = lock(&self.shared);
        if state.initial_capture_sync_sent || state.baseline.codec_type != 0 {
            bail!("串流偏好必须在新房间选择视频轨道之前恢复");
        }
        state.custom_bitrate_limit = preferences
            .custom_bitrate_limit
            .unwrap_or(MAX_CUSTOM_BITRATE_MBPS);
        set_requested_settings(&mut state, preferences.settings)?;
        state.baseline.auto_frame_quality = preferences.auto_frame_quality;
        state.confirmed_preferences = preferences;
        if let Some(audio) = preferences.audio {
            self.audio.set_settings(audio);
        }
        state.performance.set_quality(viewing_quality_label(&state));
        tracing::info!(
            ?preferences,
            "restored viewing preferences for a new room generation"
        );
        Ok(())
    }

    pub fn set_mouse_mode(&self, mode: MouseMode) -> Result<()> {
        let mut state = lock(&self.shared);
        // An explicit choice overrides the automatic hand-over, in both
        // directions, for the rest of this session.
        state.auto_mouse_declined = mode == MouseMode::View;
        let result = self.apply_mouse_mode(&mut state, mode);
        drop(state);
        self.mouse.repaint();
        result
    }

    fn apply_mouse_mode(&self, state: &mut StreamControlState, mode: MouseMode) -> Result<()> {
        expire_cursor_request(state);
        state.mouse_restore_point = None;
        if mode == MouseMode::View {
            // Local revocation never waits for remote settings.
            state.mouse.disable();
        } else {
            ensure_ready(state)?;
            if state.annotation.enabled || state.annotation.toggling() {
                bail!("请先关闭批注，再开启键鼠控制");
            }
            let (relative, _) = mouse_policy(state, mode);
            state.mouse.enable(mode, relative)?;
            state.preferred_mouse_mode = mode;
        }
        // Explicit choices may retry uncertain cursor capture; newer intent is
        // independent of an earlier cursor request still awaiting its response.
        state.cursor_sync_needed = true;
        self.refresh_mouse_policy(state);
        Ok(())
    }

    /// Hand control over as soon as it is possible, when the viewer asked for
    /// that in the connection settings and has not taken it back since.
    fn maybe_auto_take_mouse(&self, state: &mut StreamControlState) {
        if !state.auto_mouse_control
            || state.auto_mouse_declined
            || state.mouse.mode() != MouseMode::View
        {
            return;
        }
        if !state.viewing_enabled
            || state.annotation.enabled
            || state.annotation.toggling()
            || ensure_ready(state).is_err()
        {
            if !state.auto_mouse_reported {
                state.auto_mouse_reported = true;
                tracing::debug!(
                    viewing = state.viewing_enabled,
                    annotation = state.annotation.enabled,
                    ready = ensure_ready(state).is_ok(),
                    "自动键鼠控制等待连接就绪"
                );
            }
            return;
        }
        let mode = state.preferred_mouse_mode;
        let mode = if mode == MouseMode::View {
            MouseMode::Smart
        } else {
            mode
        };
        if let Err(error) = self.apply_mouse_mode(state, mode) {
            tracing::debug!(%error, "自动开启键鼠控制暂不可用");
        } else {
            tracing::info!(?mode, "已按连接设置自动开启键鼠控制");
        }
    }

    fn request_cursor_locked(&self, state: &mut StreamControlState, visible: bool) -> Result<i64> {
        expire_cursor_request(state);
        ensure_ready(state)?;
        let sequence = state.next_sequence;
        let mut baseline = state.baseline;
        baseline.cursor_capture = visible;
        // BECA40/FD9FF0 update desired capture immediately, independently of
        // ACKs. Later complete snapshots must carry this same current intent.
        state.baseline.cursor_capture = visible;
        let active_protocol = protocol(state);
        let payload = match active_protocol {
            StreamControlProtocol::CaptureSetting => encode_capture_setting(sequence, baseline)?,

            StreamControlProtocol::Negotiating => bail!("PB 特性协商尚未完成"),
            StreamControlProtocol::Unsupported => {
                bail!("对端不支持当前串流协议（需要CaptureSetting RPC）")
            }
        };
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.pending_sequences.push_back(sequence);
        state.cursor_pending = Some((sequence, visible, Instant::now()));
        state.initial_capture_sync_sent = true;
        state.cursor_error = None;
        state.last_error = None;
        state.last_notice = None;
        let result = self.send_locked(
            state,
            OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: active_protocol,
                completion: None,
            },
            Some(
                if visible {
                    "显示远端光标"
                } else {
                    "隐藏远端光标"
                }
                .into(),
            ),
        );
        if let Err(error) = &result {
            fail_cursor_request(state, sequence, error.to_string());
        }
        tracing::info!(sequence, visible, "remote cursor visibility requested");
        result
    }

    pub fn apply(&self, settings: StreamControlSettings) -> Result<i64> {
        self.apply_settings_locked(&mut lock(&self.shared), settings)
    }

    pub(crate) fn set_custom_bitrate_limit(&self, limit: Option<u32>) {
        lock(&self.shared).custom_bitrate_limit = limit
            .filter(|v| (1..=MAX_CUSTOM_BITRATE_MBPS).contains(v))
            .unwrap_or(MAX_CUSTOM_BITRATE_MBPS);
    }

    pub fn apply_color(
        &self,
        screen_id: i32,
        enabled: bool,
        quality: Option<StreamQuality>,
    ) -> Result<i64> {
        let mut state = lock(&self.shared);
        ensure_ready(&state)?;
        anyhow::ensure!(color_supported(&state), "当前会话不支持色彩切换");
        anyhow::ensure!(
            state.screens.iter().any(|screen| screen.id == screen_id),
            "显示器已断开"
        );
        let mut settings = state.settings;
        settings.true_color = enabled;
        if let Some(quality) = quality {
            settings.quality = quality;
        }
        self.apply_settings_target(&mut state, settings, Some(screen_id))
    }

    pub(crate) fn propose_format(
        &self,
        true_color: Option<bool>,
        hdr: Option<bool>,
    ) -> Result<StreamControlSettings> {
        format_proposal(&lock(&self.shared), true_color, hdr)
    }

    pub(crate) fn apply_format(
        &self,
        screen_id: i32,
        settings: StreamControlSettings,
    ) -> Result<i64> {
        self.apply_settings_target(&mut lock(&self.shared), settings, Some(screen_id))
    }

    fn apply_settings_locked(
        &self,
        state: &mut StreamControlState,
        settings: StreamControlSettings,
    ) -> Result<i64> {
        self.apply_settings_target(state, settings, None)
    }

    fn apply_settings_target(
        &self,
        mut state: &mut StreamControlState,
        settings: StreamControlSettings,
        screen: Option<i32>,
    ) -> Result<i64> {
        {
            expire_cursor_request(&mut state);
            ensure_ready(&state)?;
            let active_protocol = protocol(&state);
            if matches!(settings.quality, StreamQuality::Custom)
                && !custom_bitrate_supported(&state)
            {
                bail!("被控端不支持运行时自定义码率");
            }
            anyhow::ensure!(
                settings.quality != StreamQuality::Custom
                    || settings.custom_bitrate_mbps <= state.custom_bitrate_limit
                    || (state.settings.quality == StreamQuality::Custom
                        && settings.custom_bitrate_mbps == state.settings.custom_bitrate_mbps),
                "当前连接自定义码率最高为 {} Mbps",
                state.custom_bitrate_limit
            );
            let quality_changed = settings.quality != state.settings.quality;
            let color_changed = settings.true_color != state.settings.true_color;
            let hdr_changed = settings.hdr != state.settings.hdr;
            if hdr_changed {
                anyhow::ensure!(state.peer_capture_setting >= 6, "当前会话不支持 HDR 切换");
                if settings.hdr {
                    ensure_hdr_displays(&state)?;
                }
            }
            if color_changed {
                anyhow::ensure!(color_supported(&state), "当前会话不支持色彩切换");
            }
            let selected = if color_changed
                || hdr_changed
                || (quality_changed && !matches!(settings.quality, StreamQuality::Custom))
            {
                validate_format(&state, settings.quality, settings.true_color, settings.hdr)?
            } else {
                None
            };
            let previous_quality = state.settings.quality;
            set_requested_settings(&mut state, settings)?;
            if settings.quality == StreamQuality::Auto
                && matches!(
                    previous_quality,
                    StreamQuality::Clear | StreamQuality::High | StreamQuality::Original
                )
            {
                state.baseline.auto_frame_quality = previous_quality.protobuf();
            }
            if let Some(selected) = selected {
                apply_codec_limits(&mut state, selected);
            }
            constrain_auto_quality(&mut state);
            tracing::debug!(
                screen_id = EXISTING_SESSION_TRACKS,
                enable_hdr = state.baseline.enable_hdr,
                requested_fps = state.baseline.requested_fps,
                fps_count = state.baseline.fps_count,
                frame_quality = state.baseline.frame_quality,
                auto_frame_quality = state.baseline.auto_frame_quality,
                max_scale_width = state.baseline.max_scale_width,
                max_scale_height = state.baseline.max_scale_height,
                chroma_format = state.baseline.chroma_format,
                codec_type = state.baseline.codec_type,
                max_custom_bitrate = state.baseline.max_custom_bitrate,
                "runtime capture-setting snapshot prepared"
            );

            let sequence = state.next_sequence;
            let payload = match active_protocol {
                StreamControlProtocol::CaptureSetting => {
                    let mut request = capture_setting_request(state.baseline)?;
                    if let Some(id) = screen {
                        let screen = state
                            .screens
                            .iter()
                            .find(|s| s.id == id)
                            .ok_or_else(|| anyhow!("显示器已断开"))?;
                        request.screen_id = id;
                        request.resolution_width = screen.width as i32;
                        request.resolution_height = screen.height as i32;
                        request.resolution_pixel_width = screen.pixel_width as i32;
                        request.resolution_pixel_height = screen.pixel_height as i32;
                    }
                    encode_capture_request(sequence, request)
                }

                StreamControlProtocol::Negotiating => bail!("PB 特性协商尚未完成"),
                StreamControlProtocol::Unsupported => {
                    bail!("对端不支持当前串流协议（需要CaptureSetting RPC）")
                }
            };
            state.next_sequence = state.next_sequence.wrapping_add(1);
            state.pending_sequences.push_back(sequence);
            // An explicit full snapshot also satisfies initial synchronization,
            // including a supported choice after a rejected restored preference.
            state.initial_capture_sync_sent = true;
            state.last_error = None;
            state.last_notice = None;
            let outgoing = OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: active_protocol,
                completion: None,
            };
            let switch_target = format!(
                "{} · {}",
                settings.quality.label(),
                settings.frame_rate.label(state.local_display)
            );
            state.user_settings_requested = true;
            let sent = self.send_locked(&mut state, outgoing, Some(switch_target))?;
            Ok(sent)
        }
    }

    fn send_locked(
        &self,
        state: &mut StreamControlState,
        outgoing: OutgoingControlMessage,
        target: Option<String>,
    ) -> Result<i64> {
        let sequence = outgoing.sequence;
        state.latest_requested_sequence = Some(sequence);
        if let Some(target) = target {
            state.performance.begin_stream_switch(sequence, target);
        }
        // Enqueue before releasing the state lock: an automatic request cannot
        // overtake a later explicit user choice on this reliable channel.
        if self.outgoing.send(outgoing).is_err() {
            state.initial_capture_sync_sent = false;
            state.pending_sequences.clear();
            state.pending_capture_preferences.clear();
            restore_confirmed_capture(state);
            state.last_error = Some("串流设置发送任务已经停止".to_owned());
            state
                .performance
                .fail_stream_switch(sequence, "串流设置发送任务已经停止");
            bail!("串流设置发送任务已经停止");
        }
        state
            .pending_capture_preferences
            .push_back(PendingCapturePreferences {
                sequence,
                preferences: StreamControlPreferences {
                    custom_bitrate_limit: (state.custom_bitrate_limit < MAX_CUSTOM_BITRATE_MBPS)
                        .then_some(state.custom_bitrate_limit),
                    settings: state.settings,
                    audio: None,
                    auto_frame_quality: state.baseline.auto_frame_quality,
                },
                cursor_capture: state.baseline.cursor_capture,
                persist: state.user_settings_requested,
            });
        Ok(sequence)
    }

    pub(crate) fn poll_timeouts(&self) {
        let mut state = lock(&self.shared);
        self.drive_display_changes(&mut state);
        expire_cursor_request(&mut state);
        // Readiness can complete through several different paths; checking here
        // means the hand-over does not depend on which one finished last.
        self.maybe_auto_take_mouse(&mut state);
        self.refresh_mouse_policy(&mut state);
    }

    pub(crate) fn set_data_channel_open(&self, label: &str, open: bool) {
        let mut state = lock(&self.shared);
        match label {
            "CONTROL_DATA_CHANNEL" => {
                if state.control_channel_open != open {
                    state.protocol_generation = state.protocol_generation.wrapping_add(1);
                    state.pb_connected = false;
                    state.initial_capture_sync_sent = false;
                }
                state.control_channel_open = open;
            }
            "TEXT_DATA_CHANNEL" => {
                state.text_channel_open = open;
                if !open {
                    state.initial_capture_sync_sent = false;
                }
            }
            _ => return,
        }
        if !open {
            self.clipboard.suspend();
            state.peer_clipboard = 0;
            state.annotation.disconnect();
            state.topology.disconnect();
            state
                .display_changes
                .cancel_all("连接已断开，显示设置未确认");
            state.peer_mouse_relative = None;
            state.cursor_sync_needed = false;
            state.cursor_desired_capture = true;
            state.mouse_restore_point = None;
            state.baseline.cursor_capture = true;
            state.mouse.set_ready(false);
            if let Some((sequence, _, _)) = state.cursor_pending {
                fail_cursor_request(&mut state, sequence, "连接已断开，光标设置未确认".into());
            }
            state.registered_video_tracks.clear();
            state.track_registration = None;
            state.track_registration_error = None;
            if let Some(sequence) = state.pending_sequences.back().copied() {
                state
                    .performance
                    .fail_stream_switch(sequence, format!("{label} 通道已关闭"));
            }
            state.pending_sequences.clear();
            state.pending_capture_preferences.clear();
        }
        self.maybe_send_initial_capture_sync(&mut state);
        if open
            && protocol(&state) == StreamControlProtocol::CaptureSetting
            && state.control_channel_open
            && state.text_channel_open
        {
            state.mouse.set_ready(state.mouse_transport_connected);
            self.maybe_auto_take_mouse(&mut state);
        }
        drop(state);
        if !open {
            self.cursor.clear();
        }
        self.protocol_changed.notify_one();
    }

    pub(crate) fn set_video_stream(&self, codec: VideoCodec, video_track_index: i32) {
        let mut state = lock(&self.shared);
        state.active_video_track_index = video_track_index;
        // The initial CaptureSetting may already have selected another codec
        // before the first RTP packet. Do not overwrite that intent with an
        // old-format packet still in flight during the change.
        if !state.initial_capture_sync_sent {
            state.baseline.codec_type = match codec {
                VideoCodec::H264 => 1,
                VideoCodec::H265 => 2,
            };
        }
        refresh_active_screen(&mut state);
        self.maybe_send_initial_capture_sync(&mut state);
    }

    pub(crate) fn set_capability(&self, capability: DualCapability) {
        tracing::info!(capability = %serde_json::to_string(&capability).expect("integer capability fields serialize"),
            "official dual capability model updated");
        let mut state = lock(&self.shared);
        state.capability = Some(capability);
        // Complete a pending initial configuration once its actual inputs
        // exist. After it is submitted, late/duplicate capabilities do not
        // trigger stream changes or decoder restarts.
        self.maybe_send_initial_capture_sync(&mut state);
    }

    pub(crate) fn select_viewed_video_track(&self, video_track_index: i32) {
        let mut state = lock(&self.shared);
        state.active_video_track_index = video_track_index;
        refresh_active_screen(&mut state);
    }

    pub(crate) fn mark_send_failed(&self, sequence: i64, error: &str) {
        let mut state = lock(&self.shared);
        if self.topology_send_failed(&mut state, sequence, error) {
            return;
        }
        if state.display_changes.send_failed(sequence, error) {
            return;
        }
        state
            .pending_capture_preferences
            .retain(|pending| pending.sequence != sequence);
        fail_cursor_request(&mut state, sequence, error.to_owned());
        if state
            .track_registration
            .as_ref()
            .is_some_and(|(seq, _)| *seq == sequence)
        {
            state.track_registration = None;
            state.track_registration_error = Some(format!("视频轨道注册发送失败：{error}"));
            return;
        }
        state
            .pending_sequences
            .retain(|pending| *pending != sequence);
        if state.latest_requested_sequence != Some(sequence) {
            return;
        }
        state.last_error = Some(error.to_owned());
        restore_confirmed_capture(&mut state);
        state.performance.fail_stream_switch(sequence, error);
    }

    pub(crate) fn protocol_notifications(&self) -> Arc<Notify> {
        Arc::clone(&self.protocol_changed)
    }

    pub(crate) fn set_viewing_enabled(&self, enabled: bool) {
        let mut state = lock(&self.shared);
        if enabled && !state.viewing_enabled {
            state.initial_capture_sync_sent = false;
        }
        state.viewing_enabled = enabled;
        if !enabled {
            self.clipboard.suspend();
        }
        if enabled {
            self.maybe_send_initial_capture_sync(&mut state);
        }
    }

    pub(crate) fn handshake_status(&self) -> PbHandshakeStatus {
        let state = lock(&self.shared);
        PbHandshakeStatus {
            generation: state.protocol_generation,
            open: state.control_channel_open,
            connected: state.pb_connected,
        }
    }

    pub(crate) fn mark_pb_handshake_timeout(&self) {
        let mut state = lock(&self.shared);
        if !state.pb_connected {
            // D4C410 only stops retrying. No ECHO response means no negotiated
            // feature version; no protocol is selected without a response.
            state.last_error = Some("PB 特性协商超时；画面继续播放，串流设置尚未就绪".to_owned());
        }
    }

    pub(crate) fn handle_protocol_message(
        &self,
        payload: &[u8],
        source: PbMessageSource,
    ) -> Result<()> {
        if payload.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{') {
            return self.handle_mouse_command(payload, source);
        }
        let message = PbControlMessage::decode(payload)
            .map_err(|error| anyhow!("decode UU protobuf domain message: {error}"))?;
        if let Some(PbPayload::SystemMetrics(bytes)) = &message.payload {
            self.files.metrics(bytes)?;
        }
        if let Some(PbPayload::SystemStateChange(bytes)) = &message.payload {
            if let Some(files) = ClipboardPermissionState::decode(bytes.as_slice())?.files {
                lock(&self.shared).clipboard_files_allowed = files.enabled;
            }
            let was_hidden = self.cursor.hidden();
            let result = self.cursor.receive(bytes);
            let mut state = lock(&self.shared);
            if was_hidden
                && !self.cursor.hidden()
                && smart_mouse_requested(&state)
                && let Some(cursor) = self.cursor.snapshot()
                && let Some(point) = cursor.sampled_position
            {
                state.mouse_restore_point = Some((cursor.screen_id, point));
            }
            self.refresh_mouse_policy(&mut state);
            drop(state);
            self.mouse.repaint();
            return result;
        }
        let mut echo_response = None;
        let mut handshake_changed = false;
        let mut state = lock(&self.shared);
        match message.payload {
            Some(PbPayload::SimpleAction(action)) if source == PbMessageSource::Control => {
                // F91D10 dispatches CONTROL SimpleAction to the ECHO handler;
                // TEXT and signal_app_data only reach the business observers.
                match action.action {
                    ACTION_TYPE_ECHO_REQUEST | ACTION_TYPE_ECHO_RESPONSE => {
                        if let Some(PbSimpleActionParams::FeatureFlag(flags)) = action.params {
                            self.files
                                .capabilities(flags.file_transfer_ftp, flags.file_transfer_ftp2);
                            state.peer_clipboard = flags.clipboard;
                            state.peer_capture_setting = flags.capture_setting.max(0) as u32;
                        }
                        if action.action == ACTION_TYPE_ECHO_REQUEST {
                            echo_response =
                                Some(encode_pb_echo_response(message.seq, message.timestamp));
                            tracing::debug!(
                                request_sequence = message.seq,
                                capture_setting_feature_level = state.peer_capture_setting,
                                "official protobuf ECHO_REQUEST received"
                            );
                        } else {
                            state.pb_connected = true;
                            state.mouse.set_ready(
                                state.mouse_transport_connected
                                    && state.control_channel_open
                                    && state.text_channel_open
                                    && protocol(&state) == StreamControlProtocol::CaptureSetting,
                            );
                            handshake_changed = true;
                            self.maybe_auto_take_mouse(&mut state);
                            state.last_error = (protocol(&state)
                                == StreamControlProtocol::Unsupported)
                                .then(|| "对端不支持当前串流协议（需要CaptureSetting RPC）".into());
                            tracing::info!(
                                capture_setting_feature_level = state.peer_capture_setting,
                                protocol = protocol(&state).label(),
                                "official protobuf feature negotiation completed"
                            );
                        }
                    }
                    _ => {}
                }
            }
            Some(PbPayload::ReportError(report)) => {
                if let Some(upgrade) = &state.remote_upgrade {
                    upgrade.receive(report.error_code);
                    if report.error_code == -6 {
                        state.mouse.disable();
                        state.annotation.disconnect();
                    }
                }
                // Upgrade notifications are attached only to a normal owned
                // Windows viewing session; they never execute an inbound updater.
                if report.error_code == -9 {
                    state.remote_notice = Some((
                        Instant::now(),
                        "被控端系统会话发生变化，画面会有短暂卡顿，请稍候",
                    ));
                }
                tracing::debug!(
                    action = report.action,
                    code = report.error_code,
                    type_value = report.type_value,
                    "remote capture status received"
                );
            }
            Some(PbPayload::ReportQosStats(qos)) => {
                if state.baseline.frame_quality == VIDEO_QUALITY_AUTO
                    && let Some(auto_quality) = reported_auto_quality(qos.video_quality)
                {
                    state.baseline.auto_frame_quality = auto_quality;
                    if state.confirmed_preferences.settings == state.settings
                        && state.pending_capture_preferences.is_empty()
                        && state.confirmed_preferences.auto_frame_quality != auto_quality
                    {
                        state.confirmed_preferences.auto_frame_quality = auto_quality;
                        let update = if state.user_settings_requested {
                            ViewingPreferenceUpdate::Settings(state.confirmed_preferences.saved())
                        } else {
                            ViewingPreferenceUpdate::AutoQuality(auto_quality)
                        };
                        state.preference_updates.send_replace(Some(update));
                    }
                    state.performance.set_quality(viewing_quality_label(&state));
                }
                tracing::debug!(encoder_type = %qos.encoder_type, capture_type = %qos.capture_type, probe_bps = qos.probe_bps, video_quality = qos.video_quality,
                    fast_bitrate = qos.fast_bitrate, general_bitrate = qos.general_bitrate, hd_bitrate = qos.hd_bitrate, bluray_bitrate = qos.bluray_bitrate,
                    "official QoS quality status received");
            }
            Some(PbPayload::Screens(screens)) => update_screen_baseline(&mut state, screens),
            Some(PbPayload::CaptureSettingSync(bytes)) => {
                // 4.40 F6B150/F62360 -> 1410F2780 (ordinary video) has no
                // tag-25 state consumer. EC6650 is SecondScreenSettingsModel.
                // Keep the oneof tag, but do not import that module's state.
                tracing::debug!(
                    ?source,
                    seq = message.seq,
                    bytes = bytes.len(),
                    "ignored non-viewer CaptureSettingSync; viewing settings unchanged"
                );
            }
            Some(PbPayload::RpcResponse(response)) => {
                if let Some(header) = response.response_header {
                    if !self.handle_topology_response(
                        &mut state,
                        header.request_id,
                        response.payload.as_ref(),
                    ) {
                        match response.payload {
                            Some(PbRpcResponsePayload::DrawResp(draw)) => {
                                state.annotation.response(header.request_id, draw)
                            }
                            Some(PbRpcResponsePayload::CaptureSetting(capture)) => {
                                apply_capture_setting_response(
                                    &mut state,
                                    header.request_id,
                                    capture,
                                )
                            }
                            Some(PbRpcResponsePayload::SendVideoTrackRsp(result))
                                if state
                                    .track_registration
                                    .as_ref()
                                    .is_some_and(|(seq, _)| *seq == header.request_id) =>
                            {
                                let (_, tracks) = state
                                    .track_registration
                                    .take()
                                    .expect("matching registration");
                                if result.error_code == 0 {
                                    tracing::info!(
                                        ?tracks,
                                        "remote video track pool registration confirmed"
                                    );
                                    state.registered_video_tracks = tracks;
                                } else {
                                    state.track_registration_error = Some(format!(
                                        "视频轨道注册被拒绝（{}）",
                                        result.error_code
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        self.maybe_send_initial_capture_sync(&mut state);
        self.drive_display_changes(&mut state);
        self.refresh_mouse_policy(&mut state);
        drop(state);
        if handshake_changed {
            self.protocol_changed.notify_one();
        }
        if let Some(response) = echo_response {
            self.echo_responses
                .send(response)
                .map_err(|_| anyhow!("protobuf ECHO_RESPONSE sender has stopped"))?;
        }
        Ok(())
    }

    fn handle_mouse_command(&self, payload: &[u8], source: PbMessageSource) -> Result<()> {
        if source != PbMessageSource::Control {
            return Ok(());
        }
        anyhow::ensure!(payload.len() <= 16 * 1024, "mouse command too large");
        let value: serde_json::Value = serde_json::from_slice(payload)?;
        if value.get("action").and_then(|v| v.as_str()) != Some("special_game_mouse") {
            return Ok(());
        }
        let mode = value
            .get("mouse_mode")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("invalid mouse mode"))?;
        let force = value
            .get("force_mode")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow!("invalid force mode"))?;
        let x = value.get("coordinate_x_scale");
        let y = value.get("coordinate_y_scale");
        let (relative, restore) = match mode {
            0 | 1 => {
                anyhow::ensure!(
                    force && x.is_none() && y.is_none(),
                    "invalid forced mouse command"
                );
                (Some(mode == 1), None)
            }
            2 => {
                anyhow::ensure!(
                    !force || (x.is_none() && y.is_none()),
                    "invalid mouse restore command"
                );
                let restore = match (x, y) {
                    (None, None) => None,
                    (Some(x), Some(y)) => {
                        let x = x
                            .as_f64()
                            .ok_or_else(|| anyhow!("invalid mouse restore x"))?;
                        let y = y
                            .as_f64()
                            .ok_or_else(|| anyhow!("invalid mouse restore y"))?;
                        anyhow::ensure!(
                            x.is_finite()
                                && y.is_finite()
                                && (0.0..=1.0).contains(&x)
                                && (0.0..=1.0).contains(&y),
                            "mouse restore outside screen"
                        );
                        Some([x, y])
                    }
                    _ => bail!("mouse restore coordinates must appear together"),
                };
                (None, restore)
            }
            _ => bail!("unknown mouse mode"),
        };
        let mut state = lock(&self.shared);
        state.peer_mouse_relative = relative;
        state.mouse_restore_point = if smart_mouse_requested(&state) {
            restore.and_then(|point| state.remote_display.map(|screen| (screen.screen_id, point)))
        } else {
            None
        };
        self.refresh_mouse_policy(&mut state);
        drop(state);
        self.mouse.repaint();
        Ok(())
    }

    fn refresh_mouse_policy(&self, state: &mut StreamControlState) {
        let clipboard_ready = state.viewing_enabled
            && state.pb_connected
            && state.control_channel_open
            && state.text_channel_open
            && state.mouse_transport_connected
            && state.peer_clipboard >= 1
            && state.mouse.mode() != MouseMode::View;
        // Clipboard sync is gated on seven separate conditions, and a session
        // that never syncs looks identical whichever one is missing.
        if clipboard_ready != state.clipboard_ready_reported {
            state.clipboard_ready_reported = clipboard_ready;
            tracing::debug!(
                ready = clipboard_ready,
                viewing = state.viewing_enabled,
                pb = state.pb_connected,
                control = state.control_channel_open,
                text = state.text_channel_open,
                mouse_transport = state.mouse_transport_connected,
                peer = state.peer_clipboard,
                mode = ?state.mouse.mode(),
                files_allowed = state.clipboard_files_allowed,
                "剪贴板同步条件"
            );
        }
        self.clipboard.policy(
            clipboard_ready,
            state.peer_clipboard >= 2 && state.clipboard_files_allowed,
        );
        if !state.viewing_enabled {
            return;
        }
        let mode = state.mouse.mode();
        let (relative, wanted) = mouse_policy(state, mode);
        // Relative motion is only correct while this client holds the pointer.
        // A window that could not take it says so, and absolute positioning is
        // then the only honest way to aim, whatever the policy would prefer.
        let relative = relative && state.mouse.relative_available();
        if mode == MouseMode::Smart && state.mouse.relative_mode() != relative {
            state.mouse.set_relative_mode(relative);
        }
        if state.cursor_desired_capture != wanted {
            state.cursor_desired_capture = wanted;
            state.cursor_sync_needed = true;
        }
        if !state.cursor_sync_needed || ensure_ready(state).is_err() {
            return;
        }
        state.cursor_sync_needed = false;
        if wanted == state.baseline.cursor_capture && state.cursor_error.is_none() {
            return;
        }
        // Failure does not revoke input or retry forever. A new policy change
        // or explicit choice is required before submitting again.
        if let Err(error) = self.request_cursor_locked(state, wanted) {
            state.cursor_error = Some(error.to_string());
        }
    }

    fn maybe_send_initial_capture_sync(&self, state: &mut StreamControlState) {
        if !state.viewing_enabled {
            return;
        }
        self.maybe_register_video_tracks(state);
        let outgoing = match prepare_initial_capture_sync(state) {
            Ok(Some(outgoing)) => outgoing,
            Ok(None) => return,
            Err(error) => {
                state.last_error = Some(format!("初始串流设置未发送：{error}"));
                tracing::warn!(%error, "failed to prepare official initial capture-setting sync");
                return;
            }
        };
        let _ = self.send_locked(state, outgoing, None);
    }
}

fn set_requested_settings(
    state: &mut StreamControlState,
    mut settings: StreamControlSettings,
) -> Result<()> {
    normalize_low_quality(&mut settings);
    if settings.quality == StreamQuality::Custom
        && !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.custom_bitrate_mbps)
    {
        bail!("自定义码率必须在 1..={MAX_CUSTOM_BITRATE_MBPS} Mbps 之间");
    }
    let requested_fps = settings.frame_rate.value(state.local_display);
    state.baseline.requested_fps = requested_fps;
    state.baseline.fps_count = state
        .local_display
        .refresh_hz
        .clamp(1, requested_fps.max(1));
    state.baseline.frame_quality = settings.quality.protobuf();
    state.baseline.enable_hdr = settings.hdr;
    state.baseline.chroma_format = if settings.true_color {
        CHROMA_444
    } else {
        CHROMA_420
    };
    state.baseline.max_custom_bitrate = match settings.quality {
        StreamQuality::Custom => {
            settings.custom_bitrate_mbps.min(state.custom_bitrate_limit) * 1_000_000
        }
        _ => 0,
    };
    state.settings = settings;
    Ok(())
}

fn normalize_low_quality(settings: &mut StreamControlSettings) {
    // Current 4.40.1 buildCaptureConfig also normalizes an actual low-tier
    // capability fallback to Custom 1M; this is not a separate menu entry.
    if settings.quality == StreamQuality::Fast {
        settings.quality = StreamQuality::Custom;
        settings.custom_bitrate_mbps = 1;
    }
}

pub(crate) fn encode_pb_echo_request() -> Vec<u8> {
    encode_pb_echo(ACTION_TYPE_ECHO_REQUEST, String::new(), 0, 0)
}

pub(crate) fn encode_read_only_feature_flags() -> Vec<u8> {
    PbFeatureFlag::read_only_viewer().encode_to_vec()
}

fn encode_pb_echo_response(sequence: i64, timestamp: i64) -> Vec<u8> {
    encode_pb_echo(
        ACTION_TYPE_ECHO_RESPONSE,
        format!("{{ \"seq\" : {sequence} }}"),
        sequence,
        timestamp,
    )
}

fn encode_pb_echo(action: i32, args: String, seq: i64, timestamp: i64) -> Vec<u8> {
    PbControlMessage {
        seq,
        timestamp,
        payload: Some(PbPayload::SimpleAction(PbSimpleAction {
            action,
            args,
            params: Some(PbSimpleActionParams::FeatureFlag(
                PbFeatureFlag::read_only_viewer(),
            )),
        })),
    }
    .encode_to_vec()
}

fn prepare_initial_capture_sync(
    state: &mut StreamControlState,
) -> Result<Option<OutgoingControlMessage>> {
    if state.initial_capture_sync_sent
        || !state.control_channel_open
        || !state.text_channel_open
        || !state.pb_connected
        || state.remote_display.is_none()
    {
        return Ok(None);
    }
    let active_protocol = protocol(state);
    if matches!(state.settings.quality, StreamQuality::Custom) && !custom_bitrate_supported(&state)
    {
        bail!("新连接的被控端不支持自定义码率，不能恢复该选择");
    }
    if (state.settings.hdr || state.settings.true_color) && state.capability.is_none() {
        return Ok(None);
    }
    if let Some(cap) = &state.capability {
        let original = state.settings;
        let hdr_allowed = ensure_hdr_displays(state).is_ok();
        let color_allowed = color_supported(state);
        let candidates = [
            (original.true_color, original.hdr),
            (false, original.hdr),
            (original.true_color, false),
            (false, false),
        ];
        if let Some((color, hdr)) = candidates.into_iter().find(|(color, hdr)| {
            (!*hdr || hdr_allowed)
                && (!*color || color_allowed)
                && cap.select(if *color { 3 } else { 1 }, *hdr, 0).result == 0
        }) {
            if color != original.true_color || hdr != original.hdr {
                let mut settings = original;
                settings.true_color = color;
                settings.hdr = hdr;
                set_requested_settings(state, settings)?;
                state.last_notice =
                    Some("当前设备组合无法恢复原色彩/HDR选择，本次连接已使用可用模式".into());
            }
        }
    }
    if let Some(capability) = &state.capability {
        let requested = state.settings.quality.capability_quality();
        let selected = capability.select(
            state.baseline.chroma_format as u8,
            state.settings.hdr,
            requested,
        );
        if selected.result != 0 {
            bail!(
                "双端没有可用的所选色彩视频格式（协商结果{}）",
                selected.result
            );
        }
        if !matches!(requested, 0 | 5) && selected.max_frame_quality < requested {
            let mut settings = state.settings;
            settings.quality =
                quality_from_capability(selected.max_frame_quality).ok_or_else(|| {
                    anyhow!("invalid negotiated quality {}", selected.max_frame_quality)
                })?;
            set_requested_settings(state, settings)?;
            state.last_notice = Some(format!(
                "初始画质按双端能力调整为{}",
                settings.quality.label()
            ));
        }
        apply_codec_limits(state, selected);
    }
    // Submit from screen state and negotiated capabilities, independently
    // of first-frame delivery. The evidence is in the controller-route doc.
    // If neither negotiation nor RTP supplied a codec, keep waiting rather
    // than inventing a default codec or an incomplete RPC.
    if state.baseline.codec_type == 0 {
        return Ok(None);
    }
    constrain_auto_quality(state);
    let baseline = state.baseline;
    let sequence = state.next_sequence;
    let payload = match active_protocol {
        StreamControlProtocol::CaptureSetting => encode_capture_setting(sequence, baseline)?,

        StreamControlProtocol::Negotiating | StreamControlProtocol::Unsupported => return Ok(None),
    };
    state.next_sequence = state.next_sequence.wrapping_add(1);
    state.pending_sequences.push_back(sequence);
    state.initial_capture_sync_sent = true;
    tracing::info!(
        sequence,
        protocol = active_protocol.label(),
        requested_fps = baseline.requested_fps,
        fps_count = baseline.fps_count,
        frame_quality = baseline.frame_quality,
        auto_frame_quality = baseline.auto_frame_quality,
        max_custom_bitrate = baseline.max_custom_bitrate,
        screen_id = EXISTING_SESSION_TRACKS,
        physical_width = UNCHANGED_PHYSICAL_DIMENSION,
        physical_height = UNCHANGED_PHYSICAL_DIMENSION,
        max_scale_width = baseline.max_scale_width,
        max_scale_height = baseline.max_scale_height,
        codec_type = baseline.codec_type,
        "official initial capture-setting snapshot prepared"
    );
    Ok(Some(OutgoingControlMessage {
        annotation_generation: None,
        sequence,
        payload,
        protocol: active_protocol,
        completion: None,
    }))
}

fn quality_from_capability(quality: i32) -> Option<StreamQuality> {
    match quality {
        1 => Some(StreamQuality::Fast),
        2 => Some(StreamQuality::Clear),
        3 => Some(StreamQuality::High),
        4 => Some(StreamQuality::Original),
        _ => None,
    }
}

fn color_supported(state: &StreamControlState) -> bool {
    state.peer_capture_setting >= 3
        && feature_supported(state, crate::feature_ability::Feature::ScreenChroma)
        && state.capability.is_some()
}

fn validate_color(state: &StreamControlState, enabled: bool) -> Result<()> {
    anyhow::ensure!(color_supported(state), "当前会话未开放色彩切换");
    validate_format(state, state.settings.quality, enabled, state.settings.hdr)?;
    Ok(())
}

fn validate_format(
    state: &StreamControlState,
    quality: StreamQuality,
    true_color: bool,
    hdr: bool,
) -> Result<Option<FrameQualityCapability>> {
    let Some(capability) = &state.capability else {
        return Ok(None);
    };
    let requested = quality.capability_quality();
    let selected = capability.select(if true_color { 3 } else { 1 }, hdr, requested);
    if selected.result != 0
        || (!matches!(requested, 0 | 5) && selected.max_frame_quality < requested)
    {
        bail!(
            "双端能力不支持此色彩下的{}（最高能力档位{}，结果{}）",
            quality.label(),
            selected.max_frame_quality,
            selected.result
        );
    }
    Ok(Some(selected))
}

fn ensure_hdr_displays(state: &StreamControlState) -> Result<()> {
    anyhow::ensure!(state.peer_capture_setting >= 6, "当前会话不支持 HDR");
    let cap = state
        .capability
        .as_ref()
        .ok_or_else(|| anyhow!("正在等待 HDR 能力"))?;
    anyhow::ensure!(
        cap.remote_display_info
            .iter()
            .any(|display| display.hdr == 0),
        "请先在被控端支持 HDR 的屏幕上开启 Windows HDR"
    );
    anyhow::ensure!(
        cap.local_display_info
            .iter()
            .any(|display| display.hdr == 0),
        "请先在本机支持 HDR 的屏幕上开启 Windows HDR"
    );
    Ok(())
}

fn format_proposal(
    state: &StreamControlState,
    color: Option<bool>,
    hdr: Option<bool>,
) -> Result<StreamControlSettings> {
    let mut settings = state.settings;
    if let Some(enabled) = color {
        anyhow::ensure!(color_supported(state), "当前会话不支持色彩切换");
        settings.true_color = enabled;
    }
    if let Some(enabled) = hdr {
        anyhow::ensure!(state.peer_capture_setting >= 6, "当前会话不支持 HDR");
        settings.hdr = enabled;
    }
    if hdr == Some(true) {
        ensure_hdr_displays(state)?;
    }
    let cap = state
        .capability
        .as_ref()
        .ok_or_else(|| anyhow!("正在等待串流能力"))?;
    let mut selected = cap.select(
        if settings.true_color { 3 } else { 1 },
        settings.hdr,
        settings.quality.capability_quality(),
    );
    if selected.result != 0 && settings.hdr && settings.true_color {
        if hdr == Some(true) {
            selected = cap.select(1, true, settings.quality.capability_quality());
            if selected.result == 0 {
                settings.true_color = false;
            }
        } else if color == Some(true) {
            selected = cap.select(3, false, settings.quality.capability_quality());
            if selected.result == 0 {
                settings.hdr = false;
            }
        }
    }
    anyhow::ensure!(selected.result == 0, "双方设备不支持所选色彩与 HDR 组合");
    let requested = settings.quality.capability_quality();
    if !matches!(requested, 0 | 5) && selected.max_frame_quality < requested {
        settings.quality = quality_from_capability(selected.max_frame_quality)
            .ok_or_else(|| anyhow!("无可用画质档位"))?;
    }
    Ok(settings)
}

fn apply_codec_limits(state: &mut StreamControlState, selected: FrameQualityCapability) {
    state.baseline.codec_type = selected.video_codec;
    state.baseline.max_scale_width = selected.max_width as u32;
    state.baseline.max_scale_height = selected.max_height as u32;
}

fn constrain_auto_quality(state: &mut StreamControlState) {
    if state.baseline.frame_quality != VIDEO_QUALITY_AUTO {
        return;
    }
    if let Some(row) = state
        .capability
        .as_ref()
        .and_then(|cap| {
            cap.exact(
                state.baseline.codec_type,
                state.baseline.chroma_format as u8,
                state.settings.hdr,
            )
        })
        .filter(|row| row.result == 0)
        && let Some(maximum) = quality_from_capability(row.max_frame_quality)
        && state.baseline.auto_frame_quality > maximum.protobuf()
    {
        state.baseline.auto_frame_quality = maximum.protobuf();
    }
}

fn ensure_ready(state: &StreamControlState) -> Result<()> {
    ensure_business_ready(state)?;
    if state.remote_display.is_none() {
        bail!("尚未收到活动屏幕基线，已阻止发送不完整的串流设置");
    }
    if state.baseline.codec_type == 0 {
        bail!("视频编码尚未完成协商");
    }
    Ok(())
}

fn ensure_business_ready(state: &StreamControlState) -> Result<()> {
    if !state.control_channel_open {
        bail!("UU CONTROL 通道尚未打开");
    }
    if !state.text_channel_open {
        bail!("UU TEXT 通道尚未打开");
    }
    if !state.pb_connected {
        bail!("UU PB 特性协商尚未完成");
    }
    if protocol(state) == StreamControlProtocol::Unsupported {
        bail!("对端不支持当前串流协议（需要CaptureSetting RPC）");
    }
    Ok(())
}

fn custom_bitrate_supported(state: &StreamControlState) -> bool {
    state.pb_connected
        && state.peer_capture_setting >= crate::official_version::CUSTOM_BITRATE_MIN_LEVEL
}

fn feature_supported(state: &StreamControlState, feature: crate::feature_ability::Feature) -> bool {
    state
        .features
        .as_ref()
        .is_some_and(|policy| policy.supports(feature))
}

fn protocol(state: &StreamControlState) -> StreamControlProtocol {
    if !state.pb_connected {
        StreamControlProtocol::Negotiating
    } else if state.peer_capture_setting >= crate::official_version::CAPTURE_SETTING_RPC_MIN_LEVEL {
        StreamControlProtocol::CaptureSetting
    } else {
        StreamControlProtocol::Unsupported
    }
}

fn update_screen_baseline(state: &mut StreamControlState, screens: PbScreenSources) {
    let previous_screens = state.screens.clone();
    state.screens_generation = state.screens_generation.wrapping_add(1);
    state.current_screen_id = screens.current_screen_id;
    state.screens = screens
        .screens
        .into_iter()
        .filter_map(|screen| {
            let display = RemoteDisplayInfo::from_screen(&screen)?;
            let resolution = screen.current_resolution?;
            let width = u32::try_from(resolution.width).ok()?;
            let height = u32::try_from(resolution.height).ok()?;
            if width == 0 || height == 0 {
                return None;
            }
            Some(ScreenBaseline {
                id: screen.id,
                name: screen.display_name,
                primary: screen.is_primary_screen,
                video_track_index: screen.video_track_index,
                fps: u32::try_from(screen.fps).unwrap_or_default(),
                width,
                height,
                pixel_width: u32::try_from(resolution.pixel_width).unwrap_or_default(),
                pixel_height: u32::try_from(resolution.pixel_height).unwrap_or_default(),
                dpi_scale: screen
                    .dpi_scale
                    .map(|dpi| u32::try_from(dpi.current_dpi).unwrap_or_default())
                    .unwrap_or_default(),
                resolution_type: screen.resolution_type,
                display,
            })
        })
        .collect();
    if previous_screens.iter().any(|old| {
        !state.screens.iter().any(|new| {
            old.id == new.id
                && old.video_track_index == new.video_track_index
                && old.display.screen_type == new.display.screen_type
        })
    }) {
        state.mouse.pause_layout();
    }
    state
        .topology
        .observe(state.screens_generation, &state.screens);
    for screen in &state.screens {
        tracing::debug!(screen_id = screen.id, name = %screen.name,
            primary = screen.primary, track = screen.video_track_index,
            width = screen.width, height = screen.height, "remote screen mapping");
    }
    refresh_active_screen(state);
}

fn refresh_active_screen(state: &mut StreamControlState) {
    let selected = state
        .screens
        .iter()
        .find(|screen| screen.video_track_index == state.active_video_track_index)
        .or_else(|| {
            state
                .screens
                .iter()
                .find(|screen| screen.id == state.current_screen_id)
        })
        .or_else(|| state.screens.first())
        .cloned();
    let Some(screen) = selected else {
        return;
    };
    state.remote_display = Some(RemoteDisplayState {
        screen_id: screen.id,
        video_track_index: screen.video_track_index,
        width: screen.width,
        height: screen.height,
        refresh_hz: screen.fps,
    });
    tracing::info!(
        screen_id = screen.id,
        video_track_index = screen.video_track_index,
        width = screen.width,
        height = screen.height,
        pixel_width = screen.pixel_width,
        pixel_height = screen.pixel_height,
        dpi_scale = screen.dpi_scale,
        resolution_type = screen.resolution_type,
        "official active-screen baseline synchronized"
    );
}

fn apply_capture_setting_response(
    state: &mut StreamControlState,
    request_id: i64,
    response: PbCaptureSettingResponse,
) {
    if state.display_changes.ack(request_id, &response) {
        return;
    }
    let current = state.latest_requested_sequence == Some(request_id)
        && state.pending_sequences.contains(&request_id);
    let mut reported_color = None;
    let mut failures = Vec::new();
    let mut notices = Vec::new();
    for error in response.errors {
        match error.error_code {
            0 => {}
            -6 => {
                match serde_json::from_str::<serde_json::Value>(&error.error_detail)
                    .ok()
                    .and_then(|v| v.get("error_code").and_then(|n| n.as_i64()))
                {
                    Some(code @ (0 | 1 | 2 | 3 | 4 | 5)) => {
                        reported_color = Some(matches!(code, 0 | 3 | 4));
                        if matches!(code, 1 | 2) {
                            notices.push("远端未能启用 YUV 4:4:4，已使用 YUV 4:2:0".into());
                        }
                        if matches!(code, 3 | 4) {
                            notices.push("远端已保留 YUV 4:4:4，其他显示操作未完全生效".into());
                        }
                    }
                    _ => failures.push(format_pb_error(error)),
                }
            }
            CAPTURE_RESULT_FPS_ADJUSTED => notices.push(format!(
                "远端屏幕刷新率低于请求档位，串流已按屏幕能力降档（{}）",
                format_pb_error(error)
            )),
            _ => failures.push(format_pb_error(error)),
        }
    }
    if let Some(enabled) = reported_color
        && let Some(pending) = state
            .pending_capture_preferences
            .iter_mut()
            .find(|p| p.sequence == request_id)
    {
        pending.preferences.settings.true_color = enabled;
    }
    finish_request(state, request_id, failures, notices);
    if current && let Some(enabled) = reported_color {
        apply_reported_color(state, enabled, state.user_settings_requested);
    }
}

fn apply_reported_color(state: &mut StreamControlState, enabled: bool, persist: bool) {
    state.settings.true_color = enabled;
    state.baseline.chroma_format = if enabled { CHROMA_444 } else { CHROMA_420 };
    let changed = state.confirmed_preferences.settings.true_color != enabled;
    state.confirmed_preferences.settings.true_color = enabled;
    if let Ok(Some(selected)) =
        validate_format(state, state.settings.quality, enabled, state.settings.hdr)
    {
        apply_codec_limits(state, selected);
    }
    if changed && persist {
        state
            .preference_updates
            .send_replace(Some(ViewingPreferenceUpdate::Settings(
                state.confirmed_preferences.saved(),
            )));
    }
}

fn format_pb_error(error: PbError) -> String {
    if error.error_message.is_empty() && error.error_detail.is_empty() {
        error.error_code.to_string()
    } else if error.error_detail.is_empty() {
        format!("{}: {}", error.error_code, error.error_message)
    } else {
        format!(
            "{}: {} ({})",
            error.error_code, error.error_message, error.error_detail
        )
    }
}

fn reported_auto_quality(quality: i32) -> Option<i32> {
    // F58210 -> 1042310 rejects the uninitialized PB value 0 before
    // B9A320 translates it to the GUI enum. F09580/BA3F10 then preserve
    // the translated auto sub-tier. Do not let startup QoS reset it to Clear.
    match quality {
        0 => None,
        VIDEO_QUALITY_FAST..=VIDEO_QUALITY_CUSTOM => Some(quality),
        // B9A320's unknown nonzero value maps to GUI0, which B9A2B0
        // writes back as PB General. This is not the zero/uninitialized case.
        _ => Some(VIDEO_QUALITY_GENERAL),
    }
}

fn quality_name(quality: i32) -> &'static str {
    match quality {
        VIDEO_QUALITY_BLURAY => "原画",
        VIDEO_QUALITY_HD => "超清",
        VIDEO_QUALITY_GENERAL => "高清",
        VIDEO_QUALITY_FAST => "低码率",
        _ => "高清",
    }
}

fn official_quality_label(baseline: &CaptureSettingBaseline) -> String {
    match baseline.frame_quality {
        VIDEO_QUALITY_FAST..=VIDEO_QUALITY_BLURAY => {
            quality_name(baseline.frame_quality).to_owned()
        }
        VIDEO_QUALITY_AUTO => format!("自动（{}）", quality_name(baseline.auto_frame_quality)),
        VIDEO_QUALITY_CUSTOM if baseline.max_custom_bitrate >= 1_000_000 => {
            format!("{} Mbps", baseline.max_custom_bitrate / 1_000_000)
        }
        VIDEO_QUALITY_CUSTOM => "自定义".to_owned(),
        _ => "auto".to_owned(),
    }
}

fn viewing_quality_label(state: &StreamControlState) -> String {
    official_quality_label(&state.baseline)
}

fn finish_request(
    state: &mut StreamControlState,
    request_id: i64,
    failures: Vec<String>,
    notices: Vec<String>,
) {
    if !state.pending_sequences.contains(&request_id) {
        return;
    }
    let Some(index) = state
        .pending_capture_preferences
        .iter()
        .position(|pending| pending.sequence == request_id)
    else {
        return;
    };
    let completed = state
        .pending_capture_preferences
        .remove(index)
        .expect("matched capture request");
    state
        .pending_sequences
        .retain(|pending| *pending != request_id);
    if failures.is_empty() {
        // A successful complete snapshot supersedes older snapshots. A
        // refusal does not: older requests still retain their own ACKs.
        for earlier in state.pending_capture_preferences.drain(..index) {
            state
                .pending_sequences
                .retain(|seq| *seq != earlier.sequence);
        }
        if completed.cursor_capture == state.baseline.cursor_capture {
            state.cursor_error = None;
        }
        if state
            .cursor_pending
            .is_some_and(|(seq, _, _)| !state.pending_sequences.contains(&seq))
        {
            state.cursor_pending = None;
        }
        let settings_changed = state.confirmed_preferences.saved() != completed.preferences.saved();
        state.confirmed_preferences = completed.preferences;
        if completed.persist && settings_changed {
            state
                .preference_updates
                .send_replace(Some(ViewingPreferenceUpdate::Settings(
                    completed.preferences.saved(),
                )));
        }
    } else {
        fail_cursor_request(state, request_id, failures.join("; "));
    }
    if state.latest_requested_sequence != Some(request_id) {
        return;
    }
    if failures.is_empty() {
        state.last_applied_sequence = Some(request_id);
        state.last_error = None;
        state.last_notice = (!notices.is_empty()).then(|| notices.join("; "));
        state.performance.acknowledge_stream_switch(request_id);
        state.performance.set_quality(viewing_quality_label(state));
        tracing::info!(
            sequence = request_id,
            "runtime stream settings applied by remote host"
        );
        if let Some(notice) = state.last_notice.as_deref() {
            tracing::info!(sequence = request_id, %notice, "remote host adjusted runtime stream settings");
        }
    } else {
        let error = failures.join("; ");
        restore_confirmed_capture(state);
        state.last_error = Some(error.clone());
        state.last_notice = None;
        state
            .performance
            .fail_stream_switch(request_id, error.clone());
        tracing::warn!(sequence = request_id, %error, "remote host rejected runtime stream settings");
    }
}

fn restore_confirmed_capture(state: &mut StreamControlState) {
    let confirmed = state.confirmed_preferences;
    let _ = set_requested_settings(state, confirmed.settings);
    state.baseline.auto_frame_quality = confirmed.auto_frame_quality;
    if let Ok(Some(selected)) = validate_format(
        state,
        confirmed.settings.quality,
        confirmed.settings.true_color,
        confirmed.settings.hdr,
    ) {
        apply_codec_limits(state, selected);
    }
}

fn smart_mouse_requested(state: &StreamControlState) -> bool {
    state.mouse.mode() == MouseMode::Smart
}

fn mouse_policy(state: &StreamControlState, mode: MouseMode) -> (bool, bool) {
    match mode {
        MouseMode::View => (false, true),
        MouseMode::Local => (false, false),
        MouseMode::Remote => (true, true),
        MouseMode::Smart => match state.peer_mouse_relative {
            Some(true) => (true, true),
            Some(false) => (false, false),
            // The host says a game took the mouse with `special_game_mouse`,
            // and that report is what pairs relative input with the host
            // drawing its own pointer into the picture. Until it arrives,
            // absolute positioning is the only coherent choice: inferring
            // relative input from a hidden cursor fires on an ordinary desktop
            // -- Windows hides the pointer for anyone typing, for as long as
            // they type -- and leaves the client sending deltas while nothing
            // draws a pointer to aim with.
            None => (false, false),
        },
    }
}

fn fail_cursor_request(state: &mut StreamControlState, sequence: i64, error: String) {
    if state
        .cursor_pending
        .is_some_and(|(pending, _, _)| pending == sequence)
    {
        state.cursor_pending = None;
        state.cursor_error = Some(error);
    }
}

fn expire_cursor_request(state: &mut StreamControlState) {
    if let Some((sequence, _, started)) = state.cursor_pending
        && started.elapsed() >= std::time::Duration::from_secs(10)
    {
        let error = "光标设置确认超时，远端状态未知；请重试".to_owned();
        fail_cursor_request(state, sequence, error.clone());
        state
            .pending_capture_preferences
            .retain(|pending| pending.sequence != sequence);
        state
            .pending_sequences
            .retain(|pending| *pending != sequence);
        state
            .performance
            .fail_stream_switch(sequence, error.clone());
        state.last_error = Some(error);
    }
}

fn encode_capture_setting(sequence: i64, baseline: CaptureSettingBaseline) -> Result<Vec<u8>> {
    let request = capture_setting_request(baseline)?;
    Ok(encode_capture_request(sequence, request))
}

fn capture_setting_request(baseline: CaptureSettingBaseline) -> Result<PbCaptureSettingRequest> {
    Ok(PbCaptureSettingRequest {
        fps: fps_to_protobuf(baseline.requested_fps),
        frame_quality: baseline.frame_quality,
        cursor_capture: baseline.cursor_capture,
        screen_id: EXISTING_SESSION_TRACKS,
        resolution_width: UNCHANGED_PHYSICAL_DIMENSION,
        resolution_height: UNCHANGED_PHYSICAL_DIMENSION,
        chroma_format: baseline.chroma_format,
        max_custom_bitrate: i32::try_from(baseline.max_custom_bitrate)?,
        dpi_scale: 0,
        resolution_type: RESOLUTION_DEFAULT,
        enable_hdr: baseline.enable_hdr,
        auto_frame_quality: baseline.auto_frame_quality,
        codec_type: baseline.codec_type,
        max_scale_width: i32::try_from(baseline.max_scale_width)?,
        max_scale_height: i32::try_from(baseline.max_scale_height)?,
        resolution_pixel_width: 0,
        resolution_pixel_height: 0,
        fps_count: i32::try_from(baseline.fps_count)?,
    })
}

fn encode_capture_request(sequence: i64, request: PbCaptureSettingRequest) -> Vec<u8> {
    encode_envelope(
        sequence,
        PbPayload::RpcRequest(
            PbRpcRequest {
                request_header: Some(PbRequestHeader {
                    request_id: sequence,
                }),
                capture_setting: Some(request),
                send_video_track: None,
                ..Default::default()
            }
            .encode_to_vec(),
        ),
    )
}

fn encode_envelope(sequence: i64, payload: PbPayload) -> Vec<u8> {
    let message = PbControlMessage {
        seq: sequence,
        timestamp: i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(i64::MAX),
        payload: Some(payload),
    };
    message.encode_to_vec()
}

fn fps_to_protobuf(fps: u32) -> i32 {
    match fps {
        30 => FPS_30,
        60 => FPS_60,
        90 => FPS_90,
        _ => FPS_144,
    }
}

fn frame_rate_choice(fps: u32) -> FrameRateChoice {
    match fps {
        30 => FrameRateChoice::Fps30,
        60 => FrameRateChoice::Fps60,
        90 => FrameRateChoice::Fps90,
        _ => FrameRateChoice::Fps144,
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbControlMessage {
    #[prost(int64, tag = "1")]
    seq: i64,
    #[prost(int64, tag = "2")]
    timestamp: i64,
    #[prost(
        oneof = "PbPayload",
        tags = "3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28"
    )]
    payload: Option<PbPayload>,
}

pub(crate) fn encode_port_mapping(payload: Vec<u8>) -> Vec<u8> {
    PbControlMessage {
        seq: 0,
        timestamp: 0,
        payload: Some(PbPayload::PortMappingFrame(payload)),
    }
    .encode_to_vec()
}

pub(crate) fn decode_port_mapping(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(match PbControlMessage::decode(bytes)?.payload {
        Some(PbPayload::PortMappingFrame(frame)) => Some(frame),
        _ => None,
    })
}

// main.proto's complete oneof range, read from the shipped descriptor.
// Opaque payloads remain mutually exclusive without implementing their
// non-viewing business operations. Media-specific pending handlers stay in C02.
#[derive(Clone, PartialEq, prost::Oneof)]
enum PbPayload {
    #[prost(message, tag = "3")]
    SimpleAction(PbSimpleAction),
    #[prost(bytes, tag = "4")]
    LaunchApp(Vec<u8>),
    #[prost(bytes, tag = "5")]
    ShowApp(Vec<u8>),
    #[prost(bytes, tag = "6")]
    MumuOperate(Vec<u8>),
    #[prost(message, tag = "7")]
    Screens(PbScreenSources),
    #[prost(bytes, tag = "8")]
    CaptureChange(Vec<u8>),
    #[prost(bytes, tag = "9")]
    CaptureConfig(Vec<u8>),
    #[prost(bytes, tag = "10")]
    RomMessage(Vec<u8>),
    #[prost(bytes, tag = "11")]
    SendToRom(Vec<u8>),
    #[prost(message, tag = "12")]
    ReportError(PbReportError),
    #[prost(bytes, tag = "13")]
    SystemMetrics(Vec<u8>),
    #[prost(message, tag = "14")]
    ReportQosStats(PbReportQosStats),
    #[prost(bytes, tag = "15")]
    SystemStateChange(Vec<u8>),
    #[prost(bytes, tag = "16")]
    QuerySystemState(Vec<u8>),
    #[prost(bytes, tag = "17")]
    ClipboardChange(Vec<u8>),
    #[prost(bytes, tag = "18")]
    CodecNegotiation(Vec<u8>),
    #[prost(bytes, tag = "19")]
    CaptureConfigResponse(Vec<u8>),
    #[prost(bytes, tag = "20")]
    InputEvent(Vec<u8>),
    #[prost(bytes, tag = "21")]
    RpcRequest(Vec<u8>),
    #[prost(message, tag = "22")]
    RpcResponse(PbRpcResponse),
    #[prost(bytes, tag = "23")]
    ActiveWindowChange(Vec<u8>),
    #[prost(bytes, tag = "24")]
    LaunchCloudPcApp(Vec<u8>),
    #[prost(bytes, tag = "25")]
    CaptureSettingSync(Vec<u8>),
    #[prost(bytes, tag = "26")]
    RemoteDownloadPath(Vec<u8>),
    #[prost(bytes, tag = "27")]
    PortMappingFrame(Vec<u8>),
    #[prost(bytes, tag = "28")]
    TerminalSessionChanged(Vec<u8>),
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbReportError {
    #[prost(int32, tag = "1")]
    action: i32,
    #[prost(int32, tag = "2")]
    error_code: i32,
    #[prost(string, tag = "3")]
    error_msg: String,
    #[prost(int32, tag = "4")]
    type_value: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbReportQosStats {
    #[prost(string, tag = "1")]
    encoder_type: String,
    #[prost(string, tag = "2")]
    capture_type: String,
    #[prost(uint64, tag = "3")]
    probe_bps: u64,
    #[prost(int32, tag = "4")]
    video_quality: i32,
    #[prost(uint64, tag = "5")]
    fast_bitrate: u64,
    #[prost(uint64, tag = "6")]
    general_bitrate: u64,
    #[prost(uint64, tag = "7")]
    hd_bitrate: u64,
    #[prost(uint64, tag = "8")]
    bluray_bitrate: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbSimpleAction {
    #[prost(int32, tag = "1")]
    action: i32,
    #[prost(string, tag = "2")]
    args: String,
    #[prost(oneof = "PbSimpleActionParams", tags = "3, 4, 5")]
    params: Option<PbSimpleActionParams>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
enum PbSimpleActionParams {
    #[prost(bytes, tag = "3")]
    KeyToggle(Vec<u8>),
    #[prost(message, tag = "4")]
    FeatureFlag(PbFeatureFlag),
    #[prost(uint32, tag = "5")]
    Value(u32),
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbFeatureFlag {
    #[prost(int32, tag = "1")]
    capture_setting: i32,
    #[prost(int32, tag = "2")]
    simple_action: i32,
    #[prost(int32, tag = "3")]
    system_metrics: i32,
    #[prost(int32, tag = "4")]
    private_screen: i32,
    #[prost(int32, tag = "5")]
    update_acquire: i32,
    #[prost(int32, tag = "6")]
    file_transfer_ftp: i32,
    #[prost(int32, tag = "7")]
    file_transfer_ftp2: i32,
    #[prost(int32, tag = "8")]
    clipboard: i32,
    #[prost(int32, tag = "9")]
    qos_stat: i32,
    #[prost(int32, tag = "10")]
    mumu_control: i32,
    #[prost(int32, tag = "11")]
    virtual_mouse_device: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ClipboardPermissionState {
    #[prost(message, optional, tag = "4")]
    files: Option<ClipboardPermission>,
}
#[derive(Clone, PartialEq, prost::Message)]
struct ClipboardPermission {
    #[prost(bool, tag = "1")]
    enabled: bool,
}

impl PbFeatureFlag {
    fn read_only_viewer() -> Self {
        Self {
            capture_setting: crate::official_version::CAPTURE_SETTING_LEVEL as i32,
            simple_action: 0,
            system_metrics: 0,
            private_screen: 0,
            update_acquire: 0,
            file_transfer_ftp: 2,
            file_transfer_ftp2: 2,
            clipboard: 3,
            qos_stat: 1,
            mumu_control: 0,
            virtual_mouse_device: 0,
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbRequestHeader {
    #[prost(int64, tag = "1")]
    request_id: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbResponseHeader {
    #[prost(int64, tag = "1")]
    request_id: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbRpcRequest {
    #[prost(message, optional, tag = "27")]
    draw: Option<annotation::PbDrawRequest>,
    #[prost(message, optional, tag = "1")]
    request_header: Option<PbRequestHeader>,
    #[prost(message, optional, tag = "2")]
    capture_setting: Option<PbCaptureSettingRequest>,
    #[prost(message, optional, tag = "12")]
    create_virtual_display: Option<display_topology::PbCreateVirtualDisplay>,
    #[prost(message, optional, tag = "13")]
    remove_virtual_display: Option<display_topology::PbRemoveVirtualDisplay>,
    #[prost(message, optional, tag = "14")]
    quit_super_screen: Option<display_topology::PbQuitSuperScreen>,
    #[prost(message, optional, tag = "26")]
    enter_super_screen: Option<display_topology::PbEnterSuperScreen>,
    #[prost(message, optional, tag = "15")]
    send_video_track: Option<PbSendVideoTrackRequest>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbSendVideoTrackRequest {
    #[prost(int32, repeated, tag = "1")]
    video_track_index: Vec<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbSendVideoTrackResponse {
    #[prost(int32, tag = "1")]
    error_code: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbRpcResponse {
    #[prost(message, optional, tag = "1")]
    response_header: Option<PbResponseHeader>,
    #[prost(
        oneof = "PbRpcResponsePayload",
        tags = "2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23"
    )]
    payload: Option<PbRpcResponsePayload>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
enum PbRpcResponsePayload {
    #[prost(message, tag = "2")]
    CaptureSetting(PbCaptureSettingResponse),
    #[prost(bytes, tag = "3")]
    PrivateScreenSetting(Vec<u8>),
    #[prost(bytes, tag = "4")]
    FileTransferFtpResponse(Vec<u8>),
    #[prost(bytes, tag = "5")]
    ClipResponse(Vec<u8>),
    #[prost(bytes, tag = "6")]
    TextChangeResponse(Vec<u8>),
    #[prost(bytes, tag = "7")]
    MouseSwitchResponse(Vec<u8>),
    #[prost(bytes, tag = "8")]
    CreateVirtualDisplayRsp(Vec<u8>),
    #[prost(bytes, tag = "9")]
    RemoveVirtualDisplayRsp(Vec<u8>),
    #[prost(bytes, tag = "10")]
    QuitSuperScreen(Vec<u8>),
    #[prost(message, tag = "11")]
    SendVideoTrackRsp(PbSendVideoTrackResponse),
    #[prost(bytes, tag = "12")]
    QueryPluginSettingRsp(Vec<u8>),
    #[prost(bytes, tag = "13")]
    UpdatePluginSettingRsp(Vec<u8>),
    #[prost(bytes, tag = "14")]
    StartDownloadAndInstallPluginRsp(Vec<u8>),
    #[prost(bytes, tag = "15")]
    PluginEnableNotificationRsp(Vec<u8>),
    #[prost(bytes, tag = "16")]
    PluginNotificationRsp(Vec<u8>),
    #[prost(bytes, tag = "17")]
    VirtualAudioDriverPolicyRsp(Vec<u8>),
    #[prost(bytes, tag = "18")]
    FlipScreenRsp(Vec<u8>),
    #[prost(bytes, tag = "19")]
    QuickLaunchScanRsp(Vec<u8>),
    #[prost(bytes, tag = "20")]
    QuickLaunchAppIconRsp(Vec<u8>),
    #[prost(bytes, tag = "21")]
    UpdateScreenSaverRsp(Vec<u8>),
    #[prost(bytes, tag = "22")]
    EnterSuperScreenRep(Vec<u8>),
    #[prost(message, tag = "23")]
    DrawResp(annotation::PbDrawResponse),
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbCaptureSettingRequest {
    #[prost(int32, tag = "1")]
    fps: i32,
    #[prost(int32, tag = "2")]
    frame_quality: i32,
    #[prost(bool, tag = "3")]
    cursor_capture: bool,
    #[prost(int32, tag = "4")]
    screen_id: i32,
    #[prost(int32, tag = "5")]
    resolution_width: i32,
    #[prost(int32, tag = "6")]
    resolution_height: i32,
    #[prost(int32, tag = "7")]
    chroma_format: i32,
    #[prost(int32, tag = "8")]
    max_custom_bitrate: i32,
    #[prost(int32, tag = "9")]
    dpi_scale: i32,
    #[prost(int32, tag = "10")]
    resolution_type: i32,
    #[prost(bool, tag = "11")]
    enable_hdr: bool,
    #[prost(int32, tag = "12")]
    auto_frame_quality: i32,
    #[prost(int32, tag = "13")]
    codec_type: i32,
    #[prost(int32, tag = "14")]
    max_scale_width: i32,
    #[prost(int32, tag = "15")]
    max_scale_height: i32,
    #[prost(int32, tag = "16")]
    resolution_pixel_width: i32,
    #[prost(int32, tag = "17")]
    resolution_pixel_height: i32,
    #[prost(int32, tag = "18")]
    fps_count: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbCaptureSettingResponse {
    #[prost(message, repeated, tag = "1")]
    errors: Vec<PbError>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbError {
    #[prost(int32, tag = "1")]
    error_code: i32,
    #[prost(string, tag = "2")]
    error_message: String,
    #[prost(string, tag = "3")]
    error_detail: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbScreenSources {
    #[prost(message, repeated, tag = "1")]
    screens: Vec<PbScreen>,
    #[prost(int32, tag = "2")]
    current_screen_id: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbScreen {
    #[prost(int32, tag = "1")]
    id: i32,
    #[prost(int32, tag = "2")]
    fps: i32,
    #[prost(message, repeated, tag = "3")]
    resolutions: Vec<PbWinRect>,
    #[prost(message, optional, tag = "4")]
    current_resolution: Option<PbWinRect>,
    #[prost(int32, tag = "5")]
    screen_type: i32,
    #[prost(message, optional, tag = "6")]
    init_resolution: Option<PbWinRect>,
    #[prost(bool, tag = "7")]
    is_primary_screen: bool,
    #[prost(double, tag = "8")]
    dpr: f64,
    #[prost(message, optional, tag = "9")]
    dpi_scale: Option<PbDpiScale>,
    #[prost(string, tag = "10")]
    display_name: String,
    #[prost(int32, tag = "11")]
    resolution_type: i32,
    #[prost(int32, tag = "12")]
    video_track_index: i32,
    #[prost(int32, tag = "13")]
    builtin_screen_type: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbWinRect {
    #[prost(int32, tag = "1")]
    left: i32,
    #[prost(int32, tag = "2")]
    top: i32,
    #[prost(int32, tag = "3")]
    width: i32,
    #[prost(int32, tag = "4")]
    height: i32,
    #[prost(int32, tag = "5")]
    pixel_width: i32,
    #[prost(int32, tag = "6")]
    pixel_height: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbDpiScale {
    #[prost(int32, tag = "1")]
    current_dpi: i32,
    #[prost(int32, tag = "2")]
    recommended_dpi: i32,
    #[prost(int32, repeated, tag = "3")]
    dpis: Vec<i32>,
}
