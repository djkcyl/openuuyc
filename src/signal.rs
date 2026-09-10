//! Minimal Engine.IO v4 / Socket.IO v4 framing used by the native signal client.
//!
//! Network I/O is intentionally separate from this codec so captured frames can
//! be regression-tested without contacting a live account or storing credentials.

use std::{
    collections::VecDeque,
    future::pending,
    io::{Read, Write},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use flate2::{Compression, GzBuilder, read::GzDecoder};
use serde::Deserialize;
use serde_json::Value;
use tokio::{sync::oneshot, time::timeout};
use tokio_tungstenite::tungstenite::{
    Message,
    client::IntoClientRequest,
    http::{HeaderName, HeaderValue, Request},
};
use url::Url;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

use crate::{
    api::RoomSession, control::build_control_frames, decoder::detect_native_decoder_support,
    media::ConnectionMediaProfile, performance::RemoteSenderInfo, rtc::NativePeer,
};

pub const KNOWN_EVENTS: [&str; 11] = [
    "room_info",
    "control",
    "soac",
    "forward_setting",
    "refresh_reconnect_key",
    "update_geo_ip",
    "leave",
    "released",
    "left",
    "bmsg_push",
    "publisher_disconnect",
];

pub const AUTH_HEADER: &str = "X-NRD-AUTH";
pub const RECONNECT_HEADER: &str = "X-NRD-RECONN-KEY";
pub const CONTROLLING_HEADER: &str = "X-NRD-CONTROLLING";
const STREAMER_VERSION_HEADER: &str = "streamer_version";
const STREAMER_FLAG_HEADER: &str = "streamer_flag";
const STREAMER_VERSION: &str = "V4.5.9";
const STREAMER_FLAG: &str = r#"{"sdp_flags":{"gzip_sdp":true}}"#;
const ROOM_INFO_ACK_ID: u64 = 1;
const CONTROL_ACK_ID: u64 = 2;
const ROOM_INFO_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const WEBRTC_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const RECONNECT_KEY_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const RECONNECT_KEY_MAX_AGE: Duration = Duration::from_secs(300);
const RECONNECT_KEY_ACK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SDP_BYTES: u64 = 1024 * 1024;
mod tls;
mod transport;
use transport::SignalTransport;
pub(crate) use transport::{SignalFailure, SocketState};

pub struct SignalSession {
    transport: SignalTransport,
    room_info: Value,
    reconnect_key: Option<String>,
    last_reconnect_key_refresh: Option<Instant>,
    next_refresh_ack_id: u64,
    deferred: VecDeque<EnginePacket>,
}

// Owned by the socket worker, never by a cancellable caller's read future.
struct PendingBinaryEvent {
    namespace: String,
    id: Option<u64>,
    event: String,
    args: Vec<Value>,
    expected_attachments: usize,
    attachments: Vec<Vec<u8>>,
}

#[derive(Default)]
struct NegotiationProgress {
    local_candidates_sent: usize,
    remote_candidates_installed: usize,
    remote_soac_events: usize,
    answer_installed: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum NegotiationEvent {
    OfferCreated {
        sdp_bytes: usize,
        compressed_bytes: usize,
    },
    OfferSent,
    LocalCandidateSent {
        count: usize,
    },
    AnswerInstalled {
        sdp_bytes: usize,
    },
    RemoteCandidateInstalled {
        count: usize,
    },
    Connected,
}

type NegotiationReporter<'a> = &'a (dyn Fn(NegotiationEvent) + Send + Sync);

fn report_negotiation(reporter: Option<NegotiationReporter<'_>>, event: NegotiationEvent) {
    if let Some(reporter) = reporter {
        reporter(event);
    }
}

fn handle_signal_app_data(args: &[Value], binary: &[Vec<u8>], peer: &NativePeer) -> bool {
    let Some(data) = args.first().and_then(|value| value.get("data")) else {
        return false;
    };
    if data.get("type").and_then(Value::as_str) != Some("signal_app_data") {
        return false;
    }
    let payload = data.get("signal_app_data");
    let attachment = payload
        .and_then(|value| value.get("binary_data"))
        .filter(|value| value.get("_placeholder").and_then(Value::as_bool) == Some(true))
        .and_then(|value| value.get("num"))
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| binary.get(index));
    if let Some(attachment) = attachment {
        // GameViewer F95A80 filters by the current SDK session handle, not by
        // ice_id. This SignalSession owns exactly that room generation.
        if let Err(error) = peer
            .stream_control_handle()
            .handle_protocol_message(attachment, crate::stream_control::PbMessageSource::Signal)
        {
            tracing::warn!(%error, bytes = attachment.len(), "invalid protobuf signal_app_data");
        }
    } else {
        tracing::warn!("signal_app_data omitted its binary attachment");
    }
    true
}

/// Tracks completion of a received switch request for diagnostics. Duplicate
/// suppression belongs to the remote sender-side monitor, not this state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingNetworkSwitch {
    attempt_switch_type: u8,
}

fn relay_protocol_is_tls(protocol: &str) -> bool {
    protocol.eq_ignore_ascii_case("tls")
}

fn selected_transport_matches_switch(
    selected: Option<(bool, String, String)>,
    attempt_switch_type: u8,
) -> bool {
    let Some((is_relay, local_protocol, remote_protocol)) = selected else {
        return false;
    };
    if !is_relay {
        return false;
    }
    match attempt_switch_type {
        2 => relay_protocol_is_tls(&local_protocol) && relay_protocol_is_tls(&remote_protocol),
        _ => true,
    }
}

fn apply_device_capability(
    capability: crate::capability::DeviceCapability,
    control: &ControlSessionInfo,
    peer: &NativePeer,
) {
    if capability.ice_id != control.ice_id {
        tracing::warn!(ice_id = %capability.ice_id, "ignored capabilities for another ICE session");
        return;
    }
    peer.stream_control_handle()
        .set_capability(crate::capability::DualCapability::negotiate(
            &control.local_capability,
            capability,
        ));
}

fn handle_device_capability(
    args: &[Value],
    control: &ControlSessionInfo,
    peer: &NativePeer,
) -> bool {
    match parse_forward_setting(args, control) {
        Ok(Some(ForwardSettingEvent::DeviceCapability(capability))) => {
            apply_device_capability(capability, control, peer);
            true
        }
        _ => false,
    }
}

