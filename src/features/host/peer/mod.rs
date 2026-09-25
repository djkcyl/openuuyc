//! Controlled desktop connection: session-scoped peer, capture owner and RTP sender.
use super::{VideoConfig, capture, lock};
use crate::transport::rtc::ConnectionCore;
use anyhow::{Context, Result};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::{
    api::media_engine::MediaEngine,
    data_channel::RTCDataChannel,
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        policy::ice_transport_policy::RTCIceTransportPolicy,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::{
        RTCPFeedback,
        rtp_codec::{
            RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability,
            RTPCodecType,
        },
    },
    track::track_local::{TrackLocal, track_local_static_rtp::TrackLocalStaticRTP},
};

pub(crate) struct Peer {
    core: ConnectionCore,
    pub candidates: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    screens: Arc<tokio::sync::Mutex<screens::Screens>>,
    reports: Arc<screens::Reports>,
    handle: super::SessionLease,
    control_receiver: crate::transport::uu_kcp::ControlReceiver,
    remote_ice: tokio::sync::Mutex<RemoteIce>,
    transport: super::transport::Transport,
    ice_servers: Vec<RTCIceServer>,
    relay_only: AtomicBool,
}

#[derive(Default)]
struct RemoteIce {
    current: std::collections::HashSet<String>,
    retired: std::collections::HashSet<String>,
    pending: std::collections::VecDeque<RTCIceCandidateInit>,
}

#[derive(Clone, PartialEq, Eq)]
struct Published {
    screen: capture::Screen,
    capturing: bool,
    visible: bool,
    quality: i32,
    fps: u32,
    encoder: Option<super::format::Backend>,
    capture: String,
}
#[derive(Clone, Default)]
struct ReportRoutes {
    text: Option<std::sync::Weak<RTCDataChannel>>,
    control: Option<std::sync::Weak<RTCDataChannel>>,
    control_screens: bool,
    revision: u64,
    secure_revision: u64,
}
impl ReportRoutes {
    fn received(
        &mut self,
        received: &crate::features::stream_control::publisher::Received,
    ) -> bool {
        let reroute = received
            .control_screen_reports
            .is_some_and(|v| v != self.control_screens);
        if let Some(value) = received.control_screen_reports {
            self.control_screens = value;
        }
        if reroute || received.refresh_state {
            self.revision = self.revision.wrapping_add(1);
        }
        if received.refresh_secure {
            self.secure_revision = self.secure_revision.wrapping_add(1);
        }
        reroute || received.refresh_state || received.refresh_secure
    }
}
type ReportTarget = tokio::sync::watch::Sender<ReportRoutes>;

