//! Native WebRTC controller transport.
//!
//! The peer owns ICE, TURN, DTLS, SRTP and SCTP. Signaling transport remains in
//! `signal.rs`; video RTP is reordered and assembled into complete Annex-B
//! frames for the in-process native viewer; Opus audio has one connection-owned
//! output shared by every screen window.

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::configure_twcc_receiver_with_builder;
use webrtc::api::media_engine::{
    MIME_TYPE_G722, MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS, MIME_TYPE_PCMA, MIME_TYPE_PCMU,
    MIME_TYPE_TELEPHONE_EVENT, MediaEngine,
};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::{RTCDataChannelInit, RTCDataChannelPriority};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::interceptor::report::receiver::ReceiverReport;
use webrtc::interceptor::report::sender::SenderReport as SenderReportInterceptor;
use webrtc::interceptor::twcc::receiver::Receiver as TransportFeedbackReceiver;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::{
    TransportLayerNack, nack_pairs_from_sequence_numbers,
};
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::stats::StatsReportType;
use webrtc::track::track_local::{TrackLocal, track_local_static_rtp::TrackLocalStaticRTP};
use webrtc::util::marshal::{Marshal, Unmarshal};
use webrtc_srtp::session::IncomingRtpPacket;

use crate::flexfec::FlexFecReceiver;
use crate::media::{ConnectionMediaProfile, TransportChoice, VideoCodec};
use crate::official_receiver::{
    OfficialVideoReceiver, ReceiverResult, VideoCodecKind, VideoHeaderExtensions,
};
use crate::performance::PerformanceMonitor;
use crate::rsfec::{RsFecConfig, RsFecReceiver, normalize_rtx_source};
use crate::rtcp_timing::{DEFAULT_RECEIVER_SSRC, RtcpTiming};
use crate::rtp_capture::RtpCaptureBuilder;
use crate::stream_control::{
    OutgoingControlMessage, PbMessageSource, StreamControlHandle, encode_pb_echo_request,
};
use crate::ulpfec::UlpfecReceiver;
use crate::uu_kcp::UuKcpControl;

const PLAYOUT_DELAY_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";
const VIDEO_CONTENT_TYPE_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-content-type";
const VIDEO_CAPTURE_INDEX_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index";
pub(crate) const VIDEO_IS_NEW_FRAME_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame";
const VIDEO_TIMING_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-timing";
const VIDEO_FRAME_SENDING_DELAY_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay";
const RTP_STREAM_ID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";
const REPAIRED_RTP_STREAM_ID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id";

pub const DATA_CHANNEL_LABELS: [&str; 5] = [
    "CONTROL_DATA_CHANNEL",
    "TEXT_DATA_CHANNEL",
    "STREAMER_DATA_CHANNEL",
    "FILE_DATA_CHANNEL",
    "BINARY_DATA_CHANNEL",
];

#[derive(Clone, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl From<IceServer> for RTCIceServer {
    fn from(value: IceServer) -> Self {
        Self {
            urls: value.urls,
            username: value.username,
            credential: value.credential,
        }
    }
}

pub struct NativePeer {
    connection: Arc<RTCPeerConnection>,
    connection_states: Mutex<mpsc::UnboundedReceiver<RTCPeerConnectionState>>,
    local_candidate_tx: broadcast::Sender<Option<RTCIceCandidateInit>>,
    last_pli: Arc<Mutex<Option<Instant>>>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    rtcp_timing: RtcpTiming,
    p2p_only: AtomicBool,
    rtp_capture: Option<RtpCaptureBuilder>,
    ice_servers: Vec<IceServer>,
    data_channels: DataChannels,
    uu_kcp: UuKcpControl,
    video_tracks: VideoTrackRegistry,
}

#[derive(Clone)]
struct DataChannels {
    _local_channels: Arc<Vec<Arc<RTCDataChannel>>>,
    incoming_channels: Arc<std::sync::Mutex<Vec<Arc<RTCDataChannel>>>>,
    performance: PerformanceMonitor,
    streamer_sender_started: Arc<AtomicBool>,
    stream_control: StreamControlHandle,
    uu_kcp: UuKcpControl,
    workers: Arc<SessionWorkers>,
}

struct SessionWorkers {
    shutdown: CancellationToken,
    tasks: StdMutex<Option<Vec<tokio::task::JoinHandle<()>>>>,
    closing: Mutex<()>,
}

impl SessionWorkers {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shutdown: CancellationToken::new(),
            tasks: StdMutex::new(Some(Vec::new())),
            closing: Mutex::new(()),
        })
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        let _ = self.spawn_in(self.shutdown.clone(), future);
    }

    fn spawn_in(
        &self,
        scope: CancellationToken,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Option<tokio::task::AbortHandle> {
        let mut tasks = std_mutex_lock(&self.tasks);
        if let Some(tasks) = tasks.as_mut() {
            let shutdown = self.shutdown.clone();
            let task = tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {},
                    _ = scope.cancelled() => {},
                    _ = future => {},
                }
            });
            let abort = task.abort_handle();
            tasks.push(task);
            Some(abort)
        } else {
            None
        }
    }

    async fn close(&self) {
        self.shutdown.cancel();
        let _closing = self.closing.lock().await;
        let tasks = std_mutex_lock(&self.tasks).take().unwrap_or_default();
        let count = tasks.len();
        for task in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "peer worker terminated unexpectedly");
            }
        }
        tracing::debug!(count, "all owned peer workers joined");
    }
}

impl Drop for SessionWorkers {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl DataChannels {
    fn new(
        local_channels: Vec<Arc<RTCDataChannel>>,
        performance: PerformanceMonitor,
        profile: ConnectionMediaProfile,
        uu_kcp: UuKcpControl,
        connection: Weak<RTCPeerConnection>,
    ) -> Self {
        let workers = SessionWorkers::new();
        let streamer_sender_started = Arc::new(AtomicBool::new(false));
        let (stream_control, control_messages, echo_responses) =
            StreamControlHandle::new(profile, performance.clone());
        for channel in &local_channels {
            Self::install_handlers(
                channel,
                performance.clone(),
                Arc::clone(&streamer_sender_started),
                stream_control.clone(),
                true,
                uu_kcp.clone(),
                &workers,
            );
        }
        let control_channel = local_channels
            .iter()
            .find(|channel| channel.label() == "CONTROL_DATA_CHANNEL")
            .expect("the official data-channel set always includes CONTROL_DATA_CHANNEL")
            .clone();
        let text_channel = local_channels
            .iter()
            .find(|channel| channel.label() == "TEXT_DATA_CHANNEL")
            .expect("the official data-channel set always includes TEXT_DATA_CHANNEL")
            .clone();
        workers.spawn(send_official_control_messages(
            control_channel,
            text_channel,
            control_messages,
            echo_responses,
            stream_control.clone(),
            uu_kcp.clone(),
            connection,
        ));
        Self {
            _local_channels: Arc::new(local_channels),
            incoming_channels: Arc::new(std::sync::Mutex::new(Vec::new())),
            performance,
            streamer_sender_started,
            stream_control,
            uu_kcp,
            workers,
        }
    }

    fn install_handlers(
        channel: &Arc<RTCDataChannel>,
        performance: PerformanceMonitor,
        streamer_sender_started: Arc<AtomicBool>,
        stream_control: StreamControlHandle,
        local_channel: bool,
        uu_kcp: UuKcpControl,
        workers: &Arc<SessionWorkers>,
    ) {
        if !local_channel && channel.label() == "CONTROL_DATA_CHANNEL" {
            uu_kcp.set_control_stream(channel.id(), true);
        }
        let label = channel.label().to_owned();
        let stats_channel = Arc::downgrade(channel);
        let stats_performance = performance.clone();
        let open_stream_control = stream_control.clone();
        let open_kcp = uu_kcp.clone();
        let open_workers = Arc::downgrade(workers);
        channel.on_open(Box::new(move || {
            let label = label.clone();
            let stats_channel = stats_channel.clone();
            let stats_performance = stats_performance.clone();
            let streamer_sender_started = Arc::clone(&streamer_sender_started);
            let stream_control = open_stream_control.clone();
            let uu_kcp = open_kcp.clone();
            Box::pin(async move {
                let Some(stats_channel) = stats_channel.upgrade() else {
                    return;
                };
                tracing::info!(%label, stream_id = stats_channel.id(), "UU data channel opened");
                if label == "CONTROL_DATA_CHANNEL" {
                    uu_kcp.set_control_stream(stats_channel.id(), true);
                }
                if local_channel
                    && matches!(label.as_str(), "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL")
                {
                    stream_control.set_data_channel_open(&label, true);
                }
                if label == "STREAMER_DATA_CHANNEL"
                    && streamer_sender_started
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    && let Some(workers) = open_workers.upgrade()
                {
                    workers.spawn(send_official_streamer_statistics(
                        stats_channel,
                        stats_performance,
                    ));
                }
            })
        }));

        let label = channel.label().to_owned();
        let close_stream_control = stream_control.clone();
        let closed_channel = Arc::downgrade(channel);
        channel.on_close(Box::new(move || {
            let label = label.clone();
            let stream_control = close_stream_control.clone();
            let closed_channel = closed_channel.clone();
            let uu_kcp = uu_kcp.clone();
            Box::pin(async move {
                tracing::info!(%label, "UU data channel closed");
                if label == "CONTROL_DATA_CHANNEL"
                    && let Some(channel) = closed_channel.upgrade()
                {
                    uu_kcp.set_control_stream(channel.id(), false);
                }
                if local_channel
                    && matches!(label.as_str(), "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL")
                {
                    stream_control.set_data_channel_open(&label, false);
                }
            })
        }));

        let label = channel.label().to_owned();
        channel.on_error(Box::new(move |error| {
            let label = label.clone();
            Box::pin(async move {
                tracing::warn!(%label, %error, "UU data channel error");
            })
        }));

        let label = channel.label().to_owned();
        let message_stream_control = stream_control;
        channel.on_message(Box::new(move |message| {
            let label = label.clone();
            let stream_control = message_stream_control.clone();
            Box::pin(async move {
                if label == "STREAMER_DATA_CHANNEL" {
                    // Peer media_inbounds describe the peer's receive direction.
                    // This read-only client has no UU video sender. Do not use
                    // those reports as measurements of our local video pipeline.
                    tracing::trace!(bytes = message.data.len(), "peer receiver statistics not used by read-only viewer");
                } else if matches!(
                    label.as_str(),
                    "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL"
                ) && let Err(error) = stream_control.handle_protocol_message(&message.data,
                    if label == "CONTROL_DATA_CHANNEL" { PbMessageSource::Control } else { PbMessageSource::Text })
                {
                    tracing::warn!(
                        %label,
                        %error,
                        bytes = message.data.len(),
                        is_string = message.is_string,
                        "invalid UU protobuf domain message"
                    );
                }
                tracing::trace!(%label, bytes = message.data.len(), "UU data channel message received");
            })
        }));
    }

    fn attach_remote_channel(&self, channel: Arc<RTCDataChannel>) {
        let label = channel.label().to_owned();
        tracing::info!(%label, "remote UU data channel received");
        Self::install_handlers(
            &channel,
            self.performance.clone(),
            Arc::clone(&self.streamer_sender_started),
            self.stream_control.clone(),
            false,
            self.uu_kcp.clone(),
            &self.workers,
        );
        std_mutex_lock(&self.incoming_channels).push(channel);
    }
}

async fn send_official_control_messages(
    control_channel: Arc<RTCDataChannel>,
    text_channel: Arc<RTCDataChannel>,
    mut messages: mpsc::UnboundedReceiver<OutgoingControlMessage>,
    mut echo_responses: mpsc::UnboundedReceiver<Vec<u8>>,
    stream_control: StreamControlHandle,
    uu_kcp: UuKcpControl,
    connection: Weak<RTCPeerConnection>,
) {
    const PB_CONNECT_INTERVAL: Duration = Duration::from_millis(500);
    const PB_CONNECT_MAX_ATTEMPTS: u32 = 30;
    let notifications = stream_control.protocol_notifications();
    let mut generation = None;
    let mut attempts = 0_u32;
    let mut timer: Option<tokio::time::Interval> = None;
    loop {
        let changed = notifications.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let status = stream_control.handshake_status();
        if generation != Some(status.generation) {
            generation = Some(status.generation);
            attempts = 0;
            timer = status.open.then(|| {
                // D4A070 dispatches once immediately, then every 500 ms.
                let mut timer = tokio::time::interval(PB_CONNECT_INTERVAL);
                timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                timer
            });
        }
        if !status.open || status.connected {
            timer = None;
        }
        if control_channel.ready_state() == RTCDataChannelState::Closed
            && text_channel.ready_state() == RTCDataChannelState::Closed
        {
            break;
        }
        tokio::select! {
            _ = &mut changed => continue,
            _ = async {
                if let Some(timer) = timer.as_mut() { timer.tick().await; }
                else { std::future::pending::<()>().await; }
            } => {
                // The original tick checks the active peer state before
                // consuming an attempt. A disconnected transport is not an
                // unanswered ECHO request.
                match connection.upgrade().map(|peer| peer.connection_state()) {
                    Some(RTCPeerConnectionState::Connected) => {}
                    None | Some(RTCPeerConnectionState::Closed) => break,
                    _ => continue,
                }
                if attempts >= PB_CONNECT_MAX_ATTEMPTS {
                    timer = None;
                    stream_control.mark_pb_handshake_timeout();
                    tracing::warn!(attempts, "protobuf handshake timed out; no feature version was negotiated");
                    continue;
                }
                attempts += 1;
                match send_control_message(&uu_kcp, &control_channel, encode_pb_echo_request()).await {
                    Ok(bytes) => tracing::debug!(attempt = attempts, bytes, "sent official protobuf ECHO_REQUEST"),
                    Err(error) => tracing::warn!(attempt = attempts, %error, "protobuf ECHO_REQUEST send failed"),
                }
            }
            message = messages.recv() => {
                let Some(message) = message else { break; };
                let payload = Bytes::from(message.payload);
                let is_screen_request = message.completion.is_some();
                match text_channel.send_text_bytes(&payload).await {
                    Ok(bytes) => {
                        if let Some(done) = message.completion { let _ = done.send(Ok(())); }
                        tracing::info!(sequence = message.sequence, bytes, is_screen_request, protocol = message.protocol.label(), "viewing control request sent");
                    }
                    Err(error) => {
                        let description = error.to_string();
                        if let Some(done) = message.completion { let _ = done.send(Err(description.clone())); }
                        if !is_screen_request { stream_control.mark_send_failed(message.sequence, &description); }
                        tracing::warn!(sequence = message.sequence, %error, "runtime capture-setting request send failed");
                    }
                }
            }
            response = echo_responses.recv() => {
                let Some(response) = response else { break; };
                match send_control_message(&uu_kcp, &control_channel, response).await {
                    Ok(bytes) => tracing::debug!(bytes, "sent official protobuf ECHO_RESPONSE"),
                    Err(error) => tracing::warn!(%error, "protobuf ECHO_RESPONSE send failed"),
                }
            }
        }
    }
}

async fn send_control_message(
    uu_kcp: &UuKcpControl,
    control_channel: &RTCDataChannel,
    payload: Vec<u8>,
) -> Result<usize> {
    ensure!(
        control_channel.ready_state() == RTCDataChannelState::Open,
        "UU CONTROL data channel is not open"
    );
    if uu_kcp.is_negotiated() {
        uu_kcp.send(control_channel.id(), payload).await
    } else {
        control_channel
            .send(&Bytes::from(payload))
            .await
            .map_err(anyhow::Error::from)
    }
}

async fn send_official_streamer_statistics(
    channel: Arc<RTCDataChannel>,
    performance: PerformanceMonitor,
) {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Some(payload) = build_official_streamer_statistics(&performance) else {
            continue;
        };
        let serialized = match serde_json::to_string(&payload) {
            Ok(serialized) => serialized,
            Err(error) => {
                tracing::warn!(%error, "serialize official STREAMER statistics failed");
                return;
            }
        };
        let bytes = serialized.len();
        tracing::debug!(%serialized, "local video receiver period statistics");
        if let Err(error) = channel.send_text(serialized).await {
            tracing::debug!(%error, "STREAMER statistics sender stopped");
            return;
        }
        tracing::debug!(
            bytes,
            "sent official 30-second connection_period_stats_event"
        );
    }
}