enum ForwardSettingEvent {
    DeviceCapability(crate::capability::DeviceCapability),
    SenderParaInfo {
        ice_id: String,
        infos: Vec<RemoteSenderInfo>,
    },
    SwitchNetwork {
        transport_type: u8,
        ice_id: String,
        attempt_switch_type: u8,
    },
}

fn parse_forward_setting(
    args: &[Value],
    expected: &ControlSessionInfo,
) -> Result<Option<ForwardSettingEvent>> {
    let Some(envelope) = args.first() else {
        return Ok(None);
    };
    if envelope
        .get("client_id")
        .and_then(Value::as_str)
        .is_some_and(|client_id| client_id != expected.client_id)
    {
        return Ok(None);
    }
    let data = envelope
        .get("data")
        .context("forward_setting omitted data")?;
    match data.get("type").and_then(Value::as_str) {
        Some("device_capability") => {
            let payload = data
                .get("device_capability")
                .context("device_capability event omitted payload")?;
            Ok(Some(ForwardSettingEvent::DeviceCapability(
                serde_json::from_value(payload.clone()).context("decode device_capability")?,
            )))
        }
        Some("sender_para_info") => {
            let payload = data
                .get("sender_para_info")
                .context("sender_para_info event omitted payload")?;
            let ice_id = payload
                .get("ice_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if ice_id != expected.ice_id {
                return Ok(None);
            }
            // AA2390: a nonempty array takes precedence; an empty/missing one
            // uses the legacy flat entry. No merge of contradictory shapes.
            let entries = payload
                .get("sender_media_infos")
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty());
            let parse = |entry: &Value| RemoteSenderInfo {
                video_track_index: entry
                    .get("video_track_index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                capture_impl: entry
                    .get("capture_impl")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                encoder_impl: entry
                    .get("encoder_impl")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            };
            let infos = entries.map_or_else(
                || vec![parse(payload)],
                |entries| entries.iter().map(parse).collect(),
            );
            Ok(Some(ForwardSettingEvent::SenderParaInfo {
                ice_id: ice_id.to_owned(),
                infos,
            }))
        }
        Some("switch_network_notify") => {
            let payload = data
                .get("switch_network_notify")
                .context("switch_network_notify event omitted payload")?;
            let transport_type = payload
                .get("transport_type")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or_default();
            let attempt_switch_type = payload
                .get("attempt_switch_type")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or_default();
            Ok(Some(ForwardSettingEvent::SwitchNetwork {
                transport_type,
                ice_id: payload
                    .get("ice_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                attempt_switch_type,
            }))
        }
        _ => Ok(None),
    }
}

#[derive(Clone, Deserialize)]
pub struct ControlSessionInfo {
    pub code: i64,
    pub client_id: String,
    pub ice_id: String,
    #[serde(rename = "iceServers", default)]
    pub ice_servers: Vec<ControlIceServer>,
    #[serde(default, alias = "forceRelay")]
    pub force_relay: bool,
    #[serde(default)]
    pub auto_switch_network: bool,
    #[serde(default)]
    pub possible_auto_switch_pkt_loss: u32,
    #[serde(default)]
    pub possible_auto_switch_latency: u32,
    #[serde(default)]
    pub possible_auto_switch_min_latency: u32,
    #[serde(default)]
    pub force_auto_switch_pkt_loss: u32,
    #[serde(default)]
    pub force_auto_switch_latency: u32,
    #[serde(skip)]
    pub app_control_id: String,
    #[serde(skip)]
    pub(crate) local_capability: crate::capability::DeviceCapability,
}

#[derive(Clone, Deserialize)]
pub struct ControlIceServer {
    pub urls: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

impl std::fmt::Debug for ControlSessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlSessionInfo")
            .field("code", &self.code)
            .field("client_id", &"***REDACTED***")
            .field("ice_id", &"***REDACTED***")
            .field("ice_server_count", &self.ice_servers.len())
            .field("force_relay", &self.force_relay)
            .field("auto_switch_network", &self.auto_switch_network)
            .field(
                "possible_auto_switch_pkt_loss",
                &self.possible_auto_switch_pkt_loss,
            )
            .field(
                "possible_auto_switch_latency",
                &self.possible_auto_switch_latency,
            )
            .field(
                "possible_auto_switch_min_latency",
                &self.possible_auto_switch_min_latency,
            )
            .field(
                "force_auto_switch_pkt_loss",
                &self.force_auto_switch_pkt_loss,
            )
            .field("force_auto_switch_latency", &self.force_auto_switch_latency)
            .field("app_control_id", &"***REDACTED***")
            .finish()
    }
}

impl std::fmt::Debug for ControlIceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlIceServer")
            .field("urls", &"***REDACTED***")
            .field("username", &"***REDACTED***")
            .field("credential", &"***REDACTED***")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalRole {
    Host,
    Controller,
}

pub(crate) type SignalPushHandler = std::sync::Arc<dyn Fn(&Value) + Send + Sync>;

impl SignalSession {
    pub(crate) fn socket_state(&self) -> tokio::sync::watch::Receiver<SocketState> {
        self.transport.state_receiver()
    }

    pub async fn connect(room: RoomSession, role: SignalRole) -> Result<Self> {
        Self::connect_cancellable(room, role, &tokio_util::sync::CancellationToken::new()).await
    }

    pub(crate) async fn connect_cancellable(
        room: RoomSession,
        role: SignalRole,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        Self::connect_observed(room, role, cancel, None).await
    }

