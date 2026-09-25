//! Publisher-side pacing and negotiated RTX. No viewer-side retransmission logic
//! is reused: a repair has its own SSRC, payload type and SRTP sequence number.
use super::protection::{Budget, Kind};
use super::{Lease, lock};
use async_trait::async_trait;
use bytes::Bytes;
use goog_cc::{
    transport::{PacedPacketInfo, ProbeClusterConfig},
    units::DataRate,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc;
use webrtc::{
    interceptor::{
        Attributes, Error, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader,
        RTPWriter, stream_info::StreamInfo,
    },
    rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack,
    rtp::packet::Packet,
    util::marshal::MarshalSize,
};
type Result<T> = std::result::Result<T, Error>;
type Writer = Arc<dyn RTPWriter + Send + Sync>;
const FORMAT_EPOCH_ATTRIBUTE: usize = 0x5555_484f_5354_4550;
fn format_attributes(epoch: u64) -> Attributes {
    let mut attributes = Attributes::new();
    attributes.insert(FORMAT_EPOCH_ATTRIBUTE, epoch as usize);
    attributes
}

#[derive(Clone)]
struct Bound {
    info: StreamInfo,
    writer: Writer,
}
struct Stored {
    epoch: u64,
    packet: Packet,
    serial: u64,
    repair_at: Option<Instant>,
    keyframe: bool,
    repairs: u32,
    sent_at: Instant,
    captured: Option<Instant>,
}
struct FrameMeta {
    timestamp: u32,
    keyframe: bool,
    captured: Option<Instant>,
    first_delay: Option<u16>,
}
struct History {
    format: Option<super::format::Format>,
    packets: HashMap<u16, Stored>,
    order: VecDeque<(u16, u64)>,
    bytes: usize,
    next_serial: u64,
    primary: Option<u32>,
    rtx: Option<Bound>,
    rtx_sequence: u16,
    frame: Option<FrameMeta>,
    fec: Option<Bound>,
}
struct Pacer {
    at: Instant,
    debt: f64,
    draining: bool,
    group: u64,
    rate: f64,
}
struct SendRate {
    samples: VecDeque<(Instant, usize)>,
    bytes: usize,
    started: Option<Instant>,
    updated: Instant,
    limit_base: f64,
}
impl SendRate {
    fn expire(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= Duration::from_secs(1))
        {
            if let Some((_, bytes)) = self.samples.pop_front() {
                self.bytes -= bytes;
            }
        }
    }
    fn record(&mut self, bytes: usize) {
        let now = Instant::now();
        self.expire(now);
        if self.samples.is_empty() {
            self.started = Some(now);
        }
        self.samples.push_back((now, bytes));
        self.bytes += bytes;
    }
    fn limit(&mut self, target: u32) -> u64 {
        let now = Instant::now();
        self.expire(now);
        let span = self.started.map_or(1.0, |at| {
            now.duration_since(at).as_secs_f64().clamp(0.001, 1.0)
        });
        let actual = self.bytes as f64 * 8.0 / span;
        let target = f64::from(target);
        // T189036: converge toward measured pacer output, not the user limit.
        self.limit_base = if actual > target {
            (self.limit_base.max(target)
                + now.duration_since(self.updated).as_secs_f64() * (actual - target))
                .min(actual)
        } else {
            target
        };
        self.updated = now;
        (self.limit_base * 0.7).max(0.0) as u64
    }
}
struct Shared {
    history: Mutex<History>,
    pacer: tokio::sync::Mutex<Pacer>,
    repair_rate: AtomicU64,
    send_rate: Mutex<SendRate>,
    rtt_us: AtomicU64,
    closed: AtomicBool,
    cancel: CancellationToken,
    lease: Lease,
    repairs: Mutex<(VecDeque<(u32, u16)>, HashSet<(u32, u16)>)>,
    wake: tokio::sync::Notify,
    feedback_wake: tokio::sync::Notify,
    controller: Mutex<super::congestion::Controller>,
    tcc_id: AtomicU64,
    timing_ids: Mutex<[u8; 4]>,
    queued: AtomicU64,
    queued_at: Mutex<Option<Instant>>,
    budget: Mutex<Budget>,
    fec: Mutex<Option<super::fec::Sender>>,
    fec_blocks: Mutex<VecDeque<(Bound, super::fec::Block, Packet, Instant, u64)>>,
    fec_wake: tokio::sync::Notify,
    fec_sequence: AtomicU64,
    fec_origin: u32,
    started: Instant,
    fps: AtomicU64,
    network: AtomicBool,
    egress: tokio::sync::Mutex<()>,
    waiting: Mutex<BTreeMap<(u8, u64), (Instant, u64)>>,
    next_ticket: AtomicU64,
    network_overhead: AtomicU64,
    srtp_overhead: AtomicU64,
    rtx_types: Mutex<HashMap<u8, u8>>,
    automatic: Mutex<super::network::AutoSwitch>,
    format_epoch: AtomicU64,
    repair_header: AtomicU64,
    counts: [AtomicU64; 4],
}
#[derive(Clone)]
pub(crate) struct Transport(Arc<Shared>);
impl Transport {
    pub(crate) fn new(
        lease: Lease,
        cancel: CancellationToken,
        bounds: super::parameters::Bounds,
        policy: super::network::Policy,
    ) -> Self {
        let rate = bounds.initial;
        Self(Arc::new(Shared {
            history: Mutex::new(History {
                format: None,
                packets: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                next_serial: 0,
                primary: None,
                rtx: None,
                rtx_sequence: rand::random(),
                frame: None,
                fec: None,
            }),
            pacer: tokio::sync::Mutex::new(Pacer {
                at: Instant::now(),
                debt: 0.0,
                draining: false,
                group: 1,
                rate: f64::from(rate) * 1.6 / 8.0,
            }),
            repair_rate: AtomicU64::new(u64::from(rate) * 7 / 10),
            send_rate: Mutex::new(SendRate {
                samples: VecDeque::new(),
                bytes: 0,
                started: None,
                updated: Instant::now(),
                limit_base: 0.0,
            }),
            rtt_us: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            cancel,
            lease,
            repairs: Mutex::new((VecDeque::new(), HashSet::new())),
            wake: tokio::sync::Notify::new(),
            feedback_wake: tokio::sync::Notify::new(),
            controller: Mutex::new(super::congestion::Controller::new(bounds)),
            tcc_id: AtomicU64::new(0),
            timing_ids: Mutex::new([0; 4]),
            queued: AtomicU64::new(0),
            queued_at: Mutex::new(None),
            budget: Mutex::new(Budget::default()),
            fec: Mutex::new(None),
            fec_blocks: Mutex::new(VecDeque::new()),
            fec_wake: tokio::sync::Notify::new(),
            fec_sequence: AtomicU64::new(rand::random::<u16>().into()),
            fec_origin: rand::random(),
            started: Instant::now(),
            fps: AtomicU64::new(60),
            network: AtomicBool::new(false),
            egress: tokio::sync::Mutex::new(()),
            waiting: Mutex::new(BTreeMap::new()),
            next_ticket: AtomicU64::new(0),
            network_overhead: AtomicU64::new(28),
            srtp_overhead: AtomicU64::new(16),
            rtx_types: Mutex::new(HashMap::new()),
            automatic: Mutex::new(super::network::AutoSwitch::new(policy)),
            format_epoch: AtomicU64::new(0),
            repair_header: AtomicU64::new(32),
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
        }))
    }
    pub(crate) fn configure(&self, bounds: super::parameters::Bounds, restart: bool) {
        lock(&self.0.controller).configure(bounds, restart);
    }
    pub(crate) fn media_rate(&self) -> u32 {
        let total = lock(&self.0.controller).media_allocation();
        lock(&self.0.budget).media_rate(total)
    }
    pub(crate) fn automatic_rates(&self) -> (u32, u32, f64) {
        lock(&self.0.controller).automatic_rates()
    }
    pub(crate) fn cwnd_ratio(&self) -> f64 {
        lock(&self.0.controller).cwnd_reduce_ratio
    }
    pub(crate) fn route_overhead(&self, network: usize) {
        self.0
            .network_overhead
            .store(network as u64, Ordering::Relaxed);
    }
    pub(crate) fn srtp_overhead(&self, bytes: usize) {
        self.0.srtp_overhead.store(bytes as u64, Ordering::Relaxed);
    }
    pub(crate) fn negotiated_codecs(
        &self,
        codecs: &[webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters],
    ) {
        let types = codecs
            .iter()
            .filter(|c| c.capability.mime_type.eq_ignore_ascii_case("video/rtx"))
            .filter_map(|c| {
                c.capability
                    .sdp_fmtp_line
                    .split(';')
                    .find_map(|s| {
                        s.trim()
                            .strip_prefix("apt=")
                            .and_then(|v| v.parse::<u8>().ok())
                    })
                    .map(|apt| (apt, c.payload_type))
            })
            .collect();
        *lock(&self.0.rtx_types) = types;
    }
    pub(crate) fn selected_route(
        &self,
        pair: &webrtc::ice_transport::ice_candidate_pair::RTCIceCandidatePair,
    ) {
        lock(&self.0.automatic).route(pair);
    }
    pub(crate) fn remote_report(&self, loss: u8, rtt: Duration) {
        self.rtt(rtt);
        lock(&self.0.automatic).report(loss, rtt);
    }
    pub(crate) fn network_change(&self) -> Option<u8> {
        lock(&self.0.automatic).tick(self.0.network.load(Ordering::Acquire))
    }
    pub(crate) fn network_attempt(&self) -> u8 {
        lock(&self.0.automatic).attempt()
    }
    pub(crate) fn quality(&self, automatic: bool, quality: i32, settings_changed: bool) {
        lock(&self.0.automatic).quality(automatic, quality, settings_changed);
    }
    fn overhead(&self) -> usize {
        self.network_overhead() + self.0.srtp_overhead.load(Ordering::Relaxed) as usize
    }
    fn network_overhead(&self) -> usize {
        self.0.network_overhead.load(Ordering::Relaxed) as usize
    }
    pub(crate) fn packet_limit(&self) -> usize {
        1460.min(1500usize.saturating_sub(self.overhead()))
    }
    pub(crate) fn wire_size(&self, rtp: usize) -> usize {
        rtp + self.overhead()
    }
    pub(crate) fn payload_limit(&self, header: usize, repair_header: usize) -> usize {
        self.0
            .repair_header
            .store(repair_header as u64, Ordering::Relaxed);
        // RSFEC protects the entire original RTP packet plus its length word.
        // Reserve its largest (109-source) mask, repair RTP header, MID and TCC.
        let repair = if lock(&self.0.history).fec.is_some() {
            2 + 27 + repair_header
        } else {
            0
        };
        self.packet_limit().saturating_sub(header + repair.max(2))
    }
    pub(crate) fn media_ready(&self) -> bool {
        lock(&self.0.history).primary.is_some()
    }
    pub(crate) fn network(&self, available: bool) {
        self.0.network.store(available, Ordering::Release);
        lock(&self.0.controller).network(available);
        lock(&self.0.budget).network(available);
        self.0.feedback_wake.notify_waiters();
    }
    pub(crate) fn route_changed(&self) {
        lock(&self.0.controller).route_changed();
        lock(&self.0.budget).reset(true);
        lock(&self.0.fec_blocks).clear();
        if let Some(fec) = lock(&self.0.fec).as_mut() {
            fec.clear();
        }
        self.0.feedback_wake.notify_waiters();
    }
    pub(crate) fn feedback(&self, feedback: &TransportLayerCc) {
        lock(&self.0.controller).feedback(feedback);
        self.0.feedback_wake.notify_waiters();
    }
    pub(crate) async fn control(&self) {
        let mut interval = tokio::time::interval(Duration::from_millis(25));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut report_at = Instant::now();
        while self.active() {
            tokio::select! { _=self.0.cancel.cancelled()=>break, _=interval.tick()=>{} }
            let mut controller = lock(&self.0.controller);
            controller.tick(self.0.queued.load(Ordering::Relaxed) as usize);
            self.0.repair_rate.store(
                lock(&self.0.send_rate).limit(controller.target),
                Ordering::Release,
            );
            lock(&self.0.budget).update(
                controller.target,
                controller.loss,
                controller.delay_overuse(),
                controller.link_pressure(),
                Duration::from_secs_f64(
                    self.0.queued.load(Ordering::Relaxed) as f64 * 8.0
                        / f64::from(controller.pacing.max(1)),
                ),
            );
            if report_at.elapsed() >= Duration::from_secs(5) {
                tracing::info!(
                    feedbacks = controller.feedback_count,
                    target_bps = controller.target,
                    pacing_bps = controller.pacing,
                    loss = controller.loss,
                    delay_overuse = controller.delay_overuse(),
                    link_pressure = controller.link_pressure(),
                    burst_bytes = controller.burst_bytes(),
                    queued_bytes = self.0.queued.load(Ordering::Relaxed),
                    media_packets = self.0.counts[0].load(Ordering::Relaxed),
                    rtx_packets = self.0.counts[1].load(Ordering::Relaxed),
                    fec_packets = self.0.counts[2].load(Ordering::Relaxed),
                    probe_packets = self.0.counts[3].load(Ordering::Relaxed),
                    "host send controller"
                );
                report_at = Instant::now();
            }
        }
    }
    pub(crate) fn frame(
        &self,
        timestamp: u32,
        keyframe: bool,
        captured: Option<Instant>,
        format: super::format::Format,
    ) {
        let mut history = lock(&self.0.history);
        let changed = history.format != Some(format);
        if changed {
            history.format = Some(format);
            self.0.format_epoch.fetch_add(1, Ordering::AcqRel);
            history.packets.clear();
            history.order.clear();
            history.bytes = 0;
        }
        history.frame = Some(FrameMeta {
            timestamp,
            keyframe,
            captured,
            first_delay: None,
        });
        drop(history);
        if changed {
            let mut repairs = lock(&self.0.repairs);
            repairs.0.clear();
            repairs.1.clear();
            drop(repairs);
            lock(&self.0.fec_blocks).clear();
            if let Some(fec) = lock(&self.0.fec).as_mut() {
                fec.clear();
            }
            self.0.feedback_wake.notify_waiters();
        }
    }
    pub(crate) fn fps(&self, fps: u32) {
        self.0.fps.store(fps.into(), Ordering::Relaxed);
    }
    pub(crate) fn queued_frame(&self, bytes: usize) -> QueuedFrame {
        *lock(&self.0.queued_at) = Some(Instant::now());
        self.0.queued.fetch_add(bytes as u64, Ordering::Relaxed);
        QueuedFrame {
            transport: self.clone(),
            remaining: bytes as u64,
        }
    }
    pub(crate) fn rtt(&self, rtt: Duration) {
        self.0.rtt_us.store(
            rtt.as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
    fn active(&self) -> bool {
        !self.0.closed.load(Ordering::Acquire)
            && !self.0.cancel.is_cancelled()
            && self.0.lease.requested()
    }
    async fn write(
        &self,
        parent: &Writer,
        packet: &Packet,
        attributes: &Attributes,
    ) -> Result<usize> {
        let (priority, extra) = {
            let history = lock(&self.0.history);
            if history.primary == Some(packet.header.ssrc) {
                (2, 0)
            } else if history
                .rtx
                .as_ref()
                .is_some_and(|b| b.info.ssrc == packet.header.ssrc)
            {
                (1, packet.marshal_size() + self.overhead())
            } else {
                (2, packet.marshal_size() + self.overhead())
            }
        };
        let ticket = self.ticket(priority, extra);
        let bytes = (packet.marshal_size() + self.overhead()) as f64;
        loop {
            let notified = self.0.feedback_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.active() {
                return Err(Error::ErrIoEOF);
            }
            if !self.0.network.load(Ordering::Acquire) {
                tokio::select! { _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF), _=notified=>{} }
                continue;
            }
            if !ticket.first() {
                tokio::select! { _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),_=notified=>{} }
                continue;
            }
            let egress = tokio::select! { _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),guard=self.0.egress.lock()=>guard };
            if !ticket.first() {
                drop(egress);
                continue;
            }
            let (base_rate, burst) = {
                let c = lock(&self.0.controller);
                (
                    f64::from(c.pacing.max(300_000)) / 8.0,
                    c.burst_bytes() as f64,
                )
            };
            let mut pacer = self.0.pacer.lock().await;
            let now = Instant::now();
            pacer.debt =
                (pacer.debt - now.duration_since(pacer.at).as_secs_f64() * pacer.rate).max(0.0);
            pacer.at = now;
            if pacer.debt == 0.0 {
                pacer.draining = false;
                pacer.group = pacer.group.wrapping_add(1);
            }
            // T381470: default DrainQueue is enabled. Screenshare gives a
            // 6ms queue limit; drain using remaining time, with a 1ms floor.
            let queued = self.0.queued.load(Ordering::Relaxed) as f64;
            let oldest = lock(&self.0.waiting).values().map(|v| v.0).min();
            let oldest = oldest.into_iter().chain(*lock(&self.0.queued_at)).min();
            let age = oldest.map_or(0., |at| now.saturating_duration_since(at).as_secs_f64());
            let required = queued / (0.006 - age).max(0.001);
            let rate = base_rate
                .max(required)
                .max(if pacer.debt > 0.0 { pacer.rate } else { 0.0 });
            pacer.rate = rate;
            // T38197A: send a burst, then repay the full media debt. Idle
            // time cannot accumulate credit for a later oversized burst.
            if !pacer.draining && pacer.debt < burst.min(rate * 0.040) {
                pacer.debt += bytes;
                let group = pacer.group;
                drop(pacer);
                return self
                    .emit_locked(
                        parent,
                        packet,
                        attributes,
                        PacedPacketInfo::default(),
                        Some(group),
                    )
                    .await;
            }
            pacer.draining = true;
            let delay = Duration::from_secs_f64(pacer.debt / rate);
            drop(pacer);
            drop(egress);
            tokio::select! {_ = self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),_=tokio::time::sleep(delay)=>{},_=notified=>{}}
        }
    }
    async fn emit(
        &self,
        parent: &Writer,
        packet: &Packet,
        attributes: &Attributes,
        pacing: PacedPacketInfo,
        burst: Option<u64>,
    ) -> Result<usize> {
        let ticket = self.ticket(7, packet.marshal_size() + self.overhead());
        loop {
            let notified = self.0.feedback_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.active() {
                return Err(Error::ErrIoEOF);
            }
            if !ticket.first() || !self.0.network.load(Ordering::Acquire) {
                tokio::select! {_=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),_=notified=>{}}
                continue;
            }
            let _egress = tokio::select! {_=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),guard=self.0.egress.lock()=>guard};
            if ticket.first() {
                return self
                    .emit_locked(parent, packet, attributes, pacing, burst)
                    .await;
            }
        }
    }
    fn ticket(&self, priority: u8, extra: usize) -> Ticket {
        let key = (priority, self.0.next_ticket.fetch_add(1, Ordering::Relaxed));
        lock(&self.0.waiting).insert(key, (Instant::now(), extra as u64));
        self.0.queued.fetch_add(extra as u64, Ordering::Relaxed);
        self.0.feedback_wake.notify_waiters();
        Ticket {
            transport: self.clone(),
            key,
        }
    }
    async fn emit_locked(
        &self,
        parent: &Writer,
        packet: &Packet,
        attributes: &Attributes,
        pacing: PacedPacketInfo,
        burst: Option<u64>,
    ) -> Result<usize> {
        if !self.active() {
            return Err(Error::ErrIoEOF);
        }
        if attributes
            .get(&FORMAT_EPOCH_ATTRIBUTE)
            .is_some_and(|epoch| *epoch as u64 != self.0.format_epoch.load(Ordering::Acquire))
        {
            return Ok(0);
        }
        let id = self.0.tcc_id.load(Ordering::Relaxed) as u8;
        let mut packet = packet.clone();
        let [absolute, timing, sending_delay, offset] = *lock(&self.0.timing_ids);
        let now = Instant::now();
        let (capture, delay) = {
            let mut h = lock(&self.0.history);
            if h.primary == Some(packet.header.ssrc) {
                if let Some(f) = h
                    .frame
                    .as_mut()
                    .filter(|f| f.timestamp == packet.header.timestamp)
                {
                    let delay = f.captured.map(|at| {
                        *f.first_delay.get_or_insert_with(|| {
                            ((now.saturating_duration_since(at).as_micros() + 500) / 1000)
                                .min(u16::MAX.into()) as u16
                        })
                    });
                    (f.captured, delay)
                } else {
                    (None, None)
                }
            } else if h
                .rtx
                .as_ref()
                .is_some_and(|r| r.info.ssrc == packet.header.ssrc)
                && packet.payload.len() >= 2
            {
                let seq = u16::from_be_bytes([packet.payload[0], packet.payload[1]]);
                (h.packets.get(&seq).and_then(|p| p.captured), None)
            } else {
                (None, None)
            }
        };
        if absolute != 0 && packet.header.get_extension(absolute).is_some() {
            let ntp = webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                std::time::SystemTime::now(),
            );
            let value = ((ntp >> 14) as u32 & 0xFFFFFF).to_be_bytes();
            packet
                .header
                .set_extension(absolute, Bytes::copy_from_slice(&value[1..]))?;
        }
        if let Some(capture) = capture {
            let delta = ((now.saturating_duration_since(capture).as_micros() + 500) / 1000)
                .min(u16::MAX.into()) as u16;
            if let Some(mut value) = packet
                .header
                .get_extension(timing)
                .filter(|v| v.len() == 13)
                .map(|v| v.to_vec())
            {
                value[7..9].copy_from_slice(&delta.to_be_bytes());
                packet.header.set_extension(timing, value.into())?;
            }
            if offset != 0 && packet.header.get_extension(offset).is_some() {
                let ticks = (now.saturating_duration_since(capture).as_micros() * 9 / 100)
                    .min(0x7FFFFF) as u32;
                packet
                    .header
                    .set_extension(offset, Bytes::copy_from_slice(&ticks.to_be_bytes()[1..]))?;
            }
        }
        if let Some(delay) = delay
            .filter(|_| sending_delay != 0 && packet.header.get_extension(sending_delay).is_some())
        {
            packet
                .header
                .set_extension(sending_delay, Bytes::copy_from_slice(&delay.to_be_bytes()))?;
        }
        if id > 14 && packet.header.extension_profile != 0x1000 {
            packet.header.extension = true;
            packet.header.extension_profile = 0x1000;
            let length: usize = packet
                .header
                .extensions
                .iter()
                .map(|e| e.payload.len() + 2)
                .sum();
            packet.header.extensions_padding = (4 - length % 4) % 4;
        }
        let sequence = if id != 0 {
            let seq = lock(&self.0.controller).next_sequence();
            packet
                .header
                .set_extension(id, Bytes::copy_from_slice(&(seq as u16).to_be_bytes()))?;
            Some(seq)
        } else {
            None
        };
        let size = tokio::select! {
            _=self.0.cancel.cancelled()=>return Err(Error::ErrIoEOF),
            result=parent.write(&packet, attributes)=>result?,
        };
        if size > 0 {
            let wire = size + self.network_overhead();
            lock(&self.0.send_rate).record(wire);
            if let Some(sequence) = sequence {
                lock(&self.0.controller).sent(sequence, wire, pacing, burst);
            }
            let (primary, fec) = {
                let history = lock(&self.0.history);
                (
                    history.primary == Some(packet.header.ssrc),
                    history
                        .fec
                        .as_ref()
                        .is_some_and(|f| f.info.ssrc == packet.header.ssrc),
                )
            };
            let kind = if pacing.probe_cluster_id != PacedPacketInfo::NOT_APROBE {
                Kind::Probe
            } else if primary {
                Kind::Media
            } else if fec {
                Kind::Fec
            } else {
                Kind::Rtx
            };
            self.0.counts[match kind {
                Kind::Media => 0,
                Kind::Rtx => 1,
                Kind::Fec => 2,
                Kind::Probe => 3,
            }]
            .fetch_add(1, Ordering::Relaxed);
            lock(&self.0.budget).record(kind, wire);
            if primary {
                self.remember(&packet);
                self.fec_source(&packet);
            }
        }
        Ok(size)
    }
    fn fec_source(&self, packet: &Packet) {
        if !lock(&self.0.budget).fec_enabled() {
            if let Some(f) = lock(&self.0.fec).as_mut() {
                f.clear();
            }
            return;
        }
        let (bound, key) = {
            let h = lock(&self.0.history);
            (
                h.fec.clone(),
                h.frame
                    .as_ref()
                    .is_some_and(|f| f.timestamp == packet.header.timestamp && f.keyframe),
            )
        };
        let Some(bound) = bound else { return };
        let (loss, rtt, target) = {
            let c = lock(&self.0.controller);
            (c.loss, c.rtt.ms().max(0) as u64, c.target)
        };
        let result = {
            let mut f = lock(&self.0.fec);
            let Some(f) = f.as_mut() else { return };
            f.push(
                packet,
                key,
                loss,
                rtt,
                self.0.fps.load(Ordering::Relaxed) as u32,
            )
        };
        match result {
            Ok(Some(block)) => {
                lock(&self.0.budget).fec_demand(f64::from(target) * loss.clamp(0.05, 0.35));
                let mut queue = lock(&self.0.fec_blocks);
                if queue.len() < 8 {
                    queue.push_back((
                        bound,
                        block,
                        packet.clone(),
                        Instant::now(),
                        self.0.format_epoch.load(Ordering::Acquire),
                    ));
                    self.0.fec_wake.notify_one();
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error,"host FEC source rejected"),
        }
    }
    pub(crate) async fn fec_worker(&self) {
        while self.active() {
            let wake = self.0.fec_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            let item = lock(&self.0.fec_blocks).pop_front();
            let Some((bound, block, source, _, epoch)) = item else {
                tokio::select! {_=self.0.cancel.cancelled()=>return,_=wake=>{}};
                continue;
            };
            let (count, id) = lock(&self.0.budget).reserve_fec(
                block.desired,
                self.wire_size(
                    block.shard_size + 27 + self.0.repair_header.load(Ordering::Relaxed) as usize,
                ),
            );
            if count == 0 {
                continue;
            }
            let mut reservation = Reserved {
                transport: self.clone(),
                id,
                actual: 0,
            };
            let keyframe = block.keyframe;
            let payloads = match block.payloads(count) {
                Ok(p) => p,
                Err(error) => {
                    tracing::warn!(%error,"host FEC encode failed");
                    continue;
                }
            };
            tracing::debug!(
                repairs = payloads.len(),
                keyframe,
                "host FEC block generated"
            );
            for payload in payloads {
                if !self.active() {
                    break;
                }
                let mut packet = Packet {
                    header: webrtc::rtp::header::Header {
                        version: 2,
                        extension: source.header.extension_profile == 0x1000,
                        extension_profile: if source.header.extension_profile == 0x1000 {
                            0x1000
                        } else {
                            0
                        },
                        ssrc: bound.info.ssrc,
                        payload_type: bound.info.payload_type,
                        sequence_number: self.0.fec_sequence.fetch_add(1, Ordering::Relaxed) as u16,
                        timestamp: self
                            .0
                            .fec_origin
                            .wrapping_add((self.0.started.elapsed().as_micros() * 9 / 100) as u32),
                        ..Default::default()
                    },
                    payload: payload.into(),
                };
                if let Some(mid) = bound
                    .info
                    .rtp_header_extensions
                    .iter()
                    .find(|e| e.uri == "urn:ietf:params:rtp-hdrext:sdes:mid")
                {
                    if let Some(value) = source.header.get_extension(mid.id as u8) {
                        let _ = packet.header.set_extension(mid.id as u8, value);
                    }
                }
                let attributes = format_attributes(epoch);
                match self.write(&bound.writer, &packet, &attributes).await {
                    Ok(0) => break,
                    Ok(size) => reservation.actual += size + self.network_overhead(),
                    Err(error) => {
                        if self.active() {
                            tracing::debug!(%error,"host FEC send failed");
                        }
                        break;
                    }
                }
            }
        }
    }
    fn probe_packet(&self) -> Option<(Bound, Packet, u64)> {
        let mut history = lock(&self.0.history);
        let bound = history.rtx.clone()?;
        let mut packet = history.order.iter().rev().find_map(|(seq, serial)| {
            history
                .packets
                .get(seq)
                .filter(|p| p.serial == *serial && p.packet.marshal_size() >= 200)
                .map(|p| p.packet.clone())
        })?;
        let mut payload = Vec::with_capacity(packet.payload.len() + 2);
        payload.extend_from_slice(&packet.header.sequence_number.to_be_bytes());
        payload.extend_from_slice(&packet.payload);
        packet.payload = payload.into();
        packet.header.ssrc = bound.info.ssrc;
        packet.header.payload_type = *lock(&self.0.rtx_types).get(&packet.header.payload_type)?;
        packet.header.sequence_number = history.rtx_sequence;
        history.rtx_sequence = history.rtx_sequence.wrapping_add(1);
        Some((bound, packet, self.0.format_epoch.load(Ordering::Acquire)))
    }
    pub(crate) async fn probes(&self) {
        let mut active = None::<(ProbeClusterConfig, Instant, usize, i64)>;
        while self.active() {
            tokio::select! {_=self.0.cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(2))=>{}}
            if self.0.tcc_id.load(Ordering::Relaxed) == 0 {
                continue;
            }
            if active.is_none() {
                if lock(&self.0.history).packets.is_empty() {
                    continue;
                }
                let mut c = lock(&self.0.controller);
                while let Some(cluster) = c.probes.pop_front() {
                    if c.probe_is_current(&cluster) {
                        active = Some((cluster, Instant::now(), 0, 0));
                        break;
                    }
                }
            }
            let Some((cluster, started, sent, bursts)) = active.as_mut() else {
                continue;
            };
            if !lock(&self.0.controller).probe_is_current(cluster) {
                active = None;
                continue;
            }
            let rate = cluster.target_data_rate.bps_or(0).max(0) as u64;
            if rate == 0 {
                active = None;
                continue;
            }
            let minimum =
                ((rate as u128 * cluster.target_duration.us().max(0) as u128) / 8_000_000) as usize;
            let schedule_us = *sent as u64 * 8_000_000 / rate;
            if *bursts > 0 && started.elapsed().as_micros() > u128::from(schedule_us + 10_000) {
                active = None;
                continue;
            }
            if started.elapsed().as_micros() < u128::from(schedule_us) {
                continue;
            }
            let delta = cluster.min_probe_delta.us().max(2_000) as u64;
            let burst = ((rate as u128 * delta as u128) / 8_000_000)
                .max(200)
                .min(128_000) as usize;
            let mut bytes = 0;
            while bytes < burst && self.active() {
                let Some((bound, packet, epoch)) = self.probe_packet() else {
                    break;
                };
                let info = PacedPacketInfo {
                    send_bitrate: DataRate::from_bits_per_sec(rate as i64),
                    probe_cluster_id: cluster.id,
                    probe_cluster_min_probes: cluster.target_probe_count as i64,
                    probe_cluster_min_bytes: minimum as i64,
                    probe_cluster_bytes_sent: *sent as i64,
                };
                match self
                    .emit(
                        &bound.writer,
                        &packet,
                        &format_attributes(epoch),
                        info,
                        None,
                    )
                    .await
                {
                    Ok(0) => break,
                    Ok(size) => {
                        bytes += size + self.network_overhead();
                        *sent += size + self.network_overhead();
                    }
                    Err(_) => break,
                }
            }
            *bursts += 1;
            if *bursts >= cluster.target_probe_count as i64 && *sent >= minimum {
                active = None;
            }
        }
    }
    fn remember(&self, packet: &Packet) {
        if !self.active() {
            return;
        }
        let mut history = lock(&self.0.history);
        history.next_serial = history.next_serial.wrapping_add(1);
        let serial = history.next_serial;
        let seq = packet.header.sequence_number;
        if let Some(old) = history.packets.remove(&seq) {
            history.bytes -= old.packet.marshal_size();
        }
        history.bytes += packet.marshal_size();
        let keyframe = history
            .frame
            .as_ref()
            .is_some_and(|f| f.timestamp == packet.header.timestamp && f.keyframe);
        let captured = history
            .frame
            .as_ref()
            .filter(|f| f.timestamp == packet.header.timestamp)
            .and_then(|f| f.captured);
        history.packets.insert(
            seq,
            Stored {
                epoch: self.0.format_epoch.load(Ordering::Acquire),
                packet: packet.clone(),
                serial,
                repair_at: None,
                keyframe,
                repairs: 0,
                sent_at: Instant::now(),
                captured,
            },
        );
        history.order.push_back((seq, serial));
        self.prune_history(&mut history);
    }
    fn prune_history(&self, history: &mut History) {
        let minimum = Duration::from_micros(
            self.0
                .rtt_us
                .load(Ordering::Relaxed)
                .saturating_mul(3)
                .max(1_000_000),
        );
        while let Some(&(seq, serial)) = history.order.front() {
            let Some(packet) = history.packets.get(&seq).filter(|p| p.serial == serial) else {
                history.order.pop_front();
                continue;
            };
            let age = packet.repair_at.unwrap_or(packet.sent_at).elapsed();
            let hard = history.packets.len() >= 38400 || history.bytes > 64 * 1024 * 1024;
            if !hard
                && history
                    .primary
                    .is_some_and(|ssrc| lock(&self.0.repairs).1.contains(&(ssrc, seq)))
            {
                break;
            }
            let expired = age >= minimum
                && (history.packets.len() >= 24000 || age >= minimum.saturating_mul(3));
            if !hard && !expired {
                break;
            }
            history.order.pop_front();
            if let Some(packet) = history.packets.remove(&seq) {
                history.bytes -= packet.packet.marshal_size();
            }
        }
    }
    pub(crate) fn nack(&self, nack: &TransportLayerNack) {
        if !self.active() || lock(&self.0.history).primary != Some(nack.media_ssrc) {
            return;
        }
        lock(&self.0.budget).nack();
        let mut queue = lock(&self.0.repairs);
        for pair in &nack.nacks {
            for seq in pair.into_iter() {
                if queue.1.len() >= 38400 {
                    break;
                }
                if queue.1.insert((nack.media_ssrc, seq)) {
                    queue.0.push_back((nack.media_ssrc, seq));
                }
            }
        }
        self.0.wake.notify_one();
    }
    pub(crate) async fn repairs(&self) {
        let mut window = VecDeque::<(Instant, usize)>::new();
        let mut window_bytes = 0;
        loop {
            let wake = self.0.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if !self.active() {
                return;
            }
            let pending = lock(&self.0.repairs).0.pop_front();
            let Some((ssrc, seq)) = pending else {
                tokio::select! {_=self.0.cancel.cancelled()=>return,_=&mut wake=>{}}
                continue;
            };
            let result = self.repair(ssrc, seq, &mut window, &mut window_bytes).await;
            lock(&self.0.repairs).1.remove(&(ssrc, seq));
            if let Err(error) = result {
                if self.active() {
                    tracing::debug!(%error,"host RTX write failed");
                }
            }
        }
    }
    async fn repair(
        &self,
        ssrc: u32,
        seq: u16,
        window: &mut VecDeque<(Instant, usize)>,
        window_bytes: &mut usize,
    ) -> Result<()> {
        if !self.active() {
            return Ok(());
        }
        let next = {
            let mut history = lock(&self.0.history);
            self.prune_history(&mut history);
            if history.primary != Some(ssrc) {
                return Ok(());
            }
            let Some(bound) = history.rtx.clone() else {
                return Ok(());
            };
            let Some(stored) = history.packets.get_mut(&seq) else {
                return Ok(());
            };
            let rtt = self.0.rtt_us.load(Ordering::Relaxed);
            if stored
                .repair_at
                .is_some_and(|at| rtt > 0 && at.elapsed() < Duration::from_micros(rtt))
            {
                return Ok(());
            }
            while window
                .front()
                .is_some_and(|(at, _)| at.elapsed() >= Duration::from_secs(1))
            {
                if let Some((_, bytes)) = window.pop_front() {
                    *window_bytes -= bytes;
                }
            }
            let bytes = stored.packet.marshal_size() + 2 + self.overhead();
            let Some(payload_type) = lock(&self.0.rtx_types)
                .get(&stored.packet.header.payload_type)
                .copied()
            else {
                return Ok(());
            };
            // Current UU RetransmissionRateLimit: ratio .7, 1000ms window.
            if !stored.keyframe
                && stored.repairs > 1
                && (*window_bytes + bytes) as u64 > self.0.repair_rate.load(Ordering::Acquire) / 8
            {
                return Ok(());
            }
            let Some(reservation) = lock(&self.0.budget).reserve_rtx(bytes, stored.repairs) else {
                return Ok(());
            };
            let serial = stored.serial;
            let epoch = stored.epoch;
            let mut packet = stored.packet.clone();
            let mut payload = Vec::with_capacity(packet.payload.len() + 2);
            payload.extend_from_slice(&seq.to_be_bytes());
            payload.extend_from_slice(&packet.payload);
            packet.payload = Bytes::from(payload);
            packet.header.ssrc = bound.info.ssrc;
            packet.header.payload_type = payload_type;
            packet.header.sequence_number = history.rtx_sequence;
            history.rtx_sequence = history.rtx_sequence.wrapping_add(1);
            (bound, packet, reservation, serial, epoch)
        };
        let mut reservation = Reserved {
            transport: self.clone(),
            id: next.2,
            actual: 0,
        };
        let bytes = self
            .write(&next.0.writer, &next.1, &format_attributes(next.4))
            .await?;
        if bytes == 0 {
            return Ok(());
        }
        reservation.actual = bytes + self.network_overhead();
        if let Some(stored) = lock(&self.0.history)
            .packets
            .get_mut(&seq)
            .filter(|p| p.serial == next.3)
        {
            stored.repair_at = Some(Instant::now());
            stored.repairs = stored.repairs.saturating_add(1);
        }
        window.push_back((Instant::now(), reservation.actual));
        *window_bytes += reservation.actual;
        Ok(())
    }
}
struct Ticket {
    transport: Transport,
    key: (u8, u64),
}
impl Ticket {
    fn first(&self) -> bool {
        lock(&self.transport.0.waiting)
            .first_key_value()
            .is_some_and(|(key, _)| *key == self.key)
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some((_, extra)) = lock(&self.transport.0.waiting).remove(&self.key) {
            self.transport.0.queued.fetch_sub(extra, Ordering::Relaxed);
        }
        self.transport.0.feedback_wake.notify_waiters();
    }
}
pub(crate) struct QueuedFrame {
    transport: Transport,
    remaining: u64,
}
impl QueuedFrame {
    pub(crate) fn sent(&mut self, bytes: usize) {
        let bytes = (bytes as u64).min(self.remaining);
        self.remaining -= bytes;
        self.transport.0.queued.fetch_sub(bytes, Ordering::Relaxed);
    }
}
impl Drop for QueuedFrame {
    fn drop(&mut self) {
        *lock(&self.transport.0.queued_at) = None;
        self.transport
            .0
            .queued
            .fetch_sub(self.remaining, Ordering::Relaxed);
    }
}
struct Reserved {
    transport: Transport,
    id: u64,
    actual: usize,
}
impl Drop for Reserved {
    fn drop(&mut self) {
        lock(&self.transport.0.budget).finish(self.id, self.actual);
    }
}
impl InterceptorBuilder for Transport {
    fn build(&self, _: &str) -> Result<Arc<dyn Interceptor + Send + Sync>> {
        Ok(Arc::new(self.clone()))
    }
}
struct MediaWriter {
    parent: Writer,
    transport: Transport,
}
struct FeedbackReader {
    parent: Arc<dyn RTCPReader + Send + Sync>,
    transport: Transport,
}
#[async_trait]
impl RTCPReader for FeedbackReader {
    async fn read(
        &self,
        buf: &mut [u8],
        attributes: &Attributes,
    ) -> Result<(
        Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>>,
        Attributes,
    )> {
        let (packets, attributes) = self.parent.read(buf, attributes).await?;
        for packet in &packets {
            if let Some(feedback) = packet.as_any().downcast_ref::<TransportLayerCc>() {
                self.transport.feedback(feedback);
            }
        }
        Ok((packets, attributes))
    }
}
#[async_trait]
impl RTPWriter for MediaWriter {
    async fn write(&self, packet: &Packet, attributes: &Attributes) -> Result<usize> {
        let size = self
            .transport
            .write(&self.parent, packet, attributes)
            .await?;
        Ok(size)
    }
}
#[async_trait]
impl Interceptor for Transport {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(FeedbackReader {
            parent: reader,
            transport: self.clone(),
        })
    }
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }
    async fn bind_local_stream(&self, info: &StreamInfo, writer: Writer) -> Writer {
        for ext in &info.rtp_header_extensions {
            let index = match ext.uri.as_str() {
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time" => 0,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-timing" => 1,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay" => 2,
                "urn:ietf:params:rtp-hdrext:toffset" => 3,
                _ => continue,
            };
            if (1..=255).contains(&ext.id) {
                lock(&self.0.timing_ids)[index] = ext.id as u8;
            }
        }
        if let Some(extension) = info.rtp_header_extensions.iter().find(|e| {
            e.uri == "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
        }) {
            if (1..=255).contains(&extension.id) {
                self.0.tcc_id.store(extension.id as u64, Ordering::Relaxed);
            }
        }
        if info.mime_type.eq_ignore_ascii_case("video/rs-fec-cm256")
            && info.associated_stream.is_some()
        {
            lock(&self.0.history).fec = Some(Bound {
                info: info.clone(),
                writer: writer.clone(),
            });
            *lock(&self.0.fec) = Some(super::fec::Sender::new(info));
            return writer;
        }
        if info.mime_type.eq_ignore_ascii_case("video/rtx") {
            if info.associated_stream.is_some() {
                lock(&self.0.history).rtx = Some(Bound {
                    info: info.clone(),
                    writer: writer.clone(),
                });
            }
            return writer;
        }
        if info.mime_type.eq_ignore_ascii_case("video/h264")
            || info.mime_type.eq_ignore_ascii_case("video/h265")
        {
            lock(&self.0.history).primary = Some(info.ssrc);
            return Arc::new(MediaWriter {
                parent: writer,
                transport: self.clone(),
            });
        }
        writer
    }
    async fn unbind_local_stream(&self, info: &StreamInfo) {
        let mut history = lock(&self.0.history);
        if history.primary == Some(info.ssrc) {
            history.primary = None;
            history.packets.clear();
            history.order.clear();
            history.bytes = 0;
        }
        if history
            .rtx
            .as_ref()
            .is_some_and(|bound| bound.info.ssrc == info.ssrc)
        {
            history.rtx = None;
        }
        if history
            .fec
            .as_ref()
            .is_some_and(|bound| bound.info.ssrc == info.ssrc)
        {
            history.fec = None;
            drop(history);
            *lock(&self.0.fec) = None;
            lock(&self.0.fec_blocks).clear();
        }
    }
    async fn bind_remote_stream(
        &self,
        _: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }
    async fn unbind_remote_stream(&self, _: &StreamInfo) {}
    async fn close(&self) -> Result<()> {
        self.0.closed.store(true, Ordering::Release);
        self.0.cancel.cancel();
        lock(&self.0.budget).reset(false);
        lock(&self.0.fec_blocks).clear();
        *lock(&self.0.fec) = None;
        let mut history = lock(&self.0.history);
        history.rtx = None;
        history.fec = None;
        history.primary = None;
        history.packets.clear();
        history.order.clear();
        history.bytes = 0;
        Ok(())
    }
}