impl Peer {
    pub(crate) async fn new(
        screen: capture::Screen,
        owner: super::SessionLease,
        cancel: CancellationToken,
        displays: Arc<super::displays::Session>,
        ice_servers: Vec<RTCIceServer>,
        relay: bool,
        initial: VideoConfig,
        control_screens: bool,
        negotiated: Arc<super::format::Negotiated>,
        network: super::network::Policy,
    ) -> Result<Self> {
        let handle = owner.with_cancellation(cancel.clone());
        tracing::info!(?network, "host network switch policy");
        let initial_quality = if initial.quality == 5 {
            initial.auto_quality
        } else {
            initial.quality
        };
        let initial_bounds = if initial.quality == 5 {
            super::parameters::automatic(initial_quality, initial.fps, true)
        } else {
            super::parameters::fixed(
                initial_quality,
                initial.bitrate,
                (screen.width, screen.height),
                initial.fps,
            )
        };
        let transport = super::transport::Transport::new(
            handle.clone(),
            cancel.clone(),
            initial_bounds,
            network,
        );
        let mut registry = webrtc::interceptor::registry::Registry::new();
        registry.add(Box::new(transport.clone()));
        let mut media = MediaEngine::default();
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "audio/opus".into(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;stereo=1;useinbandfec=1".into(),
                    ..Default::default()
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/H264".into(),
                    clock_rate: 90_000,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .into(),
                    rtcp_feedback: vec![
                        RTCPFeedback {
                            typ: "transport-cc".into(),
                            parameter: String::new(),
                        },
                        RTCPFeedback {
                            typ: "nack".into(),
                            parameter: String::new(),
                        },
                        RTCPFeedback {
                            typ: "nack".into(),
                            parameter: "pli".into(),
                        },
                        RTCPFeedback {
                            typ: "ccm".into(),
                            parameter: "fir".into(),
                        },
                    ],
                    ..Default::default()
                },
                payload_type: 98,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        if negotiated.supports_codec(super::format::Codec::H265) {
            media.register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: "video/H265".into(),
                        clock_rate: 90_000,
                        sdp_fmtp_line: String::new(),
                        rtcp_feedback: vec![
                            RTCPFeedback {
                                typ: "transport-cc".into(),
                                parameter: String::new(),
                            },
                            RTCPFeedback {
                                typ: "nack".into(),
                                parameter: String::new(),
                            },
                            RTCPFeedback {
                                typ: "nack".into(),
                                parameter: "pli".into(),
                            },
                            RTCPFeedback {
                                typ: "ccm".into(),
                                parameter: "fir".into(),
                            },
                        ],
                        ..Default::default()
                    },
                    payload_type: 96,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )?;
            media.register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: "video/rtx".into(),
                        clock_rate: 90_000,
                        sdp_fmtp_line: "apt=96".into(),
                        ..Default::default()
                    },
                    payload_type: 97,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )?;
        }
        // Keep the ordinary UU audio_0 SDP identity; no capture or mic policy is enabled.
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/rtx".into(),
                    clock_rate: 90_000,
                    sdp_fmtp_line: "apt=98".into(),
                    ..Default::default()
                },
                payload_type: 99,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        for uri in [
            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            "urn:ietf:params:rtp-hdrext:toffset",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay",
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            "urn:ietf:params:rtp-hdrext:sdes:mid",
            "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame",
            "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
        ] {
            media.register_header_extension(
                RTCRtpHeaderExtensionCapability { uri: uri.into() },
                RTPCodecType::Video,
                None,
            )?;
        }
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/rs-fec-cm256".into(),
                    clock_rate: 90_000,
                    sdp_fmtp_line: "max-k=109;repair-window=10000000;rtx-as-source=1".into(),
                    ..Default::default()
                },
                payload_type: 36,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        let mut settings = ConnectionCore::settings();
        settings.enable_sender_rtx(true);
        settings.enable_sender_rsfec(true);
        let core = ConnectionCore::new(
            media,
            registry,
            settings,
            RTCConfiguration {
                ice_servers: ice_servers.clone(),
                ice_transport_policy: if relay {
                    RTCIceTransportPolicy::Relay
                } else {
                    RTCIceTransportPolicy::All
                },
                ..Default::default()
            },
        )
        .await?;
        let connection = core.connection.clone();
        // Failed construction cancels role workers; the core owns transport cleanup.
        let construction = cancel.clone().drop_guard();
        let (candidate_tx, candidates) = mpsc::unbounded_channel();
        connection.on_ice_candidate(Box::new(move |c| {
            if let Some(c) = c.and_then(|c| c.to_json().ok()) {
                let _ = candidate_tx.send(c);
            }
            Box::pin(async {})
        }));
        let connected = Arc::new(AtomicBool::new(false));
        let ready = connected.clone();
        let state_handle = handle.clone();
        let state_transport = transport.clone();
        let state_dtls = Arc::downgrade(&connection.sctp().transport());
        let stopped = cancel.clone();
        connection.on_peer_connection_state_change(Box::new(move |state| {
            tracing::info!(?state, "host peer connection state changed");
            state_transport.network(state == RTCPeerConnectionState::Connected);
            ready.store(
                state == RTCPeerConnectionState::Connected,
                Ordering::Release,
            );
            state_handle.update(
                true,
                state == RTCPeerConnectionState::Connected,
                match state {
                    RTCPeerConnectionState::Connected => "正在被远程访问",
                    RTCPeerConnectionState::Disconnected => "连接暂时中断",
                    RTCPeerConnectionState::Failed => "连接已中断",
                    RTCPeerConnectionState::Closed => "等待连接",
                    _ => "正在建立远程连接",
                },
            );
            if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                stopped.cancel();
            }
            let dtls = state_dtls.clone();
            let transport = state_transport.clone();
            Box::pin(async move {
                if state == RTCPeerConnectionState::Connected {
                    if let Some(dtls) = dtls.upgrade() {
                        transport.srtp_overhead(dtls.rtp_authentication_overhead().await);
                    }
                }
            })
        }));
        let audio = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: "audio/opus".into(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;stereo=1;useinbandfec=1".into(),
                ..Default::default()
            },
            "audio_0".into(),
            "audio_0".into(),
        ));
        connection
            .add_track(audio as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let screen_pool = screens::Screens::new(
            connection.clone(),
            handle.clone(),
            cancel.clone(),
            connected.clone(),
            initial,
            negotiated.clone(),
            transport.clone(),
            screen.clone(),
            displays,
        )
        .await?;
        let sender = screen_pool.slots[0].sender.clone();
        let reports = screen_pool.reports.clone();
        let stream_transports: Vec<_> = screen_pool
            .slots
            .iter()
            .map(|s| s.transport.clone())
            .collect();
        let screens = Arc::new(tokio::sync::Mutex::new(screen_pool));
        let route_transport = transport.clone();
        sender
            .transport()
            .ice_transport()
            .on_selected_candidate_pair_change(Box::new(move |pair| {
                tracing::info!(local_type=?pair.local.typ, remote_type=?pair.remote.typ,
                    local_relay_protocol=%pair.local.relay_protocol,
                    remote_relay_protocol=%pair.remote.relay_protocol,
                    "host selected transport path");
                route_transport.route_changed();
                route_transport.selected_route(&pair);
                let transport = route_transport.clone();
                Box::pin(async move {
                    let ip = if pair
                        .local
                        .address
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_ipv6())
                    {
                        40
                    } else {
                        20
                    };
                    let network = ip
                        + if pair.local.protocol
                            == webrtc::ice_transport::ice_protocol::RTCIceProtocol::Tcp
                        {
                            20
                        } else {
                            8
                        };
                    transport.route_overhead(network);
                })
            }));
        let config = Arc::new(Mutex::new(initial));
        let kcp = core.control.clone();
        let (report_target, report_receiver) = tokio::sync::watch::channel(ReportRoutes {
            control_screens,
            ..Default::default()
        });
        let cursor = tokio::spawn(cursor::run(
            reports.clone(),
            report_receiver.clone(),
            cancel.clone(),
            handle.clone(),
        ));
        let publisher = tokio::spawn(publish_state(
            screens.clone(),
            reports.clone(),
            stream_transports.clone(),
            report_receiver,
            cancel.clone(),
            kcp.clone(),
            screen.clone(),
        ));
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<(u16, Vec<u8>)>();
        let control_screen = screen.clone();
        let control_config = config.clone();
        let control_stop = cancel.clone();
        let control_negotiated = negotiated.clone();
        let control_target = report_target.clone();
        let control_receiver: crate::transport::uu_kcp::ControlReceiver =
            Arc::new(move |stream_id, bytes| {
                if control_stop.is_cancelled() {
                    return Ok(());
                }
                let responses = crate::features::stream_control::publisher::receive(
                    bytes,
                    true,
                    &control_screen,
                    &mut lock(&control_config),
                    &control_negotiated,
                )?;
                control_target.send_if_modified(|routes| routes.received(&responses));
                for response in responses.messages {
                    control_tx
                        .send((stream_id, response))
                        .context("host CONTROL response queue closed")?;
                }
                Ok(())
            });
        let control_kcp = kcp.clone();
        let control_stop = cancel.clone();
        let control_responses = tokio::spawn(async move {
            loop {
                let item = tokio::select! { _=control_stop.cancelled()=>break, item=control_rx.recv()=>item };
                let Some((id, bytes)) = item else {
                    break;
                };
                let result = tokio::select! { _=control_stop.cancelled()=>break, result=control_kcp.send(id, bytes)=>result };
                if let Err(error) = result {
                    tracing::warn!(%error,"host KCP CONTROL response failed");
                }
            }
        });
        let channel_screens = screens.clone();
        let channel_cancel = cancel.clone();
        let channel_kcp = kcp.clone();
        connection.on_data_channel(Box::new(move |channel| {
            let screens = channel_screens.clone();
            let stop = channel_cancel.clone();
            let kcp = channel_kcp.clone();
            let report_target = report_target.clone();
            Box::pin(async move {
                bind_channel(channel, screens, stop, kcp, report_target).await;
            })
        }));
        let mut stream_tasks = Vec::new();
        for transport in &stream_transports {
            let repairs = transport.clone();
            stream_tasks.push(tokio::spawn(async move {
                repairs.repairs().await;
            }));
            let fec = transport.clone();
            stream_tasks.push(tokio::spawn(async move {
                fec.fec_worker().await;
            }));
        }
        let control_transport = transport.clone();
        let control_worker = tokio::spawn(async move {
            control_transport.control().await;
        });
        let probe_transport = transport.clone();
        let probe_worker = tokio::spawn(async move {
            probe_transport.probes().await;
        });
        // Transport-cc may use media_ssrc=0, which is separate from the video
        // sender's RTCP stream. Consume it through the authenticated SRTCP session.
        let zero_transport = transport.clone();
        let zero_cancel = cancel.clone();
        let zero_dtls = sender.transport();
        let zero_feedback = tokio::spawn(async move {
            let stream = loop {
                if zero_cancel.is_cancelled() {
                    return;
                }
                if let Some(stream) = zero_dtls.rtcp_read_stream(0).await {
                    break stream;
                }
                tokio::select! { _=zero_cancel.cancelled()=>return, _=tokio::time::sleep(Duration::from_millis(20))=>{} }
            };
            let mut bytes = vec![0u8; 65536];
            loop {
                let reports = tokio::select! { _=zero_cancel.cancelled()=>break, r=stream.read_rtcp(&mut bytes)=>r };
                let reports = match reports {
                    Ok(reports) => reports,
                    Err(webrtc_srtp::Error::Rtcp(error)) => {
                        tracing::debug!(%error,"discarded malformed host transport feedback");
                        continue;
                    }
                    Err(_) => break,
                };
                for report in reports {
                    if let Some(feedback)=report.as_any().downcast_ref::<webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc>() {
                        zero_transport.feedback(feedback);
                    }
                }
            }
        });
        let peer_transport = transport.clone();
        construction.disarm();
        Ok(Self {
            core,
            candidates,
            cancel,
            tasks: {
                stream_tasks.extend([
                    zero_feedback,
                    control_worker,
                    probe_worker,
                    control_responses,
                    publisher,
                    cursor,
                ]);
                stream_tasks
            },
            screens,
            reports,
            handle: owner,
            control_receiver,
            remote_ice: Default::default(),
            transport: peer_transport,
            ice_servers,
            relay_only: AtomicBool::new(relay),
        })
    }

    async fn restart_network(&self, network_type: u8) -> Result<()> {
        // T B055D0: answerer policy is distinct from the controller switch API.
        let (relay, tls) = match network_type {
            0 => (true, self.transport.network_attempt() == 2),
            1 => (true, true),
            2 | 3 => (false, false),
            _ => anyhow::bail!("无效的ICE网络类型"),
        };
        let mut servers = self.ice_servers.clone();
        if tls {
            for server in &mut servers {
                server
                    .urls
                    .retain(|url| url.to_ascii_lowercase().contains("turns:"));
            }
            servers.retain(|server| !server.urls.is_empty());
            anyhow::ensure!(!servers.is_empty(), "当前会话未提供TURNS服务器");
        }
        let policy = if relay {
            RTCIceTransportPolicy::Relay
        } else {
            RTCIceTransportPolicy::All
        };
        self.core.configure_ice(servers, policy).await?;
        self.relay_only.store(relay, Ordering::Release);
        tracing::info!(network_type, relay, tls, "host ICE restart policy applied");
        Ok(())
    }
    pub(crate) async fn local_candidate(
        &self,
        mut candidate: RTCIceCandidateInit,
    ) -> Option<RTCIceCandidateInit> {
        if self.ended() || !self.handle.requested() || candidate.candidate.is_empty() {
            return None;
        }
        if self.relay_only.load(Ordering::Acquire)
            && !candidate
                .candidate
                .to_ascii_lowercase()
                .contains(" typ relay")
        {
            return None;
        }
        let sdp = self.core.connection.local_description().await?.sdp;
        if candidate
            .username_fragment
            .as_ref()
            .is_some_and(|fragment| {
                !sdp.lines()
                    .filter_map(|line| line.strip_prefix("a=ice-ufrag:"))
                    .any(|f| f == fragment)
            })
        {
            return None;
        }
        if candidate.sdp_mid.as_ref().is_none_or(String::is_empty) {
            let index = usize::from(candidate.sdp_mline_index.unwrap_or(0));
            candidate.sdp_mid = sdp
                .split("m=")
                .skip(1)
                .nth(index)
                .and_then(|media| media.lines().find_map(|line| line.strip_prefix("a=mid:")))
                .map(str::to_owned);
        }
        Some(candidate)
    }
    pub(crate) async fn answer(
        &self,
        sdp: String,
        restart: bool,
        network_type: u8,
    ) -> Result<String> {
        tracing::info!(sdp_bytes = sdp.len(), "host received video offer");
        let mixed_kcp = crate::transport::rtc::negotiated_mixed_kcp_version(&sdp)?;
        let fragments: std::collections::HashSet<String> = sdp
            .lines()
            .filter_map(|l| l.strip_prefix("a=ice-ufrag:"))
            .map(str::to_owned)
            .collect();
        anyhow::ensure!(!fragments.is_empty(), "主控SDP缺少ICE代次");
        let description = RTCSessionDescription::offer(sdp)?;
        if restart {
            if let Err(error) = self.restart_network(network_type).await {
                // Native B055D0 logs SetConfig failure and retains the live peer.
                tracing::warn!(%error,network_type,"host ICE restart policy rejected; retaining active configuration");
            }
        }
        let mut ice = self.remote_ice.lock().await;
        self.core
            .connection
            .set_remote_description(description)
            .await?;
        let previous = std::mem::replace(&mut ice.current, fragments);
        for fragment in previous {
            if !ice.current.contains(&fragment) {
                ice.retired.insert(fragment);
            }
        }
        let mut pending = std::mem::take(&mut ice.pending);
        while let Some(candidate) = pending.pop_front() {
            if candidate
                .username_fragment
                .as_ref()
                .is_none_or(|f| ice.current.contains(f))
            {
                if let Err(error) = self.core.connection.add_ice_candidate(candidate).await {
                    // T AFA310: a rejected candidate is local to that candidate,
                    // including when it arrived before the offer. Keep answering.
                    tracing::debug!(%error, "discarded queued host ICE candidate; SDP negotiation retained");
                }
            } else if candidate
                .username_fragment
                .as_ref()
                .is_some_and(|f| !ice.retired.contains(f))
            {
                ice.pending.push_back(candidate);
            }
        }
        drop(ice);
        let sdp = self
            .core
            .answer("audio_0 video_0 video_1 video_2 video_3 video_4", mixed_kcp)
            .await?;
        anyhow::ensure!(!self.ended() && self.handle.requested(), "画面会话已取消");
        self.core
            .activate_control(mixed_kcp, self.control_receiver.clone())?;
        Ok(sdp)
    }
    pub(crate) async fn add_candidate(&self, mut candidate: RTCIceCandidateInit) -> Result<()> {
        if self.ended() || !self.handle.requested() {
            return Ok(());
        }
        candidate.username_fragment = candidate.effective_username_fragment();
        let mut ice = self.remote_ice.lock().await;
        if candidate
            .username_fragment
            .as_ref()
            .is_some_and(|f| ice.retired.contains(f))
        {
            return Ok(());
        }
        if ice.current.is_empty()
            || candidate
                .username_fragment
                .as_ref()
                .is_some_and(|f| !ice.current.contains(f))
        {
            anyhow::ensure!(ice.pending.len() < 256, "等待SDP的ICE候选过多");
            ice.pending.push_back(candidate);
            return Ok(());
        }
        self.core.connection.add_ice_candidate(candidate).await?;
        Ok(())
    }
    pub(crate) fn access_revoked(&self) -> bool {
        !self.handle.requested()
    }
    pub(crate) fn displays(&self) -> Vec<crate::protocol::capability::DisplayCapability> {
        lock(&self.reports.catalog)
            .iter()
            .map(|info| crate::protocol::capability::DisplayCapability {
                id: info.screen.id,
                fps: info.screen.fps,
                kind: info.kind,
                hdr: i32::from(info.screen.hdr),
            })
            .collect()
    }
    pub(crate) fn ended(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub(crate) fn network_change(&self) -> Option<u8> {
        if self.ended() {
            None
        } else {
            self.transport.network_change()
        }
    }
    pub(crate) async fn close(mut self) {
        self.cancel.cancel();
        let _ = self.core.close().await;
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        self.screens.lock().await.close().await;
        self.handle.finish();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
        // Normal close has already joined these owners. This also covers a
        // cancelled close future or an exceptional drop by the signal owner.
        let closing = self.core.close_detached();
        let screens = self.screens.clone();
        tokio::spawn(async move {
            if let Some(closing) = closing {
                let _ = closing.await;
            }
            screens.lock().await.close().await;
        });
    }
}