    pub(crate) async fn connect_observed(
        room: RoomSession,
        role: SignalRole,
        cancel: &tokio_util::sync::CancellationToken,
        push: Option<SignalPushHandler>,
    ) -> Result<Self> {
        room.validate()?;
        let connect_timeout = Duration::from_millis(room.ws_connect_timeout_ms.max(100) as u64);
        let mut session = Self {
            transport: SignalTransport::spawn(room, role, push),
            room_info: Value::Null,
            reconnect_key: None,
            last_reconnect_key_refresh: None,
            next_refresh_ack_id: 3,
            deferred: VecDeque::new(),
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow!("signaling connection cancelled")),
            result = async {
            timeout(connect_timeout, session.wait_namespace())
                .await.map_err(|_| SignalFailure::Socket("initial connection deadline expired (2009)".into()))??;
            timeout(ROOM_INFO_TIMEOUT, session.login_room())
                .await.map_err(|_| SignalFailure::Socket("room_info ACK timed out (2012)".into()))??;
            Ok(())
            } => result,
        };
        if let Err(error) = result {
            let _ = session.graceful_close().await;
            return Err(error);
        }
        tracing::debug!(room_info = %session.room_info, ?role, "signaling room login complete");
        Ok(session)
    }

    async fn wait_namespace(&mut self) -> Result<()> {
        loop {
            match self.transport.next_packet().await? {
                EnginePacket::Message(SocketPacket::Connect { namespace, .. })
                    if namespace == "/" =>
                {
                    return Ok(());
                }
                packet => self.deferred.push_back(packet),
            }
        }
    }

    async fn login_room(&mut self) -> Result<()> {
        self.send_text(encode_event("room_info", &[], Some(ROOM_INFO_ACK_ID))?)
            .await?;
        loop {
            match self.transport.next_packet().await? {
                EnginePacket::Message(SocketPacket::Ack { id, args, .. })
                    if id == ROOM_INFO_ACK_ID =>
                {
                    let info = args
                        .first()
                        .and_then(Value::as_object)
                        .filter(|info| !info.is_empty())
                        .ok_or_else(|| {
                            SignalFailure::Socket("room_info ACK is empty or invalid (2011)".into())
                        })?;
                    self.room_info = Value::Object(info.clone());
                    return Ok(());
                }
                packet => self.deferred.push_back(packet),
            }
        }
    }

    pub(crate) async fn start_control(
        &mut self,
        controller_device_id: &str,
        profile: ConnectionMediaProfile,
    ) -> Result<ControlSessionInfo> {
        tracing::debug!(?profile, "starting control handshake");
        let decoder_support = detect_native_decoder_support(profile)?;
        tracing::info!(
            ?decoder_support,
            "advertising native decoder configuration capabilities"
        );
        let frames = build_control_frames(
            controller_device_id,
            CONTROL_ACK_ID,
            &decoder_support,
            profile,
        )?;
        let app_control_id = frames.app_control_id;
        self.send_binary_packet(frames.header, frames.attachment)?;

        let mut info = timeout(CONTROL_TIMEOUT, self.wait_control_ack())
            .await
            .context("control handshake timed out")??;
        info.app_control_id = app_control_id;
        info.local_capability = decoder_support;
        tracing::debug!(
            ice_server_count = info.ice_servers.len(),
            force_relay = info.force_relay,
            auto_switch_network = info.auto_switch_network,
            possible_auto_switch_pkt_loss = info.possible_auto_switch_pkt_loss,
            possible_auto_switch_latency = info.possible_auto_switch_latency,
            possible_auto_switch_min_latency = info.possible_auto_switch_min_latency,
            force_auto_switch_pkt_loss = info.force_auto_switch_pkt_loss,
            force_auto_switch_latency = info.force_auto_switch_latency,
            "control handshake accepted"
        );
        Ok(info)
    }

    pub(crate) async fn negotiate(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        reporter: Option<NegotiationReporter<'_>>,
    ) -> Result<()> {
        let mut local_candidates = peer.local_ice_candidates();
        let mut progress = NegotiationProgress::default();
        let offer = peer.create_offer().await?;
        tracing::trace!(sdp = %offer, "local WebRTC offer");
        let attachment = gzip_sdp(&offer)?;
        tracing::debug!(
            sdp_bytes = offer.len(),
            gzip_bytes = attachment.len(),
            "sending WebRTC offer"
        );
        report_negotiation(
            reporter,
            NegotiationEvent::OfferCreated {
                sdp_bytes: offer.len(),
                compressed_bytes: attachment.len(),
            },
        );
        let payload = serde_json::json!({
            "client_id": info.client_id,
            "data": {
                "app_control_id": info.app_control_id,
                "gzip_sdp": { "_placeholder": true, "num": 0 },
                "ice_id": info.ice_id,
                "ice_network_type": 3,
                "sdp": "",
                "type": "offer"
            }
        });
        let header = encode_binary_event("soac", &[payload], None, 1)?;
        self.send_binary_packet(header, attachment)?;
        report_negotiation(reporter, NegotiationEvent::OfferSent);

        match timeout(
            WEBRTC_CONNECT_TIMEOUT,
            self.exchange_ice(info, peer, &mut local_candidates, &mut progress, reporter),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let ice = peer.ice_diagnostics().await;
                tracing::error!(
                    peer_state = %peer.connection_state(),
                    ice_state = %peer.ice_connection_state(),
                    local_candidates = progress.local_candidates_sent,
                    remote_soac_events = progress.remote_soac_events,
                    answer_installed = progress.answer_installed,
                    diagnostics = %ice,
                    "WebRTC connection timed out"
                );
                Err(SignalFailure::Peer(format!(
                    "WebRTC did not connect within the official 12-second window (peer: {}, ICE: {}, local candidates sent: {}, remote soac events: {}, answer installed: {}; {})",
                    peer.connection_state(),
                    peer.ice_connection_state(),
                    progress.local_candidates_sent,
                    progress.remote_soac_events,
                    progress.answer_installed,
                    ice
                )).into())
            }
        }
    }

    async fn restart_ice(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        ice_network_type: u8,
    ) -> Result<()> {
        let offer = peer.create_restart_offer().await?;
        let attachment = gzip_sdp(&offer)?;
        let payload = serde_json::json!({
            "client_id": info.client_id,
            "data": {
                "app_control_id": info.app_control_id,
                "gzip_sdp": { "_placeholder": true, "num": 0 },
                "ice_id": info.ice_id,
                "ice_network_type": ice_network_type,
                "sdp": "",
                "type": "restart_ice"
            }
        });
        self.send_binary_packet(
            encode_binary_event("soac", &[payload], None, 1)?,
            attachment,
        )?;
        tracing::info!(
            "ICE restart offer sent; the persistent SOAC loop will install the answer and candidates while the old selected path remains active"
        );
        Ok(())
    }

    async fn apply_official_network_switch(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        transport_type: u8,
        attempt_switch_type: u8,
    ) -> Result<()> {
        peer.switch_ice_network(transport_type, attempt_switch_type)
            .await?;
        self.restart_ice(info, peer, transport_type).await
    }

    async fn send_soac_candidate(
        &mut self,
        info: &ControlSessionInfo,
        candidate: &RTCIceCandidateInit,
        relay_only: bool,
    ) -> Result<()> {
        if candidate.candidate.is_empty() {
            return Ok(());
        }
        if relay_only
            && !candidate
                .candidate
                .to_ascii_lowercase()
                .contains(" typ relay")
        {
            return Ok(());
        }
        tracing::trace!(candidate = ?candidate, "sending local ICE candidate");
        let sdp_mid = candidate
            .sdp_mid
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("0");
        let payload = serde_json::json!({
            "client_id": info.client_id,
            "data": {
                "app_control_id": info.app_control_id,
                "candidate": {
                    "candidate": candidate.candidate,
                    "sdpMid": sdp_mid,
                    "sdpMLineIndex": candidate.sdp_mline_index,
                    "usernameFragment": candidate.username_fragment
                },
                "ice_id": info.ice_id,
                "ice_network_type": 0,
                "type": "candidate"
            }
        });
        self.send_text(encode_event("soac", &[payload], None)?)
            .await
    }

    async fn exchange_ice(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        local_candidates: &mut tokio::sync::broadcast::Receiver<Option<RTCIceCandidateInit>>,
        progress: &mut NegotiationProgress,
        reporter: Option<NegotiationReporter<'_>>,
    ) -> Result<()> {
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

        let mut local_gathering_complete = false;
        let mut answer_installed = false;
        let mut pending_remote_candidates = Vec::new();
        let mut pending_events = VecDeque::new();

        loop {
            if peer.connection_state() == RTCPeerConnectionState::Connected {
                self.deferred.append(&mut pending_events);
                self.transport.set_controlling(true);
                report_negotiation(reporter, NegotiationEvent::Connected);
                return Ok(());
            }
            tokio::select! {
                candidate = local_candidates.recv(), if !local_gathering_complete => {
                    match candidate {
                        Ok(Some(candidate)) => {
                            self.send_soac_candidate(info, &candidate, false).await?;
                            progress.local_candidates_sent += 1;
                            report_negotiation(
                                reporter,
                                NegotiationEvent::LocalCandidateSent {
                                    count: progress.local_candidates_sent,
                                },
                            );
                        }
                        Ok(None) => local_gathering_complete = true,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "local ICE candidate broadcast lagged during negotiation");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            local_gathering_complete = true;
                        }
                    }
                },
                packet = self.next_packet() => match packet? {
                    EnginePacket::Message(SocketPacket::Event { namespace, id, event, args, binary })
                        if event == "forward_setting" =>
                    {
                        if !handle_signal_app_data(&args, &binary, peer)
                            && !handle_device_capability(&args, info, peer) {
                            pending_events.push_back(EnginePacket::Message(SocketPacket::Event {
                                namespace, id, event, args, binary,
                            }));
                        }
                    }
                    EnginePacket::Message(SocketPacket::Event { event, args, .. })
                        if event == "soac" =>
                    {
                        progress.remote_soac_events += 1;
                        tracing::trace!(args = ?args, "received remote SOAC event");
                        match parse_remote_soac(&args, Some(info))? {
                            Some(RemoteSoac::Answer { sdp, restart_ice }) => {
                                tracing::debug!(restart_ice, sdp_bytes = sdp.len(), "installing remote WebRTC answer");
                                tracing::trace!(sdp = %sdp, "remote WebRTC answer");
                                let sdp_bytes = sdp.len();
                                peer.set_remote_answer(sdp, restart_ice).await?;
                                answer_installed = true;
                                progress.answer_installed = true;
                                report_negotiation(
                                    reporter,
                                    NegotiationEvent::AnswerInstalled { sdp_bytes },
                                );
                                for candidate in pending_remote_candidates.drain(..) {
                                    peer.add_remote_candidate(candidate).await?;
                                    progress.remote_candidates_installed += 1;
                                    report_negotiation(
                                        reporter,
                                        NegotiationEvent::RemoteCandidateInstalled {
                                            count: progress.remote_candidates_installed,
                                        },
                                    );
                                }
                            }
                            Some(RemoteSoac::Candidate(candidate)) if answer_installed => {
                                tracing::trace!(candidate = ?candidate, "installing remote ICE candidate");
                                peer.add_remote_candidate(candidate).await?;
                                progress.remote_candidates_installed += 1;
                                report_negotiation(
                                    reporter,
                                    NegotiationEvent::RemoteCandidateInstalled {
                                        count: progress.remote_candidates_installed,
                                    },
                                );
                            }
                            Some(RemoteSoac::Candidate(candidate)) => {
                                tracing::trace!(candidate = ?candidate, "queueing remote ICE candidate before answer");
                                pending_remote_candidates.push(candidate);
                            }
                            None => {}
                        }
                    }
                    EnginePacket::Ping(payload) => self.send_text(encode_pong(&payload)).await?,
                    EnginePacket::Close | EnginePacket::Message(SocketPacket::Disconnect { .. }) => {
                        bail!("signaling server closed during WebRTC negotiation")
                    }
                    EnginePacket::Message(SocketPacket::ConnectError { .. }) => {
                        bail!("signaling server rejected WebRTC negotiation")
                    }
                    _ => {}
                },
                state = peer.next_connection_state() => match state {
                    Some(RTCPeerConnectionState::Connected) => {
                        self.deferred.append(&mut pending_events);
                        self.transport.set_controlling(true);
                        report_negotiation(reporter, NegotiationEvent::Connected);
                        return Ok(())
                    },
                    Some(RTCPeerConnectionState::Failed) => {
                        return Err(SignalFailure::Peer("native WebRTC peer failed during negotiation".into()).into());
                    }
                    Some(RTCPeerConnectionState::Closed) => {
                        return Err(SignalFailure::Peer("native WebRTC peer closed during negotiation".into()).into());
                    }
                    None => bail!("native WebRTC connection state stream closed"),
                    Some(_) => {}
                }
            }
        }
    }

    async fn wait_control_ack(&mut self) -> Result<ControlSessionInfo> {
        loop {
            match self.transport.next_packet().await? {
                EnginePacket::Message(SocketPacket::Ack { id, args, .. })
                    if id == CONTROL_ACK_ID =>
                {
                    if args.len() < 2 || args[0].as_str() != Some("success") {
                        bail!("remote device rejected the control handshake");
                    }
                    let info: ControlSessionInfo = serde_json::from_value(args[1].clone())
                        .context("control ACK has an unexpected payload")?;
                    if info.code != 0
                        || info.client_id.is_empty()
                        || info.ice_id.is_empty()
                        || info.ice_servers.is_empty()
                    {
                        bail!("control ACK did not contain a usable session configuration");
                    }
                    return Ok(info);
                }
                EnginePacket::Message(SocketPacket::ConnectError { .. }) => {
                    bail!("Socket.IO rejected the control handshake")
                }
                EnginePacket::Ping(payload) => self.send_text(encode_pong(&payload)).await?,
                packet => self.deferred.push_back(packet),
            }
        }
    }

    pub async fn keep_alive(
        mut self,
        mut shutdown: oneshot::Receiver<()>,
        peer: Option<std::sync::Arc<NativePeer>>,
        control: Option<ControlSessionInfo>,
    ) -> Result<()> {
        let mut next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
        let mut pending_key_refresh: Option<(u64, tokio::time::Instant)> = None;
        let mut local_candidates = peer.as_ref().map(|peer| peer.local_ice_candidates());
        let mut local_candidate_stream_open = local_candidates.is_some();
        let mut active_ice_network_type = 3_u8;
        let mut pending_network_switch = None::<PendingNetworkSwitch>;
        let network_control = peer
            .as_ref()
            .map(|peer| peer.stream_control_handle().network_control());
        let mut manual_network_requests =
            network_control.as_ref().map(|control| control.subscribe());
        let mut network_switch_check = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        network_switch_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let signal_result = tokio::select! {
            biased;
            _ = &mut shutdown => Ok(()),
            result = async {
            if peer.is_some() != control.is_some() { bail!("controller signaling lifecycle is incomplete"); }
            loop {
            tokio::select! {
                changed = async {
                    if let Some(requests) = &mut manual_network_requests { requests.changed().await }
                    else { pending().await }
                }, if manual_network_requests.is_some() => {
                    if changed.is_err() { manual_network_requests = None; continue; }
                    let desired = *manual_network_requests.as_mut().expect("manual request channel").borrow_and_update();
                    let Some(relay) = desired else { continue; };
                    let peer = peer.as_ref().expect("manual switch peer");
                    let info = control.as_ref().expect("manual switch session");
                    let network = network_control.as_ref().expect("manual switch state");
                    if !peer.can_switch_ice_network() || !self.transport.is_connected() {
                        network.fail("当前连接尚未就绪，无法切换线路");
                        continue;
                    }
                    if info.force_relay && !relay {
                        network.fail("本次连接要求使用中转");
                        continue;
                    }
                    // Official manual ABI: enabled -> kRelay/attempt 2 (TURNS),
                    // disabled -> kAll/attempt 3. Existing SOAC owns the answer.
                    let (transport_type, attempt) = if relay { (1, 2) } else { (3, 3) };
                    match self.apply_official_network_switch(info, peer, transport_type, attempt).await {
                        Ok(()) => {
                            peer.accept_manual_network_policy();
                            active_ice_network_type = transport_type;
                            pending_network_switch = None;
                            network.submitted(relay);
                            tracing::info!(relay, "manual network policy submitted");
                        }
                        Err(error) => {
                            tracing::warn!(%error, relay, "manual network switch failed; retaining current path");
                            network.fail("线路切换未完成，可稍后重试");
                        }
                    }
                }
                _ = async {
                    if let Some((_, deadline)) = pending_key_refresh {
                        tokio::time::sleep_until(deadline).await;
                    } else { pending::<()>().await; }
                } => {
                    tracing::warn!("refresh_reconnect_key ACK timed out; keeping the current signaling socket");
                    pending_key_refresh = None;
                    next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
                }
                _ = tokio::time::sleep_until(next_key_check), if pending_key_refresh.is_none() => {
                    // UU's loop starts with an expired refresh age, checks
                    // after 30s, and waits another 30s after each operation
                    // completes. Header receipt is not a successful refresh.
                    if self.transport.is_connected() && self.last_reconnect_key_refresh.is_none_or(|last| last.elapsed() > RECONNECT_KEY_MAX_AGE) {
                        let ack_id = self.next_refresh_ack_id;
                        self.next_refresh_ack_id = ack_id.wrapping_add(1).max(3);
                        let refresh = encode_event(
                            "refresh_reconnect_key",
                            &[],
                            Some(ack_id),
                        )?;
                        if let Err(error) = self.send_text(refresh).await {
                            tracing::warn!(%error, "refresh_reconnect_key request failed; waiting for the socket state machine");
                        } else {
                            pending_key_refresh = Some((ack_id, tokio::time::Instant::now() + RECONNECT_KEY_ACK_TIMEOUT));
                            tracing::debug!(ack_id, "requested signaling reconnect key refresh");
                        }
                    }
                    next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
                }
                candidate = async {
                    local_candidates
                        .as_mut()
                        .expect("controller candidate stream exists")
                        .recv()
                        .await
                }, if local_candidate_stream_open => {
                    match candidate {
                        Ok(Some(candidate)) => {
                            let info = control.as_ref().expect("controller control info exists");
                            self.send_soac_candidate(
                                info,
                                &candidate,
                                active_ice_network_type == 1,
                            )
                            .await?;
                            tracing::debug!("sent continual-gathering local ICE candidate");
                        }
                        Ok(None) => {
                            tracing::debug!("local ICE candidate generation completed");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "continual ICE candidate broadcast lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            local_candidate_stream_open = false;
                        }
                    }
                }
                packet = self.next_packet() => {
                    match packet {
                        Err(error) => break Err(error),
                        Ok(EnginePacket::Message(SocketPacket::Ack { id, args, .. }))
                            if pending_key_refresh.is_some_and(|(pending_id, _)| id == pending_id) =>
                        {
                            if args.len() < 2 || args[0].as_str() != Some("success") {
                                tracing::warn!("refresh_reconnect_key ACK was rejected");
                            } else {
                                if let Some(value) = args[1].get("reconnect_key") {
                                    self.reconnect_key = value.as_str().filter(|key| !key.is_empty()).map(str::to_owned);
                                }
                                // Official success may omit the field when
                                // an existing non-empty key remains valid.
                                if self.reconnect_key.is_some() {
                                    self.transport.set_reconnect_key(self.reconnect_key.clone());
                                    self.last_reconnect_key_refresh = Some(Instant::now());
                                    tracing::debug!(ack_id = id, "signaling reconnect key refreshed");
                                } else {
                                    tracing::warn!(ack_id = id, "refresh_reconnect_key ACK left an empty key");
                                }
                            }
                            pending_key_refresh = None;
                            next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
                        }
                        Ok(EnginePacket::Message(SocketPacket::Event { event, args, .. }))
                            if event == "soac" =>
                        {
                            if let Some(peer) = peer.as_ref() {
                                match parse_remote_soac(&args, control.as_ref())? {
                                    Some(RemoteSoac::Candidate(candidate)) => {
                                        peer.add_remote_candidate(candidate).await?;
                                    }
                                    Some(RemoteSoac::Answer { sdp, restart_ice }) => {
                                        peer.set_remote_answer(sdp, restart_ice).await?;
                                    }
                                    None => {}
                                }
                            }
                        }
                        Ok(EnginePacket::Message(SocketPacket::Event { event, args, binary, .. }))
                            if event == "forward_setting" =>
                        {
                            let Some(peer) = peer.as_ref() else { continue; };
                            if handle_signal_app_data(&args, &binary, peer) { continue; }
                            let control = control.as_ref().expect("controller control info exists");
                            let parsed = match parse_forward_setting(&args, control) {
                                Ok(parsed) => parsed,
                                Err(error) => {
                                    tracing::warn!(%error, "discarded invalid forward_setting event");
                                    continue;
                                }
                            };
                            match parsed {
                                Some(ForwardSettingEvent::DeviceCapability(capability)) => {
                                    apply_device_capability(capability, control, peer);
                                }
                                Some(ForwardSettingEvent::SenderParaInfo { ice_id, infos }) => {
                                    tracing::debug!(%ice_id, ?infos, "official sender_para_info received");
                                    peer.performance_monitor().set_remote_senders(infos);
                                }
                                Some(ForwardSettingEvent::SwitchNetwork {
                                    transport_type,
                                    ice_id,
                                    attempt_switch_type,
                                }) => {
                                    if !peer.allows_network_switch() {
                                        tracing::debug!("ignored switch_network_notify because the user selected P2P-only transport");
                                        continue;
                                    }
                                    if ice_id != control.ice_id {
                                        tracing::warn!(%ice_id, "ignored switch_network_notify for another ICE session");
                                        continue;
                                    }
                                    if transport_type == 0 {
                                        tracing::warn!(transport_type, attempt_switch_type, "ignored invalid switch_network_notify");
                                        continue;
                                    }
                                    if !peer.can_switch_ice_network() {
                                        tracing::debug!(
                                            ice_state = %peer.ice_connection_state(),
                                            peer_state = %peer.connection_state(),
                                            "ignored switch_network_notify until the current ICE connection is established"
                                        );
                                        continue;
                                    }
                                    if let Err(error) = self
                                        .apply_official_network_switch(
                                            control,
                                            peer,
                                            transport_type,
                                            attempt_switch_type,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            %error,
                                            transport_type,
                                            attempt_switch_type,
                                            "official ICE network switch attempt failed; keeping the old selected path"
                                        );
                                        pending_network_switch = None;
                                        continue;
                                    }
                                    peer.performance_monitor()
                                        .record_network_switch_attempt(attempt_switch_type);
                                    active_ice_network_type = transport_type;
                                    pending_network_switch = Some(PendingNetworkSwitch {
                                        attempt_switch_type,
                                    });
                                }
                                None => {}
                            }
                        }
                        Ok(EnginePacket::Ping(payload)) => {
                            self.send_text(encode_pong(&payload)).await?;
                        }
                        Ok(EnginePacket::Close)
                        | Ok(EnginePacket::Message(SocketPacket::Disconnect { .. })) => {
                            break Err(SignalFailure::NamespaceClosed.into());
                        }
                        Ok(EnginePacket::Message(SocketPacket::ConnectError { .. })) => {
                            break Err(anyhow!("signaling server rejected the room session"));
                        }
                        _ => {}
                    }
                }
                _ = network_switch_check.tick(), if peer.is_some() => {
                    let peer = peer.as_ref().expect("controller peer exists");
                    let selected = peer.selected_transport().await;
                    if let Some(network) = &network_control {
                        network.connected(peer.can_switch_ice_network());
                        network.observe_route(selected.as_ref().is_some_and(|s| s.0));
                    }
                    let Some(pending) = pending_network_switch else { continue; };
                    if selected_transport_matches_switch(
                        selected,
                        pending.attempt_switch_type,
                    ) {
                        tracing::info!(
                            attempt_switch_type = pending.attempt_switch_type,
                            "official ICE network switch converged on the requested relay path"
                        );
                        peer.performance_monitor()
                            .record_network_switch_success(pending.attempt_switch_type);
                        pending_network_switch = None;
                    }
                }
                state = async {
                    match peer.as_ref() {
                        Some(peer) => peer.next_connection_state().await,
                        None => pending().await,
                    }
                } => {
                    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                    match state {
                        Some(RTCPeerConnectionState::Connected) => self.transport.set_controlling(true),
                        Some(RTCPeerConnectionState::Disconnected) => {
                            pending_network_switch = None;
                            tracing::warn!(
                                "native WebRTC peer connection temporarily disconnected; preserving the current ICE session"
                            );
                        }
                        Some(RTCPeerConnectionState::Failed) => {
                            break Err(SignalFailure::Peer("native WebRTC ICE connection failed (3021)".into()).into());
                        }
                        Some(RTCPeerConnectionState::Closed) => {
                            self.transport.set_controlling(false);
                            break Err(anyhow!("native WebRTC peer connection closed"));
                        }
                        None if peer.is_some() => {
                            break Err(anyhow!("native WebRTC state stream ended"));
                        }
                        _ => {}
                    }
                }
            }
        }} => result,
        };
        if let Some(network) = &network_control {
            network.close();
        }
        self.transport.set_controlling(false);
        let socket_result = self.graceful_close().await;
        let peer_result = match peer {
            Some(peer) => peer.close().await,
            None => Ok(()),
        };
        signal_result.and(socket_result).and(peer_result)
    }

    pub(crate) async fn close(mut self) -> Result<()> {
        self.graceful_close().await
    }

    async fn next_packet(&mut self) -> Result<EnginePacket> {
        if let Some(packet) = self.deferred.pop_front() {
            return Ok(packet);
        }
        self.transport.next_packet().await
    }

    async fn send_text(&mut self, value: impl Into<String>) -> Result<()> {
        self.transport
            .send(vec![Message::Text(value.into().into())])
    }

    fn send_binary_packet(&mut self, header: String, attachment: Vec<u8>) -> Result<()> {
        let mut packet = Vec::with_capacity(attachment.len() + 1);
        packet.push(0x04);
        packet.extend_from_slice(&attachment);
        self.transport.send(vec![
            Message::Text(header.into()),
            Message::Binary(packet.into()),
        ])
    }

    async fn graceful_close(&mut self) -> Result<()> {
        self.transport.close().await
    }
}

