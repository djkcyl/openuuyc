use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::decoder::{
    DecodedBatch, DecodedFrame, DecoderOutputIssue, NativeVideoDecoder, RenderSurface,
};
use crate::decoder_pool::DecoderPool;
use crate::decoder_result::VideoDecodeResult;
use crate::media::VideoCodec;
use crate::performance::{PerformanceMonitor, PerformanceSnapshot};
use crate::rtc::{EncodedVideoFrame, FrameSenderTiming, VideoFrameSink, VideoReceiverFeedback};
use crate::stream_control::StreamControlHandle;
use crate::video_color::RenderColor;
use crate::video_format::{VideoFormatSignature, parse_annex_b_format};

pub(crate) mod device_switch;
mod performance_panel;
mod screens;
mod stream_menu;
use stream_menu::{StreamControlUi, show_stream_control_window};
mod annotation;
mod display_transition;

#[cfg(windows)]
mod windows_cursor;

#[cfg(windows)]
mod windows_keyboard;

#[cfg(windows)]
mod windows_mouse;

#[cfg(windows)]
#[path = "viewer/windows_presenter.rs"]
pub(crate) mod presenter;
#[cfg(not(windows))]
#[path = "viewer/linux_presenter.rs"]
pub(crate) mod presenter;

/// Windows installs a low-level keyboard hook so system keys reach the remote
/// desktop. X11 and Wayland deliver keys through the focused window instead.
pub(crate) struct DesktopInputHook {
    #[cfg(windows)]
    _hook: windows_keyboard::KeyboardHook,
}
#[cfg(windows)]
pub(crate) fn desktop_input_message(message: *const std::ffi::c_void) -> bool {
    windows_keyboard::message(message) || windows_mouse::router().message(message)
}
pub(crate) fn desktop_input_hook() -> Result<DesktopInputHook> {
    #[cfg(windows)]
    {
        windows_keyboard::remove_unused_raw_keyboard()?;
        windows_keyboard::KeyboardHook::install().map(|hook| DesktopInputHook { _hook: hook })
    }
    #[cfg(not(windows))]
    Ok(DesktopInputHook {})
}

#[cfg(windows)]
mod windows_ui;

const CONNECTION_PROGRESS_STEPS: u8 = 13;

#[derive(Clone, Debug)]
pub enum ConnectionProgressState {
    Working,
    Ready,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ConnectionProgress {
    pub step: u8,
    pub title: String,
    pub detail: String,
    pub state: ConnectionProgressState,
    pub(crate) background: Option<crate::wallpaper::Source>,
}

#[derive(Clone, Default)]
pub(crate) struct ViewerDisplayHandle {
    pub surface_writer: Option<crate::decoder::windows_surface::D3D11SurfaceWriter>,
}

impl ConnectionProgress {
    pub fn working(step: u8, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            step,
            title: title.into(),
            detail: detail.into(),
            state: ConnectionProgressState::Working,
            background: None,
        }
    }

    pub fn ready(detail: impl Into<String>) -> Self {
        Self {
            step: CONNECTION_PROGRESS_STEPS,
            title: "连接完成".to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Ready,
            background: None,
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            step: 0,
            title: "无法建立连接".to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Failed,
            background: None,
        }
    }
}

impl ConnectionProgress {
    pub(crate) fn background(source: crate::wallpaper::Source) -> Self {
        Self {
            background: Some(source),
            ..Self::working(0, "", "")
        }
    }
}

fn configure_viewer_visuals(ctx: &egui::Context) {
    crate::ui::theme::configure(ctx);
}

pub(crate) fn run_connecting_viewer_window(
    alias: String,
    progress: std_mpsc::Receiver<ConnectionProgress>,
    session: std_mpsc::Receiver<ViewerWindowEvent>,
    display_sender: oneshot::Sender<ViewerDisplayHandle>,
) -> Result<()> {
    {
        presenter::run_connecting(presenter::ConnectingWindowsRunConfig {
            alias,
            progress,
            session,
            display_sender,
        })
    }
}

pub(crate) enum ViewerWindowEvent {
    Close,
    Playing(Box<NativeViewerSession>),
    Reconnect {
        alias: String,
        window: Option<winit::window::WindowId>,
        progress: std_mpsc::Receiver<ConnectionProgress>,
        display: oneshot::Sender<ViewerDisplayHandle>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ViewerPreferences {
    performance_mode: PerformancePanelMode,
    intercept_shortcuts: bool,
}

impl Default for ViewerPreferences {
    fn default() -> Self {
        Self {
            performance_mode: PerformancePanelMode::Compact,
            intercept_shortcuts: true,
        }
    }
}

pub(super) struct ConnectionProgressApp {
    alias: String,
    receiver: std_mpsc::Receiver<ConnectionProgress>,
    details_open: bool,
    events: Vec<(Duration, ConnectionProgress)>,
    current: ConnectionProgress,
    started_at: Instant,
    background: Option<crate::wallpaper::Source>,
    wallpapers: crate::wallpaper::Wallpapers,
}

impl ConnectionProgressApp {
    pub(super) fn new(alias: String, receiver: std_mpsc::Receiver<ConnectionProgress>) -> Self {
        let current = ConnectionProgress::working(1, "准备连接", "正在读取本地会话和设备配置");
        Self {
            alias,
            receiver,
            details_open: false,
            events: vec![(Duration::ZERO, current.clone())],
            current,
            started_at: Instant::now(),
            background: None,
            wallpapers: Default::default(),
        }
    }

    pub(super) fn drain(&mut self) {
        while let Ok(mut progress) = self.receiver.try_recv() {
            if let Some(source) = progress.background.take() {
                if self.background.as_ref() != Some(&source) {
                    self.wallpapers.clear();
                    self.background = Some(source);
                }
                continue;
            }
            if progress.step == 0 {
                progress.step = self.current.step;
            }
            self.events
                .push((self.started_at.elapsed(), progress.clone()));
            if matches!(progress.state, ConnectionProgressState::Ready) {
                tracing::debug!("connection UI reached ready state");
            }
            self.current = progress;
        }
    }
}

mod connection_progress;

#[derive(Debug)]
struct DecodedVideoFrame {
    is_new_picture: Option<bool>,
    width: u32,
    height: u32,
    surface: RenderSurface,
    color: RenderColor,
    received_at: Instant,
    decoded_at: Instant,
    assembly_delay: Duration,
    input_queue_delay: Duration,
    decode_pipeline_delay: Duration,
    rotation: u16,
    sender_timing: FrameSenderTiming,
}

struct FrameTiming {
    is_new_picture: Option<bool>,
    color: RenderColor,
    rtp_timestamp: u32,
    received_at: Instant,
    assembled_at: Instant,
    submitted_at: Instant,
    rotation: u16,
    keyframe: bool,
    sender_timing: FrameSenderTiming,
}

struct DecodedForwardContext<'a> {
    frame_queue: &'a Mutex<VecDeque<DecodedVideoFrame>>,
    frame_wake: &'a FrameWake,
    performance: &'a PerformanceMonitor,
    receiver_feedback: &'a mpsc::UnboundedSender<VideoReceiverFeedback>,
    inflight: &'a mut HashMap<i64, u32>,
}

const OFFICIAL_DECODER_INFLIGHT_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug)]
struct DecoderCutoverDecision {
    drop_frame: bool,
    request_keyframe: bool,
    reset_decoder: bool,
    hard_reset: bool,
    source_changed: bool,
    content_changed: bool,
    resolution_changed: bool,
    pressure_recovery: bool,
}

