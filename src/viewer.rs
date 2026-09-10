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

mod screens;
mod stream_menu;
use stream_menu::{StreamControlUi, show_stream_control_window};

#[cfg(windows)]
mod windows_presenter;
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
}

#[derive(Clone, Default)]
pub(crate) struct ViewerDisplayHandle {
    #[cfg(windows)]
    pub surface_writer: Option<crate::decoder::windows_surface::D3D11SurfaceWriter>,
}

impl ConnectionProgress {
    pub fn working(step: u8, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            step,
            title: title.into(),
            detail: detail.into(),
            state: ConnectionProgressState::Working,
        }
    }

    pub fn ready(detail: impl Into<String>) -> Self {
        Self {
            step: CONNECTION_PROGRESS_STEPS,
            title: "连接完成".to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Ready,
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            step: 0,
            title: "无法建立连接".to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Failed,
        }
    }
}

fn configure_viewer_visuals(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(11, 15, 22);
    visuals.window_fill = egui::Color32::from_rgb(17, 22, 31);
    visuals.extreme_bg_color = egui::Color32::from_rgb(7, 10, 15);
    visuals.selection.bg_fill = egui::Color32::from_rgb(55, 124, 255);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(25, 32, 44);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(34, 44, 59);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(55, 124, 255);
    visuals.window_corner_radius = 14.0.into();
    ctx.set_visuals(visuals);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.interaction.selectable_labels = false;
    ctx.set_style_of(egui::Theme::Dark, style);
}

