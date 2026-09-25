//! Sending-side business protocol; deliberately separate from viewer consumers.
use super::*;
use crate::host::{VideoConfig, capture::Screen};

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ConnectOptions {
    #[prost(int32, tag = "1")]
    pub kind: i32,
    #[prost(int32, tag = "2")]
    pub screen_id: i32,
    #[prost(message, optional, tag = "3")]
    pub params: Option<CaptureParams>,
    #[prost(message, repeated, tag = "4")]
    pub decoders: Vec<DecoderCapability>,
    #[prost(bool, tag = "5")]
    pub force_virtual: bool,
    #[prost(string, tag = "9")]
    pub device_id: String,
    #[prost(int32, tag = "10")]
    pub connect_type: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct DecoderCapability {
    #[prost(int32, tag = "1")]
    pub fps: i32,
    #[prost(int32, tag = "2")]
    pub codec: i32,
    #[prost(int32, tag = "3")]
    pub width: i32,
    #[prost(int32, tag = "4")]
    pub height: i32,
    #[prost(int32, tag = "5")]
    pub chroma: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CaptureParams {
    #[prost(int32, tag = "1")]
    pub fps: i32,
    #[prost(int32, tag = "2")]
    pub quality: i32,
    #[prost(bool, tag = "3")]
    pub cursor_capture: bool,
    #[prost(int32, tag = "7")]
    pub chroma: i32,
    #[prost(int32, tag = "8")]
    pub bitrate: i32,
    #[prost(bool, tag = "9")]
    pub hdr: bool,
    #[prost(int32, tag = "10")]
    pub auto_quality: i32,
    #[prost(int32, tag = "11")]
    pub fps_count: i32,
}

fn flags() -> PbFeatureFlag {
    PbFeatureFlag {
        capture_setting: 6,
        qos_stat: 1,
        ..Default::default()
    }
}

pub(crate) fn echo(seq: i64, timestamp: i64, request: bool) -> Vec<u8> {
    PbControlMessage {
        seq,
        timestamp,
        payload: Some(PbPayload::SimpleAction(PbSimpleAction {
            action: if request { 0 } else { 1 },
            args: serde_json::json!({"seq":seq}).to_string(),
            params: Some(PbSimpleActionParams::FeatureFlag(flags())),
        })),
    }
    .encode_to_vec()
}

pub(crate) fn screen_state(screen: &Screen, capturing: bool) -> Vec<u8> {
    let rect = PbWinRect {
        left: screen.left,
        top: screen.top,
        width: screen.width as i32,
        height: screen.height as i32,
        pixel_width: screen.width as i32,
        pixel_height: screen.height as i32,
    };
    PbControlMessage {
        payload: Some(PbPayload::Screens(PbScreenSources {
            current_screen_id: screen.id,
            screens: vec![PbScreen {
                id: screen.id,
                fps: screen.fps.min(i32::MAX as u32) as i32,
                resolutions: vec![rect.clone()],
                current_resolution: Some(rect.clone()),
                init_resolution: Some(rect),
                screen_type: 0,
                is_primary_screen: screen.primary,
                dpr: 1.0,
                dpi_scale: screen.dpi_scale.map(|dpi| PbDpiScale {
                    current_dpi: dpi as i32,
                    recommended_dpi: dpi as i32,
                    dpis: vec![dpi as i32],
                }),
                display_name: screen.name.clone(),
                resolution_type: 1,
                video_track_index: if capturing { 0 } else { -1 },
                builtin_screen_type: 0,
            }],
        })),
        ..Default::default()
    }
    .encode_to_vec()
}

pub(crate) fn capture_change(screen: &Screen, capturing: bool) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Change {
        #[prost(int32, tag = "1")]
        kind: i32,
        #[prost(int32, tag = "2")]
        value: i32,
    }
    PbControlMessage {
        payload: Some(PbPayload::CaptureChange(
            Change {
                kind: if capturing { 0 } else { 99 },
                value: if capturing { screen.id } else { 0 },
            }
            .encode_to_vec(),
        )),
        ..Default::default()
    }
    .encode_to_vec()
}
pub(crate) fn permissions(visible: bool) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Permission {
        #[prost(bool, tag = "2")]
        video: bool,
        #[prost(bool, tag = "3")]
        visible: bool,
        #[prost(bool, tag = "4")]
        audio: bool,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct State {
        #[prost(message, optional, tag = "3")]
        permission: Option<Permission>,
    }
    PbControlMessage {
        payload: Some(PbPayload::SystemStateChange(
            State {
                permission: Some(Permission {
                    video: true,
                    visible,
                    audio: false,
                }),
            }
            .encode_to_vec(),
        )),
        ..Default::default()
    }
    .encode_to_vec()
}
pub(crate) fn quality_report(
    quality: i32,
    probe: u32,
    source: (u32, u32),
    fps: u32,
    encoder: crate::host::format::Backend,
    capture: &str,
) -> Vec<u8> {
    // T C8FF40: custom has no fast/general/hd/bluray reference budgets.
    let budget = |q| {
        if quality == 6 {
            0
        } else {
            u64::from(crate::host::parameters::fixed(q, 0, source, fps).maximum)
        }
    };
    PbControlMessage {
        payload: Some(PbPayload::ReportQosStats(PbReportQosStats {
            encoder_type: encoder.qos_type().into(),
            capture_type: capture.into(),
            probe_bps: probe.into(),
            video_quality: quality,
            fast_bitrate: budget(1),
            general_bitrate: budget(2),
            hd_bitrate: budget(3),
            bluray_bitrate: budget(4),
        })),
        ..Default::default()
    }
    .encode_to_vec()
}

pub(crate) fn config(params: Option<&CaptureParams>) -> VideoConfig {
    let Some(p) = params else {
        return VideoConfig::default();
    };
    VideoConfig {
        fps: fps(p.fps, p.fps_count),
        requested_fps: fps(p.fps, 0),
        fps_limit: if p.fps_count > 0 {
            p.fps_count as u32
        } else {
            144
        },
        bitrate: bitrate(p.quality, p.auto_quality, p.bitrate),
        quality: p.quality.clamp(1, 6),
        auto_quality: p.auto_quality.clamp(1, 4),
        revision: 0,
        reported_quality: if p.quality == 5 {
            p.auto_quality.clamp(1, 4)
        } else {
            p.quality.clamp(1, 6)
        },
        sending: true,
        capturing: true,
        cursor_capture: p.cursor_capture,
        ..Default::default()
    }
}
fn fps(level: i32, count: i32) -> u32 {
    let requested = match level {
        1 => 30,
        2 => 60,
        3 => 90,
        4 => 144,
        _ => 30,
    };
    if count > 0 {
        requested.min(count as u32)
    } else {
        requested
    }
}
fn bitrate(quality: i32, auto: i32, custom: i32) -> u32 {
    match if quality == 5 { auto } else { quality } {
        4 => 30_000_000,
        3 => 14_000_000,
        // Both CaptureParams and CaptureSetting carry bits/second on the wire.
        6 => custom.clamp(1_000_000, MAX_CUSTOM_BITRATE_MBPS as i32 * 1_000_000) as u32,
        _ => 8_000_000,
    }
}

/// Unsupported operations never succeed silently or reach a local side-effect handler.
pub(crate) fn receive(
    bytes: &[u8],
    control: bool,
    screen: &Screen,
    config: &mut VideoConfig,
    negotiated: &crate::host::format::Negotiated,
) -> Result<Vec<Vec<u8>>> {
    if bytes.first() == Some(&b'{') {
        return Ok(Vec::new());
    }
    let msg = PbControlMessage::decode(bytes)?;
    match msg.payload {
        Some(PbPayload::SimpleAction(action)) if control && action.action == 0 => {
            Ok(vec![echo(msg.seq, msg.timestamp, false)])
        }
        Some(PbPayload::SimpleAction(action)) if !control && matches!(action.action, 7 | 8) => {
            let args: serde_json::Value = serde_json::from_str(&action.args)?;
            // The current server also has a silent-upgrade variant of action 8.
            // Only the explicitly selected existing screen is in our scope.
            anyhow::ensure!(
                args.get("scene").is_none_or(|v| v.as_str() == Some("")),
                "不支持的采集重启场景"
            );
            anyhow::ensure!(
                args.get("screen_id")
                    .and_then(serde_json::Value::as_i64)
                    .is_some_and(|id| id == -1 || id == i64::from(screen.id)),
                "只能启停已选择的屏幕"
            );
            config.capturing = action.action == 8;
            tracing::info!(
                capturing = config.capturing,
                "host capture selection applied"
            );
            // CaptureChange/ScreenSources follow the capture owner's actual transition.
            Ok(Vec::new())
        }
        Some(PbPayload::RpcRequest(bytes)) if !control => {
            let request = PbRpcRequest::decode(bytes.as_slice())?;
            let header = request.request_header.map(|h| PbResponseHeader {
                request_id: h.request_id,
            });
            let payload = if let Some(setting) = request.capture_setting {
                tracing::info!(
                    screen_id = setting.screen_id,
                    width = setting.resolution_width,
                    height = setting.resolution_height,
                    dpi = setting.dpi_scale,
                    fps = setting.fps,
                    fps_count = setting.fps_count,
                    quality = setting.frame_quality,
                    chroma = setting.chroma_format,
                    codec = setting.codec_type,
                    hdr = setting.enable_hdr,
                    "host capture setting received"
                );
                let mut errors = Vec::new();
                if (setting.resolution_width > 0 && setting.resolution_width as u32 != screen.width)
                    || (setting.resolution_height > 0
                        && setting.resolution_height as u32 != screen.height)
                    || (setting.resolution_pixel_width > 0
                        && setting.resolution_pixel_width as u32 != screen.width)
                    || (setting.resolution_pixel_height > 0
                        && setting.resolution_pixel_height as u32 != screen.height)
                    || (setting.dpi_scale > 0 && screen.dpi_scale != Some(setting.dpi_scale as u32))
                    || !matches!(setting.resolution_type, 0 | 1)
                {
                    errors.push(PbError {
                        error_code: -1,
                        error_message: "本机尚未开放远程显示设置".into(),
                        ..Default::default()
                    });
                } else if setting.screen_id != EXISTING_SESSION_TRACKS
                    && setting.screen_id != screen.id
                {
                    errors.push(PbError {
                        error_code: -1,
                        error_message: "只能观看本机已选择共享的屏幕".into(),
                        ..Default::default()
                    });
                } else {
                    let mut next = *config;
                    next.fps = fps(setting.fps, setting.fps_count);
                    next.requested_fps = fps(setting.fps, 0);
                    next.fps_limit = if setting.fps_count > 0 {
                        setting.fps_count as u32
                    } else {
                        144
                    };
                    next.cursor_capture = setting.cursor_capture;
                    next.bitrate = bitrate(
                        setting.frame_quality,
                        setting.auto_frame_quality,
                        setting.max_custom_bitrate,
                    );
                    next.quality = setting.frame_quality.clamp(1, 6);
                    next.auto_quality = setting.auto_frame_quality.clamp(1, 4);
                    next.revision = next.revision.wrapping_add(1);
                    let codec = crate::host::format::Codec::from_wire(setting.codec_type);
                    let selected = if (0..=2).contains(&setting.codec_type) {
                        negotiated.apply(
                            &mut next,
                            codec,
                            if setting.chroma_format == 3 { 3 } else { 1 },
                            setting.enable_hdr,
                            (screen.width, screen.height),
                        )
                    } else {
                        Err(anyhow!("不支持的编码类型"))
                    };
                    match selected {
                        Ok(()) => *config = next,
                        Err(error) => errors.push(PbError {
                            error_code: -1,
                            error_message: error.to_string(),
                            ..Default::default()
                        }),
                    }
                    tracing::info!(
                        fps = config.fps,
                        bitrate = config.bitrate,
                        quality = config.quality,
                        "host capture setting applied"
                    );
                }
                Some(PbRpcResponsePayload::CaptureSetting(
                    PbCaptureSettingResponse { errors },
                ))
            } else if let Some(tracks) = request.send_video_track {
                let valid = tracks.video_track_index.iter().all(|index| *index == 0);
                if valid {
                    config.sending = tracks.video_track_index.contains(&0);
                    tracing::info!(
                        sending = config.sending,
                        "host video track selection applied"
                    );
                }
                Some(PbRpcResponsePayload::SendVideoTrackRsp(
                    PbSendVideoTrackResponse {
                        error_code: if valid { 0 } else { -1 },
                    },
                ))
            } else {
                None
            };
            if let Some(payload) = payload {
                Ok(vec![
                    PbControlMessage {
                        seq: msg.seq,
                        timestamp: msg.timestamp,
                        payload: Some(PbPayload::RpcResponse(PbRpcResponse {
                            response_header: header,
                            payload: Some(payload),
                        })),
                    }
                    .encode_to_vec(),
                ])
            } else {
                Ok(Vec::new())
            }
        }
        _ => Ok(Vec::new()),
    }
}
