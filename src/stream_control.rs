//! Official runtime stream-setting protocol.
//!
//! UU uses two different reliable DataChannels for this state machine:
//! protobuf ECHO/feature negotiation is binary data on CONTROL, while both
//! `CaptureSettingRequest` and the legacy `CaptureConfig` are protobuf bytes
//! sent with the text PPID on TEXT. Capture requests are complete snapshots,
//! so no request is produced until the active remote screen's physical mode is
//! known from `ScreenSources`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use prost::Message as _;
use tokio::sync::{Notify, mpsc};

use crate::adaptive_bitrate::BudgetPolicy;
pub use crate::adaptive_bitrate::{AdaptiveBitrateSnapshot, BudgetPhase};
use crate::capability::{DualCapability, FrameQualityCapability};
use crate::media::{ConnectionMediaProfile, FrameRateChoice, LocalDisplayInfo, VideoCodec};
pub use crate::network_control::NetworkControlSnapshot;
use crate::performance::PerformanceMonitor;

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
const RESOLUTION_ORIGINAL: i32 = 1;
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
    Adaptive,
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
            Self::Custom | Self::Adaptive => 5,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（原画）",
            Self::Original => "原画 20M",
            Self::High => "高清 8M",
            Self::Clear => "清晰 2M",
            Self::Custom => "自定义",
            Self::Adaptive => "受限自适应",
            Self::Fast => "快速（480P）",
        }
    }

    const fn protobuf(self) -> i32 {
        match self {
            Self::Auto => VIDEO_QUALITY_AUTO,
            Self::Original => VIDEO_QUALITY_BLURAY,
            Self::High => VIDEO_QUALITY_HD,
            Self::Clear => VIDEO_QUALITY_GENERAL,
            Self::Custom | Self::Adaptive => VIDEO_QUALITY_CUSTOM,
            Self::Fast => VIDEO_QUALITY_FAST,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamControlProtocol {
    Negotiating,
    CaptureSetting { feature_level: u32 },
    LegacyCaptureConfig,
}

impl StreamControlProtocol {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Negotiating => "协商中",
            Self::CaptureSetting { .. } => "CaptureSetting",
            Self::LegacyCaptureConfig => "CaptureConfig（兼容）",
        }
    }

    pub const fn supports_custom_bitrate(self) -> bool {
        matches!(self, Self::CaptureSetting { feature_level: 4.. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StreamControlSettings {
    pub frame_rate: FrameRateChoice,
    pub quality: StreamQuality,
    pub custom_bitrate_mbps: u32,
    pub adaptive_ceiling_mbps: u32,
    pub stability_priority: bool,
}

/// User-facing state survives a room replacement, unlike PB sequence numbers,
/// screen snapshots, or the newly negotiated codec/transport.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamControlPreferences {
    pub(crate) settings: StreamControlSettings,
    audio: Option<crate::audio::AudioSettings>,
    auto_frame_quality: i32,
    adaptive_start_mbps: Option<u32>,
}

impl StreamControlPreferences {
    pub(crate) fn from_saved(
        mut settings: StreamControlSettings,
        display: LocalDisplayInfo,
    ) -> Self {
        if !settings.frame_rate.is_supported(display) {
            settings.frame_rate = FrameRateChoice::Auto;
        }
        Self {
            settings,
            audio: None,
            auto_frame_quality: VIDEO_QUALITY_BLURAY,
            adaptive_start_mbps: None,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteScreen {
    pub id: i32,
    pub name: String,
    pub primary: bool,
    pub video_track_index: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Clone, Debug)]
pub struct StreamControlSnapshot {
    pub local_display: LocalDisplayInfo,
    pub settings: StreamControlSettings,
    pub remote_display: Option<RemoteDisplayState>,
    pub screens: Vec<RemoteScreen>,
    pub screens_generation: u64,

    pub protocol: StreamControlProtocol,
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
    pub adaptive: Option<AdaptiveBitrateSnapshot>,
    pub persistence_error: Option<String>,
    pub network: NetworkControlSnapshot,
}

#[derive(Clone)]
pub struct StreamControlHandle {
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
}

struct StreamControlState {
    available_video_tracks: Vec<i32>,
    registered_video_tracks: Vec<i32>,
    track_registration: Option<(i64, Vec<i32>)>,
    track_registration_error: Option<String>,
    local_display: LocalDisplayInfo,
    active_video_track_index: i32,
    current_screen_id: i32,
    screens: Vec<ScreenBaseline>,
    screens_generation: u64,

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
    next_sequence: i64,
    pending_sequences: VecDeque<i64>,
    last_applied_sequence: Option<i64>,
    latest_requested_sequence: Option<i64>,
    budget: Option<BudgetPolicy>,
    last_error: Option<String>,
    last_notice: Option<String>,
    preference_updates: tokio::sync::watch::Sender<Option<StreamControlSettings>>,
    user_preference_pending: Option<(i64, StreamControlSettings)>,
    persistence_error: Option<String>,
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
        let state = StreamControlState {
            available_video_tracks: Vec::new(),
            registered_video_tracks: Vec::new(),
            track_registration: None,
            track_registration_error: None,
            local_display: profile.local_display,
            active_video_track_index: 0,
            current_screen_id: 0,
            screens: Vec::new(),
            screens_generation: 0,

            settings: StreamControlSettings {
                frame_rate,
                quality: StreamQuality::Auto,
                custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
                adaptive_ceiling_mbps: 20,
                stability_priority: true,
            },
            baseline: CaptureSettingBaseline {
                requested_fps: profile.stream_fps,
                fps_count,
                frame_quality: VIDEO_QUALITY_AUTO,
                auto_frame_quality: VIDEO_QUALITY_BLURAY,
                cursor_capture: false,
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
            next_sequence: 1,
            pending_sequences: VecDeque::new(),
            last_applied_sequence: None,
            latest_requested_sequence: None,
            budget: None,
            last_error: None,
            last_notice: None,
            preference_updates: tokio::sync::watch::channel(None).0,
            user_preference_pending: None,
            persistence_error: None,
            performance,
        };
        state.performance.set_quality(viewing_quality_label(&state));
        let audio = crate::audio::AudioPlayback::new();
        audio.set_settings(crate::audio::AudioSettings {
            volume: 100,
            muted: profile.muted,
        });
        (
            Self {
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

    pub fn snapshot(&self) -> StreamControlSnapshot {
        let state = lock(&self.shared);
        let active_protocol = protocol(&state);
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
            local_display: state.local_display,
            settings: state.settings,
            remote_display: state.remote_display,
            screens_generation: state.screens_generation,

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
                })
                .collect(),
            protocol: active_protocol,
            control_channel_open: state.control_channel_open,
            text_channel_open: state.text_channel_open,
            pb_connected: state.pb_connected,
            ready: waiting_for.is_none(),
            waiting_for,
            pending_sequence: state.pending_sequences.back().copied(),
            pending_count: state.pending_sequences.len(),
            last_applied_sequence: state.last_applied_sequence,
            last_error: state.last_error.clone(),
            last_notice: state.last_notice.clone(),
            adaptive: state.budget.as_ref().map(BudgetPolicy::snapshot),
            persistence_error: state.persistence_error.clone(),
            network: self.network.snapshot(),
        }
    }

    pub(crate) fn audio(&self) -> crate::audio::AudioPlayback {
        self.audio.clone()
    }

    pub(crate) fn preferences(&self) -> StreamControlPreferences {
        let state = lock(&self.shared);
        StreamControlPreferences {
            settings: state.settings,
            audio: Some(self.audio.settings()),
            auto_frame_quality: state.baseline.auto_frame_quality,
            adaptive_start_mbps: state
                .budget
                .as_ref()
                .and_then(|b| b.snapshot().applied_mbps),
        }
    }
    pub(crate) fn network_control(&self) -> crate::network_control::NetworkControl {
        self.network.clone()
    }

    pub fn set_relay_enabled(&self, enabled: bool) -> Result<()> {
        self.network.request(enabled)
    }

    /// Ordinary desktop capture only. Negative IDs include all-screen actions
    /// and are never accepted from a single monitor/window operation.
    pub async fn set_screen_capture(&self, screen_id: i32, active: bool) -> Result<()> {
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
            ensure_ready(&state)?;
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
            ensure_ready(&state)?;
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
        };
        let payload = encode_envelope(sequence, PbPayload::RpcRequest(request.encode_to_vec()));
        if self
            .outgoing
            .send(OutgoingControlMessage {
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
    ) -> tokio::sync::watch::Receiver<Option<StreamControlSettings>> {
        lock(&self.shared).preference_updates.subscribe()
    }

    pub(crate) fn set_persistence_error(&self, error: Option<String>) {
        lock(&self.shared).persistence_error = error;
    }

    pub(crate) fn restore_preferences(&self, preferences: StreamControlPreferences) -> Result<()> {
        let mut state = lock(&self.shared);
        if state.initial_capture_sync_sent || state.baseline.codec_type != 0 {
            bail!("串流偏好必须在新房间选择视频轨道之前恢复");
        }
        set_requested_settings(&mut state, preferences.settings)?;
        if state.settings.quality == StreamQuality::Adaptive {
            state.budget = Some(BudgetPolicy::new(
                state.settings.adaptive_ceiling_mbps,
                state.settings.stability_priority,
                Instant::now(),
            ));
            if let Some(cap) = preferences.adaptive_start_mbps {
                state.baseline.max_custom_bitrate =
                    cap.clamp(1, state.settings.adaptive_ceiling_mbps) * 1_000_000;
            }
        }
        state.baseline.auto_frame_quality = preferences.auto_frame_quality;
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

    pub fn apply(&self, settings: StreamControlSettings) -> Result<i64> {
        {
            let mut state = lock(&self.shared);
            ensure_ready(&state)?;
            let active_protocol = protocol(&state);
            if matches!(
                settings.quality,
                StreamQuality::Custom | StreamQuality::Adaptive
            ) && !active_protocol.supports_custom_bitrate()
            {
                bail!("被控端不支持运行时自定义码率");
            }
            let quality_changed = settings.quality != state.settings.quality;
            let selected = if quality_changed
                && !matches!(
                    settings.quality,
                    StreamQuality::Custom | StreamQuality::Adaptive
                ) {
                validate_quality(&state, settings.quality)?
            } else {
                None
            };
            let previous_quality = state.settings.quality;
            // FPS edits must not silently restore a budget that was already
            // reduced. An explicitly changed ceiling starts a fresh assessment.
            let retain_budget = settings.quality == StreamQuality::Adaptive
                && state.settings.quality == StreamQuality::Adaptive
                && settings.adaptive_ceiling_mbps == state.settings.adaptive_ceiling_mbps;
            let retained_cap = retain_budget
                .then(|| {
                    state.budget.as_ref().and_then(|b| {
                        let budget = b.snapshot();
                        budget.pending_mbps.or(budget.applied_mbps)
                    })
                })
                .flatten();
            set_requested_settings(&mut state, settings)?;
            state.budget = if settings.quality == StreamQuality::Adaptive {
                if let Some(cap) = retained_cap {
                    state.baseline.max_custom_bitrate =
                        cap.min(settings.adaptive_ceiling_mbps) * 1_000_000;
                }
                if retain_budget {
                    state
                        .budget
                        .take()
                        .map(|mut budget| {
                            budget.set_automatic(settings.stability_priority);
                            budget
                        })
                        .or_else(|| {
                            Some(BudgetPolicy::new(
                                settings.adaptive_ceiling_mbps,
                                settings.stability_priority,
                                Instant::now(),
                            ))
                        })
                } else {
                    Some(BudgetPolicy::new(
                        settings.adaptive_ceiling_mbps,
                        settings.stability_priority,
                        Instant::now(),
                    ))
                }
            } else {
                None
            };
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
                StreamControlProtocol::CaptureSetting { .. } => {
                    encode_capture_setting(sequence, state.baseline)?
                }
                StreamControlProtocol::LegacyCaptureConfig => {
                    encode_legacy_capture_config(sequence, state.baseline)?
                }
                StreamControlProtocol::Negotiating => bail!("PB 特性协商尚未完成"),
            };
            state.next_sequence = state.next_sequence.wrapping_add(1);
            state.pending_sequences.push_back(sequence);
            // An explicit full snapshot also satisfies initial synchronization,
            // including a supported choice after a rejected restored preference.
            state.initial_capture_sync_sent = true;
            state.last_error = None;
            state.last_notice = None;
            let outgoing = OutgoingControlMessage {
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
            let sent = self.send_locked(&mut state, outgoing, Some(switch_target))?;
            state.user_preference_pending = Some((sent, settings));
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
        if let Some(budget) = &mut state.budget {
            budget.submitted(
                sequence,
                state.baseline.max_custom_bitrate / 1_000_000,
                Instant::now(),
            );
        }
        if let Some(target) = target {
            state.performance.begin_stream_switch(sequence, target);
        }
        // Enqueue before releasing the state lock: an automatic request cannot
        // overtake a later explicit user choice on this reliable channel.
        if self.outgoing.send(outgoing).is_err() {
            state.initial_capture_sync_sent = false;
            state.pending_sequences.clear();
            state.last_error = Some("串流设置发送任务已经停止".to_owned());
            if let Some(budget) = &mut state.budget {
                budget.suspend();
            }
            state
                .performance
                .fail_stream_switch(sequence, "串流设置发送任务已经停止");
            bail!("串流设置发送任务已经停止");
        }
        Ok(sequence)
    }

    pub(crate) fn observe_budget(
        &self,
        performance: &PerformanceMonitor,
        rtt: Option<std::time::Duration>,
        route: Option<&str>,
        connected: bool,
    ) {
        // No full performance snapshot/history sorting in the observer.
        let mut state = lock(&self.shared);
        if state.settings.quality != StreamQuality::Adaptive {
            return;
        }
        let sample = performance.budget_sample(rtt);
        let ready = connected
            && route.is_some()
            && ensure_ready(&state).is_ok()
            && state.latest_requested_sequence == state.last_applied_sequence;
        let context = format!(
            "{}:{:?}:{}",
            route.unwrap_or(""),
            state.remote_display,
            state.baseline.codec_type
        );
        let Some(budget) = &mut state.budget else {
            return;
        };
        let proposal = budget.observe(sample, &context, ready);
        if budget.snapshot().phase == BudgetPhase::Suspended
            && let Some(sequence) = state
                .latest_requested_sequence
                .filter(|seq| state.pending_sequences.contains(seq))
        {
            state.pending_sequences.clear();
            state.last_error = Some("视频预算确认超时，远端是否生效未知；已暂停自动调整".into());
            state
                .performance
                .fail_stream_switch(sequence, "视频预算确认超时");
        }
        if let Some(cap) = proposal
            && let Err(error) = self.request_budget_locked(&mut state, cap)
        {
            state.last_error = Some(error.to_string());
            if let Some(budget) = &mut state.budget {
                budget.suspend();
            }
        }
    }

    fn request_budget_locked(&self, state: &mut StreamControlState, cap: u32) -> Result<i64> {
        ensure_ready(state)?;
        if state.settings.quality != StreamQuality::Adaptive
            || !protocol(state).supports_custom_bitrate()
        {
            bail!("当前模式不支持受限自适应");
        }
        if state.latest_requested_sequence != state.last_applied_sequence {
            bail!("请等待当前串流设置确认");
        }
        if !(1..=state.settings.adaptive_ceiling_mbps).contains(&cap) {
            bail!("视频预算超过用户上限");
        }
        let mut baseline = state.baseline;
        baseline.max_custom_bitrate = cap * 1_000_000;
        let sequence = state.next_sequence;
        let payload = encode_capture_setting(sequence, baseline)?;
        state.baseline = baseline;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.pending_sequences.push_back(sequence);
        state.last_error = None;
        tracing::info!(
            sequence,
            cap_mbps = cap,
            ceiling_mbps = state.settings.adaptive_ceiling_mbps,
            "bounded adaptive video budget requested"
        );
        let protocol = protocol(state);
        self.send_locked(
            state,
            OutgoingControlMessage {
                sequence,
                payload,
                protocol,
                completion: None,
            },
            Some(format!("受限自适应 · 视频预算 {cap} Mbps")),
        )
    }

    pub fn adopt_budget_suggestion(&self) -> Result<i64> {
        let mut state = lock(&self.shared);
        let cap = state
            .budget
            .as_ref()
            .and_then(|b| b.snapshot().suggested_mbps)
            .ok_or_else(|| anyhow!("当前没有有效的试调建议"))?;
        self.request_budget_locked(&mut state, cap)
    }

    /// Explicitly retry the user's ceiling; only this action forgets a recent
    /// failed bound. Routine recovery does not repeatedly hit that failed rate.
    pub fn reassess_budget(&self) -> Result<i64> {
        let mut state = lock(&self.shared);
        ensure_ready(&state)?;
        if state.settings.quality != StreamQuality::Adaptive {
            bail!("请先选择受限自适应");
        }
        let settings = state.settings;
        state.budget = Some(BudgetPolicy::new(
            settings.adaptive_ceiling_mbps,
            settings.stability_priority,
            Instant::now(),
        ));
        // A prior timed-out attempt has no known result; this explicit full
        // snapshot supersedes it and provides a new acknowledgment boundary.
        state.latest_requested_sequence = state.last_applied_sequence;
        state.pending_sequences.clear();
        self.request_budget_locked(&mut state, settings.adaptive_ceiling_mbps)
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
            state.registered_video_tracks.clear();
            state.track_registration = None;
            state.track_registration_error = None;
            if let Some(budget) = &mut state.budget {
                budget.suspend();
            }
            if let Some(sequence) = state.pending_sequences.back().copied() {
                state
                    .performance
                    .fail_stream_switch(sequence, format!("{label} 通道已关闭"));
            }
            state.pending_sequences.clear();
        }
        self.maybe_send_initial_capture_sync(&mut state);
        drop(state);
        self.protocol_changed.notify_one();
    }

    pub(crate) fn set_video_stream(&self, codec: VideoCodec, video_track_index: i32) {
        let mut state = lock(&self.shared);
        state.active_video_track_index = video_track_index;
        state.baseline.codec_type = match codec {
            VideoCodec::H264 => 1,
            VideoCodec::H265 => 2,
        };
        refresh_active_screen(&mut state);
        self.maybe_send_initial_capture_sync(&mut state);
    }

    pub(crate) fn set_capability(&self, capability: DualCapability) {
        tracing::info!(capability = %serde_json::to_string(&capability).expect("integer capability fields serialize"),
            "official dual capability model updated; no capture request triggered");
        // B770F0 updates the model only. Do not turn late/duplicate capability
        // messages into unsolicited stream changes or decoder restarts.
        lock(&self.shared).capability = Some(capability);
    }

    pub(crate) fn select_viewed_video_track(&self, video_track_index: i32) {
        let mut state = lock(&self.shared);
        state.active_video_track_index = video_track_index;
        refresh_active_screen(&mut state);
    }

    pub(crate) fn mark_send_failed(&self, sequence: i64, error: &str) {
        let mut state = lock(&self.shared);
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
        state.pending_sequences.clear();
        if let Some(budget) = &mut state.budget {
            budget.suspend();
        }
        state.last_error = Some(error.to_owned());
        state.performance.fail_stream_switch(sequence, error);
    }

    pub(crate) fn protocol_notifications(&self) -> Arc<Notify> {
        Arc::clone(&self.protocol_changed)
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
            // feature version, not permission to invent a legacy handshake.
            state.last_error = Some("PB 特性协商超时；画面继续播放，串流设置尚未就绪".to_owned());
        }
    }

    pub(crate) fn handle_protocol_message(
        &self,
        payload: &[u8],
        source: PbMessageSource,
    ) -> Result<()> {
        let message = PbControlMessage::decode(payload)
            .map_err(|error| anyhow!("decode UU protobuf domain message: {error}"))?;
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
                            handshake_changed = true;
                            state.last_error = None;
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
            Some(PbPayload::ReportQosStats(qos)) => {
                if state.baseline.frame_quality == VIDEO_QUALITY_AUTO
                    && let Some(auto_quality) = reported_auto_quality(qos.video_quality)
                {
                    state.baseline.auto_frame_quality = auto_quality;
                    state.performance.set_quality(viewing_quality_label(&state));
                }
                tracing::debug!(encoder_type = %qos.encoder_type, capture_type = %qos.capture_type, probe_bps = qos.probe_bps, video_quality = qos.video_quality,
                    fast_bitrate = qos.fast_bitrate, general_bitrate = qos.general_bitrate, hd_bitrate = qos.hd_bitrate, bluray_bitrate = qos.bluray_bitrate,
                    "official QoS quality status received");
            }
            Some(PbPayload::Screens(screens)) => update_screen_baseline(&mut state, screens),
            Some(PbPayload::CaptureSettingSync(bytes)) => {
                // GameViewer 141041270 (ordinary video) has no tag-25 state
                // consumer. E50910 belongs to SecondScreenSettingsModel.
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
                    match response.payload {
                        Some(PbRpcResponsePayload::CaptureSetting(capture)) => {
                            apply_capture_setting_response(&mut state, header.request_id, capture)
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
                                state.track_registration_error =
                                    Some(format!("视频轨道注册被拒绝（{}）", result.error_code));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(PbPayload::CaptureConfigResponse(response)) => {
                apply_legacy_capture_response(&mut state, response)
            }
            _ => {}
        }
        self.maybe_send_initial_capture_sync(&mut state);
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

    fn maybe_send_initial_capture_sync(&self, state: &mut StreamControlState) {
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
    settings: StreamControlSettings,
) -> Result<()> {
    if !settings.frame_rate.is_supported(state.local_display) {
        bail!("串流帧率超过本机显示档位");
    }
    if settings.quality == StreamQuality::Custom
        && !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.custom_bitrate_mbps)
    {
        bail!("自定义码率必须在 1..={MAX_CUSTOM_BITRATE_MBPS} Mbps 之间");
    }
    if settings.quality == StreamQuality::Adaptive
        && !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.adaptive_ceiling_mbps)
    {
        bail!("视频码率上限必须在 1..={MAX_CUSTOM_BITRATE_MBPS} Mbps 之间");
    }
    let requested_fps = settings.frame_rate.value(state.local_display);
    state.baseline.requested_fps = requested_fps;
    state.baseline.fps_count = state
        .local_display
        .refresh_hz
        .clamp(1, requested_fps.max(1));
    state.baseline.frame_quality = settings.quality.protobuf();
    state.baseline.max_custom_bitrate = match settings.quality {
        StreamQuality::Custom => settings.custom_bitrate_mbps * 1_000_000,
        StreamQuality::Adaptive => settings.adaptive_ceiling_mbps * 1_000_000,
        _ => 0,
    };
    state.settings = settings;
    Ok(())
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
        || state.baseline.codec_type == 0
    {
        return Ok(None);
    }
    let active_protocol = protocol(state);
    if matches!(
        state.settings.quality,
        StreamQuality::Custom | StreamQuality::Adaptive
    ) && !active_protocol.supports_custom_bitrate()
    {
        bail!("新连接的被控端不支持自定义码率，不能恢复该选择");
    }
    if let Some(capability) = &state.capability {
        let requested = state.settings.quality.capability_quality();
        let selected = capability.select(CHROMA_420 as u8, false, requested);
        if selected.result != 0 {
            bail!(
                "双端没有可用的420 SDR视频格式（协商结果{}）",
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
    constrain_auto_quality(state);
    let baseline = state.baseline;
    let sequence = state.next_sequence;
    let payload = match active_protocol {
        StreamControlProtocol::CaptureSetting { .. } => encode_capture_setting(sequence, baseline)?,
        StreamControlProtocol::LegacyCaptureConfig => {
            encode_legacy_capture_config(sequence, baseline)?
        }
        StreamControlProtocol::Negotiating => return Ok(None),
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

fn validate_quality(
    state: &StreamControlState,
    quality: StreamQuality,
) -> Result<Option<FrameQualityCapability>> {
    let Some(capability) = &state.capability else {
        return Ok(None);
    };
    let requested = quality.capability_quality();
    let selected = capability.select(CHROMA_420 as u8, false, requested);
    if selected.result != 0 || selected.max_frame_quality < requested {
        bail!(
            "双端能力不支持{}（最高能力档位{}，结果{}）",
            quality.label(),
            selected.max_frame_quality,
            selected.result
        );
    }
    Ok(Some(selected))
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
        .and_then(|cap| cap.exact(state.baseline.codec_type, CHROMA_420 as u8, false))
        .filter(|row| row.result == 0)
        && let Some(maximum) = quality_from_capability(row.max_frame_quality)
        && state.baseline.auto_frame_quality > maximum.protobuf()
    {
        state.baseline.auto_frame_quality = maximum.protobuf();
    }
}

fn ensure_ready(state: &StreamControlState) -> Result<()> {
    if !state.control_channel_open {
        bail!("UU CONTROL 通道尚未打开");
    }
    if !state.text_channel_open {
        bail!("UU TEXT 通道尚未打开");
    }
    if !state.pb_connected {
        bail!("UU PB 特性协商尚未完成");
    }
    if state.remote_display.is_none() {
        bail!("尚未收到活动屏幕基线，已阻止发送不完整的串流设置");
    }
    if state.baseline.codec_type == 0 {
        bail!("视频编码尚未完成协商");
    }
    Ok(())
}

fn protocol(state: &StreamControlState) -> StreamControlProtocol {
    if !state.pb_connected {
        StreamControlProtocol::Negotiating
    } else if state.peer_capture_setting >= 1 {
        StreamControlProtocol::CaptureSetting {
            feature_level: state.peer_capture_setting,
        }
    } else {
        StreamControlProtocol::LegacyCaptureConfig
    }
}

fn update_screen_baseline(state: &mut StreamControlState, screens: PbScreenSources) {
    state.screens_generation = state.screens_generation.wrapping_add(1);
    state.current_screen_id = screens.current_screen_id;
    state.screens = screens
        .screens
        .into_iter()
        .filter_map(|screen| {
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
            })
        })
        .collect();
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
    let mut failures = Vec::new();
    let mut notices = Vec::new();
    for error in response.errors {
        match error.error_code {
            0 => {}
            CAPTURE_RESULT_FPS_ADJUSTED => notices.push(format!(
                "远端屏幕刷新率低于请求档位，串流已按屏幕能力降档（{}）",
                format_pb_error(error)
            )),
            _ => failures.push(format_pb_error(error)),
        }
    }
    finish_request(state, request_id, failures, notices);
}

fn apply_legacy_capture_response(
    state: &mut StreamControlState,
    response: PbCaptureConfigResponse,
) {
    let failures = (response.error_code != 0)
        .then(|| {
            format!(
                "{}: {}",
                response.error_code,
                if response.error_message.is_empty() {
                    "CaptureConfig failed"
                } else {
                    &response.error_message
                }
            )
        })
        .into_iter()
        .collect();
    finish_request(state, response.request_seq, failures, Vec::new());
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

fn official_quality_label(baseline: &CaptureSettingBaseline) -> String {
    match baseline.frame_quality {
        VIDEO_QUALITY_FAST => "480P".to_owned(),
        VIDEO_QUALITY_GENERAL => "720P".to_owned(),
        VIDEO_QUALITY_HD => "1080P".to_owned(),
        VIDEO_QUALITY_BLURAY => "4K".to_owned(),
        VIDEO_QUALITY_AUTO => match baseline.auto_frame_quality {
            VIDEO_QUALITY_FAST => "480P auto".to_owned(),
            VIDEO_QUALITY_GENERAL => "720P auto".to_owned(),
            VIDEO_QUALITY_HD => "1080P auto".to_owned(),
            VIDEO_QUALITY_BLURAY => "4K auto".to_owned(),
            _ => "auto".to_owned(),
        },
        VIDEO_QUALITY_CUSTOM if baseline.max_custom_bitrate >= 1_000_000 => {
            format!("{} Mbps", baseline.max_custom_bitrate / 1_000_000)
        }
        VIDEO_QUALITY_CUSTOM => "自定义".to_owned(),
        _ => "auto".to_owned(),
    }
}

fn viewing_quality_label(state: &StreamControlState) -> String {
    if state.settings.quality == StreamQuality::Adaptive {
        let applied = state
            .budget
            .as_ref()
            .and_then(|budget| budget.snapshot().applied_mbps);
        applied.map_or_else(
            || "受限自适应 · 待确认".into(),
            |cap| format!("受限自适应 {cap}M"),
        )
    } else {
        official_quality_label(&state.baseline)
    }
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
    if let Some((sequence, settings)) = state.user_preference_pending
        && sequence == request_id
    {
        if failures.is_empty() {
            state.preference_updates.send_replace(Some(settings));
        }
        state.user_preference_pending = None;
    }
    state
        .pending_sequences
        .retain(|pending| *pending != request_id);
    if state.latest_requested_sequence != Some(request_id) {
        return;
    }
    // These are complete snapshots on one ordered channel. A terminal response
    // for the newest request supersedes all older outstanding UI requests.
    state.pending_sequences.clear();
    if failures.is_empty() {
        if let Some(budget) = &mut state.budget {
            budget.acknowledge(request_id, Instant::now());
        }
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
        if let Some(budget) = &mut state.budget {
            budget.suspend();
        }
        let error = failures.join("; ");
        state.last_error = Some(error.clone());
        state.last_notice = None;
        state
            .performance
            .fail_stream_switch(request_id, error.clone());
        tracing::warn!(sequence = request_id, %error, "remote host rejected runtime stream settings");
    }
}

fn encode_capture_setting(sequence: i64, baseline: CaptureSettingBaseline) -> Result<Vec<u8>> {
    let request = PbCaptureSettingRequest {
        fps: fps_to_protobuf(baseline.requested_fps),
        frame_quality: baseline.frame_quality,
        cursor_capture: baseline.cursor_capture,
        screen_id: EXISTING_SESSION_TRACKS,
        resolution_width: UNCHANGED_PHYSICAL_DIMENSION,
        resolution_height: UNCHANGED_PHYSICAL_DIMENSION,
        chroma_format: baseline.chroma_format,
        max_custom_bitrate: i32::try_from(baseline.max_custom_bitrate)?,
        dpi_scale: 0,
        resolution_type: RESOLUTION_ORIGINAL,
        enable_hdr: baseline.enable_hdr,
        auto_frame_quality: baseline.auto_frame_quality,
        codec_type: baseline.codec_type,
        max_scale_width: i32::try_from(baseline.max_scale_width)?,
        max_scale_height: i32::try_from(baseline.max_scale_height)?,
        resolution_pixel_width: 0,
        resolution_pixel_height: 0,
        fps_count: i32::try_from(baseline.fps_count)?,
    };
    Ok(encode_envelope(
        sequence,
        PbPayload::RpcRequest(
            PbRpcRequest {
                request_header: Some(PbRequestHeader {
                    request_id: sequence,
                }),
                capture_setting: Some(request),
                send_video_track: None,
            }
            .encode_to_vec(),
        ),
    ))
}

fn encode_legacy_capture_config(
    sequence: i64,
    baseline: CaptureSettingBaseline,
) -> Result<Vec<u8>> {
    if baseline.frame_quality == VIDEO_QUALITY_CUSTOM {
        bail!("legacy CaptureConfig does not support custom bitrate");
    }
    let capture = PbCaptureConfig {
        client: 3,
        fps: match baseline.requested_fps {
            30 => 0,
            60 => 1,
            90 => 2,
            _ => 3,
        },
        frame_quality: baseline.frame_quality,
        cursor_capture: baseline.cursor_capture,
        screen_id: EXISTING_SESSION_TRACKS,
        resolution_width: UNCHANGED_PHYSICAL_DIMENSION,
        resolution_height: UNCHANGED_PHYSICAL_DIMENSION,
        config_flag: 0,
    };
    Ok(encode_envelope(sequence, PbPayload::CaptureConfig(capture)))
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

#[cfg(test)]
mod budget_control_tests {
    use super::*;

    #[test]
    fn track_registration_ack_does_not_confirm_capture_settings() {
        let (control, mut outgoing) = ready_control();
        lock(&control.shared).initial_capture_sync_sent = true;
        control.set_available_video_tracks(vec![2, -1, 0, 2]);
        let registration = outgoing.try_recv().unwrap();
        let wire = PbControlMessage::decode(registration.payload.as_slice()).unwrap();
        let Some(PbPayload::RpcRequest(bytes)) = wire.payload else {
            panic!("track registration RPC");
        };
        let registration_request = PbRpcRequest::decode(bytes.as_slice()).unwrap();
        assert!(registration_request.capture_setting.is_none());
        assert_eq!(
            registration_request
                .send_video_track
                .unwrap()
                .video_track_index,
            [0, 2]
        );
        for (id, accepted) in [
            (registration.sequence + 99, false),
            (registration.sequence, true),
        ] {
            let reply = encode_envelope(
                id,
                PbPayload::RpcResponse(PbRpcResponse {
                    response_header: Some(PbResponseHeader { request_id: id }),
                    payload: Some(PbRpcResponsePayload::SendVideoTrackRsp(
                        PbSendVideoTrackResponse { error_code: 0 },
                    )),
                }),
            );
            control
                .handle_protocol_message(&reply, PbMessageSource::Text)
                .unwrap();
            assert_eq!(
                lock(&control.shared).registered_video_tracks == [0, 2],
                accepted
            );
            assert!(control.snapshot().last_applied_sequence.is_none());
        }
        control.set_available_video_tracks(vec![0, 2]);
        assert!(outgoing.try_recv().is_err());
        // Focus is observational: it must not rewrite the shared codec or
        // manufacture a CaptureSetting request from another screen's decoder.
        let settings = control.snapshot().settings;
        control.select_viewed_video_track(2);
        assert_eq!(control.snapshot().settings, settings);
        assert_eq!(lock(&control.shared).baseline.codec_type, 2);
        assert!(outgoing.try_recv().is_err());
    }

    fn ready_control() -> (
        StreamControlHandle,
        mpsc::UnboundedReceiver<OutgoingControlMessage>,
    ) {
        let display = LocalDisplayInfo {
            width: 2560,
            height: 1440,
            refresh_hz: 144,
        };
        let profile = crate::media::ConnectionMediaOptions::default()
            .resolve(display)
            .unwrap();
        let (control, receiver, _) =
            StreamControlHandle::new(profile, PerformanceMonitor::new("test"));
        {
            let mut state = lock(&control.shared);
            state.control_channel_open = true;
            state.text_channel_open = true;
            state.pb_connected = true;
            state.peer_capture_setting = 6;
            state.baseline.codec_type = 2;
            state.remote_display = Some(RemoteDisplayState {
                screen_id: 7,
                video_track_index: 0,
                width: 1920,
                height: 1200,
                refresh_hz: 120,
            });
        }
        (control, receiver)
    }

    fn ack(control: &StreamControlHandle, sequence: i64, failed: bool) {
        let errors = if failed {
            vec![PbError {
                error_code: -1,
                ..Default::default()
            }]
        } else {
            vec![]
        };
        let wire = encode_envelope(
            sequence,
            PbPayload::RpcResponse(PbRpcResponse {
                response_header: Some(PbResponseHeader {
                    request_id: sequence,
                }),
                payload: Some(PbRpcResponsePayload::CaptureSetting(
                    PbCaptureSettingResponse { errors },
                )),
            }),
        );
        control
            .handle_protocol_message(&wire, PbMessageSource::Text)
            .unwrap();
    }

    fn request(
        receiver: &mut mpsc::UnboundedReceiver<OutgoingControlMessage>,
    ) -> PbCaptureSettingRequest {
        let packet = receiver.try_recv().unwrap();
        let wire = PbControlMessage::decode(packet.payload.as_slice()).unwrap();
        let Some(PbPayload::RpcRequest(bytes)) = wire.payload else {
            panic!("expected RPC settings")
        };
        let request = PbRpcRequest::decode(bytes.as_slice())
            .unwrap()
            .capture_setting
            .unwrap();
        assert_eq!(
            (
                request.screen_id,
                request.resolution_width,
                request.resolution_height,
                request.dpi_scale
            ),
            (-2, -1, -1, 0)
        );
        assert_eq!(
            (
                request.resolution_pixel_width,
                request.resolution_pixel_height
            ),
            (0, 0)
        );
        request
    }

    #[test]
    fn budget_control_preserves_read_only_fields_and_manual_choice_wins_late_ack() {
        let (control, mut receiver) = ready_control();
        let mut saved = control.preference_updates();
        let mut settings = control.snapshot().settings;
        settings.quality = StreamQuality::Adaptive;
        settings.adaptive_ceiling_mbps = 40;
        let first = control.apply(settings).unwrap();
        assert_eq!(request(&mut receiver).max_custom_bitrate, 40_000_000);
        assert_eq!(control.snapshot().adaptive.unwrap().applied_mbps, None);
        assert!(!saved.has_changed().unwrap());
        ack(&control, first, false);
        assert_eq!(*saved.borrow_and_update(), Some(settings));
        let lowered = {
            let mut state = lock(&control.shared);
            assert!(control.request_budget_locked(&mut state, 41).is_err());
            control.request_budget_locked(&mut state, 30).unwrap()
        };
        assert_eq!(request(&mut receiver).max_custom_bitrate, 30_000_000);
        settings.quality = StreamQuality::Auto;
        let explicit = control.apply(settings).unwrap();
        let auto = request(&mut receiver);
        assert_eq!(
            (auto.frame_quality, auto.max_custom_bitrate),
            (VIDEO_QUALITY_AUTO, 0)
        );
        ack(&control, explicit, false);
        assert_eq!(*saved.borrow_and_update(), Some(settings));
        ack(&control, lowered, false);
        assert!(!saved.has_changed().unwrap());
        let current = control.snapshot();
        assert_eq!(current.last_applied_sequence, Some(explicit));
        assert_eq!(current.pending_count, 0);
        assert!(current.adaptive.is_none());
        control.observe_budget(&PerformanceMonitor::new("test"), None, Some("relay"), true);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn rejected_budget_and_channel_close_stop_adjustments_and_fps_keeps_lower_budget() {
        let (control, mut receiver) = ready_control();
        let mut saved = control.preference_updates();
        let mut settings = control.snapshot().settings;
        settings.quality = StreamQuality::Adaptive;
        settings.adaptive_ceiling_mbps = 40;
        let first = control.apply(settings).unwrap();
        request(&mut receiver);
        ack(&control, first, false);
        assert_eq!(*saved.borrow_and_update(), Some(settings));
        let lowered = control
            .request_budget_locked(&mut lock(&control.shared), 30)
            .unwrap();
        request(&mut receiver);
        settings.frame_rate = FrameRateChoice::Fps60;
        let fps = control.apply(settings).unwrap();
        assert_eq!(request(&mut receiver).max_custom_bitrate, 30_000_000);
        ack(&control, lowered, false);
        assert_eq!(control.snapshot().last_applied_sequence, Some(first));
        ack(&control, fps, true);
        assert!(!saved.has_changed().unwrap());
        assert_eq!(
            control.snapshot().adaptive.unwrap().phase,
            BudgetPhase::Suspended
        );
        let retry = control.reassess_budget().unwrap();
        assert_eq!(request(&mut receiver).max_custom_bitrate, 40_000_000);
        ack(&control, retry, false);
        assert!(!saved.has_changed().unwrap());
        control.set_data_channel_open("TEXT_DATA_CHANNEL", false);
        assert!(control.reassess_budget().is_err());
        assert_eq!(
            control.snapshot().adaptive.unwrap().phase,
            BudgetPhase::Suspended
        );
        assert!(receiver.try_recv().is_err());
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
        tags = "3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27"
    )]
    payload: Option<PbPayload>,
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
    #[prost(message, tag = "9")]
    CaptureConfig(PbCaptureConfig),
    #[prost(bytes, tag = "10")]
    RomMessage(Vec<u8>),
    #[prost(bytes, tag = "11")]
    SendToRom(Vec<u8>),
    #[prost(bytes, tag = "12")]
    ReportError(Vec<u8>),
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
    #[prost(message, tag = "19")]
    CaptureConfigResponse(PbCaptureConfigResponse),
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

impl PbFeatureFlag {
    fn read_only_viewer() -> Self {
        Self {
            capture_setting: 6,
            simple_action: 0,
            system_metrics: 0,
            private_screen: 0,
            update_acquire: 0,
            file_transfer_ftp: 0,
            file_transfer_ftp2: 0,
            clipboard: 0,
            qos_stat: 1,
            mumu_control: 0,
            virtual_mouse_device: 0,
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbCaptureConfig {
    #[prost(int32, tag = "1")]
    client: i32,
    #[prost(int32, tag = "2")]
    fps: i32,
    #[prost(int32, tag = "3")]
    frame_quality: i32,
    #[prost(bool, tag = "4")]
    cursor_capture: bool,
    #[prost(int32, tag = "5")]
    screen_id: i32,
    #[prost(int32, tag = "6")]
    resolution_width: i32,
    #[prost(int32, tag = "7")]
    resolution_height: i32,
    #[prost(int64, tag = "8")]
    config_flag: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PbCaptureConfigResponse {
    #[prost(int64, tag = "1")]
    request_seq: i64,
    #[prost(int64, tag = "2")]
    config_flag: i64,
    #[prost(int32, tag = "3")]
    error_code: i32,
    #[prost(string, tag = "4")]
    error_message: String,
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
    #[prost(message, optional, tag = "1")]
    request_header: Option<PbRequestHeader>,
    #[prost(message, optional, tag = "2")]
    capture_setting: Option<PbCaptureSettingRequest>,
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
    #[prost(bytes, tag = "23")]
    DrawResp(Vec<u8>),
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