pub(crate) fn run_connecting_viewer_window(
    alias: String,
    progress: std_mpsc::Receiver<ConnectionProgress>,
    session: std_mpsc::Receiver<ViewerWindowEvent>,
    display_sender: oneshot::Sender<ViewerDisplayHandle>,
) -> Result<()> {
    #[cfg(windows)]
    {
        windows_presenter::run_connecting(windows_presenter::ConnectingWindowsRunConfig {
            alias,
            progress,
            session,
            display_sender,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (alias, progress, session, display_sender);
        crate::ui::ensure_supported()
    }
}

pub(crate) enum ViewerWindowEvent {
    Close,
    Playing(Box<NativeViewerSession>),
    Reconnect {
        progress: std_mpsc::Receiver<ConnectionProgress>,
        display: oneshot::Sender<ViewerDisplayHandle>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ViewerPreferences {
    performance_mode: PerformancePanelMode,
    #[cfg(windows)]
    aspect_locked: bool,
}

impl Default for ViewerPreferences {
    fn default() -> Self {
        Self {
            performance_mode: PerformancePanelMode::Compact,
            #[cfg(windows)]
            aspect_locked: true,
        }
    }
}

pub(super) struct ConnectionProgressApp {
    alias: String,
    receiver: std_mpsc::Receiver<ConnectionProgress>,
    steps: Vec<ConnectionProgress>,
    events: Vec<(Duration, ConnectionProgress)>,
    current: ConnectionProgress,
    started_at: Instant,
}

impl ConnectionProgressApp {
    pub(super) fn new(alias: String, receiver: std_mpsc::Receiver<ConnectionProgress>) -> Self {
        let current = ConnectionProgress::working(1, "准备连接", "正在读取本地会话和设备配置");
        Self {
            alias,
            receiver,
            steps: vec![current.clone()],
            events: vec![(Duration::ZERO, current.clone())],
            current,
            started_at: Instant::now(),
        }
    }

    pub(super) fn drain(&mut self) {
        while let Ok(mut progress) = self.receiver.try_recv() {
            if progress.step == 0 {
                progress.step = self.current.step;
            }
            self.events
                .push((self.started_at.elapsed(), progress.clone()));
            if let Some(existing) = self
                .steps
                .iter_mut()
                .find(|existing| existing.step == progress.step)
            {
                *existing = progress.clone();
            } else {
                self.steps.push(progress.clone());
                self.steps.sort_by_key(|step| step.step);
            }
            if matches!(progress.state, ConnectionProgressState::Ready) {
                tracing::debug!("connection UI reached ready state");
            }
            self.current = progress;
        }
    }
}

impl ConnectionProgressApp {
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.drain();
        ctx.request_repaint_after(Duration::from_millis(33));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(11, 15, 22))
                    .inner_margin(egui::Margin::same(28)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(crate::APP_NAME)
                            .size(22.0)
                            .strong()
                            .color(egui::Color32::WHITE),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(&self.alias)
                            .size(15.0)
                            .color(egui::Color32::from_rgb(145, 174, 220)),
                    );
                });
                ui.add_space(24.0);
                ui.columns(2, |columns| {
                    columns[0].set_width(310.0);
                    columns[0].label(
                        egui::RichText::new("连接进度")
                            .size(13.0)
                            .color(egui::Color32::from_rgb(130, 143, 162)),
                    );
                    columns[0].add_space(12.0);
                    for step in &self.steps {
                        let complete = step.step < self.current.step
                            || matches!(step.state, ConnectionProgressState::Ready);
                        let active = step.step == self.current.step;
                        let color = if matches!(step.state, ConnectionProgressState::Failed) {
                            egui::Color32::from_rgb(255, 105, 105)
                        } else if complete {
                            egui::Color32::from_rgb(80, 216, 151)
                        } else if active {
                            egui::Color32::from_rgb(83, 145, 255)
                        } else {
                            egui::Color32::from_rgb(91, 102, 119)
                        };
                        columns[0].horizontal(|ui| {
                            ui.colored_label(color, if complete { "●" } else { "○" });
                            ui.label(egui::RichText::new(&step.title).color(if active {
                                egui::Color32::WHITE
                            } else {
                                color
                            }));
                        });
                    }

                    columns[1].vertical(|ui| {
                        ui.add_space(14.0);
                        if matches!(self.current.state, ConnectionProgressState::Working) {
                            ui.spinner();
                        }
                        let accent = match self.current.state {
                            ConnectionProgressState::Working => {
                                egui::Color32::from_rgb(83, 145, 255)
                            }
                            ConnectionProgressState::Ready => egui::Color32::from_rgb(80, 216, 151),
                            ConnectionProgressState::Failed => {
                                egui::Color32::from_rgb(255, 105, 105)
                            }
                        };
                        ui.add_space(14.0);
                        ui.label(
                            egui::RichText::new(&self.current.title)
                                .size(26.0)
                                .strong()
                                .color(accent),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(&self.current.detail)
                                .size(14.0)
                                .color(egui::Color32::from_rgb(185, 195, 210)),
                        );
                        ui.add_space(24.0);
                        ui.add(
                            egui::ProgressBar::new(
                                f32::from(self.current.step.min(CONNECTION_PROGRESS_STEPS))
                                    / f32::from(CONNECTION_PROGRESS_STEPS),
                            )
                            .desired_width(360.0)
                            .animate(matches!(
                                self.current.state,
                                ConnectionProgressState::Working
                            )),
                        );
                        ui.add_space(18.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("实时连接诊断")
                                    .size(13.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(218, 226, 238)),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{:.1} 秒",
                                            self.started_at.elapsed().as_secs_f64()
                                        ))
                                        .size(11.0)
                                        .color(egui::Color32::from_rgb(105, 118, 136)),
                                    );
                                },
                            );
                        });
                        egui::ScrollArea::vertical()
                            .max_height(230.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for (elapsed, event) in &self.events {
                                    ui.horizontal_top(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{:>5.2}s",
                                                elapsed.as_secs_f64()
                                            ))
                                            .monospace()
                                            .size(10.0)
                                            .color(egui::Color32::from_rgb(92, 106, 126)),
                                        );
                                        ui.vertical(|ui| {
                                            ui.label(
                                                egui::RichText::new(&event.title)
                                                    .size(11.0)
                                                    .strong()
                                                    .color(egui::Color32::from_rgb(196, 207, 223)),
                                            );
                                            ui.label(
                                                egui::RichText::new(&event.detail)
                                                    .size(10.0)
                                                    .color(egui::Color32::from_rgb(121, 135, 155)),
                                            );
                                        });
                                    });
                                    ui.add_space(5.0);
                                }
                            });
                        if matches!(self.current.state, ConnectionProgressState::Failed) {
                            ui.add_space(20.0);
                            if ui.button("关闭窗口").clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                    });
                });
            });
    }
}

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
    #[cfg(windows)]
    render_thread: Arc<Mutex<Option<std::thread::Thread>>>,
}