fn build_official_streamer_statistics(
    performance: &PerformanceMonitor,
) -> Option<serde_json::Value> {
    let tracks = performance.video_tracks();
    if !tracks.is_empty() {
        let records: Vec<_> = tracks
            .iter()
            .filter_map(build_official_streamer_statistics)
            .flat_map(|value| {
                value["connection_period_stats_event"]["media_inbounds"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        return Some(
            serde_json::json!({"connection_period_stats_event":{"media_inbounds":records}}),
        );
    }
    let snapshot = performance.snapshot();
    let video_track_index = snapshot.video_track_index?;
    let (packets_received, packets_lost) = performance.streamer_period_totals();
    let mut record = serde_json::Map::new();
    record.insert(
        "video_track_index".into(),
        serde_json::json!(video_track_index),
    );
    record.insert(
        "renderer_impl".into(),
        serde_json::json!("OpenUUYC native viewer"),
    );
    record.insert("decoder_impl".into(), serde_json::json!(snapshot.decoder));

    // The official decoder reads these counters as u32. Keep the UI's wider
    // cumulative counters, but serialize the declared wire representation.
    for (field, value) in [
        ("total_packets_received", packets_received),
        ("total_packets_lost", packets_lost),
        ("total_received_frames", snapshot.total_received_frames),
        ("total_decoded_frames", snapshot.total_decoded_frames),
        (
            "total_actual_rendered_frames",
            snapshot.total_actual_rendered_frames,
        ),
        (
            "total_key_frames_decoded",
            snapshot.total_key_frames_decoded,
        ),
    ] {
        record.insert(field.into(), serde_json::json!(value as u32));
    }
    for (field, value) in [
        (
            "total_retransmitted_packets_recovered",
            snapshot.rtx_packets_accepted,
        ),
        (
            "total_retransmitted_packets_received",
            snapshot.rtx_packets_received,
        ),
        (
            "total_fec_packets_recovered",
            snapshot.fec_packets_recovered,
        ),
        ("total_fec_packets_received", snapshot.fec_packets_received),
        ("total_small_jank_count", snapshot.small_jank_count),
        ("total_jank_count", snapshot.jank_count),
        ("total_big_jank_count", snapshot.big_jank_count),
    ] {
        record.insert(field.into(), serde_json::json!(value));
    }
    for (field, value) in [
        ("avg_decode_fps", snapshot.decode_fps),
        ("avg_render_fps", snapshot.render_fps),
        ("avg_received_fps", snapshot.receive_fps),
    ] {
        insert_stat_integer(&mut record, field, Some(value), i32::MAX as u64);
    }

    if let Some(stats) = &snapshot.pipeline_stats {
        for (field, value) in [
            ("streamer_avg_actual_fps", stats.source_fps),
            ("streamer_avg_actual_received_fps", Some(stats.received_fps)),
            (
                "avg_assembly_ms_interval",
                stats.assembly.map(|v| v.average_ms),
            ),
            ("avg_decode_ms_interval", stats.decode.map(|v| v.average_ms)),
        ] {
            insert_stat_integer(&mut record, field, value, i32::MAX as u64);
        }
        for (field, value) in [
            ("max_assembly_ms_interval", stats.assembly.map(|v| v.max_ms)),
            ("max_decode_ms_interval", stats.decode.map(|v| v.max_ms)),
            (
                "streamer_avg_sending_delay_interval",
                stats.sending.map(|v| v.average_ms),
            ),
            (
                "streamer_max_sending_delay_interval",
                stats.sending.map(|v| v.max_ms),
            ),
            // frame_total_delay is processing + sending + measured RTT, NOT
            // the clock-correlated E2E sample. Do not fabricate its period
            // percentiles from a different distribution.
            (
                "streamer_frame_capture_delay_ms_p50",
                stats.capture.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_capture_delay_ms_p90",
                stats.capture.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_encode_delay_ms_p50",
                stats.encode.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_encode_delay_ms_p90",
                stats.encode.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_pacer_delay_ms_p50",
                stats.pacer.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_pacer_delay_ms_p90",
                stats.pacer.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_transport_delay_ms_p50",
                stats.transport.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_transport_delay_ms_p90",
                stats.transport.map(|v| v.p90_ms),
            ),
            // The extended frame-timing distribution contains measured
            // (flags & 4) frames only. Our all-frame local assembly/decode
            // diagnostics are already reported above under their own names;
            // they must not impersonate that separate measured distribution.
            (
                "streamer_frame_e2e_delay_ms_avg",
                stats.e2e.map(|v| v.average_ms),
            ),
            (
                "streamer_frame_e2e_delay_ms_p50",
                stats.e2e.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_e2e_delay_ms_p90",
                stats.e2e.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_e2e_delay_ms_p99",
                stats.e2e.map(|v| v.p99_ms),
            ),
        ] {
            insert_stat_integer(&mut record, field, value, u32::MAX as u64);
        }
    }
    // Missing fields keep the official defaults. In particular an unmeasured
    // pre/after-switch sample is not a measured zero-loss/zero-latency sample.
    Some(serde_json::json!({"connection_period_stats_event": {"media_inbounds": [record]}}))
}

fn insert_stat_integer(
    record: &mut serde_json::Map<String, serde_json::Value>,
    field: &str,
    value: Option<f64>,
    maximum: u64,
) {
    if let Some(value) = value.filter(|v| v.is_finite() && *v >= 0.0 && *v <= maximum as f64) {
        record.insert(field.into(), serde_json::json!(value.trunc() as u64));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardedTrack {
    pub kind: MediaKind,
    pub id: String,
    pub codec: String,
    pub payload_type: u8,
    pub ssrc: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpForwardConfig {
    /// `None` registers every negotiated video track for screen selection.
    pub video_track_id: Option<String>,
}

pub struct RtpForwarder {
    stop: CancellationToken,
    announcements: mpsc::UnboundedReceiver<ForwardedTrack>,
    forwarding_started: watch::Sender<bool>,
    tracks: VideoTrackRegistry,
    selected_video: Option<Arc<VideoTrackSource>>,
}

#[derive(Clone, Default)]
pub(crate) struct VideoTrackRegistry {
    entries: Arc<StdMutex<HashMap<i32, Arc<VideoTrackSource>>>>,
    changed: Arc<tokio::sync::Notify>,
}

pub(crate) struct VideoTrackSource {
    pub metadata: ForwardedTrack,
    pub index: i32,
    pub performance: PerformanceMonitor,
    sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    pub feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    started: watch::Sender<bool>,
    keyframes: watch::Receiver<u64>,
    last_pli: Arc<Mutex<Option<Instant>>>,
    nack_rtt_micros: Arc<AtomicU64>,
}

impl VideoTrackRegistry {
    pub(crate) fn get(&self, index: i32) -> Option<Arc<VideoTrackSource>> {
        std_mutex_lock(&self.entries).get(&index).cloned()
    }

    fn all(&self) -> Vec<Arc<VideoTrackSource>> {
        std_mutex_lock(&self.entries).values().cloned().collect()
    }
}

impl VideoTrackSource {
    pub(crate) async fn add_sink(&self, sink: VideoFrameSink) {
        self.sinks.lock().await.push(sink);
    }

    pub(crate) fn start(&self) {
        self.started.send_replace(true);
    }
}

impl Drop for RtpForwarder {
    fn drop(&mut self) {
        // Join handles remain with NativePeer until its explicit close, even
        // when the media consumer disappears before the signaling owner.
        self.stop.cancel();
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum VideoReceiverFeedback {
    DecoderFinished {
        frame_id: i64,
        result: crate::decoder_result::VideoDecodeResult,
    },
    DecodeTiming {
        duration: Duration,
        finished_at: Instant,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct EncodedVideoFrame {
    pub frame_id: i64,
    pub data: Bytes,
    pub rtp_timestamp: u32,
    pub received_at: Instant,
    pub assembled_at: Instant,
    pub keyframe: bool,
    pub rotation: u16,
    pub content_type: u8,
    pub video_capture_index: Option<u16>,
    pub is_new_picture: Option<bool>,
    pub sender_timing: FrameSenderTiming,
    pub codec: VideoCodec,
    pub parameter_format: Option<crate::video_format::VideoFormatSignature>,
    pub color_space: Option<crate::video_color::VideoColorSpace>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameSenderTiming {
    pub capture_at: Option<Instant>,
    pub capture_delay: Option<Duration>,
    pub encode_delay: Option<Duration>,
    pub pacer_delay: Option<Duration>,
    pub sending_delay: Option<Duration>,
    pub transport_delay: Option<Duration>,
}

#[derive(Clone)]
pub(crate) enum VideoFrameSink {
    Unbounded {
        sender: mpsc::UnboundedSender<EncodedVideoFrame>,
        wake: std::thread::Thread,
    },
}

impl VideoFrameSink {
    fn send(&self, frame: EncodedVideoFrame) -> bool {
        match self {
            Self::Unbounded { sender, wake } => {
                let sent = sender.send(frame).is_ok();
                if sent {
                    wake.unpark();
                }
                sent
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PlayoutDelay {
    pub min: Duration,
    pub max: Duration,
}

impl RtpForwarder {
    pub async fn next_track(&mut self) -> Option<ForwardedTrack> {
        let track = self.announcements.recv().await?;
        if track.kind == MediaKind::Video && self.selected_video.is_none() {
            self.selected_video = track
                .id
                .strip_prefix("video_")
                .and_then(|id| id.parse().ok())
                .and_then(|index| self.tracks.get(index));
        }
        Some(track)
    }

    pub fn video_keyframe_generation(&self) -> u64 {
        self.selected_video
            .as_ref()
            .map_or(0, |track| *track.keyframes.borrow())
    }

    pub async fn wait_for_video_keyframe_after(&mut self, generation: u64, wait: Duration) -> bool {
        let Some(track) = &self.selected_video else {
            return false;
        };
        let mut video_keyframes = track.keyframes.clone();
        tokio::time::timeout(wait, async {
            loop {
                if *video_keyframes.borrow() > generation {
                    return true;
                }
                if video_keyframes.changed().await.is_err() {
                    return false;
                }
                video_keyframes.borrow_and_update();
            }
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) async fn add_video_sink(&self, sink: VideoFrameSink) {
        self.selected_video
            .as_ref()
            .expect("video selected before attaching sink")
            .add_sink(sink)
            .await;
    }

    pub(crate) fn video_receiver_feedback(&self) -> mpsc::UnboundedSender<VideoReceiverFeedback> {
        self.selected_video
            .as_ref()
            .expect("video selected before attaching decoder")
            .feedback
            .clone()
    }

    pub fn start(&self) {
        self.forwarding_started.send_replace(true);
        if let Some(track) = &self.selected_video {
            track.start();
        }
    }
}

const RTP_MAX_PACKET_AGE: u16 = 10_000;
const RTP_MAX_NACK_PACKETS: usize = 1_000;
const RTP_NACK_PROCESS_INTERVAL: Duration = Duration::from_millis(20);
const RTP_DEFAULT_RTT: Duration = Duration::from_millis(50);
const RTP_NACK_MAX_RETRIES: u8 = 17;
const RTP_NACK_BACKOFF_BASE: f64 = 1.25;
const RTP_NACK_BACKOFF_START: Duration = Duration::from_millis(50);
const PLI_MIN_INTERVAL: Duration = Duration::from_millis(200);
const KEYFRAME_PACKET_WINDOW: Duration = Duration::from_millis(200);
const ACTIVE_STREAM_WINDOW: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum NackFilter {
    Sequence,
    Time,
}

struct NackEntry {
    sequence_number: u16,
    send_at_sequence_number: u16,
    created_at: Instant,
    sent_at: Option<Instant>,
    retries: u8,
    retries_because_of_sequence: u8,
    retries_because_of_rtt: u8,
}

#[derive(Default)]
struct NackBatch {
    sequences: Vec<u16>,
    request_keyframe: bool,
}

#[derive(Default)]
struct RtcpFeedbackBuffer {
    request_keyframe: bool,
    nack_sequences: Vec<u16>,
}

impl RtcpFeedbackBuffer {
    fn buffer(&mut self, batch: NackBatch) {
        self.request_keyframe |= batch.request_keyframe;
        self.nack_sequences.extend(batch.sequences);
    }

    async fn flush(
        &mut self,
        connection: &RTCPeerConnection,
        last_pli: &Mutex<Option<Instant>>,
        media_ssrc: u32,
    ) {
        let request_keyframe = std::mem::take(&mut self.request_keyframe);
        let nack_sequences = std::mem::take(&mut self.nack_sequences);
        if request_keyframe {
            if let Err(error) =
                send_picture_loss_indication(connection, last_pli, media_ssrc, false).await
            {
                tracing::warn!(%error, "buffered keyframe request failed");
            }
        } else if !nack_sequences.is_empty()
            && let Err(error) =
                send_transport_layer_nack(connection, media_ssrc, &nack_sequences).await
        {
            tracing::warn!(%error, count = nack_sequences.len(), "buffered RTCP NACK send failed");
        }
    }
}

struct NackReceiveResult {
    batch: NackBatch,
    nack_count: u8,
}

struct NackRequester {
    initialized: bool,
    newest_sequence_number: u16,
    rtt: Duration,
    entries: HashMap<u16, NackEntry>,
    keyframes: Vec<u16>,
    recovered: Vec<u16>,
    final_lost_packets: u64,
}

fn parse_playout_delay(payload: &[u8]) -> Option<PlayoutDelay> {
    let [first, second, third] = payload else {
        return None;
    };
    let min_units = (u16::from(*first) << 4) | (u16::from(*second) >> 4);
    let max_units = (u16::from(*second & 0x0f) << 8) | u16::from(*third);
    if min_units > max_units {
        return None;
    }
    Some(PlayoutDelay {
        min: Duration::from_millis(u64::from(min_units) * 10),
        max: Duration::from_millis(u64::from(max_units) * 10),
    })
}

struct TrackForwardContext {
    workers: Weak<SessionWorkers>,
    stop: CancellationToken,
    kind: MediaKind,
    codec: String,
    codec_fmtp: String,
    video_payload_codecs: HashMap<u8, (String, String)>,
    rtx_payload_apt: HashMap<u8, u8>,
    red_payload_types: HashSet<u8>,
    ulpfec_payload_types: HashSet<u8>,
    flexfec_payload_types: HashSet<u8>,
    rsfec_config: Option<RsFecConfig>,
    extmap_allow_mixed: bool,
    audio: crate::audio::AudioPlayback,
    audio_generation: Option<u64>,
    connection: Arc<RTCPeerConnection>,
    last_pli: Arc<Mutex<Option<Instant>>>,
    video_annexb_sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    remote_ntp: Arc<StdMutex<RemoteNtpEstimator>>,
    receiver_feedback: Option<mpsc::UnboundedReceiver<VideoReceiverFeedback>>,
}

#[derive(Clone, Copy)]
struct RtcpClockMeasurement {
    unwrapped_rtp: i64,
    remote_ntp_ms: f64,
}

struct RemoteNtpEstimator {
    anchor: Instant,
    last_unwrapped_rtp: Option<i64>,
    measurements: VecDeque<RtcpClockMeasurement>,
    clock_offsets_ms: VecDeque<f64>,
    consecutive_invalid: u8,
    mapping_established: bool,
}

impl RemoteNtpEstimator {
    const MAX_MEASUREMENTS: usize = 20;
    const MAX_RTP_JUMP: i64 = 1 << 25;
    const MAX_NTP_JUMP_MS: f64 = 60.0 * 60.0 * 1_000.0;
    const MAX_INVALID_SAMPLES: u8 = 3;

    fn new(now: Instant) -> Self {
        Self {
            anchor: now,
            last_unwrapped_rtp: None,
            measurements: VecDeque::with_capacity(Self::MAX_MEASUREMENTS),
            clock_offsets_ms: VecDeque::with_capacity(Self::MAX_MEASUREMENTS),
            consecutive_invalid: 0,
            mapping_established: false,
        }
    }

    fn update(
        &mut self,
        ntp_time: u64,
        rtp_timestamp: u32,
        rtt: Duration,
        received_at: Instant,
    ) -> bool {
        if ntp_time == 0 {
            return false;
        }
        let unwrapped_rtp = unwrap_rtp_timestamp(self.last_unwrapped_rtp, rtp_timestamp);
        let remote_ntp_ms = ntp_to_millis(ntp_time);
        if self.measurements.iter().any(|measurement| {
            measurement.unwrapped_rtp == unwrapped_rtp || measurement.remote_ntp_ms == remote_ntp_ms
        }) {
            return false;
        }

        let invalid = self.measurements.front().is_some_and(|latest| {
            remote_ntp_ms <= latest.remote_ntp_ms
                || remote_ntp_ms - latest.remote_ntp_ms > Self::MAX_NTP_JUMP_MS
                || unwrapped_rtp <= latest.unwrapped_rtp
                || unwrapped_rtp - latest.unwrapped_rtp > Self::MAX_RTP_JUMP
        });
        if invalid {
            self.consecutive_invalid = self.consecutive_invalid.saturating_add(1);
            if self.consecutive_invalid < Self::MAX_INVALID_SAMPLES {
                return false;
            }
            self.measurements.clear();
            self.clock_offsets_ms.clear();
            self.last_unwrapped_rtp = None;
            self.mapping_established = false;
        }
        self.consecutive_invalid = 0;
        self.last_unwrapped_rtp = Some(unwrapped_rtp);
        let local_arrival_ms = received_at
            .saturating_duration_since(self.anchor)
            .as_secs_f64()
            * 1_000.0;
        let sender_arrival_ms = remote_ntp_ms + rtt.as_secs_f64() * 500.0;
        tracing::trace!(target: "openuuyc::rtc::clock", ntp_time, rtp_timestamp,
            unwrapped_rtp, remote_ntp_ms, local_arrival_ms, rtt_ms = rtt.as_secs_f64() * 1000.0,
            invalid_reset = invalid, "RTCP clock mapping sample");
        self.measurements.push_front(RtcpClockMeasurement {
            unwrapped_rtp,
            remote_ntp_ms,
        });
        self.clock_offsets_ms
            .push_back(local_arrival_ms - sender_arrival_ms);
        if self.measurements.len() > Self::MAX_MEASUREMENTS {
            self.measurements.pop_back();
        }
        if self.clock_offsets_ms.len() > Self::MAX_MEASUREMENTS {
            self.clock_offsets_ms.pop_front();
        }
        let established = self.measurements.len() >= 2;
        let became_established = established && !self.mapping_established;
        self.mapping_established = established;
        became_established
    }

    fn estimate(&self, rtp_timestamp: u32) -> Option<Instant> {
        if self.measurements.len() < 2 || self.clock_offsets_ms.len() < 2 {
            return None;
        }
        let latest = self.measurements.front()?;
        let unwrapped = unwrap_rtp_timestamp(Some(latest.unwrapped_rtp), rtp_timestamp);
        let count = self.measurements.len() as f64;
        let base_x = latest.unwrapped_rtp as f64;
        let base_y = latest.remote_ntp_ms;
        let average_x = self
            .measurements
            .iter()
            .map(|measurement| measurement.unwrapped_rtp as f64 - base_x)
            .sum::<f64>()
            / count;
        let average_y = self
            .measurements
            .iter()
            .map(|measurement| measurement.remote_ntp_ms - base_y)
            .sum::<f64>()
            / count;
        let (variance, covariance) =
            self.measurements
                .iter()
                .fold((0.0, 0.0), |(variance, covariance), measurement| {
                    let x = measurement.unwrapped_rtp as f64 - base_x - average_x;
                    let y = measurement.remote_ntp_ms - base_y - average_y;
                    (variance + x * x, covariance + x * y)
                });
        if variance.abs() < 1.0e-8 {
            return None;
        }
        let slope = covariance / variance;
        let remote_capture_ms =
            base_y + average_y + (unwrapped as f64 - base_x - average_x) * slope;
        let mut offsets = self.clock_offsets_ms.iter().copied().collect::<Vec<_>>();
        offsets.sort_by(f64::total_cmp);
        let clock_offset_ms = offsets[offsets.len() / 2];
        add_signed_millis(self.anchor, remote_capture_ms + clock_offset_ms)
    }
}

fn unwrap_rtp_timestamp(previous: Option<i64>, timestamp: u32) -> i64 {
    previous.map_or(i64::from(timestamp), |previous| {
        previous + i64::from(timestamp.wrapping_sub(previous as u32) as i32)
    })
}

fn ntp_to_millis(ntp_time: u64) -> f64 {
    let seconds = (ntp_time >> 32) as u32;
    let fractions = ntp_time as u32;
    f64::from(seconds) * 1_000.0 + f64::from(fractions) * (1_000.0 / 4_294_967_296.0)
}

fn add_signed_millis(anchor: Instant, milliseconds: f64) -> Option<Instant> {
    if !milliseconds.is_finite() {
        return None;
    }
    if milliseconds >= 0.0 {
        anchor.checked_add(Duration::from_secs_f64(milliseconds / 1_000.0))
    } else {
        anchor.checked_sub(Duration::from_secs_f64(-milliseconds / 1_000.0))
    }
}

async fn observe_remote_ntp(
    receiver: Arc<RTCRtpReceiver>,
    media_ssrc: u32,
    estimator: Arc<StdMutex<RemoteNtpEstimator>>,
    rtcp_timing: RtcpTiming,
) {
    while receiver.read_rtcp().await.is_ok() {
        if let Some((report, rtt)) = rtcp_timing.fresh_sender_clock(media_ssrc) {
            let mapping_established = estimator
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .update(report.ntp_time, report.rtp_time, rtt, report.received_at);
            if mapping_established {
                tracing::info!(media_ssrc, "remote RTP-to-local-NTP mapping established");
            }
        }
    }
    tracing::debug!(media_ssrc, "remote RTCP sender-report stream ended");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoIngressOrigin {
    Primary,
    Rtx,
    RsFec,
}

struct VideoIngressPacket {
    ordinal: u64,
    received_at: Instant,
    wire_bytes: usize,
    origin: VideoIngressOrigin,
    packet: RtpPacket,
    raw: Bytes,
}

#[derive(Clone, Copy, Debug, Default)]
struct MediaRecoveryFlags {
    recovered_from_rtx: bool,
    recovered_by_fec: bool,
}

struct PendingMediaPacket {
    packet: RtpPacket,
    flags: MediaRecoveryFlags,
    received_at: Instant,
    rsfec_source: Option<Bytes>,
}

struct OrderedVideoIngress {
    receiver: broadcast::Receiver<IncomingRtpPacket>,
    media_ssrc: u32,
    rtx_ssrc: u32,
    fec_ssrc: u32,
}

impl OrderedVideoIngress {
    async fn open(track: &webrtc::track::track_remote::TrackRemote) -> Result<Self> {
        let receiver = track
            .subscribe_incoming_rtp()
            .await
            .context("subscribe to ordered decrypted RTP ingress")?;
        let (media_ssrc, rtx_ssrc, fec_ssrc) = track
            .associated_ssrcs()
            .await
            .context("resolve associated media/RTX/FEC SSRCs")?;
        Ok(Self {
            receiver,
            media_ssrc,
            rtx_ssrc,
            fec_ssrc,
        })
    }

    async fn recv(&mut self) -> Result<VideoIngressPacket> {
        loop {
            let incoming = match self.receiver.recv().await {
                Ok(incoming) => incoming,
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    // A lagged observation subscriber must not tear down the media
                    // session. The SRTP session and its per-SSRC readers continue to
                    // receive packets; the missing sequence range is reported to the
                    // receiver state machine below and recovered with NACK/PLI just
                    // like packets lost on the network.
                    tracing::warn!(
                        dropped,
                        "ordered RTP ingress subscriber lagged; continuing from the oldest available packet"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    bail!("ordered RTP ingress closed")
                }
            };
            let wire_bytes = incoming.data.len();
            let mut raw = incoming.data.as_ref();
            let packet = match RtpPacket::unmarshal(&mut raw) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::debug!(%error, ordinal = incoming.ordinal, wire_bytes,
                        "dropping malformed decrypted RTP before demux");
                    continue;
                }
            };
            let origin = if packet.header.ssrc == self.media_ssrc {
                VideoIngressOrigin::Primary
            } else if self.rtx_ssrc != 0 && packet.header.ssrc == self.rtx_ssrc {
                VideoIngressOrigin::Rtx
            } else if self.fec_ssrc != 0 && packet.header.ssrc == self.fec_ssrc {
                VideoIngressOrigin::RsFec
            } else {
                continue;
            };
            return Ok(VideoIngressPacket {
                ordinal: incoming.ordinal,
                received_at: incoming.received_at,
                wire_bytes,
                origin,
                packet,
                raw: incoming.data,
            });
        }
    }
}

// Per-track abort handles; the peer owns and joins the actual task handles.
struct VideoInterceptorDrainers(Vec<tokio::task::AbortHandle>);

impl VideoInterceptorDrainers {
    fn start(
        track: Arc<webrtc::track::track_remote::TrackRemote>,
        rtx_ssrc: u32,
        fec_ssrc: u32,
        workers: &SessionWorkers,
        stop: &CancellationToken,
    ) -> Self {
        let mut tasks = Vec::with_capacity(3);
        let media_track = Arc::clone(&track);
        tasks.extend(workers.spawn_in(stop.clone(), async move {
            while media_track.read_rtp().await.is_ok() {}
        }));
        if rtx_ssrc != 0 {
            let rtx_track = Arc::clone(&track);
            tasks.extend(workers.spawn_in(stop.clone(), async move {
                while rtx_track.read_rtx().await.is_ok() {}
            }));
        }
        if fec_ssrc != 0 {
            tasks.extend(workers.spawn_in(stop.clone(), async move {
                while track.read_fec().await.is_ok() {}
            }));
        }
        Self(tasks)
    }
}

impl Drop for VideoInterceptorDrainers {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl NackRequester {
    fn new() -> Self {
        Self {
            initialized: false,
            newest_sequence_number: 0,
            rtt: RTP_DEFAULT_RTT,
            entries: HashMap::new(),
            keyframes: Vec::new(),
            recovered: Vec::new(),
            final_lost_packets: 0,
        }
    }

    fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    fn on_received(
        &mut self,
        sequence_number: u16,
        starts_keyframe: bool,
        recovered: bool,
    ) -> NackReceiveResult {
        let now = Instant::now();
        if !self.initialized {
            self.initialized = true;
            self.newest_sequence_number = sequence_number;
            if starts_keyframe {
                self.keyframes.push(sequence_number);
            }
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count: 0,
            };
        }
        if sequence_number == self.newest_sequence_number {
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count: 0,
            };
        }
        if sequence_ahead_of(self.newest_sequence_number, sequence_number) {
            let nack_count = self
                .entries
                .remove(&sequence_number)
                .map_or(0, |entry| entry.retries);
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count,
            };
        }

        if starts_keyframe && !self.keyframes.contains(&sequence_number) {
            self.keyframes.push(sequence_number);
        }
        Self::erase_before_cutoff(&mut self.keyframes, sequence_number);
        if recovered {
            if !self.recovered.contains(&sequence_number) {
                self.recovered.push(sequence_number);
            }
            Self::erase_before_cutoff(&mut self.recovered, sequence_number);
            return NackReceiveResult {
                batch: NackBatch::default(),
                nack_count: 0,
            };
        }

        let missing_start = self.newest_sequence_number.wrapping_add(1);
        let request_keyframe = self.add_missing(missing_start, sequence_number);
        self.newest_sequence_number = sequence_number;
        let mut batch = self.get_batch(NackFilter::Sequence, now);
        batch.request_keyframe |= request_keyframe;
        NackReceiveResult {
            batch,
            nack_count: 0,
        }
    }

    fn process(&mut self) -> NackBatch {
        self.get_batch(NackFilter::Time, Instant::now())
    }

    fn clear_up_to(&mut self, sequence_number: u16) {
        self.entries
            .retain(|sequence, _| sequence_ahead_or_at(*sequence, sequence_number));
        self.keyframes
            .retain(|sequence| sequence_ahead_or_at(*sequence, sequence_number));
        self.recovered
            .retain(|sequence| sequence_ahead_or_at(*sequence, sequence_number));
    }

    fn add_missing(&mut self, start: u16, end: u16) -> bool {
        let before_age_cleanup = self.entries.len();
        let cutoff = end.wrapping_sub(RTP_MAX_PACKET_AGE);
        self.entries
            .retain(|sequence, _| sequence_ahead_or_at(*sequence, cutoff));
        self.final_lost_packets = self
            .final_lost_packets
            .saturating_add((before_age_cleanup - self.entries.len()) as u64);
        let count = usize::from(end.wrapping_sub(start));
        while self.entries.len().saturating_add(count) > RTP_MAX_NACK_PACKETS
            && self.remove_until_keyframe()
        {}
        if self.entries.len().saturating_add(count) > RTP_MAX_NACK_PACKETS {
            let existing = self.entries.len();
            self.final_lost_packets = self.final_lost_packets.saturating_add(existing as u64);
            self.entries.clear();
            tracing::warn!(
                max_packets = RTP_MAX_NACK_PACKETS,
                existing,
                missing_start = start,
                received_sequence = end,
                apparent_gap = count,
                "NACK list full; clearing it and requesting keyframe"
            );
            return true;
        }
        let mut sequence = start;
        while sequence != end {
            if !self.recovered.contains(&sequence) {
                self.entries.entry(sequence).or_insert(NackEntry {
                    sequence_number: sequence,
                    send_at_sequence_number: sequence,
                    created_at: Instant::now(),
                    sent_at: None,
                    retries: 0,
                    retries_because_of_sequence: 0,
                    retries_because_of_rtt: 0,
                });
            }
            sequence = sequence.wrapping_add(1);
        }
        false
    }

    fn remove_until_keyframe(&mut self) -> bool {
        self.keyframes.sort_unstable_by(sequence_order);
        self.keyframes.dedup();
        while let Some(keyframe) = self.keyframes.first().copied() {
            let before = self.entries.len();
            self.entries
                .retain(|sequence, _| sequence_ahead_or_at(*sequence, keyframe));
            if self.entries.len() != before {
                self.final_lost_packets = self
                    .final_lost_packets
                    .saturating_add((before - self.entries.len()) as u64);
                return true;
            }
            self.keyframes.remove(0);
        }
        false
    }

    fn get_batch(&mut self, filter: NackFilter, now: Instant) -> NackBatch {
        let newest = self.newest_sequence_number;
        let rtt = self.rtt;
        let mut sequences = Vec::new();
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            let retry_delay = nack_retry_delay(rtt, entry.retries);
            let should_send = match filter {
                NackFilter::Sequence => {
                    entry.sent_at.is_none()
                        && sequence_ahead_or_at(newest, entry.send_at_sequence_number)
                }
                NackFilter::Time => entry
                    .sent_at
                    .is_none_or(|sent_at| now.duration_since(sent_at) >= retry_delay),
            };
            if should_send {
                sequences.push(entry.sequence_number);
                entry.retries = entry.retries.saturating_add(1);
                match filter {
                    NackFilter::Sequence => {
                        entry.retries_because_of_sequence =
                            entry.retries_because_of_sequence.saturating_add(1);
                    }
                    NackFilter::Time => {
                        entry.retries_because_of_rtt =
                            entry.retries_because_of_rtt.saturating_add(1);
                    }
                }
                entry.sent_at = Some(now);
            }
            let keep = entry.retries < RTP_NACK_MAX_RETRIES;
            if !keep {
                tracing::warn!(
                    sequence_number = entry.sequence_number,
                    current_size = before,
                    in_nack_list_ms = now.duration_since(entry.created_at).as_millis(),
                    rtt_ms = rtt.as_secs_f64() * 1000.0,
                    retries_because_of_sequence = entry.retries_because_of_sequence,
                    retries_because_of_rtt = entry.retries_because_of_rtt,
                    "sequence number removed from NACK list due to max retries"
                );
            }
            keep
        });
        self.final_lost_packets = self
            .final_lost_packets
            .saturating_add((before - self.entries.len()) as u64);
        sequences.sort_unstable_by(sequence_order);
        NackBatch {
            sequences,
            request_keyframe: false,
        }
    }

    fn erase_before_cutoff(sequences: &mut Vec<u16>, newest: u16) {
        let cutoff = newest.wrapping_sub(RTP_MAX_PACKET_AGE);
        sequences.retain(|sequence| sequence_ahead_or_at(*sequence, cutoff));
    }

    fn outstanding(&self) -> usize {
        self.entries.len()
    }

    fn final_lost_packets(&self) -> u64 {
        self.final_lost_packets
    }
}

fn sequence_ahead_of(newer: u16, older: u16) -> bool {
    let distance = newer.wrapping_sub(older);
    if distance == 0x8000 {
        newer > older
    } else {
        distance != 0 && distance < 0x8000
    }
}

fn sequence_ahead_or_at(newer: u16, older: u16) -> bool {
    newer == older || sequence_ahead_of(newer, older)
}

fn sequence_order(left: &u16, right: &u16) -> std::cmp::Ordering {
    if left == right {
        std::cmp::Ordering::Equal
    } else if sequence_ahead_of(*left, *right) {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Less
    }
}

fn nack_retry_delay(rtt: Duration, retries: u8) -> Duration {
    // 1D84C3..1D8510: RTT rounded to milliseconds, exponential retry term
    // truncated to milliseconds; compare their minimum, not sub-ms floats.
    let rtt_ms = ((rtt.as_micros() + 500) / 1_000) as u64;
    let backoff_ms = (RTP_NACK_BACKOFF_START.as_millis() as f64
        * RTP_NACK_BACKOFF_BASE.powi(i32::from(retries))) as u64;
    Duration::from_millis(rtt_ms.min(backoff_ms))
}

impl Default for RtpForwardConfig {
    fn default() -> Self {
        Self {
            video_track_id: Some("video_0".to_owned()),
        }
    }
}

async fn sample_network_performance(
    connection: Arc<RTCPeerConnection>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    rtcp_timing: RtcpTiming,
    stream_control: StreamControlHandle,
    tracks: VideoTrackRegistry,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pipeline_sample = 0_u8;
    let mut last_candidate_pair = None;
    loop {
        interval.tick().await;
        let mut selected_candidate_ids = None;
        let mut selected_route = None;
        if let Some(pair) = connection
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await
        {
            let route = connection_route(
                pair.local.typ,
                &pair.local.address,
                pair.remote.typ,
                &pair.remote.address,
            );
            let candidate_pair = format!(
                "{}:{} -> {}:{} ({route}; {}/{}; relay {}/{})",
                pair.local.address,
                pair.local.port,
                pair.remote.address,
                pair.remote.port,
                pair.local.protocol,
                pair.remote.protocol,
                pair.local.relay_protocol,
                pair.remote.relay_protocol
            );
            if last_candidate_pair.as_ref() != Some(&candidate_pair) {
                tracing::info!(
                    local_type = %pair.local.typ,
                    remote_type = %pair.remote.typ,
                    local_protocol = %pair.local.protocol,
                    remote_protocol = %pair.remote.protocol,
                    local_generation = pair.local.generation,
                    remote_generation = pair.remote.generation,
                    local_network_id = pair.local.network_id,
                    remote_network_id = pair.remote.network_id,
                    local_network_cost = pair.local.network_cost,
                    remote_network_cost = pair.remote.network_cost,
                    local_relay_protocol = %pair.local.relay_protocol,
                    remote_relay_protocol = %pair.remote.relay_protocol,
                    pair = %candidate_pair,
                    "selected ICE candidate pair changed"
                );
                last_candidate_pair = Some(candidate_pair);
            }
            tracing::trace!(
                local_type = %pair.local.typ,
                local_addr = %format_args!("{}:{}", pair.local.address, pair.local.port),
                remote_type = %pair.remote.typ,
                remote_addr = %format_args!("{}:{}", pair.remote.address, pair.remote.port),
                local_network_id = pair.local.network_id,
                remote_network_id = pair.remote.network_id,
                local_network_cost = pair.local.network_cost,
                remote_network_cost = pair.remote.network_cost,
                route,
                "selected ICE candidate pair"
            );
            performance.set_connection(format!(
                "{} {route}",
                pair.local.protocol.to_string().to_uppercase()
            ));
            selected_candidate_ids = Some((pair.local.stats_id, pair.remote.stats_id));
            selected_route = Some(route);
        }
        let mut delay_seconds = None;
        let rtcp_rtt = rtcp_timing.publish_rtt();
        for report in connection.get_stats().await.reports.into_values() {
            match report {
                StatsReportType::CandidatePair(pair)
                    if selected_candidate_ids
                        .as_ref()
                        .is_some_and(|(local_id, remote_id)| {
                            pair.local_candidate_id == *local_id
                                && pair.remote_candidate_id == *remote_id
                        }) =>
                {
                    let current = pair.current_round_trip_time;
                    let average = (pair.responses_received != 0)
                        .then(|| pair.total_round_trip_time / pair.responses_received as f64);
                    let candidate = (current.is_finite() && current >= 0.0)
                        .then_some(current)
                        .or(average.filter(|value| value.is_finite() && *value >= 0.0));
                    if candidate.is_some() {
                        delay_seconds = candidate;
                    }
                }
                StatsReportType::LocalCandidate(stats)
                    if selected_route == Some("relay")
                        && selected_candidate_ids
                            .as_ref()
                            .is_some_and(|(local_id, _)| stats.id == *local_id) =>
                {
                    let transport = match stats.relay_protocol.as_str() {
                        "tls" => "TURNS",
                        "tcp" => "TCP",
                        "udp" => "UDP",
                        _ => "TURN",
                    };
                    performance.set_connection(format!("{transport} relay"));
                }
                _ => {}
            }
        }
        performance.set_measured_media_rtt(rtcp_rtt);
        for track in tracks.all() {
            let measured = rtcp_timing.rtt_for(track.metadata.ssrc);
            track.performance.set_measured_media_rtt(measured);
            if let Some(rtt) = measured {
                track.nack_rtt_micros.store(
                    rtt.as_micros().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
            }
        }
        if let Some(delay) = rtcp_rtt.or_else(|| delay_seconds.map(Duration::from_secs_f64)) {
            performance.set_current_delay(delay);
        }
        if let Some(rtt) = rtcp_rtt {
            nack_rtt_micros.store(
                rtt.as_micros().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
        }
        stream_control.observe_budget(
            &performance,
            rtcp_rtt,
            last_candidate_pair.as_deref(),
            selected_candidate_ids.is_some()
                && connection.connection_state() == RTCPeerConnectionState::Connected,
        );
        pipeline_sample = pipeline_sample.wrapping_add(1);
        if pipeline_sample.is_multiple_of(5) {
            let snapshot = performance.snapshot();
            let pipeline = snapshot.pipeline_stats.as_ref();
            tracing::debug!(
                local_current_ms = format_args!("{:.1}", snapshot.local_frame_delay_ms),
                local_average_ms = format_args!("{:.1}", snapshot.local_frame_delay_average_ms),
                local_p95_ms = format_args!("{:.1}", snapshot.local_frame_delay_p95_ms),
                assembly_ms = format_args!("{:.1}", snapshot.assembly_delay_ms),
                input_queue_ms = format_args!("{:.1}", snapshot.input_queue_delay_ms),
                decode_pipeline_ms = format_args!("{:.1}", snapshot.decode_pipeline_delay_ms),
                surface_ms = format_args!("{:.1}", snapshot.surface_transfer_delay_ms),
                present_wait_ms = format_args!("{:.1}", snapshot.present_wait_delay_ms),
                render_queue_ms = format_args!("{:.1}", snapshot.render_queue_delay_ms),
                ingress_queue = snapshot.ingress_queue_packets,
                outstanding_nacks = snapshot.outstanding_nacks,
                final_loss_percent = format_args!("{:.2}", snapshot.packet_loss_percent),
                predecode_drops = snapshot.predecode_dropped_frames,
                decoder_queue = snapshot.decoder_queue_frames,
                presentation_queue = snapshot.presentation_queue_frames,
                receive_fps = format_args!("{:.1}", snapshot.receive_fps),
                decode_fps = format_args!("{:.1}", snapshot.decode_fps),
                render_fps = format_args!("{:.1}", snapshot.render_fps),
                actual_fps = format_args!("{:.1}", snapshot.actual_fps),
                actual_frames = snapshot.total_actual_rendered_frames,
                marked_frames = snapshot.total_marked_rendered_frames,
                sender_capture_p50_ms = pipeline.and_then(|p| p.capture).map(|p| p.p50_ms),
                sender_encode_p50_ms = pipeline.and_then(|p| p.encode).map(|p| p.p50_ms),
                sender_pacer_p50_ms = pipeline.and_then(|p| p.pacer).map(|p| p.p50_ms),
                sender_total_average_ms = pipeline.and_then(|p| p.sending).map(|p| p.average_ms),
                transport_p50_ms = pipeline.and_then(|p| p.transport).map(|p| p.p50_ms),
                e2e_average_ms = pipeline.and_then(|p| p.e2e).map(|p| p.average_ms),
                e2e_p90_ms = pipeline.and_then(|p| p.e2e).map(|p| p.p90_ms),
                "media pipeline snapshot"
            );
        }
        if matches!(
            connection.connection_state(),
            RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
        ) {
            break;
        }
    }
}

fn connection_route(
    local_type: RTCIceCandidateType,
    local_address: &str,
    remote_type: RTCIceCandidateType,
    remote_address: &str,
) -> &'static str {
    if local_type == RTCIceCandidateType::Relay || remote_type == RTCIceCandidateType::Relay {
        "relay"
    } else if (private_route_address(local_address) && private_route_address(remote_address))
        || (local_type == RTCIceCandidateType::Host
            && remote_type == RTCIceCandidateType::Host
            && same_global_ipv6_prefix(local_address, remote_address))
    {
        "LAN"
    } else {
        // Official D66EA0 requires same-/64 global IPv6 for public host/host.
        // Candidate origin alone never establishes LAN.
        "P2P"
    }
}

fn same_global_ipv6_prefix(local: &str, remote: &str) -> bool {
    let (Ok(local), Ok(remote)) = (
        local.parse::<std::net::Ipv6Addr>(),
        remote.parse::<std::net::Ipv6Addr>(),
    ) else {
        return false;
    };
    let local = local.octets();
    let remote = remote.octets();
    local[0] & 0xe0 == 0x20 && remote[0] & 0xe0 == 0x20 && local[..8] == remote[..8]
}

fn private_route_address(address: &str) -> bool {
    let Ok(address) = address.parse::<IpAddr>() else {
        return false;
    };
    match address {
        IpAddr::V4(address) => {
            address.is_private()
                || address.is_link_local()
                || address.is_loopback()
                || address.octets()[0] == 0
                || address.octets()[0] >= 224
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unicast_link_local()
                || address.segments()[0] & 0xfe00 == 0xfc00
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_private() || v4.is_link_local() || v4.is_loopback())
        }
    }
}

fn candidate_is_relay(candidate: &str) -> bool {
    candidate
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|fields| {
            fields[0].eq_ignore_ascii_case("typ") && fields[1].eq_ignore_ascii_case("relay")
        })
}

fn remove_relay_candidates_from_sdp(sdp: &str) -> String {
    let separator = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut filtered = sdp
        .lines()
        .filter(|line| !line.starts_with("a=candidate:") || !candidate_is_relay(line))
        .collect::<Vec<_>>()
        .join(separator);
    if sdp.ends_with(separator) {
        filtered.push_str(separator);
    }
    filtered
}

// Local send-controller bootstrap, not an incoming video bitrate limit.
// UU's DataRate parser treats the factory trial's bare start:8100 as kbps.
// This read-only client has no outgoing RTP media/probes or encoder/pacer
// pushback: available => configured bootstrap, unavailable => zero. A future
// media sender must supply its real evolving estimate instead of this projection.
const UU_LOCAL_SEND_START_BPS: u64 = 8_100_000;

fn uu_transport_feedback_interval(send_bitrate_bps: u64) -> Duration {
    // 1DFE68: a 68-byte feedback budget at 5% of the send-side estimate,
    // bounded by the configured 50..250 ms interval (100 ms before an update).
    let feedback_bps = send_bitrate_bps / 20;
    let micros = (68_u64 * 8 * 1_000_000)
        .checked_div(feedback_bps)
        .unwrap_or(250_000)
        .clamp(50_000, 250_000);
    Duration::from_micros(micros)
}

impl NativePeer {
    pub async fn new(ice_servers: Vec<IceServer>, transport: TransportChoice) -> Result<Self> {
        let profile = crate::media::ConnectionMediaOptions::default()
            .resolve(crate::media::LocalDisplayInfo::FALLBACK)?;
        Self::new_with_profile(ice_servers, transport, profile).await
    }

    pub(crate) async fn new_with_profile(
        mut ice_servers: Vec<IceServer>,
        transport: TransportChoice,
        profile: ConnectionMediaProfile,
    ) -> Result<Self> {
        let configured_ice_servers = ice_servers.clone();
        let mut ice_url_schemes = BTreeMap::<String, usize>::new();
        for url in ice_servers.iter().flat_map(|server| &server.urls) {
            let scheme = url
                .split_once(':')
                .map_or("unknown", |(scheme, _)| scheme)
                .to_ascii_lowercase();
            *ice_url_schemes.entry(scheme).or_default() += 1;
        }
        if transport == TransportChoice::P2p {
            for server in &mut ice_servers {
                server
                    .urls
                    .retain(|url| url.to_ascii_lowercase().starts_with("stun:"));
            }
            ice_servers.retain(|server| !server.urls.is_empty());
        }
        tracing::debug!(
            ?transport,
            ice_server_count = ice_servers.len(),
            ?ice_url_schemes,
            "creating native ICE transport"
        );
        let mut media_engine = MediaEngine::default();
        register_uu_codecs(&mut media_engine)?;
        register_uu_header_extensions(&mut media_engine)?;
        let mut registry = Registry::new();
        let rtp_capture = RtpCaptureBuilder::from_environment()?;
        if let Some(capture) = &rtp_capture {
            registry.add(Box::new(capture.clone()));
        }
        let rtcp_timing = RtcpTiming::new();
        // Register before RR: the report worker must write through the XR
        // adapter, not capture the unwrapped transport writer.
        registry.add(Box::new(rtcp_timing.clone()));
        registry.add(Box::new(
            ReceiverReport::builder()
                .with_jittered_media_intervals(Duration::from_secs(1), Duration::from_secs(5)),
        ));
        registry.add(Box::new(SenderReportInterceptor::builder()));
        let (feedback_interval_tx, feedback_interval_rx) =
            watch::channel(Duration::from_millis(100));
        let registry = configure_twcc_receiver_with_builder(
            registry,
            &mut media_engine,
            TransportFeedbackReceiver::builder().with_interval_updates(feedback_interval_rx),
        )
        .context("register WebRTC transport feedback interceptors")?;
        let mut setting_engine = SettingEngine::default();
        // UU SrtpSession::SetKey uses a 1024-packet replay window. SRTCP
        // has an independent index/window policy and is not changed here.
        setting_engine.set_srtp_replay_protection_window(1024);
        setting_engine.set_continual_gathering(true);
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        let connection = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers: ice_servers.into_iter().map(Into::into).collect(),
                ice_transport_policy: if transport == TransportChoice::Relay {
                    RTCIceTransportPolicy::Relay
                } else {
                    RTCIceTransportPolicy::All
                },
                ..Default::default()
            })
            .await
            .context("create native peer connection")?,
        );
        let (local_candidate_tx, _) = broadcast::channel(256);
        let local_candidate_tx_for_handler = local_candidate_tx.clone();
        connection.on_ice_candidate(Box::new(move |candidate| {
            let candidate = candidate.and_then(|candidate| candidate.to_json().ok());
            let _ = local_candidate_tx_for_handler.send(candidate);
            Box::pin(async {})
        }));
        let performance = PerformanceMonitor::new("自动");
        let nack_rtt_micros = Arc::new(AtomicU64::new(
            RTP_DEFAULT_RTT.as_micros().min(u128::from(u64::MAX)) as u64,
        ));
        let transceiver = |direction| RTCRtpTransceiverInit {
            direction,
            send_encodings: Vec::new(),
        };
        let receive_only = || transceiver(RTCRtpTransceiverDirection::Recvonly);
        // The official controller always exposes an `audio_0` Opus sender,
        // even when no microphone samples are produced. A bare sendrecv
        // transceiver omits the MSID/SSRC block and the controlled client does
        // not treat that offer as the desktop-controller profile.
        let audio_track: Arc<dyn TrackLocal + Send + Sync> = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;stereo=1;useinbandfec=1".to_owned(),
                ..Default::default()
            },
            "audio_0".to_owned(),
            "audio_0".to_owned(),
        ));
        connection
            .add_transceiver_from_track(
                audio_track,
                Some(transceiver(RTCRtpTransceiverDirection::Sendrecv)),
            )
            .await
            .context("add official audio_0 send/receive transceiver")?;
        // The official controller offers five recvonly video m-lines. They are
        // not simulcast layers of one transceiver; the host answers each one
        // independently and uses them for its desktop/auxiliary video tracks.
        for index in 0..5 {
            connection
                .add_transceiver_from_kind(RTPCodecType::Video, Some(receive_only()))
                .await
                .with_context(|| format!("add receive video transceiver {index}"))?;
        }

        let mut local_channels = Vec::with_capacity(DATA_CHANNEL_LABELS.len());
        for label in DATA_CHANNEL_LABELS {
            let channel = connection
                .create_data_channel(label, Some(official_data_channel_init(label)))
                .await
                .with_context(|| format!("create {label}"))?;
            local_channels.push(channel);
        }
        let uu_kcp = UuKcpControl::default();
        let video_tracks = VideoTrackRegistry::default();
        let data_channels = DataChannels::new(
            local_channels,
            performance.clone(),
            profile,
            uu_kcp.clone(),
            Arc::downgrade(&connection),
        );
        data_channels.workers.spawn(sample_network_performance(
            Arc::clone(&connection),
            performance.clone(),
            Arc::clone(&nack_rtt_micros),
            rtcp_timing.clone(),
            data_channels.stream_control.clone(),
            video_tracks.clone(),
        ));
        let data_channels_for_remote = data_channels.clone();
        connection.on_data_channel(Box::new(move |channel| {
            data_channels_for_remote.attach_remote_channel(channel);
            Box::pin(async {})
        }));

        let (connection_state_tx, connection_states) = mpsc::unbounded_channel();
        connection.on_peer_connection_state_change(Box::new(move |state| {
            let connection_state_tx = connection_state_tx.clone();
            // 18A25C/18B44F -> 18874E -> 149E08 -> 1DFE68. An initial
            // estimate exists even without outgoing media. Unknown/Connecting
            // retain the last interval; a usable or lost secure transport updates it.
            let send_bitrate_bps = match state {
                RTCPeerConnectionState::Connected => Some(UU_LOCAL_SEND_START_BPS),
                RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed => Some(0),
                _ => None,
            };
            if let Some(bitrate) = send_bitrate_bps {
                let interval = uu_transport_feedback_interval(bitrate);
                if feedback_interval_tx.send_if_modified(|current| {
                    let changed = *current != interval;
                    *current = interval;
                    changed
                }) {
                    tracing::debug!(
                        send_bitrate_bps = bitrate,
                        interval_ms = interval.as_millis(),
                        "updated UU read-only transport feedback budget"
                    );
                }
            }
            Box::pin(async move {
                tracing::debug!(%state, "WebRTC peer state changed");
                let _ = connection_state_tx.send(state);
            })
        }));
        connection.on_ice_connection_state_change(Box::new(move |state| {
            Box::pin(async move {
                tracing::debug!(%state, "ICE connection state changed");
            })
        }));
        connection.on_ice_gathering_state_change(Box::new(move |state| {
            Box::pin(async move {
                tracing::debug!(%state, "ICE gathering state changed");
            })
        }));
        connection.on_signaling_state_change(Box::new(move |state| {
            Box::pin(async move {
                tracing::debug!(%state, "WebRTC signaling state changed");
            })
        }));

        Ok(Self {
            connection,
            connection_states: Mutex::new(connection_states),
            local_candidate_tx,
            last_pli: Arc::new(Mutex::new(None)),
            performance,
            nack_rtt_micros,
            rtcp_timing,
            rtp_capture,
            p2p_only: AtomicBool::new(transport == TransportChoice::P2p),
            ice_servers: configured_ice_servers,
            data_channels,
            uu_kcp,
            video_tracks,
        })
    }

    pub fn performance_monitor(&self) -> PerformanceMonitor {
        self.performance.clone()
    }

    pub(crate) fn video_tracks(&self) -> VideoTrackRegistry {
        self.video_tracks.clone()
    }

    pub(crate) fn spawn_viewing_task(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        self.data_channels.workers.spawn(task);
    }

    pub(crate) fn stream_control_handle(&self) -> StreamControlHandle {
        self.data_channels.stream_control.clone()
    }

    pub(crate) fn set_stream_video_stream(&self, codec: VideoCodec, video_track_index: i32) {
        self.select_video_statistics(video_track_index);
        self.data_channels
            .stream_control
            .set_video_stream(codec, video_track_index);
    }

    pub(crate) fn select_viewed_video_track(&self, video_track_index: i32) {
        self.select_video_statistics(video_track_index);
        self.data_channels
            .stream_control
            .select_viewed_video_track(video_track_index);
    }

    fn select_video_statistics(&self, video_track_index: i32) {
        if let Some(track) = self.video_tracks.get(video_track_index) {
            self.rtcp_timing.select_video(track.metadata.ssrc);
        }
        if let Ok(index) = u64::try_from(video_track_index) {
            self.performance.set_active_video_track(index);
        }
    }

    pub(crate) fn allows_network_switch(&self) -> bool {
        !self.p2p_only.load(Ordering::Acquire)
    }

    pub(crate) fn configure_network_control(
        &self,
        transport: TransportChoice,
        server_forced: bool,
    ) {
        let has_turns = self
            .ice_servers
            .iter()
            .flat_map(|server| &server.urls)
            .any(|url| url.to_ascii_lowercase().contains("turns:"));
        self.stream_control_handle().network_control().configure(
            transport == TransportChoice::Relay,
            server_forced,
            has_turns,
        );
    }

    pub(crate) fn accept_manual_network_policy(&self) {
        self.p2p_only.store(false, Ordering::Release);
    }

    pub(crate) async fn selected_transport(&self) -> Option<(bool, String, String)> {
        let pair = self
            .connection
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await?;
        let is_relay = pair.local.typ == RTCIceCandidateType::Relay
            || pair.remote.typ == RTCIceCandidateType::Relay;
        Some((
            is_relay,
            pair.local.relay_protocol,
            pair.remote.relay_protocol,
        ))
    }

    pub(crate) async fn selected_route_details(&self) -> Option<String> {
        let pair = self
            .connection
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await?;
        let route = connection_route(
            pair.local.typ,
            &pair.local.address,
            pair.remote.typ,
            &pair.remote.address,
        );
        let protocol = pair.local.protocol.to_string().to_uppercase();
        let relay = if route == "relay" {
            let relay_protocol = if !pair.local.relay_protocol.is_empty() {
                pair.local.relay_protocol.as_str()
            } else {
                pair.remote.relay_protocol.as_str()
            };
            if relay_protocol.is_empty() {
                String::new()
            } else {
                format!(" / {}", relay_protocol.to_uppercase())
            }
        } else {
            String::new()
        };
        Some(format!(
            "{route} · {protocol}{relay} · {}:{} → {}:{} · {:?}/{:?}",
            pair.local.address,
            pair.local.port,
            pair.remote.address,
            pair.remote.port,
            pair.local.typ,
            pair.remote.typ
        ))
    }

    pub(crate) async fn switch_ice_network(
        &self,
        transport_type: u8,
        attempt_switch_type: u8,
    ) -> Result<()> {
        if transport_type == 0 {
            bail!("unsupported official ICE transport type {transport_type}");
        }
        let tls_only = transport_type != 3 && attempt_switch_type == 2;
        let selected_servers = if tls_only {
            self.ice_servers
                .iter()
                .filter_map(|server| {
                    let mut selected = server.clone();
                    selected
                        .urls
                        .retain(|url| url.to_ascii_lowercase().contains("turns:"));
                    (!selected.urls.is_empty()).then_some(selected)
                })
                .collect::<Vec<_>>()
        } else {
            self.ice_servers.clone()
        };
        if selected_servers.is_empty() && transport_type != 3 {
            bail!(
                "control ACK did not provide a {} server",
                if tls_only { "TURNS" } else { "usable ICE" }
            );
        }
        let mut configuration = self.connection.get_configuration().await;
        configuration.ice_transport_policy = if transport_type == 3 {
            RTCIceTransportPolicy::All
        } else {
            RTCIceTransportPolicy::Relay
        };
        configuration.ice_servers = selected_servers.into_iter().map(Into::into).collect();
        self.connection
            .set_configuration(configuration)
            .await
            .context("apply official relay ICE configuration")?;
        tracing::info!(
            transport_type,
            attempt_switch_type,
            relay_transport = if transport_type == 3 {
                "automatic"
            } else if tls_only {
                "TURNS only"
            } else {
                "relay, UDP preferred"
            },
            "official ICE network switch configuration applied"
        );
        Ok(())
    }

    pub async fn create_offer(&self) -> Result<String> {
        let offer = self
            .connection
            .create_offer(None)
            .await
            .context("create controller SDP offer")?;
        let mut sdp = offer.sdp.clone();
        self.connection
            .set_local_description(offer)
            .await
            .context("install controller SDP offer")?;
        // The upstream WebRTC stack validates that SetLocalDescription receives
        // the byte-for-byte offer it generated. UU's transport attributes are
        // signaling extensions, not SDP state consumed by the local ICE/DTLS
        // implementation, so append them only to the wire copy afterwards.
        apply_uu_application_attributes(&mut sdp)?;
        Ok(sdp)
    }

    pub async fn create_restart_offer(&self) -> Result<String> {
        let offer = self
            .connection
            .create_offer(Some(RTCOfferOptions {
                ice_restart: true,
                ..Default::default()
            }))
            .await
            .context("create controller ICE-restart offer")?;
        let mut sdp = offer.sdp.clone();
        self.connection
            .set_local_description(offer)
            .await
            .context("install controller ICE-restart offer")?;
        apply_uu_application_attributes(&mut sdp)?;
        Ok(sdp)
    }

    pub fn local_ice_candidates(&self) -> broadcast::Receiver<Option<RTCIceCandidateInit>> {
        self.local_candidate_tx.subscribe()
    }

    pub async fn set_remote_answer(&self, mut sdp: String, restart_ice: bool) -> Result<()> {
        tracing::debug!(restart_ice, "installing remote SDP answer");
        let mixed_kcp_version = negotiated_mixed_kcp_version(&sdp);
        if self.p2p_only.load(Ordering::Acquire) {
            sdp = remove_relay_candidates_from_sdp(&sdp);
        }
        let answer = RTCSessionDescription::answer(sdp).context("parse remote SDP answer")?;
        // Receiver::tracks is populated only after async transport startup.
        // Register the negotiated MSIDs, not just tracks that already sent RTP.
        let mut indexes = Vec::new();
        for media in answer.unmarshal()?.media_descriptions {
            if media.media_name.media != "video" || media.media_name.port.value == 0 {
                continue;
            }
            for attribute in media.attributes {
                let Some(value) = attribute.value else {
                    continue;
                };
                let words: Vec<_> = value.split_whitespace().collect();
                let id = if attribute.key == "msid" {
                    words.get(1)
                } else if attribute.key == "ssrc"
                    && words.get(1).is_some_and(|word| word.starts_with("msid:"))
                {
                    words.get(2)
                } else {
                    None
                };
                if let Some(index) = id
                    .and_then(|id| id.strip_prefix("video_"))
                    .and_then(|id| id.parse::<i32>().ok())
                {
                    indexes.push(index);
                }
            }
        }
        self.connection
            .set_remote_description(answer)
            .await
            .context("install remote SDP answer")?;
        self.data_channels
            .stream_control
            .set_available_video_tracks(indexes);
        if let Some(version) = mixed_kcp_version {
            if let Some(active_version) = self.uu_kcp.negotiated_version() {
                ensure!(
                    active_version == version,
                    "UU mixed-KCP version changed across an ICE restart"
                );
                return Ok(());
            }
            self.uu_kcp.start(
                self.connection.sctp(),
                version,
                self.data_channels.stream_control.clone(),
            )?;
        } else if self.uu_kcp.is_negotiated() {
            self.uu_kcp.close().await;
        }
        Ok(())
    }

    pub async fn request_keyframe(&self, media_ssrc: u32) -> Result<()> {
        let track = self
            .video_tracks
            .all()
            .into_iter()
            .find(|track| track.metadata.ssrc == media_ssrc);
        let last_pli = track
            .as_ref()
            .map_or(&self.last_pli, |track| &track.last_pli);
        send_picture_loss_indication(&self.connection, last_pli, media_ssrc, true).await?;
        Ok(())
    }

    pub async fn next_connection_state(&self) -> Option<RTCPeerConnectionState> {
        self.connection_states.lock().await.recv().await
    }

    pub fn connection_state(&self) -> RTCPeerConnectionState {
        self.connection.connection_state()
    }

    pub fn ice_connection_state(&self) -> RTCIceConnectionState {
        self.connection.ice_connection_state()
    }

    /// UU only accepts a manual/automatic ICE-network switch after the
    /// existing transport has reached an established state.  Applying a
    /// relay-only configuration while ICE is still checking can discard the
    /// only usable candidate generation and produces the multi-second freezes
    /// that the official client avoids.
    pub(crate) fn can_switch_ice_network(&self) -> bool {
        matches!(
            self.connection.ice_connection_state(),
            RTCIceConnectionState::Connected | RTCIceConnectionState::Completed
        )
    }

    pub async fn ice_diagnostics(&self) -> String {
        let mut local = BTreeMap::<String, usize>::new();
        let mut remote = BTreeMap::<String, usize>::new();
        let mut pairs = BTreeMap::<String, usize>::new();
        let mut requests_sent = 0_u64;
        let mut responses_received = 0_u64;
        for report in self.connection.get_stats().await.reports.into_values() {
            match report {
                StatsReportType::LocalCandidate(candidate) => {
                    *local
                        .entry(format!("{:?}", candidate.candidate_type))
                        .or_default() += 1;
                }
                StatsReportType::RemoteCandidate(candidate) => {
                    *remote
                        .entry(format!("{:?}", candidate.candidate_type))
                        .or_default() += 1;
                }
                StatsReportType::CandidatePair(pair) => {
                    *pairs.entry(format!("{:?}", pair.state)).or_default() += 1;
                    requests_sent += pair.requests_sent;
                    responses_received += pair.responses_received;
                }
                _ => {}
            }
        }
        format!(
            "local={local:?}, remote={remote:?}, pairs={pairs:?}, checks_sent={requests_sent}, responses_received={responses_received}"
        )
    }

    pub async fn add_remote_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        if self.p2p_only.load(Ordering::Acquire) && candidate_is_relay(&candidate.candidate) {
            tracing::trace!(candidate = ?candidate, "ignoring remote relay candidate during P2P-only attempt");
            return Ok(());
        }
        tracing::trace!(candidate = ?candidate, "adding remote ICE candidate to peer");
        self.connection
            .add_ice_candidate(candidate)
            .await
            .context("install remote ICE candidate")
    }

    /// Install a decrypted-RTP forwarding callback. The returned channel emits
    /// negotiated codec/PT metadata needed by local media consumers.
    pub async fn install_rtp_forwarder(&self, config: RtpForwardConfig) -> Result<RtpForwarder> {
        let audio = self.data_channels.stream_control.audio();
        let (announcement_tx, announcement_rx) = mpsc::unbounded_channel();
        let (forwarding_started, forwarding_ready) = watch::channel(false);
        let tracks = self.video_tracks.clone();
        // The connection owns this callback. A strong self-reference here
        // survives PeerConnection::close, which does not clear on_track.
        let connection = Arc::downgrade(&self.connection);
        let workers = Arc::downgrade(&self.data_channels.workers);
        let stop = self.data_channels.workers.shutdown.child_token();
        let track_stop = stop.clone();
        let performance = self.performance.clone();
        let nack_rtt_micros = Arc::clone(&self.nack_rtt_micros);
        let rtcp_timing = self.rtcp_timing.clone();
        let rtp_capture = self.rtp_capture.clone();

        self.connection
            .on_track(Box::new(move |track, receiver, _| {
                let Some(owner) = workers.upgrade() else {
                    return Box::pin(async {});
                };
                let workers = workers.clone();
                let stop = track_stop.clone();
                let config = config.clone();
                let audio = audio.clone();
                let tracks = tracks.clone();
                let announcement_tx = announcement_tx.clone();
                let mut forwarding_ready = forwarding_ready.clone();
                let connection = connection.clone();
                let last_pli = Arc::new(Mutex::new(None));
                let mut performance = performance.clone();
                let nack_rtt_micros = Arc::new(AtomicU64::new(nack_rtt_micros.load(Ordering::Relaxed)));
                let rtcp_timing = rtcp_timing.clone();
                let rtp_capture = rtp_capture.clone();
                let _ = owner.spawn_in(stop.clone(), async move {
                    let Some(connection) = connection.upgrade() else {
                        return;
                    };
                    let kind = track.kind();
                    let id = track.id();
                    let selected = match kind {
                        RTPCodecType::Video => {
                            config
                                .video_track_id
                                .as_ref()
                                .is_none_or(|wanted| wanted == &id)
                        }
                        RTPCodecType::Audio => id == "audio_0",
                        _ => false,
                    };
                    if !selected {
                        return;
                    }

                    let media_kind = match kind {
                        RTPCodecType::Video => MediaKind::Video,
                        RTPCodecType::Audio => MediaKind::Audio,
                        _ => return,
                    };
                    let codec_parameters = track.codec();
                    let audio_generation = (media_kind == MediaKind::Audio).then(|| audio.select_source(
                        &codec_parameters.capability.mime_type, codec_parameters.capability.clock_rate,
                        codec_parameters.capability.channels)).flatten();
                    let codec = codec_parameters.capability.mime_type;
                    let codec_fmtp = codec_parameters.capability.sdp_fmtp_line;
                    let video_annexb_sinks = Arc::new(Mutex::new(Vec::new()));
                    let (video_keyframe_tx, keyframes) = watch::channel(0_u64);
                    let (feedback, feedback_rx) = mpsc::unbounded_channel();
                    let receiver_feedback = (media_kind == MediaKind::Video).then_some(feedback_rx);
                    if media_kind == MediaKind::Video {
                        let Some(index) = id.strip_prefix("video_").and_then(|id| id.parse::<i32>().ok()).filter(|id| *id >= 0) else {
                            tracing::warn!(track_id = %id, "unrecognized UU video track identifier");
                            return;
                        };
                        performance = performance.for_video_track(index as u64);
                        performance.set_video_codec(format!("{codec} · RTP PT {}", track.payload_type()));
                        let (started, ready) = watch::channel(false);
                        forwarding_ready = ready;
                        let source = Arc::new(VideoTrackSource {
                            metadata: ForwardedTrack { kind: media_kind, id: id.clone(), codec: codec.clone(), payload_type: track.payload_type(), ssrc: track.ssrc() },
                            index, performance: performance.clone(), sinks: Arc::clone(&video_annexb_sinks),
                            feedback, started, keyframes, last_pli: Arc::clone(&last_pli),
                            nack_rtt_micros: Arc::clone(&nack_rtt_micros),
                        });
                        {
                            let mut entries = std_mutex_lock(&tracks.entries);
                            if entries.contains_key(&index) {
                                tracing::warn!(index, "duplicate live UU video track ignored");
                                return;
                            }
                            entries.insert(index, source);
                        }
                        tracks.changed.notify_waiters();
                    }
                    // TrackRemote narrows its mutable params to the payload type of
                    // the first media packet. RTX apt belongs to the negotiated
                    // receiver codec table and must be captured from the receiver
                    // itself, before packet-driven track updates can erase it.
                    let parameters = receiver.get_parameters().await;
                    let extmap_allow_mixed =
                        connection
                            .local_description()
                            .await
                            .is_some_and(|description| {
                                description
                                    .sdp
                                    .lines()
                                    .take_while(|line| !line.starts_with("m="))
                                    .any(|line| line == "a=extmap-allow-mixed")
                            });
                    let rsfec_config = parameters
                        .codecs
                        .iter()
                        .find(|codec| {
                            codec
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/rs-fec-cm256")
                        })
                        .map(|codec| RsFecConfig::from_fmtp(&codec.capability.sdp_fmtp_line));
                    if let Some(capture) = &rtp_capture {
                        capture.record_codecs(track.ssrc(), &parameters, extmap_allow_mixed);
                    }
                    let video_payload_codecs = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/H264")
                                || entry
                                    .capability
                                    .mime_type
                                    .eq_ignore_ascii_case("video/H265")
                                || entry
                                    .capability
                                    .mime_type
                                    .eq_ignore_ascii_case("video/HEVC")
                        })
                        .map(|entry| {
                            (
                                entry.payload_type,
                                (
                                    entry.capability.mime_type.clone(),
                                    entry.capability.sdp_fmtp_line.clone(),
                                ),
                            )
                        })
                        .collect();
                    let rtx_payload_apt = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry.capability.mime_type.eq_ignore_ascii_case("video/rtx")
                        })
                        .filter_map(|entry| {
                            let apt = entry
                                .capability
                                .sdp_fmtp_line
                                .split(';')
                                .find_map(|part| part.trim().strip_prefix("apt="))?
                                .parse::<u8>()
                                .ok()?;
                            Some((entry.payload_type, apt))
                        })
                        .collect();
                    let flexfec_payload_types = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/flexfec-03")
                        })
                        .map(|entry| entry.payload_type)
                        .collect();
                    let red_payload_types = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry.capability.mime_type.eq_ignore_ascii_case("video/red")
                        })
                        .map(|entry| entry.payload_type)
                        .collect();
                    let ulpfec_payload_types = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/ulpfec")
                        })
                        .map(|entry| entry.payload_type)
                        .collect();
                    let ssrc = track.ssrc();
                    let remote_ntp =
                        Arc::new(StdMutex::new(RemoteNtpEstimator::new(Instant::now())));
                    let Some(owner) = workers.upgrade() else {
                        return;
                    };
                    if media_kind == MediaKind::Video {
                        // The first selected viewer, not the last arriving track,
                        // selects the session's RTT display source.
                        let _ = owner.spawn_in(
                            stop.clone(),
                            observe_remote_ntp(
                                Arc::clone(&receiver),
                                ssrc,
                                Arc::clone(&remote_ntp),
                                rtcp_timing,
                            ),
                        );
                    }
                    tracing::info!(
                        ?media_kind,
                        track_id = %id,
                        stream_id = %track.stream_id(),
                        codec = %codec,
                        payload_type = track.payload_type(),
                        ssrc,
                        red_payload_types = ?red_payload_types,
                        ulpfec_payload_types = ?ulpfec_payload_types,
                        flexfec_payload_types = ?flexfec_payload_types,
                        "remote RTP track selected"
                    );
                    let _ = announcement_tx.send(ForwardedTrack {
                        kind: media_kind,
                        id,
                        codec: codec.clone(),
                        payload_type: track.payload_type(),
                        ssrc,
                    });

                    let _ = owner.spawn_in(stop.clone(), async move {
                        forward_remote_track(
                            track,
                            forwarding_ready,
                            video_keyframe_tx,
                            TrackForwardContext {
                                workers,
                                stop,
                                kind: media_kind,
                                codec,
                                codec_fmtp,
                                video_payload_codecs,
                                rtx_payload_apt,
                                red_payload_types,
                                ulpfec_payload_types,
                                flexfec_payload_types,
                                rsfec_config,
                                extmap_allow_mixed,
                                audio,
                                audio_generation,
                                connection,
                                last_pli,
                                video_annexb_sinks,
                                performance,
                                nack_rtt_micros,
                                remote_ntp,
                                receiver_feedback,
                            },
                        )
                        .await;
                    });
                });
                Box::pin(async {})
            }));

        Ok(RtpForwarder {
            stop,
            announcements: announcement_rx,
            forwarding_started,
            tracks: self.video_tracks.clone(),
            selected_video: None,
        })
    }

    pub async fn close(&self) -> Result<()> {
        self.data_channels.workers.close().await;
        self.data_channels.stream_control.audio().close().await;
        self.uu_kcp.close().await;
        self.connection
            .close()
            .await
            .context("close native peer connection")
    }
}

