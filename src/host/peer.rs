//! First desktop publisher: session-scoped peer, capture owner and RTP sender.
use super::{Lease, VideoConfig, capture, encoder, lock, output_size};
use anyhow::{Context, Result};
use bytes::Bytes;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::{
    api::{APIBuilder, media_engine::MediaEngine, setting_engine::SettingEngine},
    data_channel::RTCDataChannel,
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        policy::ice_transport_policy::RTCIceTransportPolicy,
        sdp::session_description::RTCSessionDescription,
    },
    rtp::{codecs::h264::H264Payloader, header::Header, packet::Packet, packetizer::Payloader},
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
    pub connection: Arc<RTCPeerConnection>,
    pub candidates: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    capture: Option<std::thread::JoinHandle<()>>,
    handle: Lease,
    kcp: crate::uu_kcp::UuKcpControl,
    control_receiver: crate::uu_kcp::ControlReceiver,
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
    quality: i32,
    fps: u32,
    encoder: Option<super::format::Backend>,
    capture: String,
}
type TextTarget = tokio::sync::watch::Sender<Option<std::sync::Weak<RTCDataChannel>>>;

// Until construction hands ownership to Peer, every error/cancel exit must
// stop its application workers and close the underlying transport as well.
struct PendingPeer {
    connection: Option<Arc<RTCPeerConnection>>,
    cancel: CancellationToken,
}
impl Drop for PendingPeer {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.cancel.cancel();
            tokio::spawn(async move {
                let _ = connection.close().await;
            });
        }
    }
}