struct DecoderCutoverState {
    waiting_for_keyframe: bool,
    pressure_recovery: bool,
    current_source_id: Option<u16>,
    current_content_type: u8,
    current_width: u32,
    current_height: u32,
    generation: u32,
}

impl DecoderCutoverState {
    const fn new() -> Self {
        Self {
            waiting_for_keyframe: false,
            pressure_recovery: false,
            current_source_id: None,
            current_content_type: 0,
            current_width: 0,
            current_height: 0,
            generation: 0,
        }
    }

    fn evaluate(
        &mut self,
        frame: &EncodedVideoFrame,
        format: Option<VideoFormatSignature>,
    ) -> DecoderCutoverDecision {
        if self.pressure_recovery && !frame.keyframe {
            return DecoderCutoverDecision {
                drop_frame: true,
                request_keyframe: false,
                reset_decoder: false,
                hard_reset: false,
                source_changed: false,
                content_changed: false,
                resolution_changed: false,
                pressure_recovery: true,
            };
        }

        let source_changed = frame
            .video_capture_index
            .zip(self.current_source_id)
            .is_some_and(|(source, previous)| source != previous);
        let content_changed =
            frame.content_type != 0 && frame.content_type != self.current_content_type;
        let (next_width, next_height) =
            format.map_or((0, 0), |format| (format.coded_width, format.coded_height));
        let resolution_changed = frame.keyframe
            && next_width != 0
            && next_height != 0
            && self.current_width != 0
            && self.current_height != 0
            && (next_width != self.current_width || next_height != self.current_height);
        let cutover = frame.keyframe || source_changed || content_changed;

        if self.waiting_for_keyframe {
            if !frame.keyframe {
                return DecoderCutoverDecision {
                    drop_frame: true,
                    request_keyframe: false,
                    reset_decoder: false,
                    hard_reset: false,
                    source_changed,
                    content_changed,
                    resolution_changed,
                    pressure_recovery: self.pressure_recovery,
                };
            }
            self.waiting_for_keyframe = false;
        } else if !frame.keyframe && cutover {
            self.waiting_for_keyframe = true;
            return DecoderCutoverDecision {
                drop_frame: true,
                request_keyframe: true,
                reset_decoder: false,
                hard_reset: false,
                source_changed,
                content_changed,
                resolution_changed,
                pressure_recovery: self.pressure_recovery,
            };
        }

        let pressure_recovery = self.pressure_recovery;
        let hard_reset = pressure_recovery
            || source_changed
            || (self.current_content_type != 0 && content_changed)
            || resolution_changed;
        let reset_decoder = frame.keyframe && (self.pressure_recovery || cutover);
        if reset_decoder {
            self.generation = self.generation.wrapping_add(1);
            self.pressure_recovery = false;
        }
        if content_changed {
            self.current_content_type = frame.content_type;
        }
        if let Some(source) = frame.video_capture_index {
            self.current_source_id = Some(source);
        }
        if frame.keyframe && next_width != 0 && next_height != 0 {
            self.current_width = next_width;
            self.current_height = next_height;
        }

        DecoderCutoverDecision {
            drop_frame: false,
            request_keyframe: false,
            reset_decoder,
            hard_reset,
            source_changed,
            content_changed,
            resolution_changed,
            pressure_recovery,
        }
    }

    fn replace_instance(&mut self) {
        let generation = self.generation.wrapping_add(1);
        *self = Self::new();
        self.generation = generation;
    }

    fn note_inflight_pressure(&mut self, inflight: usize, keyframe: bool) -> bool {
        if keyframe || self.pressure_recovery || inflight <= OFFICIAL_DECODER_INFLIGHT_LIMIT {
            return false;
        }
        self.pressure_recovery = true;
        true
    }

    const fn token(&self, frame_index: u32) -> i64 {
        ((self.generation as u64) << 32 | frame_index as u64) as i64
    }
}

type FrameQueue = Arc<Mutex<VecDeque<DecodedVideoFrame>>>;

/// UU's configured direct path serially presents every decoded frame. Receive
/// timing has already controlled decoder admission; there is no second clock.
fn take_next_frame(
    queue: &mut VecDeque<DecodedVideoFrame>,
    performance: &PerformanceMonitor,
) -> Option<DecodedVideoFrame> {
    let frame = queue.pop_front();
    performance.set_presentation_queue_frames(queue.len());
    frame
}

#[derive(Clone, Default)]
struct FrameWake {
    visible: Arc<AtomicBool>,

    render_thread: Arc<Mutex<Option<std::thread::Thread>>>,
}

impl FrameWake {
    fn install_render_thread(&self, thread: std::thread::Thread) {
        *mutex_lock(&self.render_thread) = Some(thread);
        self.visible.store(true, Ordering::Release);
    }

    fn notify(&self) {
        if let Some(thread) = mutex_lock(&self.render_thread).as_ref() {
            thread.unpark();
        }
    }
}

struct DecodeActivity {
    software: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    idle: tokio::sync::watch::Receiver<u64>,
}

pub struct NativeViewerSession {
    // A negotiated track may be reassigned after a display topology change.
    screen_binding: std::sync::atomic::AtomicU64,
    track_index: i32,
    screens: Option<Box<screens::ScreenPlayback>>,
    title: String,
    video_sink: Option<mpsc::UnboundedSender<EncodedVideoFrame>>,
    frame_queue: FrameQueue,
    frame_wake: FrameWake,
    startup_receiver: Option<oneshot::Receiver<Result<(), String>>>,
    performance: PerformanceMonitor,
    stream_control: StreamControlHandle,
    shutdown: Arc<AtomicBool>,
    manager_thread: Option<JoinHandle<()>>,
    manager_wake: std::thread::Thread,
    fatal_error: Arc<Mutex<Option<String>>>,
    decode_activity: Box<DecodeActivity>,
}

#[derive(Clone)]
pub struct ViewerCloseHandle {
    shutdown: Arc<AtomicBool>,
    frame_wake: FrameWake,
    manager_wake: std::thread::Thread,
}

impl ViewerCloseHandle {
    pub fn close(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.manager_wake.unpark();
        self.frame_wake.notify();
    }
}

