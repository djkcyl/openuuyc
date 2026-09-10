//! Verified controller handshake payloads.
//!
//! `ConnectOptions` is carried as a Socket.IO binary attachment. It must not be
//! converted to UTF-8 or base64: the official client sends the protobuf bytes
//! unchanged after a binary-event placeholder.

use anyhow::{Result, bail};
use serde_json::json;
use uuid::Uuid;

use crate::{
    api::PROTOCOL_VERSION,
    capability::{CodecCapability, DeviceCapability},
    media::ConnectionMediaProfile,
};

// Streamer decoder implementation identifiers observed in the official client:
// 32 DXVA11, 33 NvDec, 34 VideoToolbox, 35 AsyncMediaCodec,
// 36 SyncMediaCodec, 37 Software.  The adapter identifier is part of the
// capability contract and can affect the encoder's selected frame-rate/format;
// it must describe the local native adapter instead of being forced to 37.

pub(crate) struct ControlFrames {
    pub header: String,
    pub attachment: Vec<u8>,
    pub app_control_id: String,
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct WireField<'a> {
    number: u32,
    value: WireValue<'a>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

#[cfg(test)]
fn decode_wire_fields(mut input: &[u8]) -> Result<Vec<WireField<'_>>> {
    let mut fields = Vec::new();
    while !input.is_empty() {
        let (key, rest) = take_varint(input)?;
        input = rest;
        let number = u32::try_from(key >> 3).map_err(|_| anyhow::anyhow!("field overflow"))?;
        let value = match key & 7 {
            0 => {
                let (value, rest) = take_varint(input)?;
                input = rest;
                WireValue::Varint(value)
            }
            1 => {
                if input.len() < 8 {
                    bail!("truncated fixed64 field");
                }
                input = &input[8..];
                continue;
            }
            2 => {
                let (length, rest) = take_varint(input)?;
                let length = usize::try_from(length)
                    .map_err(|_| anyhow::anyhow!("length-delimited field overflow"))?;
                if rest.len() < length {
                    bail!("truncated length-delimited field");
                }
                let (value, rest) = rest.split_at(length);
                input = rest;
                WireValue::Bytes(value)
            }
            5 => {
                if input.len() < 4 {
                    bail!("truncated fixed32 field");
                }
                input = &input[4..];
                continue;
            }
            wire => bail!("unsupported protobuf wire type {wire}"),
        };
        fields.push(WireField { number, value });
    }
    Ok(fields)
}

#[cfg(test)]
fn take_varint(mut input: &[u8]) -> Result<(u64, &[u8])> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let Some((&byte, rest)) = input.split_first() else {
            bail!("truncated protobuf varint");
        };
        input = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, input));
        }
    }
    bail!("protobuf varint exceeds 64 bits")
}

#[cfg(test)]
fn varint_field(fields: &[WireField<'_>], number: u32) -> Option<u64> {
    fields.iter().find_map(|field| {
        (field.number == number)
            .then_some(field.value)
            .and_then(|value| match value {
                WireValue::Varint(value) => Some(value),
                WireValue::Bytes(_) => None,
            })
    })
}

#[cfg(test)]
fn bytes_field<'a>(fields: &[WireField<'a>], number: u32) -> Option<&'a [u8]> {
    fields.iter().find_map(|field| {
        (field.number == number)
            .then_some(field.value)
            .and_then(|value| match value {
                WireValue::Bytes(value) => Some(value),
                WireValue::Varint(_) => None,
            })
    })
}

#[cfg(test)]
fn text_field<'a>(fields: &[WireField<'a>], number: u32) -> Option<&'a str> {
    bytes_field(fields, number).and_then(|value| std::str::from_utf8(value).ok())
}

pub(crate) fn build_control_frames(
    controller_device_id: &str,
    ack_id: u64,
    decoder_support: &DeviceCapability,
    profile: ConnectionMediaProfile,
) -> Result<ControlFrames> {
    build_control_frames_with_id(
        controller_device_id,
        ack_id,
        &Uuid::new_v4().to_string(),
        decoder_support,
        profile,
    )
}