enum RemoteSoac {
    Answer { sdp: String, restart_ice: bool },
    Candidate(RTCIceCandidateInit),
}

fn parse_remote_soac(
    args: &[Value],
    expected: Option<&ControlSessionInfo>,
) -> Result<Option<RemoteSoac>> {
    let Some(data) = args
        .first()
        .and_then(|value| value.get("data"))
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    if let Some(expected) = expected
        && (data
            .get("ice_id")
            .and_then(Value::as_str)
            .is_some_and(|value| value != expected.ice_id.as_str())
            || data
                .get("app_control_id")
                .and_then(Value::as_str)
                .is_some_and(|value| value != expected.app_control_id.as_str()))
    {
        return Ok(None);
    }
    match data.get("type").and_then(Value::as_str) {
        Some(kind @ ("answer" | "restart_ice")) => {
            let sdp = data
                .get("sdp")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .context("soac answer did not contain SDP")?;
            Ok(Some(RemoteSoac::Answer {
                sdp: sdp.to_owned(),
                restart_ice: kind == "restart_ice",
            }))
        }
        Some("candidate") => {
            let candidate = data
                .get("candidate")
                .cloned()
                .context("soac candidate did not contain candidate data")?;
            let candidate = serde_json::from_value(candidate)
                .context("soac candidate has an unexpected payload")?;
            Ok(Some(RemoteSoac::Candidate(candidate)))
        }
        _ => Ok(None),
    }
}

