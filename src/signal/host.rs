//! Host role of the authenticated online room; no second login or parallel room.
use super::*;
use crate::host::{capture, encoder, peer::Peer};
use prost::Message as _;
use std::sync::Arc;

pub(crate) enum Event {
    Change,
    Candidate(RTCIceCandidateInit),
    Poll,
}
pub(crate) struct Session {
    client: Arc<crate::client::AuthenticatedClient>,
    desired: tokio::sync::watch::Receiver<Option<crate::host::ShareRequest>>,
    selected: Option<capture::Screen>,
    authorization: Option<crate::host::Lease>,
    peer: Option<Peer>,
    client_id: String,
    ice_id: String,
    app_control_id: String,
    initialized: bool,
    capabilities: Vec<crate::host::format::Capability>,
    active_encoder: Option<(i32, (u32, u32), String)>,
}
impl Session {
    pub(crate) fn new(client: Arc<crate::client::AuthenticatedClient>) -> Self {
        Self {
            desired: client.host.subscribe(),
            client,
            selected: None,
            authorization: None,
            peer: None,
            client_id: String::new(),
            ice_id: String::new(),
            app_control_id: String::new(),
            initialized: false,
            capabilities: Vec::new(),
            active_encoder: None,
        }
    }
    pub(crate) async fn next(&mut self) -> Event {
        if !self.initialized {
            self.initialized = true;
            return Event::Change;
        }
        tokio::select! {
            _=self.desired.changed()=>Event::Change,
            candidate=async {match &mut self.peer {Some(peer)=>peer.candidates.recv().await,None=>pending().await}}=>{
                candidate.map_or(Event::Poll,Event::Candidate)
            },
            _=tokio::time::sleep(Duration::from_millis(250))=>Event::Poll,
        }
    }
    pub(crate) async fn apply(&mut self, event: Event, signal: &mut SignalSession) -> Result<()> {
        match event {
            Event::Change => {
                let selected = self.desired.borrow_and_update().clone();
                self.selected = None;
                self.authorization = None;
                self.end_control(signal).await;
                if let Some(request) = selected {
                    let lease = self.client.host.lease(&request);
                    if !lease.requested() {
                        return Ok(());
                    }
                    let screen = request.screen;
                    lease.update(false, false, "正在准备画面共享…");
                    let selected = screen.clone();
                    let probing = lease.clone();
                    let probe = tokio::task::spawn_blocking(
                        move || -> Result<Vec<crate::host::format::Capability>> {
                            let _runtime = encoder::Runtime::new()?;
                            let mut desktop = capture::Desktop::open_selected(&selected)?;
                            encoder::probe(&mut desktop, &probing)
                        },
                    )
                    .await
                    .context("画面能力检查线程退出")?;
                    if !lease.requested() {
                        return Ok(());
                    }
                    let result = match probe {
                        Ok(capabilities) => {
                            self.capabilities = capabilities;
                            self.client.set_controllable(true).await
                        }
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(()) if lease.requested() && self.client.is_active() => {
                            self.selected = Some(screen);
                            lease.update(true, false, "等待另一台设备连接");
                            self.authorization = Some(lease);
                        }
                        Ok(()) => {
                            let _ = self.client.set_controllable(false).await;
                        }
                        Err(error) => {
                            lease.stop_with_error(format!("无法开启共享：{error:#}"));
                        }
                    }
                } else {
                    self.client.host.update(false, false, "画面共享已关闭");
                    if let Err(error) = self.client.set_controllable(false).await {
                        self.client.host.update(
                            false,
                            false,
                            format!("本地共享已关闭，在线状态更新失败：{error:#}"),
                        );
                    }
                }
            }
            Event::Candidate(candidate) => {
                let Some(peer) = self.peer.as_ref() else {
                    return Ok(());
                };
                let Some(candidate) = peer.local_candidate(candidate).await else {
                    return Ok(());
                };
                let payload = serde_json::json!({"client_id":self.client_id,"data":{
                    "app_control_id":self.app_control_id,"ice_id":self.ice_id,"ice_network_type":0,"type":"candidate","candidate":candidate}});
                signal
                    .send_text(encode_event("soac", &[payload], None)?)
                    .await?;
            }
            Event::Poll => {
                if let Some(attempt) = self.peer.as_ref().and_then(Peer::network_change) {
                    let payload = serde_json::json!({"client_id":self.client_id,"data":{"type":"switch_network_notify","switch_network_notify":{
                        "transport_type":1,"ice_id":self.ice_id,"attempt_switch_type":attempt}}});
                    signal
                        .send_text(encode_event("forward_setting", &[payload], None)?)
                        .await?;
                }
                if self.peer.is_some() && self.client.host.requested() {
                    let status = self.client.host.status();
                    if let Some((implementation, maximum)) = status.encoder {
                        let capture = status.capture.as_deref().unwrap_or("DXGI");
                        let metadata = (implementation, maximum, capture.to_owned());
                        if self.active_encoder.as_ref() != Some(&metadata) {
                            use crate::host::format::Backend;
                            let encoder = match implementation {
                                0 => Backend::Nvidia,
                                1 => Backend::Amd,
                                2 => Backend::Intel,
                                _ => Backend::Software,
                            }
                            .name();
                            let payload = serde_json::json!({"client_id":self.client_id,"data":{"type":"sender_para_info","sender_para_info":{
                                "ice_id":self.ice_id,"video_track_index":0,"capture_impl":capture,"encoder_impl":encoder,
                                "sender_media_infos":[{"video_track_index":0,"capture_impl":capture,"encoder_impl":encoder}]}}});
                            signal
                                .send_text(encode_event("forward_setting", &[payload], None)?)
                                .await?;
                            tracing::info!(
                                encoder,
                                capture,
                                video_track_index = 0,
                                "host sender media info published"
                            );
                            self.active_encoder = Some(metadata);
                        }
                    }
                    if let Some(screen) = status.screen {
                        if self.selected.as_ref() != Some(&screen) {
                            self.send_capability(&screen, signal).await?;
                            self.selected = Some(screen);
                        }
                    }
                }

                if !self.client.host.requested() {
                    self.end_control(signal).await;
                } else if self.peer.as_ref().is_some_and(Peer::ended) {
                    // A media failure is not a user kick. Preserve the remote
                    // controller's normal reconnect/rejoin classification.
                    if let Some(peer) = self.peer.take() {
                        peer.close().await;
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) async fn event(
        &mut self,
        event: &str,
        args: &[Value],
        binary: &[Vec<u8>],
        signal: &mut SignalSession,
    ) -> Result<()> {
        if !self.client.is_active() || !self.client.host.requested() {
            return Ok(());
        }
        match event {
            "be-controlled" => {
                let authorization = self.authorization.clone().context("本机共享尚未准备就绪")?;
                anyhow::ensure!(authorization.requested(), "本次共享授权已结束");
                tracing::info!("host received authenticated be-controlled event");
                let value = args.first().context("缺少被控会话参数")?;
                let required = |key: &str| {
                    value
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty() && v.len() <= 256)
                        .map(str::to_owned)
                        .context("被控会话标识缺失或无效")
                };
                let client_id = required("client_id")?;
                let ice_id = required("ice_id")?;
                let app_control_id = required("app_control_id")?;
                let index = value
                    .get("app_data")
                    .filter(|v| v.get("_placeholder").and_then(Value::as_bool) == Some(true))
                    .and_then(|v| v.get("num"))
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .context("缺少ConnectOptions二进制引用")?;
                let options = crate::stream_control::publisher::ConnectOptions::decode(
                    binary
                        .get(index)
                        .context("缺少ConnectOptions二进制内容")?
                        .as_slice(),
                )?;
                if options.kind != 1 || options.force_virtual || options.connect_type != 1 {
                    bail!("首批画面共享仅接受账号内普通桌面连接")
                }
                let screen = self.selected.clone().context("本机尚未允许共享")?;
                anyhow::ensure!(
                    options.screen_id == -1 || options.screen_id == screen.id,
                    "只能连接本机明确选择共享的屏幕"
                );
                let streamer = value.get("streamer_data").context("缺少主控能力参数")?;
                let streamer = if let Some(text) = streamer.as_str() {
                    serde_json::from_str::<Value>(text)?
                } else {
                    streamer.clone()
                };
                let remote: crate::capability::DeviceCapability = serde_json::from_value(
                    streamer
                        .get("device_capability")
                        .cloned()
                        .context("缺少主控解码能力")?,
                )?;
                let negotiated = Arc::new(crate::host::format::Negotiated::new(
                    &self.capabilities,
                    &remote,
                    &options.decoders,
                )?);
                let mut initial = crate::stream_control::publisher::config(options.params.as_ref());
                let chroma = options
                    .params
                    .as_ref()
                    .map_or(1, |p| if p.chroma == 3 { 3 } else { 1 });
                let hdr = options.params.as_ref().is_some_and(|p| p.hdr);
                negotiated.apply(
                    &mut initial,
                    None,
                    chroma,
                    hdr,
                    (screen.width, screen.height),
                )?;
                let servers: Vec<ControlIceServer> = serde_json::from_value(
                    value
                        .get("iceServers")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )?;
                let servers = servers
                    .into_iter()
                    .map(|s| webrtc::ice_transport::ice_server::RTCIceServer {
                        urls: vec![s.urls],
                        username: s.username,
                        credential: s.credential,
                        ..Default::default()
                    })
                    .collect();
                if let Some(peer) = self.peer.take() {
                    peer.close().await;
                }
                let peer = Peer::new(
                    screen.clone(),
                    authorization.clone(),
                    servers,
                    value
                        .get("force_relay")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    initial,
                    negotiated,
                    crate::host::network::Policy::from_signal(value),
                )
                .await?;
                if !authorization.requested() || !self.client.is_active() {
                    peer.close().await;
                    return Ok(());
                }
                self.client_id = client_id;
                self.ice_id = ice_id;
                self.app_control_id = app_control_id;
                self.peer = Some(peer);
                self.active_encoder = None;
                self.send_capability(&screen, signal).await?;
                authorization.update(true, false, "控制端已接入，等待画面协商");
            }
            "soac" => {
                let value = args.first().context("缺少SOAC")?;
                if value
                    .get("client_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != self.client_id)
                {
                    return Ok(());
                }
                let data = value.get("data").context("缺少SOAC data")?;
                if data.get("ice_id").and_then(Value::as_str) != Some(self.ice_id.as_str())
                    || data
                        .get("app_control_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id != self.app_control_id)
                {
                    return Ok(());
                }
                let peer = self.peer.as_ref().context("没有活动被控会话")?;
                match data.get("type").and_then(Value::as_str) {
                    Some("offer" | "restart_ice") => {
                        let sdp = data
                            .get("sdp")
                            .and_then(Value::as_str)
                            .context("缺少offer SDP")?;
                        let restart =
                            data.get("type").and_then(Value::as_str) == Some("restart_ice");
                        let network_type = data
                            .get("ice_network_type")
                            .and_then(Value::as_u64)
                            .and_then(|v| u8::try_from(v).ok())
                            .unwrap_or(0);
                        let answer = peer.answer(sdp.to_owned(), restart, network_type).await?;
                        let attachment = gzip_sdp(&answer)?;
                        let payload = serde_json::json!({"client_id":self.client_id,"data":{"app_control_id":self.app_control_id,
                            "ice_id":self.ice_id,"type":if restart {"restart_ice"} else {"answer"},"ice_network_type":network_type,
                            "sdp":"","gzip_sdp":{"_placeholder":true,"num":0}}});
                        signal.send_binary_packet(
                            encode_binary_event("soac", &[payload], None, 1)?,
                            attachment,
                        )?;
                    }
                    Some("candidate") => {
                        peer.add_candidate(serde_json::from_value(
                            data.get("candidate").cloned().context("缺少candidate")?,
                        )?)
                        .await?
                    }
                    _ => {}
                }
            }
            "left" | "released" => {
                // AFE890 installs the same AFB1F0 consumer for both events;
                // AA81A0 requires a nonempty ice_id before releasing a peer.
                if args
                    .first()
                    .and_then(|v| v.get("ice_id"))
                    .and_then(Value::as_str)
                    != Some(self.ice_id.as_str())
                    || self.ice_id.is_empty()
                {
                    return Ok(());
                }
                if args.first().is_some_and(|v| {
                    v.get("ice_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id != self.ice_id)
                        || v.get("app_control_id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id != self.app_control_id)
                }) {
                    return Ok(());
                }
                if args
                    .first()
                    .and_then(|v| v.get("client_id"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != self.client_id)
                {
                    return Ok(());
                }
                if let Some(peer) = self.peer.take() {
                    peer.close().await;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(crate) async fn signaling_restored(&mut self, signal: &mut SignalSession) -> Result<()> {
        if self.peer.as_ref().is_some_and(|peer| !peer.ended())
            && self
                .authorization
                .as_ref()
                .is_some_and(|lease| lease.requested())
        {
            // A physical socket loss drops its queued messages. Publish the
            // current idempotent snapshot again; never replay old RPC actions.
            self.active_encoder = None;
            if let Some(screen) = self.selected.clone() {
                self.send_capability(&screen, signal).await?;
            }
            tracing::info!("host signaling restored; current media metadata scheduled");
        }
        Ok(())
    }
    async fn end_control(&mut self, signal: &mut SignalSession) {
        let Some(peer) = self.peer.take() else {
            return;
        };
        // S 2F97E0 -> 313FC0 -> ControlledInterface[13], T AFCAB0 ->
        // AFC830: clear_out (no arguments) precedes local peer destruction.
        // This only clears viewers of our own explicitly shared host room.
        let submitted = async {
            let packet = encode_event("clear_out", &[], None)?;
            signal
                .transport
                .send_connected(vec![Message::Text(packet.into())])
                .await
        }
        .await;
        match submitted {
            Ok(()) => tracing::info!("host active control termination submitted"),
            Err(error) => {
                tracing::warn!(%error, "host termination notice unavailable; stopping locally without replay")
            }
        }
        peer.close().await;
    }

    async fn send_capability(
        &self,
        screen: &capture::Screen,
        signal: &mut SignalSession,
    ) -> Result<()> {
        let capability = crate::capability::DeviceCapability {
            ice_id: self.ice_id.clone(),
            display_info: vec![crate::capability::DisplayCapability {
                id: screen.id,
                fps: screen.fps,
                kind: 0,
                hdr: i32::from(screen.hdr),
            }],
            video_codec_capability: self.capabilities.iter().map(|c| c.wire()).collect(),
        };
        let payload = serde_json::json!({"client_id":self.client_id,"data":{"type":"device_capability","device_capability":capability}});
        signal
            .send_text(encode_event("forward_setting", &[payload], None)?)
            .await?;
        Ok(())
    }
    pub(crate) async fn close(mut self) {
        if let Some(peer) = self.peer.take() {
            peer.close().await;
        }
        self.client.host.update(false, false, "在线房间已关闭");
    }
}