pub(crate) struct ViewerLaunchConfig {
    pub codec: VideoCodec,
    pub hardware_decode: bool,
    pub title: String,
    // Backend allocation hint only. The bitstream supplies actual dimensions.
    pub initial_width: u32,
    pub initial_height: u32,
    pub frame_rate: u32,
    pub receiver_feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    pub performance: PerformanceMonitor,
    pub stream_control: StreamControlHandle,
    pub display: ViewerDisplayHandle,
}

impl NativeViewerSession {
    pub(crate) fn set_device_switch(&mut self, switcher: device_switch::DeviceSwitcher) {
        if let Some(screens) = &mut self.screens {
            screens.device_switch = Some(switcher);
        }
    }
    pub(crate) async fn launch(config: ViewerLaunchConfig) -> Result<Self> {
        let ViewerLaunchConfig {
            codec,
            hardware_decode,
            title,
            initial_width,
            initial_height,
            frame_rate,
            receiver_feedback,
            performance,
            stream_control,
            display,
        } = config;
        let (video_sink, video_source) = mpsc::unbounded_channel();
        let frame_queue = Arc::new(Mutex::new(VecDeque::new()));
        let frame_wake = FrameWake::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let fatal_error = Arc::new(Mutex::new(None));
        let (startup_sender, startup_receiver) = oneshot::channel();
        let software_decode = Arc::new(AtomicBool::new(!hardware_decode));
        let decode_enabled = Arc::new(AtomicBool::new(true));
        let (decode_idle_sender, decode_idle) = tokio::sync::watch::channel(0);
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let manager_pause_epoch = Arc::clone(&pause_epoch);
        let manager_software = Arc::clone(&software_decode);
        let manager_enabled = Arc::clone(&decode_enabled);
        let manager_shutdown = Arc::clone(&shutdown);
        let manager_fatal_error = Arc::clone(&fatal_error);
        let manager_performance = performance.clone();
        let manager_frame_queue = Arc::clone(&frame_queue);
        let manager_frame_wake = frame_wake.clone();
        let manager_thread = std::thread::spawn(move || {
            decoder_manager(
                DecoderConfig {
                    codec,
                    width: initial_width,
                    height: initial_height,
                    frame_rate,
                    hardware_decode,
                    software_decode: manager_software,
                    decode_enabled: manager_enabled,
                    decode_idle: decode_idle_sender,
                    pause_epoch: manager_pause_epoch,

                    surface_writer: display.surface_writer,
                },
                video_source,
                manager_frame_queue,
                manager_frame_wake,
                manager_performance,
                manager_shutdown,
                manager_fatal_error,
                receiver_feedback,
                Some(startup_sender),
            );
        });
        let manager_wake = manager_thread.thread().clone();
        // Establish the owner before awaiting initialization. Dropping this
        // future (close/account cancellation) must also stop and join its worker.
        let session = Self {
            screen_binding: std::sync::atomic::AtomicU64::new(0),
            track_index: 0,
            screens: None,
            title,
            video_sink: Some(video_sink),
            frame_queue,
            frame_wake,
            performance,
            stream_control,
            shutdown,
            manager_thread: Some(manager_thread),
            manager_wake,
            fatal_error,
            startup_receiver: Some(startup_receiver),
            decode_activity: Box::new(DecodeActivity {
                software: software_decode,
                enabled: decode_enabled,
                pause_epoch,
                idle: decode_idle,
            }),
        };
        Ok(session)
    }

    /// Wait until the decoder opened against the first frame's parameter sets.
    pub(crate) async fn startup(&mut self) -> Result<()> {
        self.startup_receiver
            .take()
            .context("decoder startup already awaited")?
            .await
            .map_err(|error| anyhow!("native decoder startup ended: {error}"))?
            .map_err(|error| anyhow!("start native platform decoder: {error}"))
    }

    pub(crate) fn is_software(&self) -> bool {
        self.decode_activity.software.load(Ordering::Acquire)
    }

    pub(crate) async fn pause_software(&self) -> Result<()> {
        if !self.is_software() {
            return Ok(());
        }
        let mut idle = self.decode_activity.idle.clone();
        let epoch = self
            .decode_activity
            .pause_epoch
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        self.decode_activity.enabled.store(false, Ordering::Release);
        self.manager_wake.unpark();
        // An acknowledgement of an earlier pause cannot satisfy a new pause
        // issued immediately after resume, before the worker has run again.
        idle.wait_for(|idle| *idle >= epoch)
            .await
            .context("软件解码器未能停止")?;
        Ok(())
    }

    pub(crate) fn resume_decode(&self) -> bool {
        let paused = !self.decode_activity.enabled.swap(true, Ordering::AcqRel);
        if paused {
            self.manager_wake.unpark();
        }
        paused
    }
    pub(crate) fn decode_paused(&self) -> bool {
        !self.decode_activity.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn video_sink(&self) -> VideoFrameSink {
        VideoFrameSink::Unbounded {
            sender: self
                .video_sink
                .as_ref()
                .expect("native viewer video sink must exist while the session is alive")
                .clone(),
            wake: self.manager_wake.clone(),
        }
    }

    pub(crate) fn attach_screen_playback(
        &mut self,
        peer: &Arc<crate::rtc::NativePeer>,
        profile: crate::media::ConnectionMediaProfile,
        alias: &str,
        track: i32,
    ) {
        self.track_index = track;
        if let Some(screen) = self
            .stream_control
            .snapshot()
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track)
        {
            self.bind_screen(screen);
        }
        self.screens = Some(Box::new(screens::ScreenPlayback::new(
            peer,
            profile,
            alias,
            self.screen_id(),
        )));
    }

    pub(crate) fn screen_id(&self) -> i32 {
        self.screen_binding.load(Ordering::Acquire) as u32 as i32
    }

    pub(crate) fn screen_binding(&self) -> u64 {
        self.screen_binding.load(Ordering::Acquire)
    }

    pub(crate) fn bind_screen(&self, screen: &crate::stream_control::RemoteScreen) -> bool {
        let binding =
            (screen.id as u32 as u64) | ((screen.display.screen_type as u32 as u64) << 32);
        let changed = self.screen_binding.swap(binding, Ordering::AcqRel) != binding;
        if changed {
            mutex_lock(&self.frame_queue).clear();
            self.manager_wake.unpark();
        }
        changed
    }

    pub fn close_handle(&self) -> ViewerCloseHandle {
        ViewerCloseHandle {
            shutdown: Arc::clone(&self.shutdown),
            frame_wake: self.frame_wake.clone(),
            manager_wake: self.manager_wake.clone(),
        }
    }

    pub(crate) fn take_screen_playback(&mut self) -> Option<screens::ScreenPlayback> {
        self.screens.take().map(|owner| *owner)
    }

    pub fn ensure_running(&self) -> Result<()> {
        if let Some(error) = mutex_lock(&self.fatal_error).clone() {
            bail!("native viewer decoder stopped: {error}");
        }
        if self
            .manager_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            bail!("native viewer decoder stopped unexpectedly");
        }
        Ok(())
    }

    pub fn run(self) -> Result<()> {
        presenter::run(self)
    }
}