fn signaling_request(
    endpoint: &str,
    token: &str,
    controlling: bool,
    reconnect_key: Option<&str>,
) -> Result<Request<()>> {
    let mut url = if endpoint.contains("://") {
        Url::parse(endpoint)
    } else {
        Url::parse(&format!("wss://{endpoint}"))
    }
    .map_err(|_| anyhow!("room response contained an invalid signaling endpoint"))?;
    if url.scheme() != "wss" || url.host_str().is_none() {
        bail!("room response contained an unsupported signaling endpoint");
    }
    if url.path().is_empty() || url.path() == "/" {
        url.set_path("/socket.io/");
    }
    url.set_query(None);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .to_string();
    url.query_pairs_mut()
        .append_pair("EIO", "4")
        .append_pair("transport", "websocket")
        .append_pair("t", &timestamp);

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| anyhow!("failed to build signaling WebSocket request"))?;
    let headers = request.headers_mut();
    let mut auth =
        HeaderValue::from_str(token).context("room token is not a valid header value")?;
    auth.set_sensitive(true);
    headers.insert(HeaderName::from_static("x-nrd-auth"), auth);
    headers.insert(
        HeaderName::from_static("x-nrd-controlling"),
        HeaderValue::from_static(if controlling { "1" } else { "0" }),
    );
    if let Some(reconnect_key) = reconnect_key.filter(|value| !value.is_empty()) {
        let mut value = HeaderValue::from_str(reconnect_key)
            .context("reconnect key is not a valid header value")?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_static("x-nrd-reconn-key"), value);
    }
    headers.insert(
        HeaderName::from_static(STREAMER_VERSION_HEADER),
        HeaderValue::from_static(STREAMER_VERSION),
    );
    headers.insert(
        HeaderName::from_static(STREAMER_FLAG_HEADER),
        HeaderValue::from_static(STREAMER_FLAG),
    );
    Ok(request)
}

