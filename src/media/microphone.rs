//! Per-device microphone sender. No capture occurs until user permission,
//! the policy reply and the host's MicOpened event have all arrived.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub(crate) use crate::media::audio::encoder::Config as Encoding;
use crate::media::audio::encoder::create_encoder as make_encoder;
use anyhow::{Context, Result, anyhow, ensure};
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use tokio::sync::Notify;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

const RATE: u32 = 48_000;
const BLOCK: usize = 480;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub enabled: bool,
    pub pending: bool,
    pub remote_active: bool,
    pub capturing: bool,
    pub device: String,
    pub error: Option<String>,
    pub level: f32,
}

#[derive(Clone)]
pub(crate) struct InputDevice {
    pub id: cpal::DeviceId,
    pub name: String,
}

#[derive(Clone, Default)]
pub(crate) struct InputDevices {
    pub devices: Vec<InputDevice>,
    pub error: Option<String>,
    pub updated: Option<Instant>,
}

#[derive(Default)]
struct State {
    selected_input: Option<cpal::DeviceId>,
    faulted: bool,
    cleanup_on_connect: bool,
    enabled: bool,
    confirmed: bool,
    remote_active: bool,
    pending: Option<(i64, bool)>,
    encoding: Option<Encoding>,
    device: String,
    error: Option<String>,
}

struct Frame {
    samples: [f32; BLOCK * 2],
    timestamp: u32,
    generation: u64,
    created: Instant,
}

#[derive(Clone, Copy)]
struct SentClock {
    timestamp: u32,
    at: Instant,
    generation: u64,
    first: Instant,
}

struct Shared {
    input_devices: Mutex<InputDevices>,
    refresh_devices: AtomicBool,
    state: Mutex<State>,
    generation: AtomicU64,
    capture: AtomicBool,
    capturing: AtomicBool,
    stopped: AtomicBool,
    peak: AtomicU32,
    loss: AtomicU32,
    frames: ArrayQueue<Frame>,
    wake: Notify,
    owner: OnceLock<std::thread::Thread>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    sent: AtomicU64,
    reports: AtomicU64,
    octets: AtomicU64,
    sent_clock: Mutex<Option<SentClock>>,
    report_wake: Notify,
}