impl Drop for NativeViewerSession {
    fn drop(&mut self) {
        let stats = self.performance.snapshot();
        tracing::info!(
            rendered_frames = stats.total_rendered_frames,
            key_frames_decoded = stats.total_key_frames_decoded,
            received_fps = format_args!("{:.1}", stats.receive_fps),
            decoded_fps = format_args!("{:.1}", stats.decode_fps),
            rendered_fps = format_args!("{:.1}", stats.render_fps),
            actual_fps = format_args!("{:.1}", stats.actual_fps),
            actual_frames = stats.total_actual_rendered_frames,
            marked_frames = stats.total_marked_rendered_frames,
            packet_loss_percent = format_args!("{:.2}", stats.packet_loss_percent),
            low_latency_playout = stats.low_latency_playout,
            local_average_ms = format_args!("{:.1}", stats.local_frame_delay_average_ms),
            local_p95_ms = format_args!("{:.1}", stats.local_frame_delay_p95_ms),
            local_max_ms = format_args!("{:.1}", stats.local_frame_delay_max_ms),
            target_playout_delay_ms = format_args!("{:.1}", stats.target_playout_delay_ms),
            jitter_playout_delay_ms = format_args!("{:.1}", stats.jitter_playout_delay_ms),
            source_interval_p95_ms = format_args!("{:.1}", stats.source_cadence.p95_ms),
            receive_interval_p95_ms = format_args!("{:.1}", stats.receive_cadence.p95_ms),
            decode_interval_p95_ms = format_args!("{:.1}", stats.decode_cadence.p95_ms),
            render_interval_p95_ms = format_args!("{:.1}", stats.render_cadence.p95_ms),
            render_interval_max_ms = format_args!("{:.1}", stats.render_cadence.max_ms),
            render_queue_delay_ms = format_args!("{:.1}", stats.render_queue_delay_ms),
            presentation_queue_frames = stats.presentation_queue_frames,
            presentation_queue_peak_frames = stats.presentation_queue_peak_frames,
            rtx_packets_received = stats.rtx_packets_received,
            rtx_packets_accepted = stats.rtx_packets_accepted,
            fec_packets_received = stats.fec_packets_received,
            fec_packets_recovered = stats.fec_packets_recovered,
            outstanding_nacks = stats.outstanding_nacks,
            predecode_dropped_frames = stats.predecode_dropped_frames,
            ingress_queue_peak_packets = stats.ingress_queue_peak_packets,
            small_jank_count = stats.small_jank_count,
            jank_count = stats.jank_count,
            big_jank_count = stats.big_jank_count,
            dropped_present_frames = stats.dropped_present_frames,
            "native viewer session performance summary"
        );
        self.shutdown.store(true, Ordering::Release);
        self.video_sink.take();
        self.manager_wake.unpark();
        self.frame_wake.notify();
        // Return queued GPU samples before joining: the closing window will
        // no longer consume them. Never hold the queue mutex across join.
        mutex_lock(&self.frame_queue).clear();
        if let Some(thread) = self.manager_thread.take() {
            let started = Instant::now();
            if thread.join().is_err() {
                tracing::error!("native decoder worker panicked during its owned lifetime");
            }
            tracing::debug!(
                elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                "native decoder worker joined"
            );
        }
        // A poll already in progress when shutdown was set can publish its last
        // completed output. The joined worker cannot add any further surfaces.
        mutex_lock(&self.frame_queue).clear();
    }
}

#[derive(Clone)]
struct DecoderConfig {
    codec: VideoCodec,
    width: u32,
    height: u32,
    frame_rate: u32,
    hardware_decode: bool,
    software_decode: Arc<AtomicBool>,
    decode_enabled: Arc<AtomicBool>,
    decode_idle: tokio::sync::watch::Sender<u64>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,

    surface_writer: Option<crate::decoder::windows_surface::D3D11SurfaceWriter>,
}