impl FrameWake {
    #[cfg(windows)]
    fn install_render_thread(&self, thread: std::thread::Thread) {
        *mutex_lock(&self.render_thread) = Some(thread);
        self.visible.store(true, Ordering::Release);
    }

    fn notify(&self) {
        #[cfg(windows)]
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

#[cfg(all(test, windows))]
#[path = "viewer/software_window_tests.rs"]
mod software_window_tests;

pub struct NativeViewerSession {
    screen_id: i32,
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
        let software_decode = Arc::new(AtomicBool::new(cfg!(windows) && !hardware_decode));
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
                    #[cfg(windows)]
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
            screen_id: 0,
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
        self.screen_id = self
            .stream_control
            .snapshot()
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track)
            .map_or(0, |screen| screen.id);
        self.screens = Some(Box::new(screens::ScreenPlayback::new(
            peer,
            profile,
            alias,
            self.screen_id,
        )));
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
        #[cfg(windows)]
        {
            windows_presenter::run(self)
        }
        #[cfg(not(windows))]
        {
            crate::ui::ensure_supported()
        }
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
    #[cfg(windows)]
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
    // Open against real SPS/PPS/VPS and coded dimensions. The local display
    // is only a startup hint, not the compressed stream's allocation geometry.
    let mut pool: Option<DecoderPool> = None;
    let first_open_attempt = std::time::Instant::now();
    let max_open_wait = std::time::Duration::from_secs(3);
    let notification = crate::decoder::platform::DecoderNotification::new(
        std::thread::current(),
        Arc::clone(&shutdown),
    );
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
                    let _ = receiver_feedback.send(VideoReceiverFeedback::DecoderFinished {
                        frame_id: frame.frame_id,
                        result: VideoDecodeResult::Decoded,
                    });
                }
                Err(mpsc::error::TryRecvError::Empty) => std::thread::park(),
                Err(mpsc::error::TryRecvError::Disconnected) => break 'decode,
            }
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
        let submitted_at = Instant::now();
        let frame_id = frame.frame_id;
        let keyframe = frame.keyframe;
        let timestamp = frame.rtp_timestamp;
        let format = frame
            .parameter_format
            .or_else(|| parse_annex_b_format(frame.codec, &frame.data));
        if pool.is_none() {
            if startup_sender.is_none() && !keyframe {
                let _ = receiver_feedback.send(VideoReceiverFeedback::DecoderFinished {
                    frame_id,
                    result: VideoDecodeResult::RequestKeyframe,
                });
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
                #[cfg(windows)]
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
        timings.push_back(FrameTiming {
            is_new_picture: frame.is_new_picture,
            color: frame
                .color_space
                .map(|color| color.rendering())
                .unwrap_or_default(),
            rtp_timestamp: timestamp,
            received_at: frame.received_at,
            assembled_at: frame.assembled_at,
            submitted_at,
            rotation: frame.rotation,
            keyframe,
            sender_timing: frame.sender_timing,
        });

        let prepared = pool.prepare(frame.codec, format, keyframe);
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
            let _ =
                receiver_feedback.send(VideoReceiverFeedback::DecoderFinished { frame_id, result });
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
            let _ =
                receiver_feedback.send(VideoReceiverFeedback::DecoderFinished { frame_id, result });
            continue;
        }
        if frame.codec != active_codec {
            active_codec = frame.codec;
            performance.set_video_codec(match active_codec {
                VideoCodec::H264 => "H.264/AVC",
                VideoCodec::H265 => "H.265/HEVC",
            });
        }
        if let Some(format) = format {
            performance.set_video_format(video_format_label(active_codec, format));
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
        let _ = receiver_feedback.send(VideoReceiverFeedback::DecoderFinished { frame_id, result });
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
    #[cfg(windows)] surface_writer: Option<crate::decoder::windows_surface::D3D11SurfaceWriter>,
) -> Result<NativeVideoDecoder> {
    tracing::debug!(
        ?codec,
        width,
        height,
        has_parameter_sets = extra_data.is_some(),
        "opening native decoder with first-frame metadata"
    );
    let extra = extra_data.unwrap_or_default();
    #[cfg(windows)]
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
    #[cfg(not(windows))]
    let opened = NativeVideoDecoder::open(
        codec,
        width,
        height,
        config.frame_rate,
        config.hardware_decode,
        extra,
    );
    opened
}

fn video_format_label(codec: VideoCodec, format: VideoFormatSignature) -> String {
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
        "{codec} · {}×{}{} · {chroma} · {}-bit",
        format.visible_width, format.visible_height, coded, format.bit_depth_luma
    )
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
            #[cfg(any(windows, target_os = "macos"))]
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
        PerformancePanelMode::Detailed => show_detailed_performance(ctx, &stats, grid_id),
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
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(COMPACT_METER_WIDTH, height),
        egui::Sense::hover(),
    );
    response.on_hover_text("L / R · dBFS");
    let painter = ui.painter();
    let ink = egui::Color32::from_gray(150);
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
                    22.. => egui::Color32::from_rgb(228, 101, 94),
                    17..=21 => egui::Color32::from_rgb(223, 187, 92),
                    _ => egui::Color32::from_rgb(91, 192, 143),
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
                egui::Stroke::new(1.0, egui::Color32::from_gray(220)),
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
            egui::Stroke::new(1.0, egui::Color32::from_gray(85)),
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