impl Drop for NativePeer {
    fn drop(&mut self) {
        // Normal paths await close(). This also retires application tasks on
        // an exceptional owner drop instead of leaving them to hold the peer.
        self.data_channels.workers.shutdown.cancel();
    }
}

fn official_data_channel_init(label: &str) -> RTCDataChannelInit {
    let priority = match label {
        "CONTROL_DATA_CHANNEL" | "FILE_DATA_CHANNEL" => RTCDataChannelPriority::High,
        "TEXT_DATA_CHANNEL" | "STREAMER_DATA_CHANNEL" => RTCDataChannelPriority::Medium,
        "BINARY_DATA_CHANNEL" => RTCDataChannelPriority::Low,
        _ => RTCDataChannelPriority::Low,
    };
    RTCDataChannelInit {
        ordered: Some(true),
        priority: Some(priority),
        ..Default::default()
    }
}

async fn forward_remote_track(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    mut forwarding_ready: watch::Receiver<bool>,
    video_keyframe_tx: watch::Sender<u64>,
    context: TrackForwardContext,
) {
    let TrackForwardContext {
        workers,
        stop,
        kind,
        codec,
        codec_fmtp,
        video_payload_codecs,
        rtx_payload_apt,
        red_payload_types,
        ulpfec_payload_types,
        flexfec_payload_types,
        rsfec_config,
        extmap_allow_mixed,
        audio,
        audio_generation,
        connection,
        last_pli,
        video_annexb_sinks,
        performance,
        nack_rtt_micros,
        remote_ntp,
        receiver_feedback,
    } = context;

    if kind == MediaKind::Video {
        let Some(receiver_feedback) = receiver_feedback else {
            tracing::error!("video receiver feedback channel was not installed");
            return;
        };
        let playout_delay_extension_id = track.header_extension_id(PLAYOUT_DELAY_URI);
        forward_official_video_track(
            track,
            codec,
            codec_fmtp,
            video_payload_codecs,
            rtx_payload_apt,
            red_payload_types,
            ulpfec_payload_types,
            flexfec_payload_types,
            rsfec_config,
            extmap_allow_mixed,
            video_keyframe_tx,
            connection,
            last_pli,
            performance,
            nack_rtt_micros,
            remote_ntp,
            receiver_feedback,
            playout_delay_extension_id,
            forwarding_ready,
            video_annexb_sinks,
            workers,
            stop,
        )
        .await;
        return;
    }

    let mut first_packet = true;
    let mut startup_packet_count = 0_usize;

    while !*forwarding_ready.borrow() {
        tokio::select! {
            changed = forwarding_ready.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            received = track.read_rtp() => {
                let Ok((packet, _)) = received else {
                    tracing::debug!(?kind, "remote RTP track ended during startup buffering");
                    return;
                };
                if first_packet {
                    first_packet = false;
                    tracing::debug!(
                        ?kind,
                        sequence_number = packet.header.sequence_number,
                        timestamp = packet.header.timestamp,
                        marker = packet.header.marker,
                        payload_bytes = packet.payload.len(),
                        "first remote RTP packet received"
                    );
                }
                startup_packet_count += 1;
            }
        }
    }

    tracing::debug!(
        ?kind,
        startup_packet_count,
        "discarded pre-player RTP packets"
    );
    let Some(generation) = audio_generation else {
        return;
    };
    while let Ok((packet, _)) = track.read_rtp().await {
        audio.receive(
            generation,
            packet.payload,
            packet.header.timestamp,
            packet.header.sequence_number,
        );
    }
    tracing::debug!(?kind, "remote audio track ended");
}