#[allow(clippy::too_many_arguments)]
fn decoder_manager(
    config: DecoderConfig,
    mut video_source: mpsc::UnboundedReceiver<EncodedVideoFrame>,
    frame_queue: FrameQueue,
    frame_wake: FrameWake,
    performance: PerformanceMonitor,
    shutdown: Arc<AtomicBool>,
    fatal_error: Arc<Mutex<Option<String>>>,
    receiver_feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    mut startup_sender: Option<oneshot::Sender<Result<(), String>>>,
) {
    if shutdown.load(Ordering::Acquire) {
        return;
    }
    tracing::debug!(codec = ?config.codec, hardware_decode = config.hardware_decode,
        "native decoder initialization deferred until the first frame's parameter sets");
    let mut active_codec = config.codec;
    // Only log the format when it actually changes, not once per frame.
    let mut last_format_label: Option<String> = None;
    // Open against real SPS/PPS/VPS and coded dimensions. The local display
    // is only a startup hint, not the compressed stream's allocation geometry.
    let mut pool: Option<DecoderPool> = None;
    let first_open_attempt = std::time::Instant::now();
    let max_open_wait = std::time::Duration::from_secs(3);
    let notification = crate::decoder::platform::DecoderNotification::new(Arc::clone(&shutdown));
    let mut timings = VecDeque::<FrameTiming>::new();
    let mut inflight = HashMap::<i64, u32>::new();
    let mut cutover_state = DecoderCutoverState::new();
    let mut next_decoder_frame_index = 0_u32;

    'decode: while !shutdown.load(Ordering::Acquire) {
        if !config.decode_enabled.load(Ordering::Acquire) {
            let epoch = config.pause_epoch.load(Ordering::Acquire);
            let acknowledged = *config.decode_idle.borrow();
            if acknowledged != epoch {
                pool.take(); // Close the CPU decoder before relinquishing its global slot.
                timings.clear();
                inflight.clear();
                cutover_state.replace_instance();
                mutex_lock(&frame_queue).clear();
                config.decode_idle.send_replace(epoch);
            }
            match video_source.try_recv() {
                Ok(frame) => {
                    frame.completion.complete(VideoDecodeResult::Decoded);
                }
                Err(mpsc::error::TryRecvError::Empty) => std::thread::park(),
                Err(mpsc::error::TryRecvError::Disconnected) => break 'decode,
            }
            continue 'decode;
        }
        // Decoded GPU surfaces are leased from a finite pool. Hand off one
        // ready frame before decoding again, including renderer startup.
        // The render worker, hide/pause and shutdown paths unpark this worker.
        // No extra playout clock, frame dropping or timed polling is involved.
        if frame_wake.visible.load(Ordering::Acquire) && !mutex_lock(&frame_queue).is_empty() {
            std::thread::park();
            continue 'decode;
        }
        if let Some(current) = pool.as_mut() {
            let output = current
                .decoder()
                .map_or_else(DecodedBatch::default, NativeVideoDecoder::poll);
            process_decoded_batch(
                output,
                current,
                &mut timings,
                DecodedForwardContext {
                    frame_queue: &frame_queue,
                    frame_wake: &frame_wake,
                    performance: &performance,
                    receiver_feedback: &receiver_feedback,
                    inflight: &mut inflight,
                },
            );
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        // Poll may itself have delivered an older reordered output. Let the
        // renderer consume it before admitting another compressed picture.
        if frame_wake.visible.load(Ordering::Acquire) && !mutex_lock(&frame_queue).is_empty() {
            continue 'decode;
        }
        performance.set_decoder_queue_frames(video_source.len());
        let frame = match video_source.try_recv() {
            Ok(frame) => frame,
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::park();
                // A backend output wake is useful without a new RTP packet.
                continue 'decode;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break 'decode,
        };
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let completion = frame.completion.clone();
        let submitted_at = Instant::now();
        let frame_id = frame.frame_id;
        let keyframe = frame.keyframe;
        let timestamp = frame.rtp_timestamp;
        let format = frame
            .parameter_format
            .or_else(|| parse_annex_b_format(frame.codec, &frame.data));
        if pool.is_none() {
            if startup_sender.is_none() && !keyframe {
                completion.complete(VideoDecodeResult::RequestKeyframe);
                continue 'decode;
            }
            let extra = extract_parameter_sets(frame.codec, &frame.data);
            let pool_extra = extra.clone();
            if extra.is_none() && first_open_attempt.elapsed() < max_open_wait {
                tracing::debug!(codec = ?frame.codec, elapsed_ms = first_open_attempt.elapsed().as_millis(),
                    "first frame has no parameter sets yet; deferring decoder open");
                continue 'decode;
            }
            let (width, height) = stream_geometry(&config, format);
            let opened = open_decoder_with_metadata(
                &config,
                frame.codec,
                width,
                height,
                extra,
                config.surface_writer.clone(),
            );
            match opened {
                Ok(decoder) => {
                    config
                        .software_decode
                        .store(decoder.is_software(), Ordering::Release);
                    tracing::info!(
                        decoder = decoder.label(),
                        codec = ?frame.codec,
                        width,
                        height,
                        frame_rate = config.frame_rate,
                        "native in-process decoder opened on the first frame"
                    );
                    performance.set_decoder(decoder.label());
                    if let Some(sender) = startup_sender.take() {
                        let _ = sender.send(Ok(()));
                    }
                    let opened_pool = DecoderPool::new(
                        decoder,
                        frame.codec,
                        width,
                        height,
                        config.frame_rate,
                        config.hardware_decode,
                        pool_extra.unwrap_or_default(),
                    );
                    pool = Some(opened_pool);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    tracing::error!(codec = ?frame.codec, width, height,
                        "native decoder open failed after the first frame arrived");
                    *mutex_lock(&fatal_error) = Some(message.clone());
                    if let Some(sender) = startup_sender.take() {
                        let _ = sender.send(Err(message));
                    }
                    return;
                }
            }
        }
        let pool = pool.as_mut().expect("decoder opened before admission");
        pool.set_notification(notification.clone());
        let mut render_color = frame
            .color_space
            .map(|color| color.rendering())
            .unwrap_or_default();
        // Current UU's renderer uses the received bit_depth_minus8 (CA7F90 /
        // CADDF0 / CAC0C0), not the pending UI checkbox, to select HDR output.
        // In this product's wire contract high-bit-depth video is the HDR path.
        render_color.hdr_peak_nits = format
            .or(pool.format())
            .filter(|f| f.bit_depth_luma > 8)
            .map(|_| {
                frame
                    .color_space
                    .and_then(|c| c.hdr_metadata)
                    .map_or(1000, |m| m.max_luminance)
            });
        timings.push_back(FrameTiming {
            is_new_picture: frame.is_new_picture,
            color: render_color,
            rtp_timestamp: timestamp,
            received_at: frame.received_at,
            assembled_at: frame.assembled_at,
            submitted_at,
            rotation: frame.rotation,
            keyframe,
            sender_timing: frame.sender_timing,
        });

        let parameters = if keyframe {
            extract_parameter_sets(frame.codec, &frame.data).unwrap_or_default()
        } else {
            Bytes::new()
        };
        let prepared = pool.prepare(frame.codec, format, keyframe, parameters);
        config
            .software_decode
            .store(pool.is_software(), Ordering::Release);
        if let Some(reason) = pool.blocked_reason() {
            *mutex_lock(&fatal_error) = Some(reason.to_owned());
            break 'decode;
        }
        if prepared.replaced {
            cutover_state.replace_instance();
            inflight.clear();
            performance.set_decoder(pool.label());
        }
        if let Some(result) = prepared.result {
            if !result.accepted() {
                timings.clear();
            }
            completion.complete(result);
            continue;
        }
        if let Some(callback_result) = pool.callback_result() {
            let transition = pool.complete(callback_result, keyframe, format);
            config
                .software_decode
                .store(pool.is_software(), Ordering::Release);
            if let Some(reason) = pool.blocked_reason() {
                *mutex_lock(&fatal_error) = Some(reason.to_owned());
                break 'decode;
            }
            if transition.replaced {
                cutover_state.replace_instance();
                inflight.clear();
                performance.set_decoder(pool.label());
            }
            let result = transition
                .result
                .expect("callback state has a Decode result");
            if !result.accepted() {
                timings.clear();
            }
            completion.complete(result);
            continue;
        }
        if frame.codec != active_codec {
            active_codec = frame.codec;
            last_format_label = None;
            performance.set_video_codec(match active_codec {
                VideoCodec::H264 => "H.264/AVC",
                VideoCodec::H265 => "H.265/HEVC",
            });
        }
        if let Some(format) = format {
            let label = video_format_label(active_codec, format);
            if last_format_label.as_deref() != Some(label.as_str()) {
                tracing::info!(format = %label, "video format in use");
                last_format_label = Some(label.clone());
            }
            performance.set_video_format(label);
        }

        let cutover = cutover_state.evaluate(&frame, format);
        let mut result = if cutover.drop_frame {
            if cutover.request_keyframe {
                VideoDecodeResult::RequestKeyframe
            } else {
                VideoDecodeResult::Decoded
            }
        } else {
            VideoDecodeResult::Decoded
        };
        if cutover.reset_decoder {
            tracing::debug!(
                hard_reset = cutover.hard_reset,
                source_changed = cutover.source_changed,
                content_changed = cutover.content_changed,
                resolution_changed = cutover.resolution_changed,
                pressure_recovery = cutover.pressure_recovery,
                "UU decoder keyframe cutover"
            );
            inflight.clear();
            // The adapter advances generation before flush. Generic RTP timing
            // records remain until output retires the prefix, or Decode fails.
            let reset = pool.decoder().map_or_else(
                || Err(crate::decoder::platform::DecodeError::NoBackend.into()),
                |decoder| decoder.reset_for_keyframe(cutover.hard_reset),
            );
            if let Err(error) = reset {
                tracing::warn!(%error, hard_reset = cutover.hard_reset, "decoder cutover reset failed");
                result = VideoDecodeResult::Fallback;
            }
        }

        if !cutover.drop_frame && result == VideoDecodeResult::Decoded {
            let decode_token = cutover_state.token(next_decoder_frame_index);
            next_decoder_frame_index = next_decoder_frame_index.wrapping_add(1);
            inflight.insert(decode_token, timestamp);
            tracing::trace!(frame_id, rtp_timestamp = timestamp, decode_token,
                codec = ?frame.codec, bytes = frame.data.len(), "submitting admitted video frame");
            let decoded = pool.decoder().map_or_else(
                || DecodedBatch {
                    input_error: Some(crate::decoder::platform::DecodeError::NoBackend.into()),
                    ..Default::default()
                },
                |decoder| decoder.push(frame, decode_token),
            );
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            let input_error = process_decoded_batch(
                decoded,
                pool,
                &mut timings,
                DecodedForwardContext {
                    frame_queue: &frame_queue,
                    frame_wake: &frame_wake,
                    performance: &performance,
                    receiver_feedback: &receiver_feedback,
                    inflight: &mut inflight,
                },
            );
            if let Some(error) = input_error {
                inflight.remove(&decode_token);
                result = VideoDecodeResult::from_error(&error);
                tracing::warn!(%error, ?result, frame_id, keyframe, "native decoder input failed");
            } else if inflight.contains_key(&decode_token)
                && cutover_state.note_inflight_pressure(inflight.len(), keyframe)
            {
                result = VideoDecodeResult::RequestKeyframe;
                tracing::warn!(
                    inflight = inflight.len(),
                    "UU decoder inflight pressure requests keyframe"
                );
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let transition = pool.complete(result, keyframe, format);
        config
            .software_decode
            .store(pool.is_software(), Ordering::Release);
        if let Some(reason) = pool.blocked_reason() {
            *mutex_lock(&fatal_error) = Some(reason.to_owned());
            break 'decode;
        }
        if transition.replaced {
            cutover_state.replace_instance();
            inflight.clear();
            performance.set_decoder(pool.label());
        }
        let result = transition
            .result
            .expect("Decode completion always has a result");
        if !result.accepted() {
            timings.clear();
        }
        completion.complete(result);
    }
    performance.set_decoder_queue_frames(0);
}

/// Extract Annex-B parameter sets (H.264 SPS/PPS, H.265 VPS/SPS/PPS) from an
/// assembled frame, preserving start codes. The first complete keyframe
/// supplies these independently of the selected decoder backend.
fn extract_parameter_sets(codec: VideoCodec, data: &[u8]) -> Option<Bytes> {
    fn nal_start(data: &[u8], at: usize) -> bool {
        at + 3 <= data.len() && data[at] == 0 && data[at + 1] == 0 && data[at + 2] == 1
            || at + 4 <= data.len()
                && data[at] == 0
                && data[at + 1] == 0
                && data[at + 2] == 0
                && data[at + 3] == 1
    }
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor + 3 <= data.len() {
        if !nal_start(data, cursor) {
            cursor += 1;
            continue;
        }
        let payload = cursor + if data[cursor + 2] == 1 { 3 } else { 4 };
        let mut end = payload;
        while end < data.len() && !nal_start(data, end) {
            end += 1;
        }
        let nal = &data[payload..end];
        if let Some(&header) = nal.first() {
            let keep = match codec {
                VideoCodec::H264 => matches!(header & 0x1f, 7 | 8),
                VideoCodec::H265 => matches!((header >> 1) & 0x3f, 32..=34),
            };
            if keep {
                out.extend_from_slice(&data[cursor..end]);
            }
        }
        cursor = end;
    }
    (!out.is_empty()).then(|| Bytes::from(out))
}

/// Real coded geometry from the parsed stream format; local display size is only
/// a fallback for a not-yet-described stream.
fn stream_geometry(config: &DecoderConfig, format: Option<VideoFormatSignature>) -> (u32, u32) {
    format
        .and_then(|format| {
            (format.coded_width > 0 && format.coded_height > 0)
                .then_some((format.coded_width, format.coded_height))
        })
        .unwrap_or((config.width, config.height))
}

#[allow(clippy::too_many_arguments)]
fn open_decoder_with_metadata(
    config: &DecoderConfig,
    codec: VideoCodec,
    width: u32,
    height: u32,
    extra_data: Option<Bytes>,
    surface_writer: Option<crate::decoder::windows_surface::D3D11SurfaceWriter>,
) -> Result<NativeVideoDecoder> {
    tracing::debug!(
        ?codec,
        width,
        height,
        has_parameter_sets = extra_data.is_some(),
        "opening native decoder with first-frame metadata"
    );
    let extra = extra_data.unwrap_or_default();

    let opened = match surface_writer {
        Some(surface_writer) => NativeVideoDecoder::open_with_surface_writer(
            codec,
            width,
            height,
            config.frame_rate,
            config.hardware_decode,
            extra,
            surface_writer,
        ),
        None => NativeVideoDecoder::open(
            codec,
            width,
            height,
            config.frame_rate,
            config.hardware_decode,
            extra,
        ),
    };

    opened
}

fn video_format_label(codec: VideoCodec, format: VideoFormatSignature) -> String {
    let codec_kind = codec;
    let codec = match codec {
        VideoCodec::H264 => "H.264/AVC",
        VideoCodec::H265 => "H.265/HEVC",
    };
    let chroma = match format.chroma_format_idc {
        0 => "4:0:0",
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => "未知色度",
    };
    let coded = if format.coded_width != format.visible_width
        || format.coded_height != format.visible_height
    {
        format!(" · 编码 {}×{}", format.coded_width, format.coded_height)
    } else {
        String::new()
    };
    format!(
        "{codec} · {}×{}{} · {chroma} · {}-bit · {}",
        format.visible_width,
        format.visible_height,
        coded,
        format.bit_depth_luma,
        profile_name(codec_kind, format.profile_idc)
    )
}

/// The profile decides which hardware decode path a backend can ask for, so it
/// belongs next to the resolution in every diagnostic.
fn profile_name(codec: VideoCodec, profile_idc: u8) -> String {
    let name = match (codec, profile_idc) {
        (VideoCodec::H264, 66) => "Baseline",
        (VideoCodec::H264, 77) => "Main",
        (VideoCodec::H264, 88) => "Extended",
        (VideoCodec::H264, 100) => "High",
        (VideoCodec::H264, 110) => "High 10",
        (VideoCodec::H264, 122) => "High 4:2:2",
        (VideoCodec::H264, 244) => "High 4:4:4",
        (VideoCodec::H265, 1) => "Main",
        (VideoCodec::H265, 2) => "Main 10",
        (VideoCodec::H265, 3) => "Main Still Picture",
        (VideoCodec::H265, 4) => "Range Extensions",
        _ => return format!("profile {profile_idc}"),
    };
    format!("{name} profile")
}

fn process_decoded_batch(
    batch: DecodedBatch,
    pool: &mut DecoderPool,
    timings: &mut VecDeque<FrameTiming>,
    mut context: DecodedForwardContext<'_>,
) -> Option<anyhow::Error> {
    // A later callback failure never rolls back successful earlier output.
    forward_decoded_frames(batch.frames, timings, &mut context, pool);
    for issue in batch.output_issues {
        match issue {
            DecoderOutputIssue::Dropped(token) => {
                if context.inflight.remove(&token).is_some() {
                    tracing::debug!(token, "backend explicitly dropped a decoded input");
                }
            }
            DecoderOutputIssue::Failed { token, error } => {
                if let Some(token) = token
                    && context.inflight.remove(&token).is_none()
                {
                    tracing::debug!(token, %error, "discarding stale decoder error callback");
                    continue;
                }
                tracing::warn!(?token, %error, "native decoder output failed");
                pool.callback_failed(&error);
            }
        }
    }
    batch.input_error
}

fn forward_decoded_frames(
    decoded: Vec<DecodedFrame>,
    timings: &mut VecDeque<FrameTiming>,
    context: &mut DecodedForwardContext<'_>,
    pool: &mut DecoderPool,
) {
    for image in decoded {
        let Some(timestamp) = context.inflight.remove(&image.pts) else {
            tracing::debug!(
                decode_token = image.pts,
                "dropping stale decoder generation callback"
            );
            continue;
        };
        let Some(timing) = timing_for_timestamp(timings, timestamp) else {
            tracing::debug!(
                decode_token = image.pts,
                "dropping stale or unknown decoder callback"
            );
            continue;
        };
        let surface = match image
            .surface
            .prepare(image.width, image.height, timing.color)
        {
            Ok(surface) => surface,
            Err(error) => {
                tracing::warn!(decode_token = image.pts, %error, "native decoded color conversion failed");
                pool.callback_failed(&error);
                continue;
            }
        };
        let decoded_at = image.ready_at;
        let _ = context
            .receiver_feedback
            .send(VideoReceiverFeedback::DecodeTiming {
                duration: decoded_at.saturating_duration_since(timing.submitted_at),
                finished_at: decoded_at,
            });
        context.performance.record_decoded_frame(
            decoded_at,
            image.width,
            image.height,
            timing.keyframe,
            decoded_at.saturating_duration_since(timing.received_at),
        );
        // Hidden tabs have no presentation queue. Keep decoder feedback alive
        // during the official short capture grace period, releasing surfaces.
        if !context.frame_wake.visible.load(Ordering::Acquire) {
            continue;
        }
        let mut queue = mutex_lock(context.frame_queue);
        queue.push_back(DecodedVideoFrame {
            is_new_picture: timing.is_new_picture,
            width: image.width,
            height: image.height,
            surface,
            color: timing.color,
            received_at: timing.received_at,
            decoded_at,
            assembly_delay: timing.assembled_at.duration_since(timing.received_at),
            input_queue_delay: timing.submitted_at.duration_since(timing.assembled_at),
            decode_pipeline_delay: decoded_at.duration_since(timing.submitted_at),
            rotation: timing.rotation,
            sender_timing: timing.sender_timing,
        });
        context
            .performance
            .set_presentation_queue_frames(queue.len());
        drop(queue);
        context.frame_wake.notify();
    }
}

fn timing_for_timestamp(
    timings: &mut VecDeque<FrameTiming>,
    timestamp: u32,
) -> Option<FrameTiming> {
    // 4F5EA0 removes the older RTP prefix, not an arbitrary matched vector slot.
    while let Some(front) = timings.front() {
        let delta = front.rtp_timestamp.wrapping_sub(timestamp);
        if delta == 0 {
            return timings.pop_front();
        }
        let newer = if delta == 0x8000_0000 {
            front.rtp_timestamp > timestamp
        } else {
            (delta as i32) > 0
        };
        if newer {
            break;
        }
        timings.pop_front();
    }
    None
}

pub(super) fn show_performance_overlay(
    ctx: &egui::Context,
    performance: &PerformanceMonitor,
    audio: &crate::audio::AudioPlayback,
    mode: PerformancePanelMode,
    grid_id: &'static str,
) {
    let stats = performance.snapshot();
    match mode {
        PerformancePanelMode::Hidden => {}
        PerformancePanelMode::Compact => show_compact_performance(ctx, &stats, audio),
        PerformancePanelMode::Detailed => {
            performance_panel::show(ctx, performance, &stats, grid_id)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PerformancePanelMode {
    Hidden,
    Compact,
    Detailed,
}

impl PerformancePanelMode {
    fn next(self) -> Self {
        match self {
            Self::Compact => Self::Detailed,
            Self::Detailed => Self::Hidden,
            Self::Hidden => Self::Compact,
        }
    }
}

const COMPACT_HUD_WIDTH: f32 = 120.0;
const COMPACT_METER_WIDTH: f32 = 37.0;
const COMPACT_COLUMN_GAP: f32 = 8.0;

fn show_compact_performance(
    ctx: &egui::Context,
    stats: &PerformanceSnapshot,
    audio: &crate::audio::AudioPlayback,
) {
    ctx.request_repaint_after(Duration::from_millis(50));
    egui::Window::new("性能简报")
        .id(egui::Id::new("performance-compact"))
        // This HUD has no controls. In particular, it must not become the
        // foreground layer used by the raw-mouse and annotation hit tests.
        .interactable(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -44.0])
        .min_width(COMPACT_HUD_WIDTH)
        .max_width(COMPACT_HUD_WIDTH)
        .resizable(false)
        .collapsible(false)
        .title_bar(false)
        .frame(compact_performance_frame())
        .show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(egui::Color32::WHITE);
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.set_width(COMPACT_HUD_WIDTH);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = COMPACT_COLUMN_GAP;
                let text = ui.vertical(|ui| {
                    // Keep both columns stationary when a value gains digits.
                    ui.set_width(COMPACT_HUD_WIDTH - COMPACT_METER_WIDTH - COMPACT_COLUMN_GAP);
                    compact_hud_line(ui, &format_uptime(stats.uptime), egui::Color32::WHITE);
                    compact_hud_line(ui, &stats.connection, connection_color(&stats.connection));
                    compact_hud_line(
                        ui,
                        &format!("{:.0} fps", stats.actual_fps.max(1.0)),
                        frame_rate_color(stats),
                    );
                    compact_hud_line(
                        ui,
                        &format!("{:.1} Mbps", stats.bitrate_mbps),
                        egui::Color32::WHITE,
                    );
                    compact_hud_line(
                        ui,
                        &format_optional_ms(stats.current_delay_ms),
                        threshold_color(stats.current_delay_ms.unwrap_or_default(), 20.0, 50.0),
                    );
                    compact_hud_line(
                        ui,
                        &stats.frame_delay_ms.map_or_else(
                            || "— ms frm.".to_owned(),
                            |value| format!("{value} ms frm."),
                        ),
                        threshold_color(
                            stats.frame_delay_ms.unwrap_or_default() as f64,
                            30.0,
                            60.0,
                        ),
                    );
                    compact_hud_line(
                        ui,
                        &format!("{:.1}% loss", stats.packet_loss_percent),
                        threshold_color(stats.packet_loss_percent, 0.1, 1.0),
                    );
                    compact_hud_line(ui, &stats.quality, egui::Color32::WHITE);
                });
                compact_audio_meter(ui, audio, text.response.rect.height());
            });
        });
}

