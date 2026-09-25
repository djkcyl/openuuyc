//! Reliable business-channel binding, routing and ordered senders.
use super::statistics::send_official_streamer_statistics;
use super::workers::{SessionWorkers, std_mutex_lock};
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::{
    OutgoingControlMessage, PbMessageSource, StreamControlHandle, encode_pb_echo_request,
};
use crate::media::ConnectionMediaProfile;
use crate::transport::uu_kcp::UuKcpControl;
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::{RTCDataChannelInit, RTCDataChannelPriority};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

pub const DATA_CHANNEL_LABELS: [&str; 5] = [
    "CONTROL_DATA_CHANNEL",
    "TEXT_DATA_CHANNEL",
    "STREAMER_DATA_CHANNEL",
    "FILE_DATA_CHANNEL",
    "BINARY_DATA_CHANNEL",
];

#[derive(Clone)]
pub(super) struct DataChannels {
    pub(super) port_mapping: Arc<crate::features::port_mapping::Transport>,
    pub(super) _local_channels: Arc<Vec<Arc<RTCDataChannel>>>,
    pub(super) incoming_channels: Arc<std::sync::Mutex<Vec<Arc<RTCDataChannel>>>>,
    pub(super) performance: PerformanceMonitor,
    pub(super) streamer_sender_started: Arc<AtomicBool>,
    pub(super) stream_control: StreamControlHandle,
    pub(super) uu_kcp: UuKcpControl,
    pub(super) workers: Arc<SessionWorkers>,
}