fn show_detailed_performance(
    ctx: &egui::Context,
    stats: &PerformanceSnapshot,
    grid_id: &'static str,
) {
    egui::Window::new("性能详情")
        .id(egui::Id::new(grid_id))
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0])
        .default_width(480.0)
        .resizable(true)
        .collapsible(true)
        .frame(performance_frame())
        .show(ctx, |ui| {
            ui.set_min_width(430.0);
            ui.visuals_mut().override_text_color = Some(egui::Color32::WHITE);
            ui.horizontal(|ui| {
                ui.label(&stats.quality);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format_uptime(stats.uptime));
                });
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .max_height(560.0)
                .show(ui, |ui| {
                    egui::Grid::new((grid_id, "metrics"))
                        .num_columns(2)
                        .spacing([18.0, 5.0])
                        .show(ui, |ui| draw_performance_grid(ui, stats));
                });
        });
}

fn performance_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::from_black_alpha(225))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_white_alpha(30)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(10))
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
                .size(10.0)
                .color(color.gamma_multiply(0.78)),
        )
        .truncate(),
    )
    .on_hover_text(text);
}

fn draw_performance_grid(ui: &mut egui::Ui, stats: &PerformanceSnapshot) {
    section(ui, "网络");
    metric_colored(
        ui,
        "连接",
        &stats.connection,
        connection_color(&stats.connection),
    );
    metric(
        ui,
        "自动切路",
        &format!(
            "{}（尝试 {} / 成功 {}）",
            network_switch_phase_label(stats.network_switch_phase),
            stats.network_switch_attempts,
            stats.network_switch_successes
        ),
    );
    metric_colored(
        ui,
        "RTT",
        &format_optional_ms(stats.current_delay_ms),
        threshold_color(stats.current_delay_ms.unwrap_or_default(), 20.0, 50.0),
    );
    metric(ui, "码率", &format!("{:.1} Mbps", stats.bitrate_mbps));
    metric_colored(
        ui,
        "最终未恢复",
        &format!("{:.2}%", stats.packet_loss_percent),
        threshold_color(stats.packet_loss_percent, 0.1, 1.0),
    );
    metric_colored(
        ui,
        "RTP 抖动",
        &format!("{:.2} ms", stats.rtp_jitter_ms),
        threshold_color(stats.rtp_jitter_ms, 1.0, 5.0),
    );
    if stats.low_latency_playout {
        metric(ui, "播放时序", "UU 低延迟接收调度 · 直接呈现");
    } else {
        metric(
            ui,
            "播放时序",
            &format!(
                "调度目标 {:.1} ms（抖动估计 {:.1} ms；直接呈现）",
                stats.target_playout_delay_ms, stats.jitter_playout_delay_ms
            ),
        );
    }
    metric(
        ui,
        "有序入口",
        &format!(
            "{} 包（峰值 {}）",
            stats.ingress_queue_packets, stats.ingress_queue_peak_packets
        ),
    );
    metric(
        ui,
        "待恢复 NACK",
        &format!("{} 包", stats.outstanding_nacks),
    );
    metric(
        ui,
        "RTX 恢复",
        &format!(
            "{} / {} 回灌/收到",
            stats.rtx_packets_accepted, stats.rtx_packets_received
        ),
    );
    metric(
        ui,
        "RS-FEC",
        &format!(
            "{} 恢复 / {} repair",
            stats.fec_packets_recovered, stats.fec_packets_received
        ),
    );

    section(ui, "帧流水线");
    metric(ui, "画面更新", &format!("{:.0} FPS", stats.actual_fps));
    metric_colored(
        ui,
        "帧率",
        &format!(
            "{:.0}/{:.0}/{:.0} 接收/解码/显示",
            stats.receive_fps, stats.decode_fps, stats.render_fps
        ),
        frame_rate_color(stats),
    );
    metric_colored(
        ui,
        "FrameBuffer",
        &format!(
            "{} 帧（峰值 {}）",
            stats.frame_buffer_frames, stats.frame_buffer_peak_frames
        ),
        threshold_color(stats.frame_buffer_frames as f64, 3.0, 8.0),
    );
    metric_colored(
        ui,
        "解码队列",
        &format!(
            "{} 帧（峰值 {}）",
            stats.decoder_queue_frames, stats.decoder_queue_peak_frames
        ),
        threshold_color(stats.decoder_queue_frames as f64, 1.0, 3.0),
    );
    metric_colored(
        ui,
        "呈现丢帧",
        &format!(
            "{} 帧（{:.2}%）",
            stats.dropped_present_frames, stats.presentation_drop_percent
        ),
        threshold_color(stats.presentation_drop_percent, 0.5, 2.0),
    );
    metric_colored(
        ui,
        "呈现队列",
        &format!(
            "{} 帧（峰值 {}）",
            stats.presentation_queue_frames, stats.presentation_queue_peak_frames
        ),
        threshold_color(stats.presentation_queue_frames as f64, 1.0, 3.0),
    );
    metric(
        ui,
        "解码前快进",
        &format!("{} 帧", stats.predecode_dropped_frames),
    );
    metric_colored(
        ui,
        "卡顿分级",
        &format!(
            "{} / {} / {} 次（100–179 / ≥180 / ≥500 ms）",
            stats.small_jank_count, stats.jank_count, stats.big_jank_count
        ),
        if stats.jank_count == 0 {
            good_color()
        } else if stats.big_jank_count == 0 {
            warning_color()
        } else {
            bad_color()
        },
    );
    metric_colored(
        ui,
        "RTP 时间戳",
        &format_cadence(stats.source_cadence),
        cadence_color(stats.source_cadence),
    );
    metric_colored(
        ui,
        "组帧到达",
        &format_cadence(stats.receive_cadence),
        cadence_color(stats.receive_cadence),
    );
    metric_colored(
        ui,
        "解码输出",
        &format_cadence(stats.decode_cadence),
        cadence_color(stats.decode_cadence),
    );
    metric_colored(
        ui,
        "显示提交",
        &format_cadence(stats.render_cadence),
        if stats.render_cadence.average_ms > 0.0
            && stats.render_cadence.p95_ms > stats.render_cadence.average_ms * 1.5
        {
            warning_color()
        } else {
            good_color()
        },
    );

    metric(
        ui,
        "估算帧延迟（官方 frm）",
        &stats.frame_delay_ms.map_or_else(
            || "等待测量".to_owned(),
            |value| format!("{value} ms（处理 + 发送 + RTT）"),
        ),
    );
    section(ui, "本地流水线（含组帧/恢复）");
    metric_colored(
        ui,
        "当前单帧（含组帧/恢复）",
        &format!("{:.1} ms", stats.local_frame_delay_ms),
        threshold_color(stats.local_frame_delay_ms, 8.0, 16.7),
    );
    metric(
        ui,
        "近 300 帧",
        &format!(
            "平均 {:.1} / P95 {:.1} / 最大 {:.1} ms",
            stats.local_frame_delay_average_ms,
            stats.local_frame_delay_p95_ms,
            stats.local_frame_delay_max_ms
        ),
    );
    metric(
        ui,
        "RTP 组帧",
        &format!("{:.1} ms", stats.assembly_delay_ms),
    );
    metric(
        ui,
        "输入排队",
        &format!("{:.1} ms", stats.input_queue_delay_ms),
    );
    metric(
        ui,
        "原生解码",
        &format!("{:.1} ms", stats.decode_pipeline_delay_ms),
    );
    metric(
        ui,
        "Surface",
        &format!("{:.1} ms", stats.surface_transfer_delay_ms),
    );
    metric(
        ui,
        "Present 等待",
        &format!("{:.1} ms", stats.present_wait_delay_ms),
    );
    metric(
        ui,
        "GUI 排队",
        &format!("{:.1} ms", stats.render_queue_delay_ms),
    );
    if let Some(pipeline) = &stats.pipeline_stats {
        section(ui, "码流时序分段");
        metric(
            ui,
            "发送/接收帧率",
            &format!(
                "{} / {:.1} FPS",
                pipeline
                    .source_fps
                    .map_or_else(|| "—".to_owned(), |fps| format!("{fps:.1}")),
                pipeline.received_fps
            ),
        );
        if let Some(sending) = pipeline.sending {
            metric(
                ui,
                "发送总计",
                &format!(
                    "平均 {:.1} / 最大 {:.1} ms",
                    sending.average_ms, sending.max_ms
                ),
            );
        }
        for (label, phase) in [
            ("采集", pipeline.capture),
            ("编码", pipeline.encode),
            ("Pacer", pipeline.pacer),
            ("传输", pipeline.transport),
            ("组帧", pipeline.assembly),
            ("解码", pipeline.decode),
        ] {
            if let Some(phase) = phase {
                metric(
                    ui,
                    label,
                    &format!("P50 {:.1} / P90 {:.1} ms", phase.p50_ms, phase.p90_ms),
                );
            }
        }
        if let Some(e2e) = pipeline.e2e {
            metric_colored(
                ui,
                "采集→解码完成",
                &format!(
                    "平均 {:.1} / P50 {:.1} / P90 {:.1} / P99 {:.1} / 最大 {:.1} ms",
                    e2e.average_ms, e2e.p50_ms, e2e.p90_ms, e2e.p99_ms, e2e.max_ms
                ),
                threshold_color(e2e.p90_ms, 30.0, 60.0),
            );
        } else {
            metric(ui, "采集→解码完成", "远端未提供有效测量");
        }
    }
    if let Some(stream_switch) = stats.stream_switch.as_ref() {
        section(ui, "串流切换");
        metric(
            ui,
            "状态",
            &format!(
                "#{} {}（{:.0} ms）",
                stream_switch.sequence, stream_switch.stage, stream_switch.age_ms
            ),
        );
        metric(ui, "目标", &stream_switch.target);
        metric(
            ui,
            "控制/持续画面",
            &format!(
                "{} / {}",
                format_optional_ms(stream_switch.request_to_ack_ms),
                format_optional_ms(stream_switch.request_to_continuity_ms),
            ),
        );
        metric(
            ui,
            "关键帧/切换显示",
            &format!(
                "{} / {}",
                format_optional_ms(stream_switch.request_to_media_ms),
                format_optional_ms(stream_switch.request_to_present_ms)
            ),
        );
        metric(
            ui,
            "切换帧间隔",
            &format!(
                "{} / {} / {} 接收/解码/显示",
                format_optional_ms(stream_switch.receive_gap_ms),
                format_optional_ms(stream_switch.decode_gap_ms),
                format_optional_ms(stream_switch.presentation_gap_ms)
            ),
        );
        let resolution = format!(
            "{} → {}",
            format_resolution(stream_switch.from_resolution),
            format_resolution(stream_switch.actual_resolution)
        );
        metric(ui, "分辨率", &resolution);
        if let Some(error) = stream_switch.error.as_ref() {
            metric_colored(ui, "错误", error, bad_color());
        }
    }
    section(ui, "视频");
    metric(ui, "官方画质档位", &stats.quality);
    metric(ui, "协商编码", &stats.video_codec);
    metric(ui, "码流格式", &stats.video_format);
    metric(ui, "解码画面", &format_resolution(stats.decoded_resolution));
    metric(ui, "解码器", &stats.decoder);
    metric(ui, "远端采集", &stats.remote_capture);
    metric(ui, "远端编码器", &stats.remote_encoder);
    metric(
        ui,
        "累计帧",
        &format!(
            "{} / {} / {} 接收/解码/显示（关键帧 {}）",
            stats.total_received_frames,
            stats.total_decoded_frames,
            stats.total_rendered_frames,
            stats.total_key_frames_decoded
        ),
    );
}