impl Shared {
    fn refresh(&self, state: &State) {
        let wanted = state.enabled
            && !state.faulted
            && state.confirmed
            && state.remote_active
            && state.encoding.is_some()
            && !self.stopped.load(Ordering::Acquire);
        if self.capture.swap(wanted, Ordering::AcqRel) != wanted {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.peak.store(0, Ordering::Relaxed);
        }
        self.notify();
    }
    fn notify(&self) {
        if let Some(t) = self.owner.get() {
            t.unpark();
        }
        self.wake.notify_one();
        self.report_wake.notify_one();
    }
    fn current(&self, generation: u64) -> bool {
        self.capture.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == generation
            && !self.stopped.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub(crate) struct Microphone(Arc<Shared>);

impl Microphone {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Shared {
            input_devices: Mutex::new(InputDevices::default()),
            refresh_devices: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            generation: AtomicU64::new(0),
            capture: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            loss: AtomicU32::new(0),
            frames: ArrayQueue::new(10),
            wake: Notify::new(),
            owner: OnceLock::new(),
            thread: Mutex::new(None),
            sent: AtomicU64::new(0),
            reports: AtomicU64::new(0),
            octets: AtomicU64::new(0),
            sent_clock: Mutex::new(None),
            report_wake: Notify::new(),
        }))
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        let s = lock(&self.0.state);
        Snapshot {
            enabled: s.enabled,
            pending: s.pending.is_some(),
            remote_active: s.remote_active,
            capturing: self.0.capture.load(Ordering::Acquire)
                && self.0.capturing.load(Ordering::Acquire),
            device: s.device.clone(),
            error: s.error.clone(),
            level: f32::from_bits(self.0.peak.load(Ordering::Relaxed)),
        }
    }
    pub(crate) fn selected_input(&self) -> Option<cpal::DeviceId> {
        lock(&self.0.state).selected_input.clone()
    }
    pub(crate) fn input_devices(&self) -> InputDevices {
        let devices = lock(&self.0.input_devices).clone();
        if devices
            .updated
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
        {
            self.0.refresh_devices.store(true, Ordering::Release);
            self.0.notify();
        }
        devices
    }
    pub(crate) fn refresh_input_devices(&self) -> Result<()> {
        self.start()?;
        self.0.refresh_devices.store(true, Ordering::Release);
        self.0.notify();
        Ok(())
    }
    pub(crate) fn set_input_device(&self, device: Option<cpal::DeviceId>) {
        let mut state = lock(&self.0.state);
        if state.selected_input == device {
            return;
        }
        state.selected_input = device;
        state.device.clear();
        state.error = None;
        state.faulted = false;
        // Invalidate callbacks and queued PCM before the old device is closed.
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        self.0.capturing.store(false, Ordering::Release);
        self.0.peak.store(0, Ordering::Relaxed);
        self.0.refresh(&state);
    }
    pub(crate) fn configured(&self) -> bool {
        lock(&self.0.state).encoding.is_some()
    }
    pub(crate) fn configure(&self, encoding: Option<Encoding>) {
        let mut s = lock(&self.0.state);
        if s.encoding != encoding {
            tracing::info!(?encoding, "microphone audio sender negotiated");
            self.0.generation.fetch_add(1, Ordering::AcqRel);
        }
        s.encoding = encoding;
        self.0.refresh(&s);
    }
    pub(crate) fn request(&self, seq: i64, enabled: bool) {
        let mut s = lock(&self.0.state);
        s.cleanup_on_connect = false;
        s.enabled = enabled;
        s.faulted = false;
        if enabled {
            self.0.loss.store(0, Ordering::Relaxed);
        }
        s.confirmed = false;
        // A host can send MicOpened before the policy reply; retain that event
        // until reply, but never across a user close or a new enable request.
        s.remote_active = false;
        s.pending = Some((seq, enabled));
        s.error = None;
        self.0.refresh(&s);
    }
    pub(crate) fn response(&self, seq: i64, code: i32) {
        let mut s = lock(&self.0.state);
        let Some((pending, enabled)) = s.pending else {
            return;
        };
        if pending != seq {
            return;
        }
        s.pending = None;
        s.confirmed = code == 0 && enabled;
        if code != 0 {
            s.enabled = false;
            s.error = Some(format!("远端麦克风请求失败：{code:#x}"));
        }
        self.0.refresh(&s);
        tracing::info!(seq, code, enabled, "microphone policy reply");
    }
    pub(crate) fn send_failed(&self, seq: i64, error: &str) -> bool {
        let mut s = lock(&self.0.state);
        if !s.pending.is_some_and(|(id, _)| id == seq) {
            return false;
        }
        s.pending = None;
        s.enabled = false;
        s.confirmed = false;
        s.error = Some(format!("麦克风请求发送失败，远端状态未确认：{error}"));
        self.0.refresh(&s);
        true
    }
    pub(crate) fn event(&self, action: i32) {
        let mut s = lock(&self.0.state);
        if !s.enabled {
            return;
        }
        match action {
            23 => s.remote_active = true,
            24 => s.remote_active = false,
            27 => {
                self.0.generation.fetch_add(1, Ordering::AcqRel);
            }
            _ => return,
        }
        self.0.refresh(&s);
        tracing::info!(
            action,
            confirmed = s.confirmed,
            "remote microphone activity"
        );
    }
    pub(crate) fn disconnect(&self) {
        let mut s = lock(&self.0.state);
        s.cleanup_on_connect |= s.enabled || s.confirmed || s.pending.is_some();
        s.enabled = false;
        s.confirmed = false;
        s.remote_active = false;
        s.pending = None;
        self.0.refresh(&s);
    }
    pub(crate) fn needs_cleanup(&self) -> bool {
        lock(&self.0.state).cleanup_on_connect
    }
    pub(crate) fn retry(&self) {
        let mut s = lock(&self.0.state);
        s.error = None;
        s.faulted = false;
        self.0.generation.fetch_add(1, Ordering::AcqRel);
        self.0.refresh(&s);
    }

    fn fault(&self, error: String) {
        let mut s = lock(&self.0.state);
        s.error = Some(error);
        s.faulted = true;
        self.0.refresh(&s);
    }
    pub(crate) fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
        self.disconnect();
    }
    pub(crate) async fn close(&self) {
        self.stop();
        let thread = lock(&self.0.thread).take();
        if let Some(t) = thread {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
        tracing::info!(
            packets = self.0.sent.load(Ordering::Relaxed),
            reports = self.0.reports.load(Ordering::Relaxed),
            "microphone sender closed"
        );
    }
    pub(crate) fn start(&self) -> Result<()> {
        let mut owner = lock(&self.0.thread);
        if owner.is_none() {
            let shared = Arc::clone(&self.0);
            *owner = Some(
                std::thread::Builder::new()
                    .name("uu-microphone".into())
                    .spawn(move || capture_owner(shared))?,
            );
        }
        Ok(())
    }
    pub(crate) async fn feedback(&self, sender: Arc<RTCRtpSender>) {
        while let Ok((packets, _)) = sender.read_rtcp().await {
            let params = sender.get_parameters().await;
            for packet in packets {
                let reports = if let Some(p) = packet
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>(
                ) {
                    &p.reports
                } else if let Some(p) = packet
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::sender_report::SenderReport>()
                {
                    &p.reports
                } else {
                    continue;
                };
                for report in reports {
                    if params.encodings.iter().any(|e| e.ssrc == report.ssrc) {
                        self.0.loss.store(
                            (u32::from(report.fraction_lost) * 100 + 128) / 256,
                            Ordering::Relaxed,
                        );
                        self.0.reports.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    pub(crate) async fn send_reports(
        &self,
        connection: std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
        sender: Arc<RTCRtpSender>,
    ) {
        use webrtc::rtcp::{
            packet::Packet as RtcpPacket,
            sender_report::SenderReport,
            source_description::{
                SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
            },
        };
        let anchor = (Instant::now(), std::time::SystemTime::now());
        let mut epoch = None;
        let mut due = None;
        loop {
            let wake = self.0.report_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.0.stopped.load(Ordering::Acquire) {
                break;
            }
            let clock = (*lock(&self.0.sent_clock)).filter(|c| self.0.current(c.generation));
            match clock {
                Some(c) if epoch != Some(c.generation) => {
                    epoch = Some(c.generation);
                    due = Some(c.first + Duration::from_millis(2500));
                }
                None => {
                    epoch = None;
                    due = None;
                }
                _ => {}
            }
            let Some(deadline) = due else {
                wake.await;
                continue;
            };
            tokio::select! {
                _=&mut wake=>continue,
                _=tokio::time::sleep_until(deadline.into())=>{}
            }
            let Some(c) = (*lock(&self.0.sent_clock)).filter(|c| self.0.current(c.generation))
            else {
                continue;
            };
            let Some(peer) = connection.upgrade() else {
                break;
            };
            let params = sender.get_parameters().await;
            let Some(encoding) = params.encodings.first() else {
                continue;
            };
            let now = Instant::now();
            let report = SenderReport {
                ssrc: encoding.ssrc,
                ntp_time: webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                    anchor.1 + anchor.0.elapsed(),
                ),
                rtp_time: c.timestamp.wrapping_add(
                    (now.duration_since(c.at).as_secs_f64() * f64::from(RATE)) as u32,
                ),
                packet_count: self.0.sent.load(Ordering::Relaxed) as u32,
                octet_count: self.0.octets.load(Ordering::Relaxed) as u32,
                ..Default::default()
            };
            // webrtc builds SDP CNAME from TrackLocal::stream_id (audio_0).
            let sdes = SourceDescription {
                chunks: vec![SourceDescriptionChunk {
                    source: encoding.ssrc,
                    items: vec![SourceDescriptionItem {
                        sdes_type: SdesType::SdesCname,
                        text: Bytes::from_static(b"audio_0"),
                    }],
                }],
            };
            let packets: Vec<Box<dyn RtcpPacket + Send + Sync>> =
                vec![Box::new(report), Box::new(sdes)];
            if self.0.current(c.generation) {
                match peer.write_rtcp(&packets).await {
                    Ok(_) => tracing::debug!(
                        packets = self.0.sent.load(Ordering::Relaxed),
                        "microphone sender report sent"
                    ),
                    Err(error) => tracing::debug!(%error,"microphone sender report failed"),
                }
            }
            due = Some(Instant::now() + Duration::from_secs_f64(2.5 + rand::random::<f64>() * 5.0));
        }
    }
    pub(crate) async fn send(&self, track: Arc<TrackLocalStaticRTP>) {
        let wall_clock = (Instant::now(), std::time::SystemTime::now());
        let mut sequence = rand::random::<u16>();
        let origin = rand::random::<u32>();
        let mut generation = u64::MAX;
        let mut encoder = None;
        let mut encoding = None;
        let mut pcm = Vec::with_capacity(RATE as usize * 2 * 120 / 1000);
        let mut first_timestamp = 0;
        let mut next_timestamp = 0;
        let mut marker = true;
        let mut output = [0u8; 4000];
        loop {
            let wake = self.0.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.0.stopped.load(Ordering::Acquire) {
                break;
            }
            let Some(frame) = self.0.frames.pop() else {
                wake.await;
                continue;
            };
            if !self.0.current(frame.generation)
                || frame.created.elapsed() > Duration::from_millis(100)
            {
                continue;
            }
            if generation != frame.generation {
                generation = frame.generation;
                encoding = lock(&self.0.state).encoding;
                let Some(config) = encoding else {
                    continue;
                };
                encoder = match make_encoder(config) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        self.fault(format!("麦克风编码初始化失败：{e:?}"));
                        None
                    }
                };
                pcm.clear();
                marker = true;
            }
            let (Some(config), Some(encoder)) = (encoding, encoder.as_mut()) else {
                continue;
            };
            if !pcm.is_empty() && frame.timestamp != next_timestamp {
                pcm.clear();
                marker = true;
            }
            if pcm.is_empty() {
                first_timestamp = frame.timestamp;
            }
            next_timestamp = frame.timestamp.wrapping_add(BLOCK as u32);
            if config.stereo {
                pcm.extend_from_slice(&frame.samples);
            } else {
                pcm.extend(frame.samples.chunks_exact(2).map(|p| (p[0] + p[1]) * 0.5));
            }
            let channels = if config.stereo { 2 } else { 1 };
            if pcm.len() < RATE as usize * config.packet_ms as usize / 1000 * channels {
                continue;
            }
            let loss = self.0.loss.load(Ordering::Relaxed).min(100) as u8;
            let encoded = encoder
                .set_packet_loss(loss)
                .and_then(|_| encoder.encode_float_to_slice(&pcm, &mut output));
            let rms = (pcm.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>()
                / pcm.len().max(1) as f64)
                .sqrt();
            let level = if rms > 0.0 {
                (-20.0 * rms.log10()).round().clamp(0.0, 127.0) as u8
            } else {
                127
            };
            pcm.clear();
            let length = match encoded {
                Ok(n) => n,
                Err(e) => {
                    self.fault(format!("麦克风编码失败：{e:?}"));
                    continue;
                }
            };
            if !self.0.current(generation) || track.all_binding_paused().await {
                marker = true;
                continue;
            }
            let packet = Packet {
                header: webrtc::rtp::header::Header {
                    version: 2,
                    marker,
                    sequence_number: sequence,
                    timestamp: origin.wrapping_add(first_timestamp),
                    ..Default::default()
                },
                payload: Bytes::copy_from_slice(&output[..length]),
            };
            use webrtc::rtp::extension::{
                HeaderExtension, abs_send_time_extension::AbsSendTimeExtension,
                audio_level_extension::AudioLevelExtension,
            };
            let extensions = [
                HeaderExtension::AudioLevel(AudioLevelExtension {
                    level,
                    voice: false,
                }),
                HeaderExtension::AbsSendTime(AbsSendTimeExtension::new(
                    wall_clock.1 + wall_clock.0.elapsed(),
                )),
            ];
            match track.write_rtp_with_extensions(&packet, &extensions).await {
                Ok(n) if n > 0 => {
                    self.0.sent.fetch_add(1, Ordering::Relaxed);
                    self.0.octets.fetch_add(length as u64, Ordering::Relaxed);
                    let mut clock = lock(&self.0.sent_clock);
                    let now = Instant::now();
                    let first = clock
                        .filter(|c| c.generation == generation)
                        .map_or(now, |c| c.first);
                    *clock = Some(SentClock {
                        timestamp: packet.header.timestamp,
                        at: now,
                        generation,
                        first,
                    });
                    drop(clock);
                    self.0.report_wake.notify_one();
                    marker = false;
                }
                Ok(_) => marker = true,
                Err(e) => {
                    lock(&self.0.state).error = Some(format!("麦克风发送失败：{e}"));
                    marker = true;
                }
            }
            // A failed write may already have encrypted/submitted the packet.
            // Never reuse its SRTP sequence number on a later payload.
            sequence = sequence.wrapping_add(1);
        }
    }
}

fn capture_owner(shared: Arc<Shared>) {
    let _ = shared.owner.set(std::thread::current());
    let host = cpal::default_host();
    let origin = Instant::now();
    let failed = Arc::new(AtomicBool::new(false));
    let mut stream = None;
    let mut device_id = None;
    let mut generation = u64::MAX;
    let mut failures = 0;
    while !shared.stopped.load(Ordering::Acquire) {
        if shared.refresh_devices.swap(false, Ordering::AcqRel) {
            let available = host.input_devices().map(|devices| {
                devices
                    .filter_map(|device| {
                        Some(InputDevice {
                            id: device.id().ok()?,
                            name: device.description().ok()?.name().to_owned(),
                        })
                    })
                    .collect()
            });
            *lock(&shared.input_devices) = match available {
                Ok(devices) => InputDevices {
                    devices,
                    error: None,
                    updated: Some(Instant::now()),
                },
                Err(error) => InputDevices {
                    devices: Vec::new(),
                    error: Some(format!("读取麦克风设备失败：{error}")),
                    updated: Some(Instant::now()),
                },
            };
        }
        let (mut next, selection) = {
            let state = lock(&shared.state);
            (
                shared.generation.load(Ordering::Acquire),
                state.selected_input.clone(),
            )
        };
        if !shared.capture.load(Ordering::Acquire) {
            stream = None;
            shared.capturing.store(false, Ordering::Release);
            shared.peak.store(0, Ordering::Relaxed);
            std::thread::park_timeout(Duration::from_secs(1));
            continue;
        }
        let device = match &selection {
            Some(id) => host.device_by_id(id).filter(DeviceTrait::supports_input),
            None => host.default_input_device(),
        };
        let id = device.as_ref().and_then(|d| d.id().ok());
        if next != generation || id != device_id {
            // A default-device/hotplug change must invalidate the old capture,
            // without adopting a concurrent UI selection under the wrong epoch.
            if next == generation {
                match shared.generation.compare_exchange(
                    next,
                    next.wrapping_add(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => next = next.wrapping_add(1),
                    Err(_) => continue,
                }
            }
            stream = None;
            failed.store(false, Ordering::Release);
            failures = 0;
            device_id = id;
            generation = next;
            shared.capturing.store(false, Ordering::Release);
            shared.peak.store(0, Ordering::Relaxed);
        }
        if failed.swap(false, Ordering::AcqRel) {
            stream = None;
            failures += 1;
            match shared.generation.compare_exchange(
                generation,
                generation.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => generation = generation.wrapping_add(1),
                Err(_) => continue,
            }
            shared.capturing.store(false, Ordering::Release);
            shared.peak.store(0, Ordering::Relaxed);
        }
        if stream.is_none() && failures < 3 {
            shared.capturing.store(false, Ordering::Release);
            let result = device
                .ok_or_else(|| {
                    if selection.is_some() {
                        anyhow!("所选麦克风已断开，请重新连接或选择其他设备")
                    } else {
                        anyhow!("没有可用的默认麦克风")
                    }
                })
                .and_then(|device| {
                    open_input(
                        &device,
                        Arc::clone(&shared),
                        Arc::clone(&failed),
                        generation,
                        origin,
                    )
                });
            if !shared.current(generation) {
                drop(result);
                continue;
            }
            match result {
                Ok(s) => {
                    stream = Some(s);
                    let _state = lock(&shared.state);
                    if shared.current(generation) {
                        shared.capturing.store(true, Ordering::Release);
                    }
                }
                Err(e) => {
                    let mut state = lock(&shared.state);
                    if shared.current(generation) {
                        state.error = Some(format!("麦克风不可用：{e:#}"));
                    }
                    failures += 1;
                }
            }
        }
        std::thread::park_timeout(if stream.is_none() && failures < 3 {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        });
    }
    drop(stream);
    shared.capturing.store(false, Ordering::Release);
    shared.peak.store(0, Ordering::Relaxed);
    while shared.frames.pop().is_some() {}
}

fn open_input(
    device: &cpal::Device,
    shared: Arc<Shared>,
    failed: Arc<AtomicBool>,
    generation: u64,
    origin: Instant,
) -> Result<cpal::Stream> {
    let supported = device
        .default_input_config()
        .context("读取麦克风格式失败")?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            build_input::<f32>(device, config, shared.clone(), failed, generation, origin)
        }
        cpal::SampleFormat::I16 => {
            build_input::<i16>(device, config, shared.clone(), failed, generation, origin)
        }
        cpal::SampleFormat::I24 => {
            build_input::<cpal::I24>(device, config, shared.clone(), failed, generation, origin)
        }
        cpal::SampleFormat::I32 => {
            build_input::<i32>(device, config, shared.clone(), failed, generation, origin)
        }
        cpal::SampleFormat::F64 => {
            build_input::<f64>(device, config, shared.clone(), failed, generation, origin)
        }
        format => Err(anyhow!("麦克风格式不受支持：{format}")),
    }?;
    stream
        .play()
        .context("启动麦克风失败（请检查系统麦克风权限）")?;
    let mut s = lock(&shared.state);
    if shared.current(generation) {
        s.device = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "系统默认麦克风".into());
        s.error = None;
    }
    tracing::info!(
        rate = config.sample_rate,
        channels = config.channels,
        "microphone capture started"
    );
    Ok(stream)
}