fn build_control_frames_with_id(
    controller_device_id: &str,
    ack_id: u64,
    app_control_id: &str,
    decoder_support: &DeviceCapability,
    profile: ConnectionMediaProfile,
) -> Result<ControlFrames> {
    if controller_device_id.len() != 16
        || !controller_device_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        bail!("cannot build control request for an invalid device identifier");
    }

    let selected_capabilities = &decoder_support.video_codec_capability;
    if selected_capabilities.is_empty() {
        bail!("cannot build control request without a UU-compatible video decoder");
    }
    let streamer_data = serde_json::to_string(&json!({
        "control_id": app_control_id,
        "device_capability": decoder_support
    }))?;
    let event = json!([
        "control",
        {
            "app_control_id": app_control_id,
            "app_data": { "_placeholder": true, "num": 0 },
            "streamer_data": streamer_data
        }
    ]);

    Ok(ControlFrames {
        header: format!("451-{ack_id}{}", serde_json::to_string(&event)?),
        attachment: encode_connect_options(controller_device_id, selected_capabilities, profile),
        app_control_id: app_control_id.to_owned(),
    })
}

fn encode_connect_options(
    controller_device_id: &str,
    selected_capabilities: &[CodecCapability],
    profile: ConnectionMediaProfile,
) -> Vec<u8> {
    let mut capture = Vec::new();
    let fps_level = match profile.stream_fps {
        30 => 1,
        60 => 2,
        90 => 3,
        _ => 4,
    };
    push_varint_field(&mut capture, 1, fps_level);
    push_varint_field(&mut capture, 2, 5); // VIDEO_QUALITY_AUTO
    push_varint_field(&mut capture, 4, 3); // follow remote resolution
    // This is the controller's physical display size, not a request to resize
    // the controlled display. GameViewerServer::DisplayLayout::tryOpen falls
    // back to 1920x1080 when this field is absent and feeds that fallback into
    // display-layout initialization. The official controller always includes
    // the real local size, including when FOLLOW_REMOTE is selected.
    let mut local_resolution = Vec::new();
    push_varint_field(
        &mut local_resolution,
        1,
        u64::from(profile.local_display.width),
    );
    push_varint_field(
        &mut local_resolution,
        2,
        u64::from(profile.local_display.height),
    );
    push_bytes_field(&mut capture, 5, &local_resolution);
    push_varint_field(&mut capture, 7, 1); // YUV 4:2:0
    push_varint_field(&mut capture, 10, 4); // automatic frame quality: Blu-ray
    push_varint_field(
        &mut capture,
        11,
        u64::from(profile.local_display.refresh_hz.min(profile.stream_fps)),
    );

    let mut options = Vec::new();
    push_varint_field(&mut options, 1, 1); // desktop capture
    push_signed_int32_field(&mut options, 2, -1);
    push_bytes_field(&mut options, 3, &capture);
    let mut decoder_profiles = Vec::new();
    for capability in selected_capabilities {
        let key = (capability.video_codec, capability.chroma_sampling);
        if !decoder_profiles.contains(&key) {
            decoder_profiles.push(key);
        }
    }
    for (codec, chroma) in decoder_profiles {
        let mut decoder = Vec::new();
        // This is the receiver-side FPS capability derived from the local
        // display tier. The requested stream FPS remains in CaptureParams.
        push_varint_field(&mut decoder, 1, u64::from(profile.decoder_fps_cap));
        push_varint_field(&mut decoder, 2, codec as u64);
        let (width, height) = selected_capabilities
            .iter()
            .filter(|cap| cap.video_codec == codec && cap.chroma_sampling == chroma)
            .map(|cap| (cap.width, cap.height))
            .max()
            .unwrap_or_default();
        push_varint_field(&mut decoder, 3, width as u64);
        push_varint_field(&mut decoder, 4, height as u64);
        push_varint_field(&mut decoder, 5, u64::from(chroma));
        push_bytes_field(&mut options, 4, &decoder);
    }
    push_varint_field(&mut options, 8, 3); // desktop controller ABI value
    // ConnectOptions.device_id identifies the controller. The controlled host
    // uses it as the key for per-controller display-layout state. Sending the
    // target's own ID here selects the wrong state and can reapply a stale
    // physical monitor mode during session initialization.
    push_bytes_field(&mut options, 9, controller_device_id.as_bytes());
    push_varint_field(&mut options, 10, 1); // normal control
    // Keep initial ConnectOptions and both ECHO directions identical. Feature
    // levels are not harmless placeholders: the host reads private_screen
    // during display initialization, before the later ECHO replacement.
    let feature_flag = crate::stream_control::encode_read_only_feature_flags();
    push_bytes_field(&mut options, 11, &feature_flag);
    push_bytes_field(&mut options, 12, PROTOCOL_VERSION.as_bytes());
    options
}