#[derive(Clone, Debug, PartialEq)]
pub enum EnginePacket {
    Open(Value),
    Close,
    Ping(String),
    Pong(String),
    Message(SocketPacket),
    Upgrade,
    Noop,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SocketPacket {
    Connect {
        namespace: String,
        data: Option<Value>,
    },
    Disconnect {
        namespace: String,
    },
    Event {
        namespace: String,
        id: Option<u64>,
        event: String,
        args: Vec<Value>,
        /// Raw Socket.IO attachments, without their Engine.IO 0x04 prefix.
        binary: Vec<Vec<u8>>,
    },
    BinaryEvent {
        namespace: String,
        id: Option<u64>,
        event: String,
        args: Vec<Value>,
        attachments: usize,
    },
    Ack {
        namespace: String,
        id: u64,
        args: Vec<Value>,
    },
    ConnectError {
        namespace: String,
        data: Value,
    },
}

pub fn decode(frame: &str) -> Result<EnginePacket> {
    let (kind, payload) = frame.split_at_checked(1).context("empty Engine.IO frame")?;
    match kind {
        "0" => Ok(EnginePacket::Open(parse_json(payload, "Engine.IO open")?)),
        "1" if payload.is_empty() => Ok(EnginePacket::Close),
        "2" => Ok(EnginePacket::Ping(payload.to_owned())),
        "3" => Ok(EnginePacket::Pong(payload.to_owned())),
        "4" => Ok(EnginePacket::Message(decode_socket(payload)?)),
        "5" if payload.is_empty() => Ok(EnginePacket::Upgrade),
        "6" if payload.is_empty() => Ok(EnginePacket::Noop),
        _ => bail!("unsupported Engine.IO packet"),
    }
}

pub fn encode_pong(payload: &str) -> String {
    format!("3{payload}")
}

pub fn encode_event(event: &str, args: &[Value], id: Option<u64>) -> Result<String> {
    if event.is_empty() {
        bail!("Socket.IO event name cannot be empty");
    }
    let mut values = Vec::with_capacity(args.len() + 1);
    values.push(Value::String(event.to_owned()));
    values.extend_from_slice(args);
    let id = id.map(|value| value.to_string()).unwrap_or_default();
    Ok(format!("42{id}{}", serde_json::to_string(&values)?))
}

fn encode_binary_event(
    event: &str,
    args: &[Value],
    id: Option<u64>,
    attachment_count: usize,
) -> Result<String> {
    if attachment_count == 0 {
        bail!("Socket.IO binary event needs at least one attachment");
    }
    if event.is_empty() {
        bail!("Socket.IO event name cannot be empty");
    }
    let mut values = Vec::with_capacity(args.len() + 1);
    values.push(Value::String(event.to_owned()));
    values.extend_from_slice(args);
    let id = id.map(|value| value.to_string()).unwrap_or_default();
    Ok(format!(
        "45{attachment_count}-{id}{}",
        serde_json::to_string(&values)?
    ))
}

fn decode_socket(input: &str) -> Result<SocketPacket> {
    let (kind, mut rest) = input
        .split_at_checked(1)
        .context("empty Socket.IO packet")?;
    let namespace = if rest.starts_with('/') {
        let comma = rest
            .find(',')
            .context("Socket.IO namespace is missing comma")?;
        let namespace = rest[..comma].to_owned();
        rest = &rest[comma + 1..];
        namespace
    } else {
        "/".to_owned()
    };

    match kind {
        "0" => Ok(SocketPacket::Connect {
            namespace,
            data: (!rest.is_empty())
                .then(|| parse_json(rest, "Socket.IO connect"))
                .transpose()?,
        }),
        "1" if rest.is_empty() => Ok(SocketPacket::Disconnect { namespace }),
        "2" => {
            let (id, json) = split_ack_id(rest);
            let (event, args) = parse_event_array(json)?;
            Ok(SocketPacket::Event {
                namespace,
                id,
                event,
                args,
                binary: Vec::new(),
            })
        }
        "3" => {
            let (id, json) = split_ack_id(rest);
            let id = id.context("Socket.IO ACK has no id")?;
            Ok(SocketPacket::Ack {
                namespace,
                id,
                args: parse_array(json, "Socket.IO ACK")?,
            })
        }
        "4" => Ok(SocketPacket::ConnectError {
            namespace,
            data: parse_json(rest, "Socket.IO connect error")?,
        }),
        "5" => {
            let dash = rest
                .find('-')
                .context("Socket.IO binary event has no attachment delimiter")?;
            let attachments = rest[..dash]
                .parse::<usize>()
                .context("Socket.IO binary event has an invalid attachment count")?;
            if attachments == 0 {
                bail!("Socket.IO binary event has no attachments");
            }
            rest = &rest[dash + 1..];
            let namespace = if rest.starts_with('/') {
                let comma = rest
                    .find(',')
                    .context("Socket.IO namespace is missing comma")?;
                let namespace = rest[..comma].to_owned();
                rest = &rest[comma + 1..];
                namespace
            } else {
                namespace
            };
            let (id, json) = split_ack_id(rest);
            let (event, args) = parse_event_array(json)?;
            Ok(SocketPacket::BinaryEvent {
                namespace,
                id,
                event,
                args,
                attachments,
            })
        }
        _ => bail!("unsupported Socket.IO packet"),
    }
}

fn parse_event_array(input: &str) -> Result<(String, Vec<Value>)> {
    let values = parse_array(input, "Socket.IO event")?;
    let (first, args) = values
        .split_first()
        .context("Socket.IO event array is empty")?;
    let event = first
        .as_str()
        .context("Socket.IO event name is not a string")?
        .to_owned();
    Ok((event, args.to_vec()))
}

fn split_ack_id(input: &str) -> (Option<u64>, &str) {
    let digit_count = input.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return (None, input);
    }
    let (id, rest) = input.split_at(digit_count);
    (id.parse().ok(), rest)
}