#[allow(clippy::too_many_arguments)]
async fn forward_official_video_track(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    codec: String,
    codec_fmtp: String,
    video_payload_codecs: HashMap<u8, (String, String)>,
    rtx_payload_apt: HashMap<u8, u8>,
    red_payload_types: HashSet<u8>,
    ulpfec_payload_types: HashSet<u8>,
    flexfec_payload_types: HashSet<u8>,
    rsfec_config: Option<RsFecConfig>,
    extmap_allow_mixed: bool,
    video_keyframe_tx: watch::Sender<u64>,
    connection: Arc<RTCPeerConnection>,
    last_pli: Arc<Mutex<Option<Instant>>>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    remote_ntp: Arc<StdMutex<RemoteNtpEstimator>>,
    mut receiver_feedback: mpsc::UnboundedReceiver<VideoReceiverFeedback>,
    playout_delay_extension_id: Option<u8>,
    mut forwarding_ready: watch::Receiver<bool>,
    video_annexb_sinks: Arc<Mutex<Vec<VideoFrameSink>>>,
    workers: Weak<SessionWorkers>,
    stop: CancellationToken,
) {
    let mut ingress = match OrderedVideoIngress::open(&track).await {
        Ok(ingress) => ingress,
        Err(error) => {
            tracing::error!(%error, "open ordered video ingress failed");
            return;
        }
    };
    let _interceptor_drainers = {
        let Some(workers) = workers.upgrade() else {
            return;
        };
        VideoInterceptorDrainers::start(
            Arc::clone(&track),
            ingress.rtx_ssrc,
            ingress.fec_ssrc,
            &workers,
            &stop,
        )
    };
    let mut startup_packets = [0_u64; 3];
    while !*forwarding_ready.borrow() {
        tokio::select! {
            changed = forwarding_ready.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            incoming = ingress.recv() => {
                let Ok(incoming) = incoming else {
                    tracing::error!(error = ?incoming.err(), "ordered video ingress ended before viewer start");
                    return;
                };
                let index = match incoming.origin {
                    VideoIngressOrigin::Primary => 0,
                    VideoIngressOrigin::Rtx => 1,
                    VideoIngressOrigin::RsFec => 2,
                };
                startup_packets[index] = startup_packets[index].saturating_add(1);
            }
        }
    }
    tracing::debug!(
        primary = startup_packets[0],
        rtx = startup_packets[1],
        rsfec = startup_packets[2],
        "drained all associated RTP streams before viewer start"
    );
    let mut video_payload_codecs = video_payload_codecs;
    video_payload_codecs
        .entry(track.payload_type())
        .or_insert_with(|| (codec.clone(), codec_fmtp.clone()));
    tracing::info!(track_id = %track.id(),
        new_picture_extension_id = ?track.header_extension_id(VIDEO_IS_NEW_FRAME_URI),
        "UU picture-update metadata negotiated");
    let mut receiver = match OfficialVideoReceiver::new(
        &codec,
        VideoHeaderExtensions {
            orientation: track.header_extension_id("urn:3gpp:video-orientation"),
            content_type: track.header_extension_id(VIDEO_CONTENT_TYPE_URI),
            capture_index: track.header_extension_id(VIDEO_CAPTURE_INDEX_URI),
            is_new_picture: track.header_extension_id(VIDEO_IS_NEW_FRAME_URI),
            timing: track.header_extension_id(VIDEO_TIMING_URI),
            sending_delay: track.header_extension_id(VIDEO_FRAME_SENDING_DELAY_URI),
            color_space: track.header_extension_id(crate::video_color::COLOR_SPACE_URI),
        },
        &codec_fmtp,
    ) {
        Ok(receiver) => receiver,
        Err(error) => {
            tracing::error!(%error, %codec, "create official-compatible video receiver failed");
            return;
        }
    };
    let mut nack_requester = NackRequester::new();
    let mutable_extensions = [
        ("urn:ietf:params:rtp-hdrext:toffset", 0),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            0,
        ),
        (
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            0,
        ),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay",
            0,
        ),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
            7,
        ),
    ]
    .into_iter()
    .filter_map(|(uri, preserve)| Some((track.header_extension_id(uri)?, preserve)))
    .collect::<Vec<_>>();
    let mut fec_receiver = rsfec_config
        .map(|config| RsFecReceiver::new(track.ssrc(), config.max_k, mutable_extensions.clone()));
    let mut flexfec_receiver = FlexFecReceiver::new(track.ssrc(), mutable_extensions.clone());
    let mut ulpfec_receiver = UlpfecReceiver::new(track.ssrc(), mutable_extensions);
    let mut pending_media = VecDeque::<PendingMediaPacket>::new();
    let mut configured_payload_type = track.payload_type();
    let mut rtcp_feedback = RtcpFeedbackBuffer::default();
    let rid_extension_id = track.header_extension_id(RTP_STREAM_ID_URI);
    let repaired_rid_extension_id = track.header_extension_id(REPAIRED_RTP_STREAM_ID_URI);
    let mut nack_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + RTP_NACK_PROCESS_INTERVAL,
        RTP_NACK_PROCESS_INTERVAL,
    );
    nack_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_packet_received_at = None;
    let mut last_keyframe_packet_at = None;
    let mut last_keyframe_timestamp = None;

    loop {
        performance.set_ingress_queue_packets(ingress.receiver.len());
        enum Event {
            Media(PendingMediaPacket),
            Fec(RtpPacket, Instant),
            Feedback(VideoReceiverFeedback),
            NackTick,
            ReceiverDeadline,
        }
        let receiver_deadline = receiver.next_deadline();
        let event = if let Some(packet) = pending_media.pop_front() {
            Event::Media(packet)
        } else {
            tokio::select! {
                    incoming = ingress.recv() => {
                        let incoming = match incoming {
                            Ok(incoming) => incoming,
                            Err(error) => {
                            if connection.connection_state() == RTCPeerConnectionState::Closed
                                || error.to_string() == "ordered RTP ingress closed"
                            {
                                tracing::debug!(%error, "ordered video ingress closed with the peer connection");
                            } else {
                                tracing::error!(%error, "ordered video ingress failed");
                            }
                            break;
                        }
                    };
                    tracing::trace!(
                        ordinal = incoming.ordinal,
                        origin = ?incoming.origin,
                        "processing ordered video RTP"
                    );
                    // Account for each received datagram once, before recovery.
                    // Locally reconstructed FEC packets are not additional traffic;
                    // RTX/FEC/padding still consume bandwidth even if rejected later.
                    performance.record_rtp_packet(incoming.wire_bytes);
                    match incoming.origin {
                        VideoIngressOrigin::Primary => Event::Media(PendingMediaPacket {
                            packet: incoming.packet,
                            flags: MediaRecoveryFlags::default(),
                            received_at: incoming.received_at,
                            rsfec_source: Some(incoming.raw),
                        }),
                        VideoIngressOrigin::Rtx => Event::Media(PendingMediaPacket {
                            packet: incoming.packet,
                            flags: MediaRecoveryFlags {
                                recovered_from_rtx: true,
                                recovered_by_fec: false,
                            },
                            received_at: incoming.received_at,
                            rsfec_source: None,
                        }),
                        VideoIngressOrigin::RsFec => Event::Fec(
                            incoming.packet,
                            incoming.received_at,
                        ),
                    }
                }
                feedback = receiver_feedback.recv() => {
                    let Some(feedback) = feedback else { break; };
                    Event::Feedback(feedback)
                }
                _ = nack_interval.tick() => Event::NackTick,
                _ = async {
                    // UU posts a ready task for a zero/expired deadline. A Tokio
                    // timer rounds up to milliseconds and would add one tick to
                    // every 0/0 frame even though no wait was requested.
                    if receiver_deadline > Instant::now() {
                        tokio::time::sleep_until(receiver_deadline.into()).await;
                    }
                } => Event::ReceiverDeadline
            }
        };

        match event {
            Event::NackTick => {
                nack_requester.set_rtt(Duration::from_micros(
                    nack_rtt_micros.load(Ordering::Relaxed),
                ));
                let batch = nack_requester.process();
                rtcp_feedback.buffer(batch);
                performance.set_nack_state(
                    nack_requester.outstanding(),
                    nack_requester.final_lost_packets(),
                );
                rtcp_feedback
                    .flush(&connection, &last_pli, track.ssrc())
                    .await;
            }
            Event::ReceiverDeadline => {
                let now = Instant::now();
                let active = last_packet_received_at.is_some_and(|received_at| {
                    now.saturating_duration_since(received_at) < ACTIVE_STREAM_WINDOW
                });
                let receiving_keyframe = last_keyframe_packet_at.is_some_and(|received_at| {
                    now.saturating_duration_since(received_at) < KEYFRAME_PACKET_WINDOW
                });
                let result = receiver.poll(active, receiving_keyframe);
                if !emit_official_receiver_result(
                    result,
                    &video_annexb_sinks,
                    &video_keyframe_tx,
                    &performance,
                    &remote_ntp,
                    &connection,
                    &last_pli,
                    track.ssrc(),
                )
                .await
                {
                    break;
                }
            }
            Event::Feedback(VideoReceiverFeedback::DecodeTiming {
                duration,
                finished_at,
            }) => {
                receiver.record_decode(duration, finished_at);
            }
            Event::Feedback(VideoReceiverFeedback::DecoderFinished { frame_id, result }) => {
                let result = receiver.decoder_finished(frame_id, result);
                if let Some(sequence_number) = result.continuous_sequence {
                    nack_requester.clear_up_to(sequence_number);
                    performance.set_nack_state(
                        nack_requester.outstanding(),
                        nack_requester.final_lost_packets(),
                    );
                }
                if !emit_official_receiver_result(
                    result,
                    &video_annexb_sinks,
                    &video_keyframe_tx,
                    &performance,
                    &remote_ntp,
                    &connection,
                    &last_pli,
                    track.ssrc(),
                )
                .await
                {
                    break;
                }
            }
            Event::Fec(packet, received_at) => {
                performance.record_fec_packet_received();
                if flexfec_payload_types.contains(&packet.header.payload_type) {
                    match flexfec_receiver.receive_repair(
                        packet.header.ssrc,
                        packet.header.sequence_number,
                        &packet.payload,
                    ) {
                        Ok(recovery) => {
                            performance.record_fec_recovered(recovery.len());
                            pending_media
                                .extend(parse_flexfec_recovered_packets(recovery, received_at));
                        }
                        Err(error) => tracing::debug!(%error, "FlexFEC repair packet rejected"),
                    }
                } else {
                    let Some(fec_receiver) = fec_receiver.as_mut() else {
                        continue;
                    };
                    match fec_receiver.receive_repair(&packet.payload) {
                        Ok(recovery) => {
                            performance.record_fec_recovered(recovery.recovered_packets.len());
                            pending_media.extend(parse_rsfec_recovered_packets(
                                recovery.recovered_packets,
                                received_at,
                            ));
                        }
                        Err(error) => tracing::warn!(%error, "RSFEC repair packet rejected"),
                    }
                }
            }
            Event::Media(mut media) => {
                if media.flags.recovered_from_rtx {
                    let Some(primary_payload_type) = rtx_payload_apt
                        .get(&media.packet.header.payload_type)
                        .copied()
                    else {
                        performance.record_rtx_packet(false);
                        continue;
                    };
                    let Some(packet) =
                        recover_rtx_packet(media.packet, primary_payload_type, track.ssrc())
                    else {
                        performance.record_rtx_packet(false);
                        continue;
                    };
                    media.rsfec_source = if rsfec_config.is_some_and(|config| config.rtx_as_source)
                    {
                        match normalize_rtx_source(
                            &packet,
                            rid_extension_id,
                            repaired_rid_extension_id,
                            extmap_allow_mixed,
                        ) {
                            Ok(source) => Some(source),
                            Err(error) => {
                                tracing::debug!(%error, "RTX media retained; its RSFEC source normalization failed");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    media.packet = packet;
                }

                if media.packet.header.ssrc != track.ssrc() {
                    tracing::debug!(
                        ssrc = media.packet.header.ssrc,
                        "recovered RTP belongs to another receive stream"
                    );
                    continue;
                }

                if red_payload_types.contains(&media.packet.header.payload_type) {
                    match unwrap_red_packet(media.packet, &ulpfec_payload_types) {
                        Ok(RedPacket::Media(packet)) => {
                            media.rsfec_source = packet.marshal().ok();
                            media.packet = packet;
                        }
                        Ok(RedPacket::Ulpfec {
                            sequence_number,
                            payload,
                        }) => {
                            performance.record_fec_packet_received();
                            if media.flags.recovered_from_rtx {
                                // UU/WebRTC does not feed an RTX-recovered RED/FEC
                                // packet back into the ULPFEC erasure decoder.
                                continue;
                            }
                            match ulpfec_receiver.receive_repair(sequence_number, &payload) {
                                Ok(recovery) => {
                                    performance.record_fec_recovered(recovery.len());
                                    pending_media.extend(parse_ulpfec_recovered_packets(
                                        recovery,
                                        media.received_at,
                                    ));
                                }
                                Err(error) => {
                                    tracing::debug!(%error, "ULPFEC repair packet rejected")
                                }
                            }
                            continue;
                        }
                        Err(error) => {
                            tracing::debug!(%error, "RED packet rejected");
                            continue;
                        }
                    }
                }

                let recovered = media.flags.recovered_from_rtx || media.flags.recovered_by_fec;
                if !recovered {
                    performance.record_video_rtp_arrival(media.packet.header.timestamp);
                }
                let sequence_number = media.packet.header.sequence_number;
                let payload_type = media.packet.header.payload_type;
                // A padded RTP datagram is not necessarily an empty packet:
                // the packet parser has already removed its trailing padding.
                let is_padding = media.packet.payload.is_empty();
                if !recovered && !is_padding {
                    performance.record_primary_media_packet();
                }
                let result;

                if is_padding {
                    result = receiver.receive_padding(sequence_number);
                    nack_requester.set_rtt(Duration::from_micros(
                        nack_rtt_micros.load(Ordering::Relaxed),
                    ));
                    let nack = nack_requester.on_received(sequence_number, false, recovered);
                    rtcp_feedback.buffer(nack.batch);
                } else {
                    let Some((mime, fmtp)) = video_payload_codecs.get(&payload_type) else {
                        tracing::debug!(payload_type, "ignoring unrecognized video payload type");
                        if let Some(source) = media.rsfec_source.take() {
                            remember_fec_source(
                                &mut fec_receiver,
                                &mut flexfec_receiver,
                                &mut ulpfec_receiver,
                                &mut pending_media,
                                sequence_number,
                                source,
                                media.flags.recovered_from_rtx,
                                media.received_at,
                                &performance,
                            );
                        }
                        continue;
                    };
                    let packet_codec = if mime.eq_ignore_ascii_case("video/H264") {
                        VideoCodecKind::H264
                    } else if mime.eq_ignore_ascii_case("video/H265")
                        || mime.eq_ignore_ascii_case("video/HEVC")
                    {
                        VideoCodecKind::H265
                    } else {
                        tracing::debug!(payload_type, %mime, "ignoring unsupported video codec");
                        if let Some(source) = media.rsfec_source.take() {
                            remember_fec_source(
                                &mut fec_receiver,
                                &mut flexfec_receiver,
                                &mut ulpfec_receiver,
                                &mut pending_media,
                                sequence_number,
                                source,
                                media.flags.recovered_from_rtx,
                                media.received_at,
                                &performance,
                            );
                        }
                        continue;
                    };
                    if configured_payload_type != payload_type {
                        receiver.configure_codec(packet_codec, fmtp);
                        configured_payload_type = payload_type;
                    }
                    let Some(mut parsed) =
                        receiver.parse_video_packet(&media.packet, media.received_at, packet_codec)
                    else {
                        tracing::debug!(
                            sequence_number,
                            payload_type,
                            recovered,
                            "video payload depacketizer rejected packet before NACK"
                        );
                        if let Some(source) = media.rsfec_source.take() {
                            remember_fec_source(
                                &mut fec_receiver,
                                &mut flexfec_receiver,
                                &mut ulpfec_receiver,
                                &mut pending_media,
                                sequence_number,
                                source,
                                media.flags.recovered_from_rtx,
                                media.received_at,
                                &performance,
                            );
                        }
                        if media.flags.recovered_from_rtx {
                            performance.record_rtx_packet(false);
                        }
                        continue;
                    };

                    if !recovered {
                        let now = Instant::now();
                        last_packet_received_at = Some(now);
                        if parsed.starts_keyframe()
                            || last_keyframe_timestamp == Some(media.packet.header.timestamp)
                        {
                            last_keyframe_timestamp = Some(media.packet.header.timestamp);
                            last_keyframe_packet_at = Some(now);
                        }
                    }
                    let playout_delay = playout_delay_extension_id
                        .and_then(|id| media.packet.header.get_extension(id))
                        .and_then(|payload| parse_playout_delay(&payload));

                    nack_requester.set_rtt(Duration::from_micros(
                        nack_rtt_micros.load(Ordering::Relaxed),
                    ));
                    let nack = nack_requester.on_received(
                        parsed.sequence_number(),
                        parsed.starts_keyframe(),
                        recovered,
                    );
                    parsed.set_receive_timing(playout_delay, nack.nack_count);
                    rtcp_feedback.buffer(nack.batch);
                    rtcp_feedback
                        .flush(&connection, &last_pli, track.ssrc())
                        .await;
                    result = receiver.receive_parsed(parsed);
                }

                performance.set_nack_state(
                    nack_requester.outstanding(),
                    nack_requester.final_lost_packets(),
                );
                if let Some(sequence_number) = result.continuous_sequence {
                    nack_requester.clear_up_to(sequence_number);
                    performance.set_nack_state(
                        nack_requester.outstanding(),
                        nack_requester.final_lost_packets(),
                    );
                }
                if media.flags.recovered_from_rtx {
                    performance.record_rtx_packet(result.accepted_packet);
                }
                if !emit_official_receiver_result(
                    result,
                    &video_annexb_sinks,
                    &video_keyframe_tx,
                    &performance,
                    &remote_ntp,
                    &connection,
                    &last_pli,
                    track.ssrc(),
                )
                .await
                {
                    break;
                }

                if let Some(source) = media.rsfec_source.take() {
                    remember_fec_source(
                        &mut fec_receiver,
                        &mut flexfec_receiver,
                        &mut ulpfec_receiver,
                        &mut pending_media,
                        sequence_number,
                        source,
                        media.flags.recovered_from_rtx,
                        media.received_at,
                        &performance,
                    );
                }
            }
        }
    }
    tracing::debug!("official-compatible remote video track ended");
}

#[allow(clippy::too_many_arguments)]
async fn emit_official_receiver_result(
    result: ReceiverResult,
    video_sinks: &Mutex<Vec<VideoFrameSink>>,
    video_keyframe_tx: &watch::Sender<u64>,
    performance: &PerformanceMonitor,
    remote_ntp: &StdMutex<RemoteNtpEstimator>,
    connection: &RTCPeerConnection,
    last_pli: &Mutex<Option<Instant>>,
    media_ssrc: u32,
) -> bool {
    if result.request_keyframe {
        let _ =
            send_picture_loss_indication(connection, last_pli, media_ssrc, result.force_keyframe)
                .await;
    }
    performance.record_predecode_drops(result.predecode_drops);
    performance.set_frame_buffer_frames(result.frame_buffer_frames);
    for frame in result.frames {
        performance.set_playout_timing(
            frame.schedule.target_delay,
            frame.schedule.jitter_delay,
            frame.schedule.low_latency,
        );
        tracing::trace!(rtp_timestamp = frame.rtp_timestamp, render_at = ?frame.schedule.render_at,
            "UU receiver released frame for decoding");
        let capture_at = frame
            .video_timing
            .filter(|timing| timing.flags & 4 != 0)
            .and_then(|_| {
                remote_ntp
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .estimate(frame.rtp_timestamp)
            });
        if frame.keyframe {
            tracing::trace!(target: "openuuyc::rtc::clock", rtp_timestamp = frame.rtp_timestamp,
                capture_age_ms = ?capture_at.map(|capture| {
                    if frame.last_received_at >= capture {
                        frame.last_received_at.duration_since(capture).as_secs_f64() * 1000.0
                    } else {
                        -capture.duration_since(frame.last_received_at).as_secs_f64() * 1000.0
                    }
                }), "keyframe clock mapping");
        }
        let sender_timing = frame_sender_timing(
            capture_at,
            frame.video_timing,
            frame.frame_sending_delay_ms,
            frame.last_received_at,
        );
        performance.record_received_frame(
            frame.data.len(),
            frame.last_received_at.duration_since(frame.received_at),
            frame.rtp_timestamp,
            frame.assembled_at,
            frame.keyframe,
            frame.frame_sending_delay_ms,
        );
        if frame.keyframe {
            video_keyframe_tx.send_modify(|value| *value += 1);
        }
        let encoded = EncodedVideoFrame {
            parameter_format: frame.parameter_format,
            color_space: frame.color_space,
            frame_id: frame.frame_id,
            data: Bytes::from(frame.data),
            rtp_timestamp: frame.rtp_timestamp,
            received_at: frame.received_at,
            assembled_at: frame.assembled_at,
            keyframe: frame.keyframe,
            rotation: frame.rotation,
            content_type: frame.content_type,
            video_capture_index: frame.video_capture_index,
            is_new_picture: frame.is_new_picture,
            sender_timing,
            codec: match frame.codec {
                VideoCodecKind::H264 => VideoCodec::H264,
                VideoCodecKind::H265 => VideoCodec::H265,
            },
        };
        let mut sinks = video_sinks.lock().await;
        sinks.retain(|sink| sink.send(encoded.clone()));
        if sinks.is_empty() {
            return false;
        }
    }
    true
}

fn frame_sender_timing(
    capture_at: Option<Instant>,
    timing: Option<crate::official_receiver::VideoSendTiming>,
    frame_sending_delay_ms: Option<u16>,
    last_received_at: Instant,
) -> FrameSenderTiming {
    // UU ReceiveStatisticsProxy::OnTimingFrameInfoUpdated (323C4C) admits
    // cross-clock phase/E2E samples only with the measured flag (bit 2).
    // A sender report alone does not establish that an arbitrary RTP frame
    // carries a valid capture-domain timing measurement (notably older peers).
    let timing = timing.filter(|sample| sample.flags & 4 != 0);
    let capture_at = timing.and(capture_at);
    let millis = |value: u16| Duration::from_millis(u64::from(value));
    let capture_delay = timing.map(|timing| millis(timing.encode_start_delta_ms));
    let encode_delay = timing.and_then(|timing| {
        timing
            .encode_finish_delta_ms
            .checked_sub(timing.encode_start_delta_ms)
            .map(millis)
    });
    let pacer_delay = timing.and_then(|timing| {
        timing
            .pacer_exit_delta_ms
            .checked_sub(timing.packetization_finish_delta_ms)
            .map(millis)
    });
    let sending_delay = frame_sending_delay_ms
        .map(millis)
        .or_else(|| timing.map(|timing| millis(timing.pacer_exit_delta_ms)));
    let transport_delay = capture_at
        .zip(sending_delay)
        .and_then(|(capture_at, sending_delay)| capture_at.checked_add(sending_delay))
        .and_then(|pacer_exit| last_received_at.checked_duration_since(pacer_exit));
    FrameSenderTiming {
        capture_at,
        capture_delay,
        encode_delay,
        pacer_delay,
        sending_delay,
        transport_delay,
    }
}

fn parse_rsfec_recovered_packets(
    recovered_packets: Vec<Vec<u8>>,
    received_at: Instant,
) -> Vec<PendingMediaPacket> {
    recovered_packets
        .into_iter()
        .filter_map(|recovered| {
            let source = Bytes::from(recovered);
            let mut raw = source.as_ref();
            match RtpPacket::unmarshal(&mut raw) {
                Ok(packet) => Some(PendingMediaPacket {
                    packet,
                    flags: MediaRecoveryFlags {
                        recovered_from_rtx: false,
                        recovered_by_fec: true,
                    },
                    received_at,
                    // A packet recovered by FEC re-enters the normal media
                    // path, but is never offered back to any FEC decoder as a
                    // new source.  Only primary media and normalized
                    // RTX-as-source packets feed the repair caches.
                    rsfec_source: None,
                }),
                Err(error) => {
                    tracing::warn!(%error, "RSFEC recovered malformed RTP packet");
                    None
                }
            }
        })
        .collect()
}

fn parse_flexfec_recovered_packets(
    recovered_packets: Vec<Vec<u8>>,
    received_at: Instant,
) -> Vec<PendingMediaPacket> {
    recovered_packets
        .into_iter()
        .filter_map(|recovered| {
            let source = Bytes::from(recovered);
            let mut raw = source.as_ref();
            match RtpPacket::unmarshal(&mut raw) {
                Ok(packet) => Some(PendingMediaPacket {
                    packet,
                    flags: MediaRecoveryFlags {
                        recovered_from_rtx: false,
                        recovered_by_fec: true,
                    },
                    received_at,
                    rsfec_source: None,
                }),
                Err(error) => {
                    tracing::warn!(%error, "FlexFEC recovered malformed RTP packet");
                    None
                }
            }
        })
        .collect()
}

fn parse_ulpfec_recovered_packets(
    recovered_packets: Vec<Vec<u8>>,
    received_at: Instant,
) -> Vec<PendingMediaPacket> {
    recovered_packets
        .into_iter()
        .filter_map(|recovered| {
            let source = Bytes::from(recovered);
            let mut raw = source.as_ref();
            match RtpPacket::unmarshal(&mut raw) {
                Ok(packet) => Some(PendingMediaPacket {
                    packet,
                    flags: MediaRecoveryFlags {
                        recovered_from_rtx: false,
                        recovered_by_fec: true,
                    },
                    received_at,
                    rsfec_source: None,
                }),
                Err(error) => {
                    tracing::warn!(%error, "ULPFEC recovered malformed RTP packet");
                    None
                }
            }
        })
        .collect()
}

enum RedPacket {
    Media(RtpPacket),
    Ulpfec {
        sequence_number: u16,
        payload: Bytes,
    },
}

fn unwrap_red_packet(
    mut packet: RtpPacket,
    ulpfec_payload_types: &HashSet<u8>,
) -> Result<RedPacket> {
    let Some((&red_header, payload)) = packet.payload.as_ref().split_first() else {
        bail!("RED packet has no payload type header");
    };
    if red_header & 0x80 != 0 {
        bail!("RED packet contains multiple blocks");
    }
    let payload_type = red_header & 0x7f;
    if ulpfec_payload_types.contains(&payload_type) {
        return Ok(RedPacket::Ulpfec {
            sequence_number: packet.header.sequence_number,
            payload: Bytes::copy_from_slice(payload),
        });
    }
    packet.header.payload_type = payload_type;
    packet.payload = Bytes::copy_from_slice(payload);
    Ok(RedPacket::Media(packet))
}

#[allow(clippy::too_many_arguments)]
fn remember_fec_source(
    rsfec_receiver: &mut Option<RsFecReceiver>,
    flexfec_receiver: &mut FlexFecReceiver,
    ulpfec_receiver: &mut UlpfecReceiver,
    pending_media: &mut VecDeque<PendingMediaPacket>,
    sequence_number: u16,
    source: Bytes,
    from_rtx: bool,
    received_at: Instant,
    performance: &PerformanceMonitor,
) {
    if let Some(rsfec_receiver) = rsfec_receiver.as_mut() {
        match rsfec_receiver.remember_media(sequence_number, &source) {
            Ok(recovery) => {
                performance.record_fec_recovered(recovery.recovered_packets.len());
                pending_media.extend(parse_rsfec_recovered_packets(
                    recovery.recovered_packets,
                    received_at,
                ));
            }
            Err(error) => tracing::warn!(%error, "RSFEC media source rejected"),
        }
    }
    if from_rtx {
        return;
    }
    match flexfec_receiver.remember_media(sequence_number, &source) {
        Ok(recovery) => {
            performance.record_fec_recovered(recovery.len());
            pending_media.extend(parse_flexfec_recovered_packets(recovery, received_at));
        }
        Err(error) => tracing::debug!(%error, "FlexFEC media source rejected"),
    }
    match ulpfec_receiver.remember_media(sequence_number, &source) {
        Ok(recovery) => {
            performance.record_fec_recovered(recovery.len());
            pending_media.extend(parse_ulpfec_recovered_packets(recovery, received_at));
        }
        Err(error) => tracing::debug!(%error, "ULPFEC media source rejected"),
    }
}

fn recover_rtx_packet(
    mut packet: RtpPacket,
    primary_payload_type: u8,
    primary_ssrc: u32,
) -> Option<RtpPacket> {
    if packet.payload.len() < 2 {
        return None;
    }
    packet.header.sequence_number = u16::from_be_bytes([packet.payload[0], packet.payload[1]]);
    packet.header.payload_type = primary_payload_type;
    packet.header.ssrc = primary_ssrc;
    packet.header.padding = false;
    packet.payload = packet.payload.slice(2..);
    Some(packet)
}

async fn send_picture_loss_indication(
    connection: &RTCPeerConnection,
    last_pli: &Mutex<Option<Instant>>,
    media_ssrc: u32,
    force: bool,
) -> Result<bool> {
    let mut last = last_pli.lock().await;
    let now = Instant::now();
    if !force && last.is_some_and(|value| now.duration_since(value) < PLI_MIN_INTERVAL) {
        return Ok(false);
    }
    tracing::debug!(media_ssrc, force, "sending RTCP PLI");
    let pli: Box<dyn RtcpPacket + Send + Sync> = Box::new(PictureLossIndication {
        sender_ssrc: DEFAULT_RECEIVER_SSRC,
        media_ssrc,
    });
    connection
        .write_rtcp(&[pli])
        .await
        .context("send RTCP picture-loss indication")?;
    *last = Some(now);
    Ok(true)
}

async fn send_transport_layer_nack(
    connection: &RTCPeerConnection,
    media_ssrc: u32,
    missing_sequences: &[u16],
) -> Result<()> {
    if missing_sequences.is_empty() {
        return Ok(());
    }
    let mut sorted = missing_sequences.to_vec();
    sorted.sort_unstable_by(sequence_order);
    sorted.dedup();
    let pairs = nack_pairs_from_sequence_numbers(&sorted);
    let diagnostic_start =
        tracing::enabled!(target: "openuuyc::nack_audit", tracing::Level::DEBUG).then(Instant::now);
    tracing::debug!(target: "openuuyc::nack_audit", media_ssrc, sequences = ?sorted,
        "sending packet-repair request");
    let nack: Box<dyn RtcpPacket + Send + Sync> = Box::new(TransportLayerNack {
        sender_ssrc: DEFAULT_RECEIVER_SSRC,
        media_ssrc,
        nacks: pairs,
    });
    connection
        .write_rtcp(&[nack])
        .await
        .context("send RTCP transport-layer NACK")?;
    if let Some(start) = diagnostic_start {
        tracing::debug!(target: "openuuyc::nack_audit", media_ssrc,
            write_us = start.elapsed().as_micros(), "packet-repair request write completed");
    }
    Ok(())
}

fn apply_uu_application_attributes(sdp: &mut String) -> Result<()> {
    const EXTMAP_ALLOW_MIXED: &str = "a=extmap-allow-mixed";
    const AUDIO_MSID_SEMANTIC: &str = "a=msid-semantic: WMS audio_0";
    const MAX_MESSAGE_SIZE: &str = "a=max-message-size:524288";
    const MIXED_KCP: &str = "a=x-uuremote-mix-kcp:2";

    let uses_crlf = sdp.contains("\r\n");
    let mut lines = sdp.lines().map(str::to_owned).collect::<Vec<_>>();
    let first_media = lines
        .iter()
        .position(|line| line.starts_with("m="))
        .context("controller SDP has no media sections")?;
    let session_insert = lines[..first_media]
        .iter()
        .position(|line| line.starts_with("a=group:BUNDLE"))
        .map_or(first_media, |index| index + 1);
    for value in [EXTMAP_ALLOW_MIXED, AUDIO_MSID_SEMANTIC].into_iter().rev() {
        if !lines[..first_media].iter().any(|line| line == value) {
            lines.insert(session_insert, value.to_owned());
        }
    }
    let application = lines
        .iter()
        .position(|line| line.starts_with("m=application "))
        .context("controller SDP has no application media section")?;
    let end = lines[application + 1..]
        .iter()
        .position(|line| line.starts_with("m="))
        .map_or(lines.len(), |offset| application + 1 + offset);

    for index in (application + 1..end)
        .filter(|index| lines[*index] == "a=sendrecv")
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        lines.remove(index);
    }
    let end = lines[application + 1..]
        .iter()
        .position(|line| line.starts_with("m="))
        .map_or(lines.len(), |offset| application + 1 + offset);
    let insert_at = lines[application + 1..end]
        .iter()
        .position(|line| line.starts_with("a=sctp-port:"))
        .map_or(end, |offset| application + 2 + offset);
    for value in [MAX_MESSAGE_SIZE, MIXED_KCP].into_iter().rev() {
        if !lines[application + 1..end].iter().any(|line| line == value) {
            lines.insert(insert_at, value.to_owned());
        }
    }

    let separator = if uses_crlf { "\r\n" } else { "\n" };
    *sdp = lines.join(separator);
    sdp.push_str(separator);
    Ok(())
}

fn negotiated_mixed_kcp_version(sdp: &str) -> Option<u8> {
    sdp.lines()
        .find_map(|line| line.strip_prefix("a=x-uuremote-mix-kcp:"))
        .and_then(|version| version.trim().parse::<u8>().ok())
        .filter(|version| *version != 0)
        .map(|version| version.min(2))
}

fn register_uu_codecs(media_engine: &mut MediaEngine) -> Result<()> {
    for (mime_type, payload_type, clock_rate, channels, fmtp, transport_cc) in [
        (
            MIME_TYPE_OPUS,
            111,
            48_000,
            2,
            "minptime=10;stereo=1;useinbandfec=1",
            true,
        ),
        (MIME_TYPE_G722, 9, 8_000, 0, "", false),
        (MIME_TYPE_PCMU, 0, 8_000, 0, "", false),
        (MIME_TYPE_PCMA, 8, 8_000, 0, "", false),
        ("audio/CN", 13, 8_000, 0, "", false),
        (MIME_TYPE_TELEPHONE_EVENT, 110, 48_000, 0, "", false),
        (MIME_TYPE_TELEPHONE_EVENT, 126, 8_000, 0, "", false),
    ] {
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: mime_type.to_owned(),
                        clock_rate,
                        channels,
                        sdp_fmtp_line: fmtp.to_owned(),
                        rtcp_feedback: transport_cc
                            .then(|| RTCPFeedback {
                                typ: "transport-cc".to_owned(),
                                parameter: String::new(),
                            })
                            .into_iter()
                            .collect(),
                    },
                    payload_type,
                    ..Default::default()
                },
                RTPCodecType::Audio,
            )
            .with_context(|| format!("register UU audio codec {mime_type}/{payload_type}"))?;
    }

    let primary_feedback = vec![
        RTCPFeedback {
            typ: "goog-remb".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "transport-cc".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "ccm".to_owned(),
            parameter: "fir".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
        RTCPFeedback {
            typ: "rrtr".to_owned(),
            parameter: String::new(),
        },
    ];
    let repair_feedback = vec![
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
        RTCPFeedback {
            typ: "transport-cc".to_owned(),
            parameter: String::new(),
        },
    ];
    for (mime_type, payload_type, fmtp, rtcp_feedback) in [
        (MIME_TYPE_HEVC, 96, "", primary_feedback.clone()),
        ("video/rtx", 97, "apt=96", repair_feedback.clone()),
        (
            MIME_TYPE_H264,
            98,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
            primary_feedback,
        ),
        ("video/rtx", 99, "apt=98", repair_feedback),
        ("video/red", 100, "", Vec::new()),
        (
            "video/rtx",
            101,
            "apt=100",
            vec![RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: String::new(),
            }],
        ),
        ("video/ulpfec", 102, "", Vec::new()),
        ("video/flexfec-03", 35, "repair-window=10000000", Vec::new()),
        (
            "video/rs-fec-cm256",
            36,
            "max-k=109;repair-window=10000000;rtx-as-source=1",
            Vec::new(),
        ),
    ] {
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: mime_type.to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: fmtp.to_owned(),
                        rtcp_feedback,
                    },
                    payload_type,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .with_context(|| format!("register UU video codec {mime_type}/{payload_type}"))?;
    }
    Ok(())
}