fn maximum_header(
    extensions: &[webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionParameters],
    mid: usize,
    color: usize,
    two_byte: bool,
) -> usize {
    let prefix = if two_byte { 2 } else { 1 };
    let size: usize = extensions
        .iter()
        .filter_map(|e| {
            let size = match e.uri.as_str() {
                "urn:ietf:params:rtp-hdrext:sdes:mid" => mid,
                "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01" => 2,
                "http://www.webrtc.org/experiments/rtp-hdrext/color-space" => color,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index" => 2,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame" => 1,
                "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" => 3,
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time" => 3,
                "urn:ietf:params:rtp-hdrext:toffset" => 3,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-timing" => 13,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay" => 2,
                _ => return None,
            };
            (size > 0).then_some(prefix + size)
        })
        .sum();
    12 + if size == 0 {
        0
    } else {
        4 + size.div_ceil(4) * 4
    }
}

// RTP and RTCP SR share one wrapping 90kHz clock. Float-to-u32 casts saturate
// after about 13h15m instead of wrapping, breaking long-running SR mappings.
fn video_timestamp(origin: u32, elapsed_100ns: u64) -> u32 {
    origin.wrapping_add((u128::from(elapsed_100ns) * 9 / 1000) as u32)
}

mod capture_worker;
mod channels;
mod cursor;
mod media;
pub(crate) mod screens;
use channels::bind_channel;
use channels::publish_state;