impl Peer {
    pub(crate) async fn new(
        screen: capture::Screen,
        handle: Lease,
        ice_servers: Vec<RTCIceServer>,
        relay: bool,
        initial: VideoConfig,
        negotiated: Arc<super::format::Negotiated>,
        network: super::network::Policy,
    ) -> Result<Self> {
        let handle = handle.peer_lease()?;
        let cancel = CancellationToken::new();
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
        let mut settings = SettingEngine::default();
        settings.enable_sender_rtx(true);
        settings.enable_sender_rsfec(true);
        settings.set_sctp_max_message_size_can_send(
            webrtc::api::setting_engine::SctpMaxMessageSize::Bounded(524_288),
        );
        settings.set_data_channel_receive_limit(524_288);
        settings.set_srtp_replay_protection_window(1024);
        settings.set_srtcp_replay_protection_window(128);
        settings.set_continual_gathering(true);
        let connection = Arc::new(
            APIBuilder::new()
                .with_media_engine(media)
                .with_interceptor_registry(registry)
                .with_setting_engine(settings)
                .build()
                .new_peer_connection(RTCConfiguration {
                    ice_servers: ice_servers.clone(),
                    ice_transport_policy: if relay {
                        RTCIceTransportPolicy::Relay
                    } else {
                        RTCIceTransportPolicy::All
                    },
                    ..Default::default()
                })
                .await?,
        );
        let mut pending = PendingPeer {
            connection: Some(connection.clone()),
            cancel: cancel.clone(),
        };
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
                    RTCPeerConnectionState::Connected => "正在共享画面",
                    RTCPeerConnectionState::Disconnected => "连接暂时中断",
                    _ => "正在建立画面连接",
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
        let track = Arc::new(super::track::VideoTrack::new(
            initial.format.codec,
            negotiated.clone(),
        ));
        let sender = connection
            .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let extension_sender = sender.clone();
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
        let capture_clock = Instant::now();
        let origin = rand::random::<u32>();
        let report_sender = sender.clone();
        let report_connection = Arc::downgrade(&connection);
        let report_cancel = cancel.clone();
        let report_handle = handle.clone();
        let sent = Arc::new(Mutex::new(None::<(u32, Instant, u64, u64)>));
        let report_clock = sent.clone();
        let report_connected = connected.clone();
        let reports = tokio::spawn(async move {
            use webrtc::rtcp::{
                sender_report::SenderReport,
                source_description::{
                    SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
                },
            };
            let anchor = (Instant::now(), std::time::SystemTime::now());
            let mut previous = (Instant::now(), 0u64);
            let mut delay = Duration::from_millis(500);
            loop {
                tokio::select! {_ = report_cancel.cancelled()=>break,_=tokio::time::sleep(delay)=>{}}
                if !report_handle.requested() || !report_connected.load(Ordering::Acquire) {
                    continue;
                }
                let Some((_, _, packets, octets)) = *lock(&report_clock) else {
                    continue;
                };
                let elapsed = previous.0.elapsed().as_secs_f64().max(0.001);
                let bps = octets.saturating_sub(previous.1) as f64 * 8.0 / elapsed;
                previous = (Instant::now(), octets);
                let base = if bps >= 1000.0 {
                    (360_000.0 / (bps / 1000.0)).min(1000.0)
                } else {
                    1000.0
                };
                delay = Duration::from_secs_f64(
                    (base * (0.5 + rand::random::<f64>()) / 1000.0).max(0.001),
                );
                let Some(connection) = report_connection.upgrade() else {
                    break;
                };
                let parameters = report_sender.get_parameters().await;
                let Some(encoding) = parameters.encodings.first() else {
                    continue;
                };
                let report = SenderReport {
                    ssrc: encoding.ssrc,
                    ntp_time: webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                        anchor.1 + anchor.0.elapsed(),
                    ),
                    rtp_time: video_timestamp(
                        origin,
                        (capture_clock.elapsed().as_nanos() / 100) as u64,
                    ),
                    packet_count: packets as u32,
                    octet_count: octets as u32,
                    ..Default::default()
                };
                let sdes = SourceDescription {
                    chunks: vec![SourceDescriptionChunk {
                        source: encoding.ssrc,
                        items: vec![SourceDescriptionItem {
                            sdes_type: SdesType::SdesCname,
                            text: Bytes::from_static(b"video_0"),
                        }],
                    }],
                };
                if let Err(error) = connection
                    .write_rtcp(&[Box::new(report), Box::new(sdes)])
                    .await
                {
                    tracing::debug!(%error,"host sender report failed");
                }
            }
        });
        let config = Arc::new(Mutex::new(initial));
        let (publication, published) = tokio::sync::watch::channel(Published {
            screen: screen.clone(),
            capturing: false,
            quality: 0,
            fps: initial.fps,
            encoder: None,
            capture: "DXGI".into(),
        });
        let (text_target, text_receiver) = tokio::sync::watch::channel(None);
        let publisher = tokio::spawn(publish_state(
            published.clone(),
            text_receiver,
            transport.clone(),
            cancel.clone(),
        ));
        let kcp = crate::uu_kcp::UuKcpControl::default();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<(u16, Vec<u8>)>();
        let control_screen = screen.clone();
        let control_config = config.clone();
        let control_stop = cancel.clone();
        let control_negotiated = negotiated.clone();
        let control_receiver: crate::uu_kcp::ControlReceiver = Arc::new(move |stream_id, bytes| {
            if control_stop.is_cancelled() {
                return Ok(());
            }
            let responses = crate::stream_control::publisher::receive(
                bytes,
                true,
                &control_screen,
                &mut lock(&control_config),
                &control_negotiated,
            )?;
            for response in responses {
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
        let channel_config = config.clone();
        let channel_cancel = cancel.clone();
        let channel_kcp = kcp.clone();
        let channel_negotiated = negotiated.clone();
        let channel_publication = published.clone();
        connection.on_data_channel(Box::new(move |channel| {
            let config = channel_config.clone();
            let stop = channel_cancel.clone();
            let kcp = channel_kcp.clone();
            let negotiated = channel_negotiated.clone();
            let publication = channel_publication.clone();
            let text_target = text_target.clone();
            Box::pin(async move {
                bind_channel(
                    channel,
                    config,
                    stop,
                    kcp,
                    negotiated,
                    publication,
                    text_target,
                )
                .await;
            })
        }));
        let keyframe = Arc::new(AtomicBool::new(true));
        let request_keyframe = keyframe.clone();
        let feedback_cancel = cancel.clone();
        let feedback_transport = transport.clone();
        let repairs_transport = transport.clone();
        let repair_worker = tokio::spawn(async move {
            repairs_transport.repairs().await;
        });
        let control_transport = transport.clone();
        let control_worker = tokio::spawn(async move {
            control_transport.control().await;
        });
        let probe_transport = transport.clone();
        let probe_worker = tokio::spawn(async move {
            probe_transport.probes().await;
        });
        let fec_transport = transport.clone();
        let fec_worker = tokio::spawn(async move {
            fec_transport.fec_worker().await;
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
        let feedback = tokio::spawn(async move {
            let mut keyframes = super::keyframe::Feedback::default();
            loop {
                let reports =
                    tokio::select! {_ = feedback_cancel.cancelled()=>break,r=sender.read_rtcp()=>r};
                let reports = match reports {
                    Ok((reports, _)) => reports,
                    Err(webrtc::Error::Interceptor(webrtc::interceptor::Error::Rtcp(error))) => {
                        tracing::debug!(%error,"discarded malformed host video feedback");
                        continue;
                    }
                    Err(_) => break,
                };
                let params = sender.get_parameters().await;
                for report in reports {
                    if let Some(nack)=report.as_any().downcast_ref::<webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack>() {feedback_transport.nack(nack);}
                    let reports = report
                        .as_any()
                        .downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>()
                        .map(|r| r.reports.as_slice())
                        .or_else(|| {
                            report
                                .as_any()
                                .downcast_ref::<webrtc::rtcp::sender_report::SenderReport>()
                                .map(|r| r.reports.as_slice())
                        });
                    if let Some(reports) = reports {
                        for report in reports {
                            if !params.encodings.iter().any(|e| e.ssrc == report.ssrc)
                                || report.last_sender_report == 0
                            {
                                continue;
                            }
                            let now = (webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                                std::time::SystemTime::now(),
                            ) >> 16) as u32;
                            let rtt = now
                                .wrapping_sub(report.last_sender_report)
                                .wrapping_sub(report.delay);
                            if rtt < 0x80000000 {
                                feedback_transport.remote_report(
                                    report.fraction_lost,
                                    Duration::from_micros(u64::from(rtt) * 1_000_000 / 65536),
                                );
                            }
                        }
                    }
                    if params.encodings.first().is_some_and(|encoding| {
                        keyframes.accept(report.as_ref(), encoding.ssrc, Instant::now())
                    }) {
                        request_keyframe.store(true, Ordering::Release);
                    }
                }
            }
        });
        // A frame must be consumed before capturing another. There is no historical frame backlog.
        let (frames_tx, mut frames) = mpsc::channel::<encoder::Encoded>(1);
        let owner_cancel = cancel.clone();
        let owner_handle = handle.clone();
        let owner_keyframe = keyframe.clone();
        let owner_transport = transport.clone();
        let sending_formats = negotiated.clone();
        let sending_keyframe = keyframe.clone();
        let capture = std::thread::Builder::new()
            .name("host-screen".into())
            .spawn(move || {
                let result = capture_loop(
                    screen,
                    owner_handle.clone(),
                    owner_cancel.clone(),
                    connected,
                    config,
                    owner_keyframe,
                    frames_tx,
                    capture_clock,
                    owner_transport,
                    negotiated,
                    publication,
                );
                if let Err(error) = result {
                    let message = format!("画面采集已停止：{error:#}");
                    if error.downcast_ref::<capture::SourceGone>().is_some() {
                        owner_handle.stop_with_error(message);
                    } else {
                        owner_handle.fail(message);
                    }
                    owner_cancel.cancel();
                }
            })?;
        let send_cancel = cancel.clone();
        let send_handle = handle.clone();
        let peer_transport = transport.clone();
        let sending = tokio::spawn(async move {
            let mut payloader = H264Payloader::default();
            let mut seq = rand::random::<u16>();
            loop {
                let value = tokio::select! {_ = send_cancel.cancelled()=>break,v=frames.recv()=>v};
                let Some(frame) = value else { break };
                if !sending_formats.permits_codec(frame.format.codec) {
                    sending_keyframe.store(true, Ordering::Release);
                    continue;
                }
                transport.frame(
                    video_timestamp(origin, frame.timestamp_100ns as u64),
                    frame.keyframe,
                    frame.timing.map(|t| t.captured),
                    frame.format,
                );
                if !send_handle.requested() {
                    break;
                }
                let parameters = extension_sender.get_parameters().await.rtp_parameters;
                transport.negotiated_codecs(&parameters.codecs);
                let extensions = parameters.header_extensions;
                let color = Bytes::from(super::format::color_extension(frame.color));
                let headers:Vec<_>=extensions.iter().filter(|e|frame.timing.is_some() || !matches!(e.uri.as_str(),
                    "http://www.webrtc.org/experiments/rtp-hdrext/video-timing"|"http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay")).cloned().collect();
                let base: Vec<_> = headers
                    .iter()
                    .filter(|e| {
                        !matches!(
                            e.uri.as_str(),
                            "http://www.webrtc.org/experiments/rtp-hdrext/color-space"
                                | "http://www.webrtc.org/experiments/rtp-hdrext/video-timing"
                        )
                    })
                    .cloned()
                    .collect();
                let repair:Vec<_>=extensions.iter().filter(|e|matches!(e.uri.as_str(),"urn:ietf:params:rtp-hdrext:sdes:mid"|"http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01")).cloned().collect();
                let mixed = color.len() > 16 || extensions.iter().any(|e| e.id > 14);
                let header = maximum_header(&headers, track.mid_len(), color.len(), mixed);
                let base_header = maximum_header(&base, track.mid_len(), color.len(), mixed);
                let repair_header = maximum_header(&repair, track.mid_len(), color.len(), mixed);
                let mtu = transport.payload_limit(header, repair_header);
                let payloads = match frame.format.codec {
                    super::format::Codec::H264 => payloader
                        .payload(mtu, &Bytes::from(frame.data))
                        .map_err(anyhow::Error::from),
                    super::format::Codec::H265 => super::hevc::payloads(mtu, &frame.data),
                };
                let payloads = match payloads {
                    Ok(v) => v,
                    Err(error) => {
                        send_handle.fail(format!("视频分包失败：{error}"));
                        send_cancel.cancel();
                        break;
                    }
                };
                let count = payloads.len();
                let mut complete = count > 0;
                let packetized_at = Instant::now();
                let mut queued = transport.queued_frame(
                    payloads
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            transport.wire_size(
                                p.len() + if i + 1 == count { header } else { base_header },
                            )
                        })
                        .sum(),
                );
                for (index, payload) in payloads.into_iter().enumerate() {
                    if send_cancel.is_cancelled() || !send_handle.requested() {
                        break;
                    }
                    let mut packet = Packet {
                        header: Header {
                            version: 2,
                            extension: color.len() > 16 || extensions.iter().any(|e| e.id > 14),
                            extension_profile: if color.len() > 16
                                || extensions.iter().any(|e| e.id > 14)
                            {
                                0x1000
                            } else {
                                0
                            },
                            sequence_number: seq,
                            timestamp: video_timestamp(origin, frame.timestamp_100ns as u64),
                            marker: index + 1 == count,
                            ..Default::default()
                        },
                        payload,
                    };
                    for ext in &extensions {
                        let value = match ext.uri.as_str() {
                            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
                            | "urn:ietf:params:rtp-hdrext:toffset" => {
                                Some(Bytes::from_static(&[0, 0, 0]))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay"
                                if frame.timing.is_some() =>
                            {
                                Some(Bytes::from_static(&[0, 0]))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/video-timing"
                                if index + 1 == count && frame.timing.is_some() =>
                            {
                                let timing = frame.timing.unwrap();
                                let mut value = [0u8; 13];
                                value[0] = 4;
                                for (i, at) in
                                    [timing.encode_started, timing.encode_finished, packetized_at]
                                        .into_iter()
                                        .enumerate()
                                {
                                    let delta = ((at
                                        .saturating_duration_since(timing.captured)
                                        .as_micros()
                                        + 500)
                                        / 1000)
                                        .min(u16::MAX.into())
                                        as u16;
                                    value[1 + i * 2..3 + i * 2]
                                        .copy_from_slice(&delta.to_be_bytes());
                                }
                                Some(Bytes::copy_from_slice(&value))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index" => {
                                // T397272 -> TD08870/TD0B440: source identity,
                                // not a frame counter. This peer shares one fixed source.
                                Some(Bytes::from_static(&[0, 0]))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame" => {
                                Some(Bytes::from_static(if frame.is_new { &[1] } else { &[0] }))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" => {
                                Some(Bytes::from_static(&[0, 0, 0]))
                            }
                            "http://www.webrtc.org/experiments/rtp-hdrext/color-space"
                                if index + 1 == count =>
                            {
                                Some(color.clone())
                            }
                            _ => None,
                        };
                        if let Some(value) = value {
                            if let Err(error) = packet.header.set_extension(ext.id as u8, value) {
                                tracing::warn!(%error,"host extension encoding failed");
                            }
                        }
                    }
                    seq = seq.wrapping_add(1);
                    match track.write(frame.format.codec, &packet).await {
                        Ok(0) => {
                            complete = false;
                            break;
                        }
                        Ok(_) => {}
                        Err(webrtc::Error::ErrUnsupportedCodec) => {
                            // A reoffer may retire this already queued format.
                            sending_keyframe.store(true, Ordering::Release);
                            complete = false;
                            break;
                        }
                        Err(error) => {
                            send_handle.fail(format!("画面发送失败：{error}"));
                            send_cancel.cancel();
                            complete = false;
                            break;
                        }
                    }
                    queued.sent(transport.wire_size(
                        packet.payload.len()
                            + if index + 1 == count {
                                header
                            } else {
                                base_header
                            },
                    ));
                    let mut clock = lock(&sent);
                    let (packets, octets) = clock.as_ref().map_or((0, 0), |v| (v.2, v.3));
                    *clock = Some((
                        packet.header.timestamp,
                        Instant::now(),
                        packets + 1,
                        octets + packet.payload.len() as u64,
                    ));
                    drop(clock);
                }
                if complete && !send_cancel.is_cancelled() && send_handle.requested() {
                    send_handle.frame();
                }
            }
        });
        pending.connection = None;
        Ok(Self {
            connection,
            candidates,
            cancel,
            tasks: vec![
                feedback,
                zero_feedback,
                sending,
                reports,
                repair_worker,
                control_worker,
                probe_worker,
                fec_worker,
                control_responses,
                publisher,
            ],
            capture: Some(capture),
            handle,
            kcp,
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
        let mut configuration = self.connection.get_configuration().await;
        configuration.ice_transport_policy = if relay {
            RTCIceTransportPolicy::Relay
        } else {
            RTCIceTransportPolicy::All
        };
        configuration.ice_servers = servers;
        self.connection.set_configuration(configuration).await?;
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
        let sdp = self.connection.local_description().await?.sdp;
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
        let mixed_kcp = crate::rtc::negotiated_mixed_kcp_version(&sdp)?;
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
        self.connection.set_remote_description(description).await?;
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
                if let Err(error) = self.connection.add_ice_candidate(candidate).await {
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
        let answer = self.connection.create_answer(None).await?;
        self.connection.set_local_description(answer).await?;
        let mut sdp = self
            .connection
            .local_description()
            .await
            .map(|s| s.sdp)
            .context("缺少本地answer")?;
        crate::rtc::apply_uu_application_attributes_for_role(&mut sdp, "audio_0 video_0")?;
        anyhow::ensure!(!self.ended() && self.handle.requested(), "画面会话已取消");
        if let Some(version) = mixed_kcp {
            self.kcp.start_receiver(
                self.connection.sctp(),
                version,
                self.control_receiver.clone(),
            )?;
        } else {
            anyhow::ensure!(
                !self.kcp.is_negotiated(),
                "ICE重协商不能移除已使用的CONTROL承载"
            );
        }
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
        self.connection.add_ice_candidate(candidate).await?;
        Ok(())
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
        self.kcp.close().await;
        let _ = self.connection.close().await;
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        if let Some(owner) = self.capture.take() {
            let _ = tokio::task::spawn_blocking(move || owner.join()).await;
        }
        self.handle
            .update(self.handle.requested(), false, "等待连接");
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.kcp.stop();
        for task in &self.tasks {
            task.abort();
        }
        // Normal close has already joined these owners. This also covers a
        // cancelled close future or an exceptional drop by the signal owner.
        let connection = self.connection.clone();
        let capture = self.capture.take();
        if connection.connection_state() != RTCPeerConnectionState::Closed || capture.is_some() {
            tokio::spawn(async move {
                let _ = connection.close().await;
                if let Some(owner) = capture {
                    let _ = tokio::task::spawn_blocking(move || owner.join()).await;
                }
            });
        }
    }
}

async fn bind_channel(
    channel: Arc<RTCDataChannel>,
    config: Arc<Mutex<VideoConfig>>,
    cancel: CancellationToken,
    kcp: crate::uu_kcp::UuKcpControl,
    negotiated: Arc<super::format::Negotiated>,
    publication: tokio::sync::watch::Receiver<Published>,
    text_target: TextTarget,
) {
    let control = channel.label() == "CONTROL_DATA_CHANNEL";
    let text = channel.label() == "TEXT_DATA_CHANNEL";
    if !control && !text {
        return;
    }
    tracing::info!(channel = channel.label(), "host business channel bound");
    let weak = Arc::downgrade(&channel);
    let opening = cancel.clone();
    let opening_kcp = kcp.clone();
    let close_kcp = kcp.clone();
    let close_channel = Arc::downgrade(&channel);
    let closing_target = text_target.clone();
    channel.on_close(Box::new(move || {
        if control {
            if let Some(channel) = close_channel.upgrade() {
                close_kcp.set_control_stream(channel.id(), false);
            }
        } else {
            closing_target.send_if_modified(|current| {
                if current
                    .as_ref()
                    .is_some_and(|target| target.ptr_eq(&close_channel))
                {
                    *current = None;
                    true
                } else {
                    false
                }
            });
        }
        Box::pin(async {})
    }));
    channel.on_open(Box::new(move || {
        Box::pin(async move {
            let Some(channel) = weak.upgrade() else {
                return;
            };
            if opening.is_cancelled() {
                return;
            }
            if text {
                text_target.send_replace(Some(Arc::downgrade(&channel)));
                return;
            }
            tracing::info!(channel = channel.label(), "host business channel opened");
            if control {
                opening_kcp.set_control_stream(channel.id(), true);
            }
            let bytes = crate::stream_control::publisher::echo(1, 0, true);
            let result = send_business(&channel, control, &opening_kcp, bytes).await;
            if let Err(error) = result {
                tracing::warn!(%error,control,"host opening business message failed");
            }
        })
    }));
    let weak = Arc::downgrade(&channel);
    channel.on_message(Box::new(move |message| {
        let weak = weak.clone();
        let screen = publication.borrow().screen.clone();
        let config = config.clone();
        let cancel = cancel.clone();
        let kcp = kcp.clone();
        let negotiated = negotiated.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return;
            }
            tracing::debug!(
                control,
                bytes = message.data.len(),
                tag = message.data.first().copied(),
                "host business message received"
            );
            let responses = crate::stream_control::publisher::receive(
                &message.data,
                control,
                &screen,
                &mut lock(&config),
                &negotiated,
            );
            match responses {
                Ok(responses) => {
                    if let Some(channel) = weak.upgrade() {
                        for response in responses {
                            if cancel.is_cancelled() {
                                break;
                            }
                            let result = send_business(&channel, control, &kcp, response).await;
                            if let Err(error) = result {
                                tracing::warn!(%error,control,"host business response failed");
                            }
                        }
                    }
                }
                Err(error) => tracing::warn!(%error,"rejected malformed host business message"),
            }
        })
    }));
}

async fn send_business(
    channel: &RTCDataChannel,
    control: bool,
    kcp: &crate::uu_kcp::UuKcpControl,
    bytes: Vec<u8>,
) -> Result<usize> {
    // Once negotiated, a failed reliable message must not be replayed on SCTP.
    if control && kcp.is_negotiated() {
        return kcp.send(channel.id(), bytes).await;
    }
    let bytes = Bytes::from(bytes);
    Ok(if control {
        channel.send(&bytes).await?
    } else {
        channel.send_text_bytes(&bytes).await?
    })
}

async fn publish_state(
    mut publication: tokio::sync::watch::Receiver<Published>,
    mut target: tokio::sync::watch::Receiver<Option<std::sync::Weak<RTCDataChannel>>>,
    transport: super::transport::Transport,
    cancel: CancellationToken,
) {
    let mut last = None::<Published>;
    let mut last_probe = None::<u32>;
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _=cancel.cancelled()=>break,
            result=publication.changed()=>{if result.is_err(){break;}},
            result=target.changed()=>{if result.is_err(){break;}last=None;last_probe=None;},
            _=timer.tick()=>{},
        };
        let Some(channel) = target.borrow().as_ref().and_then(std::sync::Weak::upgrade) else {
            continue;
        };
        let state = publication.borrow_and_update().clone();
        let mut messages = Vec::new();
        if last
            .as_ref()
            .is_none_or(|old| old.screen != state.screen || old.capturing != state.capturing)
        {
            messages.push(crate::stream_control::publisher::screen_state(
                &state.screen,
                state.capturing,
            ));
            messages.push(crate::stream_control::publisher::capture_change(
                &state.screen,
                state.capturing,
            ));
            messages.push(crate::stream_control::publisher::permissions(
                state.capturing,
            ));
        }
        // S 549A80: publish changed fields or a probe delta >= 100 Ki bps.
        let probe = transport.automatic_rates().0;
        let report_quality = state.quality > 0
            && state.encoder.is_some()
            && (last_probe.is_none_or(|old| old.abs_diff(probe) >= 102_400)
                || last.as_ref().is_none_or(|old| {
                    old.quality != state.quality
                        || old.encoder != state.encoder
                        || old.capture != state.capture
                        || old.fps != state.fps
                        || old.screen.width != state.screen.width
                        || old.screen.height != state.screen.height
                }));
        if report_quality && let Some(encoder) = state.encoder {
            messages.push(crate::stream_control::publisher::quality_report(
                state.quality,
                probe,
                (state.screen.width, state.screen.height),
                state.fps,
                encoder,
                &state.capture,
            ));
        }
        let mut complete = true;
        for message in messages {
            let message = Bytes::from(message);
            let result = tokio::select! {_=cancel.cancelled()=>return,result=channel.send_text_bytes(&message)=>result};
            if let Err(error) = result {
                tracing::debug!(%error,"host publication channel unavailable");
                complete = false;
                break;
            }
        }
        if complete {
            last = Some(state);
            if report_quality {
                last_probe = Some(probe);
            }
        }
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

fn capture_loop(
    mut screen: capture::Screen,
    handle: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    config: Arc<Mutex<VideoConfig>>,
    keyframe: Arc<AtomicBool>,
    sender: mpsc::Sender<encoder::Encoded>,
    started: Instant,
    transport: super::transport::Transport,
    negotiated: Arc<super::format::Negotiated>,
    publication: tokio::sync::watch::Sender<Published>,
) -> Result<()> {
    let _runtime = encoder::Runtime::new()?;
    let mut desktop = None;
    let mut encoder = None;
    let mut current = None;
    let mut next = Instant::now();
    let mut cached = None::<capture::Frame>;
    let mut last_frame = None::<Instant>;
    let mut failed = std::collections::HashSet::new();
    let mut encode_errors = 0u32;
    let mut requested = None;
    let mut automatic = None::<super::parameters::AutoQuality>;
    let mut initial_auto = true;
    let mut admission = super::parameters::WindowAdmission::default();
    let mut generation = 0;
    let mut encoding_device = None::<(u64, windows::Win32::Graphics::Direct3D11::ID3D11Device)>;
    let mut transfer = None::<super::transfer::Transfer>;
    let mut frame_metadata = std::collections::BTreeMap::new();

    while !cancel.is_cancelled() && handle.requested() {
        let mut wanted = *lock(&config);
        if !negotiated.permits_codec(wanted.format.codec) {
            let previous = wanted.revision;
            let chroma = wanted.format.chroma;
            let hdr = wanted.format.hdr();
            negotiated.apply(
                &mut wanted,
                None,
                chroma,
                hdr,
                (screen.width, screen.height),
            )?;
            let mut active = lock(&config);
            if active.revision != previous {
                continue;
            }
            wanted.revision = previous.wrapping_add(1);
            *active = wanted;
        }

        let requested_hdr = wanted.format.hdr();
        if desktop.is_none()
            && connected.load(Ordering::Acquire)
            && transport.media_ready()
            && wanted.sending
            && wanted.capturing
        {
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
        }
        if let Some(desktop) = desktop.as_ref() {
            // T CCDCA0/CF3450 gate HDR on the actual source, not only the
            // requested setting. Keep the request so same-source HDR recovery
            // can re-enable the negotiated format without rewriting user intent.
            negotiated.apply_source(
                &mut wanted,
                (screen.width, screen.height),
                desktop.hdr_available(),
            )?;
        }
        let settings = (
            wanted.revision,
            wanted.quality,
            wanted.auto_quality,
            wanted.bitrate,
            wanted.fps,
        );
        let changed = requested != Some(settings);
        if changed {
            automatic = (wanted.quality == 5).then(|| {
                super::parameters::AutoQuality::new(wanted.auto_quality, wanted.maximum_quality)
            });
            initial_auto = true;
            requested = Some(settings);
        }
        let on_source = |codec| {
            negotiated.choices.iter().any(|c| {
                c.capability.adapter == screen.adapter
                    && c.capability.backend != super::format::Backend::Software
                    && c.capability.format.chroma == wanted.format.chroma
                    && c.capability.format.depth == wanted.format.depth
                    && c.capability.format.codec == codec
            })
        };
        let skip_cross_hevc =
            on_source(super::format::Codec::H264) && !on_source(super::format::Codec::H265);
        let candidate = negotiated
            .choices
            .iter()
            .filter(|c| {
                negotiated.permits_codec(c.capability.format.codec)
                    && c.capability.format.chroma == wanted.format.chroma
                    && c.capability.format.depth == wanted.format.depth
                    && (c.capability.format.codec == wanted.format.codec
                        || c.capability.format.codec == super::format::Codec::H264)
                    && !(skip_cross_hevc
                        && c.capability.adapter != screen.adapter
                        && c.capability.format.codec == super::format::Codec::H265)
                    && !failed.contains(&(
                        c.capability.adapter,
                        c.capability.backend,
                        c.capability.format,
                    ))
            })
            .max_by_key(|c| {
                (
                    c.capability.format == wanted.format,
                    c.capability.backend != super::format::Backend::Software,
                    c.fps.min(wanted.maximum_fps),
                    c.capability.adapter == screen.adapter,
                    c.maximum.0.max(c.maximum.1),
                )
            })
            .context("本会话的协商编码候选已全部失败")?;
        let hardware = candidate.capability.backend != super::format::Backend::Software;
        let candidate_id = (
            candidate.capability.adapter,
            candidate.capability.backend,
            candidate.capability.format,
        );
        wanted.format = candidate.capability.format;
        wanted.maximum = candidate.maximum;
        wanted.fps = wanted.fps.min(candidate.fps).min(screen.fps).max(1);
        let maximum_quality = negotiated.maximum_quality(
            wanted.format,
            (screen.width, screen.height),
            wanted.maximum,
        );
        if let Some(auto) = automatic.as_mut() {
            auto.limit(maximum_quality);
            let (probe, lower, loss) = transport.automatic_rates();
            if auto
                .observe(
                    Instant::now(),
                    wanted.fps,
                    last_frame.is_some_and(|at| at.elapsed() < Duration::from_secs(1)),
                    probe,
                    lower,
                    loss,
                )
                .is_some()
            {
                initial_auto = false;
            }
            wanted.quality = auto.quality();
        } else if wanted.quality != 6 {
            // The fallback encoder's real size limit also limits the fixed
            // quality budget and QoS label, not only the GPU texture size.
            wanted.quality = wanted.quality.min(maximum_quality);
        }
        transport.quality(automatic.is_some(), wanted.quality, changed);
        transport.fps(wanted.fps);
        let bounds = if automatic.is_some() {
            super::parameters::automatic(wanted.quality, wanted.fps, initial_auto)
        } else {
            super::parameters::fixed(
                wanted.quality,
                wanted.bitrate,
                (screen.width, screen.height),
                wanted.fps,
            )
        };
        transport.configure(bounds, changed);
        if !connected.load(Ordering::Acquire)
            || !transport.media_ready()
            || !wanted.sending
            || !wanted.capturing
        {
            if !wanted.capturing {
                desktop = None;
                encoder = None;
                current = None;
                transfer = None;
                encoding_device = None;
                publication.send_if_modified(|state| {
                    if state.capturing {
                        state.capturing = false;
                        true
                    } else {
                        false
                    }
                });
            }
            cached = None;
            last_frame = None;
            keyframe.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if Instant::now() < next {
            std::thread::sleep(
                next.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
            continue;
        }
        if sender.capacity() == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        if desktop.is_none() {
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
        }
        let wait_ms = ((1000.0 / f64::from(wanted.fps)).round() as u32).max(1);
        let capture_started = Instant::now();
        let captured = desktop.as_mut().context("缺少采集源")?.next(
            wait_ms,
            wanted.quality,
            wanted.cursor_capture,
            wanted.format.hdr(),
            wanted.maximum,
        );
        let captured = captured?;
        let capture_state = desktop.as_ref().context("缺少采集源")?;
        if generation != capture_state.generation {
            generation = capture_state.generation;
            encoder = None;
            cached = None;
            current = None;
            last_frame = None;
            transfer = None;
            keyframe.store(true, Ordering::Release);
        }
        screen = capture_state.screen.clone();
        if wanted.format.hdr() != (requested_hdr && capture_state.hdr_available()) {
            // Capture recovery or source refresh changed the effective format.
            // Rebuild preprocessing/encoder before admitting this frame.
            cached = None;
            continue;
        }
        if !capture_state.available {
            cached = None;
            publication.send_if_modified(|state| {
                if state.capturing {
                    state.capturing = false;
                    true
                } else {
                    false
                }
            });
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let frame = match captured {
            Some(frame) => {
                cached = Some(frame.clone());
                frame
            }
            None => {
                let Some(mut frame) = cached.clone() else {
                    continue;
                };
                if last_frame.is_some_and(|at| {
                    at.elapsed() < Duration::from_secs_f64(1.0 / f64::from(wanted.fps.min(30)))
                }) {
                    continue;
                }
                frame.captured = Instant::now();
                frame.is_new = false;
                frame
            }
        };
        let size = output_size(frame.width, frame.height, wanted.quality);
        let source_device = &desktop.as_ref().context("缺少采集源")?.device;
        let device = if !hardware || candidate.capability.adapter == screen.adapter {
            source_device.clone()
        } else {
            if encoding_device
                .as_ref()
                .is_none_or(|(adapter, _)| *adapter != candidate.capability.adapter)
            {
                match capture::create_device(candidate.capability.adapter) {
                    Ok((device, _)) => {
                        encoding_device = Some((candidate.capability.adapter, device))
                    }
                    Err(error) => {
                        tracing::warn!(%error,?candidate_id,"encoder adapter failed");
                        failed.insert(candidate_id);
                        continue;
                    }
                }
            }
            encoding_device.as_ref().unwrap().1.clone()
        };
        if current.is_none_or(|(old_size, old_candidate)| {
            old_size != size || old_candidate != candidate_id
        }) {
            // Release before replacement to avoid consuming a second driver
            // session solely for a size/candidate transition (T C2D840).
            drop(encoder.take());
            let created = if hardware {
                encoder::Encoder::hardware_format(
                    &device,
                    size,
                    wanted.format,
                    super::format::Rate {
                        target: bounds.initial.max(300_000),
                        peak: bounds.initial.max(300_000),
                        fps: wanted.fps,
                        quality: wanted.quality,
                    },
                )
            } else {
                encoder::Encoder::software(
                    &device,
                    size.0,
                    size.1,
                    wanted.fps,
                    bounds.initial.max(300_000),
                )
            };
            encoder = match created {
                Ok(created) => {
                    handle.encoder(
                        created.implementation(),
                        created.maximum_size(),
                        &screen,
                        desktop.as_ref().context("缺少采集源")?.backend_name(),
                    );
                    Some(created)
                }
                Err(error) => {
                    tracing::warn!(%error,?candidate_id,"host encoder initialization failed; disabling this candidate");
                    failed.insert(candidate_id);
                    encoder = None;
                    cached = None;
                    current = None;
                    continue;
                }
            };
            current = Some((size, candidate_id));
            frame_metadata.clear();
            encode_errors = 0;
            keyframe.store(true, Ordering::Release);
        }
        let active_encoder = encoder.as_mut().context("缺少编码器")?;
        let peak = transport.media_rate().min(bounds.maximum);
        let codec_minimum = (bounds.minimum / 1000).max(30) * 1000;
        let (media_rate, discard_frame) =
            admission.next(peak, codec_minimum, transport.cwnd_ratio());
        if discard_frame {
            next = capture_started + Duration::from_secs_f64(1. / f64::from(wanted.fps));
            continue;
        }
        let needs_transfer = device != *source_device;
        let prepared = (|| -> Result<Option<super::transfer::Delivery>> {
            if !needs_transfer {
                return Ok(None);
            }
            if transfer
                .as_ref()
                .is_none_or(|t| !t.matches(source_device, &device, &frame))
            {
                transfer = Some(super::transfer::Transfer::new(
                    source_device,
                    &device,
                    &frame,
                )?);
            }
            transfer.as_mut().unwrap().copy(&frame)
        })();
        if needs_transfer && prepared.as_ref().is_ok_and(|frame| frame.is_none()) {
            next = capture_started + Duration::from_secs_f64(1. / f64::from(wanted.fps));
            continue;
        }
        let encoded = (|| -> Result<_> {
            // Delivery failures share the source-loss/candidate-recovery exits.
            let delivery = prepared?;
            if active_encoder.configure_rate(super::format::Rate {
                target: media_rate,
                peak,
                fps: wanted.fps,
                quality: wanted.quality,
            })? {
                keyframe.store(true, Ordering::Release);
            }
            // Network requests are rate-limited at their RTCP entry. Explicit
            // quality/source controls and failed inputs must not wait 600ms.
            let force = keyframe.swap(false, Ordering::AcqRel);
            let encode_started = Instant::now();
            let timestamp = (frame.captured.duration_since(started).as_nanos() / 100) as i64;
            frame_metadata.insert(
                timestamp,
                (
                    frame.captured,
                    encode_started,
                    frame.is_new,
                    frame.hdr_metadata,
                ),
            );
            while frame_metadata.len() > 2048 {
                frame_metadata.pop_first();
            }
            let encoded = active_encoder.encode(
                delivery
                    .as_ref()
                    .map_or(&frame.texture, |d| &d.frame.texture),
                timestamp,
                force,
            )?;
            for output in &encoded {
                if let Some(actual) = crate::video_format::parse_annex_b_format(
                    output.format.codec.media(),
                    &output.data,
                ) {
                    anyhow::ensure!(
                        actual.chroma_format_idc == output.format.chroma
                            && actual.bit_depth_luma == output.format.depth
                            && actual.bit_depth_chroma == output.format.depth
                            && (actual.visible_width, actual.visible_height) == size,
                        "编码器实际输出偏离协商格式"
                    );
                }
            }
            Ok((encoded, force, Instant::now()))
        })();
        let (encoded, force, encode_finished) = match encoded {
            Ok(encoded) => {
                encode_errors = 0;
                encoded
            }
            Err(error) => {
                if unsafe { source_device.GetDeviceRemovedReason() }.is_err() {
                    tracing::warn!(%error,"capture graphics device was removed; recreating selected source");
                    encoder = None;
                    current = None;
                    cached = None;
                    desktop = None;
                    transfer = None;
                    encoding_device = None;
                    last_frame = None;
                    publication.send_if_modified(|state| {
                        if state.capturing {
                            state.capturing = false;
                            true
                        } else {
                            false
                        }
                    });
                    keyframe.store(true, Ordering::Release);
                    continue;
                }
                encode_errors += 1;
                tracing::warn!(%error,hardware,consecutive=encode_errors,"host encoding failed");
                keyframe.store(true, Ordering::Release);
                next = Instant::now() + Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
                if encode_errors >= 10
                    || error.downcast_ref::<encoder::SwitchCandidate>().is_some()
                    || unsafe { device.GetDeviceRemovedReason() }.is_err()
                {
                    // T C2C610: disable the failed candidate. Do not endlessly
                    // reopen that same encoder after its consecutive failures.
                    failed.insert(candidate_id);
                    encoder = None;
                    current = None;
                    cached = None;
                    last_frame = None;
                }
                continue;
            }
        };
        if force && !encoded.iter().any(|frame| frame.keyframe) {
            keyframe.store(true, Ordering::Release);
        }
        last_frame = Some(frame.captured);
        if !encoded.is_empty() {
            let mut active = lock(&config);
            if active.revision == wanted.revision {
                active.reported_quality = wanted.quality;
            }
            let state = Published {
                screen: screen.clone(),
                capturing: true,
                quality: wanted.quality,
                fps: wanted.fps,
                encoder: Some(candidate.capability.backend),
                capture: desktop.as_ref().map_or("DXGI", |d| d.backend_name()).into(),
            };
            handle.source(&screen, &state.capture);
            publication.send_if_modified(|old| {
                if *old != state {
                    *old = state;
                    true
                } else {
                    false
                }
            });
        }
        for mut frame in encoded {
            if let Some((captured, input_started, is_new, metadata)) =
                frame_metadata.remove(&frame.timestamp_100ns)
            {
                frame.is_new = is_new;
                frame.color = frame.format.color(metadata);
                frame.timing = Some(encoder::FrameTiming {
                    captured,
                    encode_started: input_started,
                    encode_finished,
                });
            }
            // No B frames: older metadata cannot belong to a later output.
            while frame_metadata
                .first_key_value()
                .is_some_and(|(&timestamp, _)| timestamp < frame.timestamp_100ns)
            {
                frame_metadata.pop_first();
            }
            sender.blocking_send(frame).context("画面发送队列已关闭")?;
        }
        // Capture/preprocess/encode time is part of the frame interval, not an
        // additional delay. Never catch up by encoding historical screenshots.
        next = capture_started + Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
    }
    Ok(())
}