fn register_uu_header_extensions(media_engine: &mut MediaEngine) -> Result<()> {
    const AUDIO_EXTENSIONS: [&str; 4] = [
        "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
        "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        "urn:ietf:params:rtp-hdrext:sdes:mid",
    ];
    // Preserve the existing extensions and negotiate UU's new-picture marker.
    // The shared audio/video ID space requires the already-offered mixed form.
    const VIDEO_EXTENSIONS: [&str; 14] = [
        "urn:3gpp:video-orientation",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-content-type",
        crate::video_color::COLOR_SPACE_URI,
        "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
        "urn:ietf:params:rtp-hdrext:toffset",
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
        "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
        "urn:ietf:params:rtp-hdrext:sdes:mid",
        "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index",
        "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay",
        VIDEO_IS_NEW_FRAME_URI,
    ];
    for (kind, uris) in [
        (RTPCodecType::Audio, AUDIO_EXTENSIONS.as_slice()),
        (RTPCodecType::Video, VIDEO_EXTENSIONS.as_slice()),
    ] {
        for uri in uris {
            media_engine
                .register_header_extension(
                    RTCRtpHeaderExtensionCapability {
                        uri: (*uri).to_owned(),
                    },
                    kind,
                    None,
                )
                .with_context(|| format!("register UU RTP header extension {uri}"))?;
        }
    }
    Ok(())
}