fn push_signed_int32_field(output: &mut Vec<u8>, number: u32, value: i32) {
    push_varint_field(output, number, value as i64 as u64);
}

fn push_varint_field(output: &mut Vec<u8>, number: u32, value: u64) {
    push_varint(output, u64::from(number) << 3);
    push_varint(output, value);
}

fn push_bytes_field(output: &mut Vec<u8>, number: u32, value: &[u8]) {
    push_varint(output, (u64::from(number) << 3) | 2);
    push_varint(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn push_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_frames_match_the_native_decoder_profile() {
        let frames = build_control_frames_with_id(
            "abcd1234efgh5678",
            2,
            "11111111-2222-4333-8444-555555555555",
            &DeviceCapability {
                video_codec_capability: [1, 2]
                    .map(|video_codec| CodecCapability {
                        video_codec,
                        width: 1920,
                        height: 1080,
                        chroma_sampling: 1,
                        bit_depth: 8,
                        codec_impl: 37,
                    })
                    .to_vec(),
                ..Default::default()
            },
            ConnectionMediaProfile {
                muted: false,
                // Keep the controller display different from the requested
                // stream tier: these two wire concepts must never collapse
                // back into one resolution value.
                local_display: crate::media::LocalDisplayInfo {
                    width: 2560,
                    height: 1440,
                    refresh_hz: 60,
                },
                stream_fps: 60,
                decoder_fps_cap: 60,
                codec: crate::media::CodecPreference::Auto,
                hardware_decode: false,
            },
        )
        .unwrap();
        assert_eq!(
            frames.attachment,
            hex_to_bytes(
                "080110ffffffffffffffffff011a140802100520032a0608801410a00b38015004583c220c083c100118800f20b8082801220c083c100218800f20b808280140034a106162636431323334656667683536373850015a0408064801620b342e33382e332e39333235"
            )
        );
        let option_fields = decode_wire_fields(&frames.attachment).unwrap();
        assert_eq!(text_field(&option_fields, 9), Some("abcd1234efgh5678"));
        assert!(bytes_field(&option_fields, 6).is_none());
        assert!(bytes_field(&option_fields, 7).is_none());
        let capture_fields = decode_wire_fields(bytes_field(&option_fields, 3).unwrap()).unwrap();
        assert_eq!(varint_field(&capture_fields, 4), Some(3));
        let local_resolution_fields =
            decode_wire_fields(bytes_field(&capture_fields, 5).unwrap()).unwrap();
        assert_eq!(varint_field(&local_resolution_fields, 1), Some(2560));
        assert_eq!(varint_field(&local_resolution_fields, 2), Some(1440));
        assert!(bytes_field(&capture_fields, 6).is_none());
        let feature_fields = decode_wire_fields(bytes_field(&option_fields, 11).unwrap()).unwrap();
        assert_eq!(varint_field(&feature_fields, 1), Some(6));

        let payload: serde_json::Value = serde_json::from_str(&frames.header[5..]).unwrap();
        let control = &payload[1];
        assert_eq!(frames.header.get(..5), Some("451-2"));
        assert_eq!(control["app_data"]["_placeholder"], true);
        assert_eq!(control["app_data"]["num"], 0);
        assert_eq!(
            control["app_control_id"],
            "11111111-2222-4333-8444-555555555555"
        );
        let streamer: serde_json::Value =
            serde_json::from_str(control["streamer_data"].as_str().unwrap()).unwrap();
        assert_eq!(streamer["control_id"], control["app_control_id"]);
        let capabilities = streamer["device_capability"]["video_codec_capability"]
            .as_array()
            .unwrap();
        assert_eq!(capabilities.len(), 2);
        assert!(capabilities.iter().all(|capability| {
            matches!(capability["video_codec"].as_u64(), Some(1 | 2))
                && capability["chroma_sampling"] == 1
                && capability["bit_depth"] == 8
        }));
    }

    fn hex_to_bytes(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }
}