fn network_switch_phase_label(phase: u8) -> &'static str {
    match phase {
        1 => "UDP relay",
        2 => "TLS relay",
        _ => "直连",
    }
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

fn format_cadence(cadence: crate::performance::CadenceMetrics) -> String {
    format!(
        "平均 {:.1} / P95 {:.1} / 最大 {:.1} ms",
        cadence.average_ms, cadence.p95_ms, cadence.max_ms
    )
}

fn cadence_color(cadence: crate::performance::CadenceMetrics) -> egui::Color32 {
    if cadence.average_ms > 0.0 && cadence.p95_ms > cadence.average_ms * 1.5 {
        warning_color()
    } else {
        good_color()
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

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .strong()
            .color(egui::Color32::LIGHT_BLUE),
    );
    ui.separator();
    ui.end_row();
}

fn metric(ui: &mut egui::Ui, label: &str, value: &str) {
    metric_colored(ui, label, value, egui::Color32::WHITE);
}

fn metric_colored(ui: &mut egui::Ui, label: &str, value: &str, color: egui::Color32) {
    ui.weak(label);
    ui.label(egui::RichText::new(value).color(color));
    ui.end_row();
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
    egui::Color32::from_rgb(105, 220, 140)
}

fn warning_color() -> egui::Color32 {
    egui::Color32::from_rgb(255, 200, 90)
}