fn std_mutex_lock<T>(lock: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_labels_distinguish_public_host_direct_from_lan_and_relay() {
        use RTCIceCandidateType::{Host, Prflx, Relay, Srflx};
        for (local_type, local, remote_type, remote, expected) in [
            (Host, "2001:db8:1::1", Host, "2001:db8:2::2", "P2P"),
            (Host, "2001:db8:1:2::1", Host, "2001:db8:1:2::2", "LAN"),
            (Host, "2001:db8:1:2::1", Prflx, "2001:db8:1:2::2", "P2P"),
            (Host, "192.0.2.1", Host, "198.51.100.2", "P2P"),
            (Host, "10.0.0.2", Host, "10.0.0.3", "LAN"),
            (Host, "10.0.0.2", Prflx, "10.0.0.3", "LAN"),
            (Host, "fd00::2", Host, "fd00::3", "LAN"),
            (Host, "::ffff:10.0.0.2", Host, "::ffff:10.0.0.3", "LAN"),
            (Host, "::ffff:192.0.2.1", Host, "::ffff:192.0.2.2", "P2P"),
            (Host, "10.0.0.2", Srflx, "198.51.100.2", "P2P"),
            (Relay, "10.0.0.2", Host, "10.0.0.3", "relay"),
            (Host, "10.0.0.2", Relay, "10.0.0.3", "relay"),
        ] {
            assert_eq!(
                connection_route(local_type, local, remote_type, remote),
                expected
            );
        }
    }

    #[test]
    fn unmeasured_video_does_not_turn_an_sr_mapping_into_pipeline_latency() {
        let now = Instant::now();
        let unrelated_capture = now - Duration::from_secs(16_980);
        let no_timing = frame_sender_timing(Some(unrelated_capture), None, Some(7), now);
        assert!(no_timing.capture_at.is_none());
        assert!(no_timing.transport_delay.is_none());
        assert_eq!(no_timing.sending_delay, Some(Duration::from_millis(7)));

        let mut timing = crate::official_receiver::VideoSendTiming {
            flags: 1,
            encode_start_delta_ms: 1,
            encode_finish_delta_ms: 4,
            packetization_finish_delta_ms: 4,
            pacer_exit_delta_ms: 7,
            network_timestamp_delta_ms: 0,
            network2_timestamp_delta_ms: 0,
        };
        let unmeasured = frame_sender_timing(Some(unrelated_capture), Some(timing), None, now);
        assert!(unmeasured.capture_at.is_none());
        assert!(unmeasured.encode_delay.is_none());

        timing.flags |= 4;
        let measured = frame_sender_timing(
            Some(now - Duration::from_millis(12)),
            Some(timing),
            None,
            now,
        );
        assert_eq!(measured.encode_delay, Some(Duration::from_millis(3)));
        assert_eq!(measured.transport_delay, Some(Duration::from_millis(5)));
    }

    #[tokio::test]
    async fn offer_has_remote_codecs_and_data_channels() {
        let peer = NativePeer::new(Vec::new(), TransportChoice::Auto)
            .await
            .unwrap();
        let sdp = peer.create_offer().await.unwrap();
        assert!(sdp.contains("H265/90000"));
        assert!(sdp.contains("H264/90000"));
        assert!(sdp.contains("opus/48000/2"));
        assert_eq!(sdp.matches("m=video").count(), 5);
        assert!(sdp.contains("m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99 100 101 102 35 36"));
        assert_eq!(sdp.matches("a=recvonly").count(), 5);
        // The extra UU metadata must survive allocation beyond the fourteen
        // one-byte IDs, without displacing an existing media extension.
        assert!(sdp.contains("a=extmap-allow-mixed"));
        for video in sdp.split("m=video").skip(1) {
            let section = video.split("\r\nm=").next().unwrap();
            assert!(section.contains(&format!("a=extmap:15 {VIDEO_IS_NEW_FRAME_URI}")));
            assert_eq!(section.matches("a=extmap:").count(), 14);
        }
        let audio_section = sdp
            .split("m=video")
            .next()
            .expect("offer must contain its audio section");
        assert!(audio_section.contains("a=sendrecv"));
        let mut repeated = sdp.clone();
        apply_uu_application_attributes(&mut repeated).unwrap();
        assert_eq!(repeated, sdp);
        assert!(sdp.contains("minptime=10;stereo=1;useinbandfec=1"));
        assert!(sdp.contains("a=fmtp:97 apt=96"));
        assert!(sdp.contains("a=fmtp:99 apt=98"));
        assert!(sdp.contains("a=rtpmap:36 rs-fec-cm256/90000"));
        assert!(sdp.contains("a=rtpmap:35 flexfec-03/90000"));
        assert!(sdp.contains("a=rtpmap:100 red/90000"));
        assert!(sdp.contains("a=rtpmap:101 rtx/90000"));
        assert!(sdp.contains("a=fmtp:101 apt=100"));
        assert!(sdp.contains("a=rtpmap:102 ulpfec/90000"));
        assert!(sdp.contains("a=fmtp:36 max-k=109;repair-window=10000000;rtx-as-source=1"));
        assert!(sdp.contains("webrtc-datachannel"));
        assert!(sdp.contains("a=msid-semantic: WMS audio_0"));
        assert!(sdp.contains("a=msid:audio_0 audio_0"));
        assert!(sdp.contains("a=max-message-size:524288"));
        assert!(sdp.contains("a=x-uuremote-mix-kcp:2"));
        assert!(!sdp.contains("a=x-uuremote-quic"));
        assert!(!sdp.contains("VP8/90000"));
        assert!(!sdp.contains("VP9/90000"));
        assert!(!sdp.contains("AV1/90000"));
        peer.close().await.unwrap();
    }

    #[test]
    fn nack_window_is_wrap_aware() {
        let mut requester = NackRequester::new();
        requester.on_received(u16::MAX - 1, false, false);
        requester.on_received(0, false, false);
        requester.on_received(2, false, false);

        assert!(requester.entries.contains_key(&u16::MAX));
        assert!(requester.entries.contains_key(&1));

        requester.clear_up_to(0);
        assert!(!requester.entries.contains_key(&u16::MAX));
        assert!(requester.entries.contains_key(&1));
    }
}