impl DataChannels {
    pub(super) fn new(
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
        let port_mapping = Arc::new(crate::features::port_mapping::Transport::default());
        for channel in &local_channels {
            stream_control.file_transfer().bind(channel);
            if channel.label() == "FILE_DATA_CHANNEL" {
                port_mapping.bind(channel);
                let binary = Arc::clone(channel);
                let mapping = Arc::clone(&port_mapping);
                let files = Arc::clone(stream_control.file_transfer());
                workers.spawn(async move {
                    binary
                        .set_buffered_amount_low_threshold(4 * 1024 * 1024)
                        .await;
                    binary
                        .on_buffered_amount_low(Box::new(move || {
                            mapping.wake();
                            files.wake();
                            Box::pin(async {})
                        }))
                        .await;
                });
            }
            Self::install_handlers(
                channel,
                performance.clone(),
                Arc::clone(&streamer_sender_started),
                stream_control.clone(),
                true,
                uu_kcp.clone(),
                &workers,
                Arc::clone(&port_mapping),
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
        let clipboard = stream_control.clipboard().clone();
        let clipboard_channel = text_channel.clone();
        workers.spawn(async move {
            clipboard.run_sender(clipboard_channel).await;
        });
        workers.spawn(send_remote_input(
            control_channel.clone(),
            uu_kcp.clone(),
            stream_control.mouse().clone(),
        ));
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
            port_mapping,
            incoming_channels: Arc::new(std::sync::Mutex::new(Vec::new())),
            performance,
            streamer_sender_started,
            stream_control,
            uu_kcp,
            workers,
        }
    }

    pub(super) fn install_handlers(
        channel: &Arc<RTCDataChannel>,
        performance: PerformanceMonitor,
        streamer_sender_started: Arc<AtomicBool>,
        stream_control: StreamControlHandle,
        local_channel: bool,
        uu_kcp: UuKcpControl,
        workers: &Arc<SessionWorkers>,
        port_mapping: Arc<crate::features::port_mapping::Transport>,
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
        let open_mapping = Arc::clone(&port_mapping);
        channel.on_open(Box::new(move || {
            open_mapping.wake();
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
        let close_mapping = Arc::clone(&port_mapping);
        channel.on_close(Box::new(move || {
            if local_channel && matches!(label.as_str(), "TEXT_DATA_CHANNEL" | "FILE_DATA_CHANNEL")
            {
                close_stream_control.file_transfer().close();
            }
            if local_channel && label == "FILE_DATA_CHANNEL" {
                close_mapping.close();
            }
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
            let mapping=Arc::clone(&port_mapping);
            let label = label.clone();
            let stream_control = message_stream_control.clone();
            Box::pin(async move {
                if matches!(label.as_str(), "TEXT_DATA_CHANNEL" | "FILE_DATA_CHANNEL") && !message.data.starts_with(b"{")
                    && stream_control.file_transfer().receive(&message.data).await.unwrap_or_else(|error| {
                        tracing::warn!(%error,"invalid file transfer message"); false
                    }) {
                    // Only solicited transfer RPCs reach their registered task.
                } else if label=="FILE_DATA_CHANNEL" {
                    if let Err(error)=mapping.receive(&message.data) {tracing::warn!(%error,"invalid port mapping message");}
                } else if label == "TEXT_DATA_CHANNEL" && !message.data.starts_with(b"{")
                    && stream_control.clipboard().receive(&message.data).unwrap_or_else(|error| {
                        stream_control.clipboard().protocol_error(error.to_string());
                        tracing::warn!(%error,"invalid clipboard message"); true
                    }) {
                    // Clipboard owns only its RPC fields, independently of video requests.
                } else if label == "STREAMER_DATA_CHANNEL" {
                    // Peer media_inbounds describe the peer's receive direction.
                    // This client has no UU video sender. Do not use
                    // those reports as measurements of our local video pipeline.
                    tracing::trace!(bytes = message.data.len(), "peer receiver statistics not used for incoming video");
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

    pub(super) fn attach_remote_channel(&self, channel: Arc<RTCDataChannel>) {
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
            Arc::clone(&self.port_mapping),
        );
        std_mutex_lock(&self.incoming_channels).push(channel);
    }
}

pub(super) async fn send_official_control_messages(
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
    let mut annotation_tick = tokio::time::interval(Duration::from_millis(16));
    annotation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
            _ = annotation_tick.tick() => stream_control.annotation_tick(),
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
                match uu_kcp.send_control(&control_channel, encode_pb_echo_request()).await {
                    Ok(bytes) => tracing::debug!(attempt = attempts, bytes, "sent official protobuf ECHO_REQUEST"),
                    Err(error) => tracing::warn!(attempt = attempts, %error, "protobuf ECHO_REQUEST send failed"),
                }
            }
            message = messages.recv() => {
                let Some(message) = message else { break; };
                if let Some(generation) = message.annotation_generation {
                    if !stream_control.annotation_message_current(message.sequence, generation) { continue; }
                }
                let is_annotation = message.annotation_generation.is_some();
                let payload = Bytes::from(message.payload);
                let is_screen_request = message.completion.is_some();
                match text_channel.send_text_bytes(&payload).await {
                    Ok(bytes) => {
                        if let Some(done) = message.completion { let _ = done.send(Ok(())); }
                        if is_annotation { tracing::debug!(sequence = message.sequence, bytes, "annotation request sent"); }
                        else { tracing::info!(sequence = message.sequence, bytes, is_screen_request, protocol = message.protocol.label(), "viewing control request sent"); }
                    }
                    Err(error) => {
                        let description = error.to_string();
                        if let Some(done) = message.completion { let _ = done.send(Err(description.clone())); }
                        if is_annotation { stream_control.annotation_send_failed(message.sequence, &description); }
                        else if !is_screen_request { stream_control.mark_send_failed(message.sequence, &description); }
                        tracing::warn!(sequence = message.sequence, %error, "runtime viewer request send failed");
                    }
                }
            }
            response = echo_responses.recv() => {
                let Some(response) = response else { break; };
                match uu_kcp.send_control(&control_channel, response).await {
                    Ok(bytes) => tracing::debug!(bytes, "sent official protobuf ECHO_RESPONSE"),
                    Err(error) => tracing::warn!(%error, "protobuf ECHO_RESPONSE send failed"),
                }
            }
        }
    }
}

pub(super) async fn send_remote_input(
    channel: Arc<RTCDataChannel>,
    kcp: UuKcpControl,
    mouse: crate::features::remote_input::RemoteInput,
) {
    let mut heartbeat = tokio::time::interval(Duration::from_millis(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keyboard_submission_seen = false;
    loop {
        let event = tokio::select! {
            event = mouse.next() => event,
            _ = heartbeat.tick() => { mouse.heartbeat(); continue; }
        };
        if !mouse.is_current(&event) {
            mouse.discard(&event);
            continue;
        }
        let result = tokio::select! {
            biased;
            _ = mouse.epoch_cancelled(event.epoch) => Err(anyhow::anyhow!("鼠标连接代次已变更")),
            result = tokio::time::timeout(event.event.send_timeout(),
                async {
                    let state=mouse.clone();let guarded=event.clone();
                    let release=matches!(event.event,crate::features::remote_input::InputEvent::Button{down:false,..}|crate::features::remote_input::InputEvent::Key{down:false,..}|crate::features::remote_input::InputEvent::AssistButton{down:false,..});
                    if kcp.is_negotiated(){kcp.send_input(channel.id(),event.event.encode(),Arc::new(move||state.is_current(&guarded)),release).await}
                    else {kcp.send_control(&channel,event.event.encode()).await}
                }) => result
                .map_err(|_| anyhow::anyhow!("鼠标输入发送超时"))
                .and_then(|result| result.map(|_| ())),
        };
        if let Err(error) = &result
            && mouse.is_current(&event)
        {
            tracing::warn!(target: "openuuyc::transport::rtc::input", %error, "mouse input transport failed");
        }
        if !keyboard_submission_seen
            && result.is_ok()
            && matches!(
                event.event,
                crate::features::remote_input::InputEvent::Key { .. }
            )
        {
            keyboard_submission_seen = true;
            tracing::debug!(target: "openuuyc::transport::rtc::input", "keyboard event submitted to CONTROL transport");
        }
        mouse.complete(&event, result);
    }
}

pub(super) fn official_data_channel_init(label: &str) -> RTCDataChannelInit {
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