fn parse_json(input: &str, description: &str) -> Result<Value> {
    serde_json::from_str(input).with_context(|| format!("invalid {description} JSON"))
}

fn parse_array(input: &str, description: &str) -> Result<Vec<Value>> {
    parse_json(input, description)?
        .as_array()
        .cloned()
        .with_context(|| format!("{description} payload is not an array"))
}

fn gzip_sdp(sdp: &str) -> Result<Vec<u8>> {
    let mut encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::new(6));
    encoder
        .write_all(sdp.as_bytes())
        .context("compress controller SDP")?;
    let compressed = encoder
        .finish()
        .context("finish controller SDP compression")?;
    Ok(compressed)
}

fn gunzip_sdp(attachment: &[u8]) -> Result<String> {
    let attachment = if attachment.starts_with(&[0x04, 0x1f, 0x8b]) {
        &attachment[1..]
    } else {
        attachment
    };
    if !attachment.starts_with(&[0x1f, 0x8b]) {
        bail!("soac SDP attachment is not gzip data");
    }
    let mut decoder = GzDecoder::new(attachment).take(MAX_SDP_BYTES + 1);
    let mut bytes = Vec::new();
    decoder
        .read_to_end(&mut bytes)
        .context("decompress remote SDP")?;
    if bytes.len() as u64 > MAX_SDP_BYTES {
        bail!("remote SDP exceeds the safety limit");
    }
    String::from_utf8(bytes).context("remote SDP is not UTF-8")
}