fn bad_color() -> egui::Color32 {
    egui::Color32::from_rgb(255, 105, 105)
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
    #[cfg(target_os = "windows")]
    {
        let fonts = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        for name in ["msyh.ttc", "msyhbd.ttc", "simhei.ttf", "simsun.ttc"] {
            paths.push(fonts.join(name));
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.extend([
            PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
            PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"),
        ]);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        paths.extend([
            PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"),
        ]);
    }
    paths
}

fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod compact_hud_tests {
    use super::*;

    #[test]
    fn changing_values_does_not_resize_or_shift_compact_hud() {
        let ctx = egui::Context::default();
        configure_viewer_visuals(&ctx);
        let audio = crate::audio::AudioPlayback::new();
        let mut stats = (*PerformanceMonitor::new("1080P").snapshot()).clone();
        let mut baseline = None;
        for wide_values in [false, true, false] {
            stats.actual_fps = if wide_values { 144.0 } else { 9.0 };
            stats.bitrate_mbps = if wide_values { 123.4 } else { 0.1 };
            stats.current_delay_ms = Some(if wide_values { 123456.0 } else { 1.0 });
            stats.frame_delay_ms = Some(if wide_values { 123456 } else { 1 });
            stats.connection = if wide_values {
                "UDP relay with a long connection state"
            } else {
                "UDP P2P"
            }
            .into();
            stats.quality = if wide_values {
                "3840x2160 automatic quality with a long description"
            } else {
                "1080P"
            }
            .into();
            stats.uptime = Duration::from_secs(if wide_values { 36_000 } else { 7 });
            for pass in 0..3 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1280.0, 760.0),
                        )),
                        ..Default::default()
                    },
                    |ui| show_compact_performance(ui.ctx(), &stats, &audio),
                );
                output.textures_delta.clear(); // Layout-only check, no GPU backend.
                let rect = ctx
                    .memory(|memory| memory.area_rect(egui::Id::new("performance-compact")))
                    .unwrap();
                if let Some(expected) = baseline {
                    assert_eq!(rect, expected, "HUD moved or resized when text changed");
                } else if pass == 2 {
                    baseline = Some(rect);
                }
            }
        }
    }
}