fn build_input<T: cpal::SizedSample + cpal::Sample>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<Shared>,
    failed: Arc<AtomicBool>,
    generation: u64,
    origin: Instant,
) -> Result<cpal::Stream>
where
    f32: cpal::FromSample<T>,
{
    ensure!(config.channels > 0, "麦克风没有声道");
    let channels = config.channels as usize;
    let mut resampler = crate::media::audio::dsp::Resampler::with_rates(config.sample_rate, RATE)?;
    let mut input = [0f32; 320];
    let mut normalized = [0f32; 1920];
    let mut frame = [0f32; BLOCK * 2];
    let mut filled = 0;
    let mut timestamp = (origin.elapsed().as_secs_f64() * f64::from(RATE)) as u64 as u32;
    let errors = shared.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if !shared.current(generation) {
                    return;
                }
                for chunk in data.chunks(channels * 160) {
                    let frames = chunk.len() / channels;
                    for (i, src) in chunk.chunks_exact(channels).enumerate() {
                        let l = <f32 as cpal::FromSample<T>>::from_sample_(src[0]);
                        let r = if channels > 1 {
                            <f32 as cpal::FromSample<T>>::from_sample_(src[1])
                        } else {
                            l
                        };
                        input[i * 2] = if l.is_finite() {
                            l.clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                        input[i * 2 + 1] = if r.is_finite() {
                            r.clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                    }
                    let mut consumed = 0;
                    while consumed < frames {
                        let result = if config.sample_rate == RATE {
                            normalized[..frames * 2].copy_from_slice(&input[..frames * 2]);
                            Ok((frames, frames))
                        } else {
                            resampler
                                .process(&input[consumed * 2..frames * 2], &mut normalized)
                                .map(|(used, produced)| (used / 2, produced / 2))
                        };
                        let Ok((used, produced)) = result else {
                            return;
                        };
                        if used == 0 && produced == 0 {
                            return;
                        }
                        consumed += used;
                        for &sample in &normalized[..produced * 2] {
                            frame[filled] = sample;
                            filled += 1;
                            if filled == frame.len() {
                                let peak = frame.iter().fold(0f32, |p, s| p.max(s.abs()));
                                shared.peak.store(peak.to_bits(), Ordering::Relaxed);
                                shared.frames.force_push(Frame {
                                    samples: frame,
                                    timestamp,
                                    generation,
                                    created: Instant::now(),
                                });
                                shared.wake.notify_one();
                                timestamp = timestamp.wrapping_add(BLOCK as u32);
                                filled = 0;
                            }
                        }
                    }
                }
            },
            move |e| {
                if !errors.current(generation) {
                    return;
                }
                lock(&errors.state).error = Some(format!("麦克风设备中断：{e}"));
                failed.store(true, Ordering::Release);
                errors.notify();
            },
            None,
        )
        .context("打开麦克风失败")
}