fn resolve_binary_event(
    namespace: String,
    id: Option<u64>,
    event: String,
    mut args: Vec<Value>,
    attachments: &[Vec<u8>],
) -> Result<EnginePacket> {
    for argument in &mut args {
        hydrate_gzip_sdp(argument, attachments)?;
    }
    Ok(EnginePacket::Message(SocketPacket::Event {
        namespace,
        id,
        event,
        args,
        binary: attachments
            .iter()
            .map(|attachment| {
                attachment
                    .strip_prefix(&[0x04])
                    .map(<[u8]>::to_vec)
                    .context("Socket.IO attachment omitted Engine.IO binary prefix")
            })
            .collect::<Result<_>>()?,
    }))
}

fn hydrate_gzip_sdp(value: &mut Value, attachments: &[Vec<u8>]) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                hydrate_gzip_sdp(value, attachments)?;
            }
        }
        Value::Object(object) => {
            let attachment_index = object
                .get("gzip_sdp")
                .and_then(Value::as_object)
                .filter(|placeholder| {
                    placeholder.get("_placeholder").and_then(Value::as_bool) == Some(true)
                })
                .and_then(|placeholder| placeholder.get("num"))
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            if let Some(index) = attachment_index {
                let attachment = attachments
                    .get(index)
                    .context("soac SDP placeholder references a missing attachment")?;
                let sdp = gunzip_sdp(attachment)?;
                object.insert("sdp".to_owned(), Value::String(sdp));
                object.remove("gzip_sdp");
            }
            for value in object.values_mut() {
                hydrate_gzip_sdp(value, attachments)?;
            }
        }
        _ => {}
    }
    Ok(())
}
