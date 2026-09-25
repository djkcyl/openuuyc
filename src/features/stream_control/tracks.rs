//! Capture activation and negotiated video-track registration.
use super::wire::{
    PbControlMessage, PbPayload, PbRequestHeader, PbRpcRequest, PbRpcRequestPayload,
    PbSendVideoTrackRequest, PbSimpleAction, encode_envelope,
};
use super::{
    OutgoingControlMessage, StreamControlHandle, StreamControlState, ensure_business_ready,
    ensure_ready, feature_supported, lock, protocol,
};
use anyhow::{Result, anyhow, bail};
use prost::Message as _;

impl StreamControlHandle {
    pub(crate) async fn stop_acquire_update(&self) -> Result<()> {
        let (complete, done) = tokio::sync::oneshot::channel();
        {
            let mut state = lock(&self.shared);
            ensure_ready(&state)?;
            if state.remote_upgrade.is_none() {
                bail!("当前会话不支持被控端更新");
            }
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            self.outgoing
                .send(OutgoingControlMessage {
                    annotation_generation: None,
                    sequence,
                    payload: PbControlMessage {
                        seq: 0,
                        timestamp: 0,
                        payload: Some(PbPayload::SimpleAction(PbSimpleAction {
                            action: 20,
                            args: String::new(),
                            params: None,
                        })),
                    }
                    .encode_to_vec(),
                    protocol: protocol(&state),
                    completion: Some(complete),
                })
                .map_err(|_| anyhow!("观看连接已关闭"))?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), done)
            .await
            .map_err(|_| anyhow!("延后安装请求发送超时"))?
            .map_err(|_| anyhow!("观看连接已关闭"))?
            .map_err(anyhow::Error::msg)
    }

    /// Ordinary desktop capture only. Negative IDs include all-screen actions
    /// and are never accepted from a single monitor/window operation.
    pub async fn set_screen_capture(&self, screen_id: i32, active: bool) -> Result<()> {
        if active {
            let state = lock(&self.shared);
            if screen_id != state.current_screen_id
                && !feature_supported(
                    &state,
                    crate::account::feature_ability::Feature::MultiScreen,
                )
            {
                bail!("官方当前能力配置未开放多屏观看");
            }
        }
        if screen_id < 0
            || !self
                .snapshot()
                .screens
                .iter()
                .any(|screen| screen.id == screen_id)
        {
            bail!("显示器已不可用");
        }
        if active {
            self.ensure_video_tracks_registered().await?;
        }
        let (complete, done) = tokio::sync::oneshot::channel();
        {
            let mut state = lock(&self.shared);
            ensure_business_ready(&state)?;
            if screen_id < 0 || !state.screens.iter().any(|screen| screen.id == screen_id) {
                bail!("显示器已不可用");
            }
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            let payload = PbControlMessage {
                seq: sequence,
                timestamp: 0,
                payload: Some(PbPayload::SimpleAction(PbSimpleAction {
                    action: if active { 8 } else { 7 },
                    args: serde_json::json!({"screen_id":screen_id}).to_string(),
                    params: None,
                })),
            }
            .encode_to_vec();
            self.outgoing
                .send(OutgoingControlMessage {
                    annotation_generation: None,
                    sequence,
                    payload,
                    protocol: protocol(&state),
                    completion: Some(complete),
                })
                .map_err(|_| anyhow!("观看连接已关闭"))?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), done)
            .await
            .map_err(|_| anyhow!("屏幕采集请求发送超时"))?
            .map_err(|_| anyhow!("观看连接已关闭"))?
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn set_available_video_tracks(&self, mut tracks: Vec<i32>) {
        tracks.retain(|index| *index >= 0);
        tracks.sort_unstable();
        tracks.dedup();
        let mut state = lock(&self.shared);
        if state.available_video_tracks != tracks {
            tracing::info!(
                ?tracks,
                "negotiated remote video tracks available for capture registration"
            );
            state.available_video_tracks = tracks;
            state.track_registration_error = None;
        }
        self.maybe_register_video_tracks(&mut state);
    }

    pub(super) async fn ensure_video_tracks_registered(&self) -> Result<()> {
        {
            let mut state = lock(&self.shared);
            ensure_business_ready(&state)?;
            if state.available_video_tracks.is_empty() {
                bail!("被控端未协商可用的视频轨道");
            }
            state.track_registration_error = None;
            self.maybe_register_video_tracks(&mut state);
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let state = lock(&self.shared);
                    if let Some(error) = &state.track_registration_error {
                        bail!("{error}");
                    }
                    if state.available_video_tracks == state.registered_video_tracks {
                        return Ok(());
                    }
                    if !state.pb_connected {
                        bail!("观看连接已断开");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| {
            let mut state = lock(&self.shared);
            state.track_registration = None;
            state.track_registration_error = Some("视频轨道注册未收到确认".into());
            anyhow!("视频轨道注册未收到确认")
        })?
    }

    pub(super) fn maybe_register_video_tracks(&self, state: &mut StreamControlState) {
        if !state.pb_connected
            || !state.text_channel_open
            || state.available_video_tracks.is_empty()
            || state.available_video_tracks == state.registered_video_tracks
            || state.track_registration.is_some()
            || state.track_registration_error.is_some()
        {
            return;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        let tracks = state.available_video_tracks.clone();
        let request = PbRpcRequest {
            request_header: Some(PbRequestHeader {
                request_id: sequence,
            }),
            payload: Some(PbRpcRequestPayload::SendVideoTrack(
                PbSendVideoTrackRequest {
                    video_track_index: tracks.clone(),
                },
            )),
        };
        let payload = encode_envelope(sequence, PbPayload::RpcRequest(request.encode_to_vec()));
        if self
            .outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: protocol(state),
                completion: None,
            })
            .is_ok()
        {
            state.track_registration = Some((sequence, tracks));
        } else {
            state.track_registration_error = Some("视频轨道注册发送失败".into());
        }
    }
}