fn compact_audio_meter(ui: &mut egui::Ui, audio: &crate::audio::AudioPlayback, height: f32) {
    let settings = audio.settings();
    let muted = settings.muted || settings.volume == 0;
    let fill = audio.output_levels().map(|level| {
        if level > 0.0 {
            ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
        } else {
            0.0
        }
    });
    let now = ui.input(|input| input.time);
    let peak_id = ui.id().with("stereo-meter-peaks");
    let peaks = ui.ctx().data_mut(|data| {
        let mut peaks = data
            .get_temp::<[(f32, f64); 2]>(peak_id)
            .unwrap_or_default();
        for channel in 0..2 {
            if muted {
                peaks[channel] = (0.0, now);
            } else if fill[channel] >= peaks[channel].0 || now >= peaks[channel].1 {
                peaks[channel] = (fill[channel], now + 0.8);
            }
        }
        data.insert_temp(peak_id, peaks);
        peaks
    });
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(COMPACT_METER_WIDTH, height),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let ink = crate::ui::theme::MUTED;
    let font = egui::FontId::monospace(7.5);
    let top = rect.top() + 12.0;
    let bottom = rect.bottom() - 4.0;
    let meter_height = bottom - top;
    let y_at = |level: f32| bottom - meter_height * level;
    for (channel, name) in ["L", "R"].into_iter().enumerate() {
        let left = rect.left() + channel as f32 * 8.0;
        painter.text(
            egui::pos2(left + 2.5, rect.top()),
            egui::Align2::CENTER_TOP,
            name,
            font.clone(),
            ink,
        );
        for segment in 0..24 {
            let color = if fill[channel] * 24.0 > segment as f32 {
                match segment {
                    22.. => crate::ui::theme::RED,
                    17..=21 => crate::ui::theme::AMBER,
                    _ => crate::ui::theme::GREEN,
                }
            } else {
                egui::Color32::from_white_alpha(24)
            };
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(left, y_at((segment + 1) as f32 / 24.0)),
                    egui::vec2(5.0, (meter_height / 24.0 - 1.0).max(1.0)),
                ),
                0.5,
                color,
            );
        }
        if peaks[channel].0 > 0.0 {
            let y = y_at(peaks[channel].0);
            painter.line_segment(
                [egui::pos2(left, y), egui::pos2(left + 5.0, y)],
                egui::Stroke::new(1.0, crate::ui::theme::TEXT),
            );
        }
    }
    painter.text(
        egui::pos2(rect.right(), rect.top()),
        egui::Align2::RIGHT_TOP,
        "dB",
        font.clone(),
        ink,
    );
    for db in [0, -6, -12, -24, -36, -48, -60] {
        let y = y_at((db as f32 + 60.0) / 60.0);
        painter.line_segment(
            [
                egui::pos2(rect.left() + 16.0, y),
                egui::pos2(rect.left() + 18.0, y),
            ],
            egui::Stroke::new(1.0, crate::ui::theme::DISABLED),
        );
        painter.text(
            egui::pos2(rect.right(), y),
            egui::Align2::RIGHT_CENTER,
            db.to_string(),
            font.clone(),
            ink,
        );
    }
}