#[cfg(test)]
mod parameter_set_tests {
    use super::*;

    #[test]
    fn h264_extracts_sps_pps_preserving_start_codes() {
        // sps (type 7), pps (type 8), idr slice (type 5): only parameter sets kept.
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x40]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88]);
        let sets = extract_parameter_sets(VideoCodec::H264, &data).expect("parameter sets present");
        assert!(sets.starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(sets.windows(3).any(|w| w == [0, 0, 1]));
        assert!(!sets.contains(&0x65));
        // The slice nalus must have been excluded entirely (not just its content).
        let mut walked = 0;
        while walked + 4 <= sets.len() {
            if sets[walked..walked + 4] == [0, 0, 0, 1] {
                let nal_type = matches!(sets[walked + 4] & 0x1f, 7 | 8);
                assert!(nal_type, "only parameter set NALs survive");
            }
            walked += 1;
        }
    }

    #[test]
    fn h265_extracts_vps_sps_pps() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 1, 0x40, 0x01]); // vps (type 32)
        data.extend_from_slice(&[0, 0, 0, 1, 0x42, 0x01]); // sps (type 33)
        data.extend_from_slice(&[0, 0, 0, 1, 0x44, 0x01]); // pps (type 34)
        data.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01]); // idr (type 19)
        let sets = extract_parameter_sets(VideoCodec::H265, &data).expect("parameter sets present");
        let mut seen = Vec::new();
        let mut walked = 0;
        while walked + 3 <= sets.len() {
            let four = walked + 4 <= sets.len() && sets[walked..walked + 4] == [0, 0, 0, 1];
            if four || sets[walked..walked + 3] == [0, 0, 1] {
                let payload = walked + 3 + usize::from(four);
                let nal_type = (sets[payload] >> 1) & 0x3f;
                if (32..=34).contains(&nal_type) {
                    seen.push(nal_type);
                }
                walked = payload;
            } else {
                walked += 1;
            }
        }
        assert_eq!(seen, vec![32, 33, 34]);
    }

    #[test]
    fn no_parameter_sets_returns_none() {
        let data = [0, 0, 0, 1, 0x65, 0x88, 0, 0, 1, 0x41];
        assert!(extract_parameter_sets(VideoCodec::H264, &data).is_none());
    }
}