fn compact_performance_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::from_black_alpha(165))
        .stroke(egui::Stroke::NONE)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(7, 5))
}

fn compact_hud_line(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .monospace()
                .size(crate::ui::theme::MICRO)
                .color(color.gamma_multiply(0.78)),
        )
        .truncate(),
    );
}

fn connection_color(connection: &str) -> egui::Color32 {
    if connection.contains("P2P") || connection.contains("LAN") {
        good_color()
    } else if connection.to_ascii_lowercase().contains("relay") {
        warning_color()
    } else {
        egui::Color32::WHITE
    }
}

fn frame_rate_color(stats: &PerformanceSnapshot) -> egui::Color32 {
    let frame_ratio = if stats.receive_fps <= 1.0 {
        1.0
    } else {
        stats.render_fps / stats.receive_fps
    };
    if frame_ratio >= 0.98 {
        good_color()
    } else if frame_ratio >= 0.9 {
        warning_color()
    } else {
        bad_color()
    }
}

fn format_uptime(value: Duration) -> String {
    let seconds = value.as_secs();
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_resolution(value: Option<(u32, u32)>) -> String {
    value.map_or_else(
        || "—".to_owned(),
        |(width, height)| format!("{width}×{height}"),
    )
}

fn threshold_color(value: f64, good_max: f64, warning_max: f64) -> egui::Color32 {
    if value <= good_max {
        good_color()
    } else if value <= warning_max {
        warning_color()
    } else {
        bad_color()
    }
}

fn good_color() -> egui::Color32 {
    crate::ui::theme::GREEN
}

fn warning_color() -> egui::Color32 {
    crate::ui::theme::AMBER
}

fn bad_color() -> egui::Color32 {
    crate::ui::theme::RED
}

fn format_optional_ms(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |value| format!("{value:.0} ms"))
}

pub(crate) fn install_system_cjk_font(ctx: &egui::Context) {
    let candidates = system_cjk_font_candidates();
    let Some((path, bytes)) = candidates
        .into_iter()
        .find_map(|path| std::fs::read(&path).ok().map(|bytes| (path, bytes)))
    else {
        tracing::warn!("no system CJK font found; non-Latin labels may be unavailable");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    let name = "system-cjk".to_owned();
    fonts
        .font_data
        .insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push(name.clone());
    }
    ctx.set_fonts(fonts);
    ctx.request_repaint();
    tracing::debug!(path = %path.display(), "installed system CJK font for native viewer");
}

fn system_cjk_font_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    #[cfg(windows)]
    {
        let fonts = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        for name in ["msyh.ttc", "msyhbd.ttc", "simhei.ttf", "simsun.ttc"] {
            paths.push(fonts.join(name));
        }
    }

    // Distributions place CJK fonts under a few well-known names; fontconfig
    // is not linked in, so the list is walked directly.
    #[cfg(not(windows))]
    {
        for path in [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/source-han-sans/SourceHanSans-Regular.otf",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        ] {
            paths.push(PathBuf::from(path));
        }
    }

    paths
}

fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
