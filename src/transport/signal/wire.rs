//! Engine.IO/Socket.IO framing and bounded compressed SDP attachment parsing.
use super::MAX_SDP_BYTES;
use anyhow::{Context as _, Result, bail};
use flate2::read::GzDecoder;
use flate2::{Compression, GzBuilder};
use serde_json::Value;
use std::io::{Read, Write};

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

pub(super) fn encode_binary_event(
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

pub(super) fn decode_socket(input: &str) -> Result<SocketPacket> {
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

pub(super) fn parse_event_array(input: &str) -> Result<(String, Vec<Value>)> {
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

pub(super) fn split_ack_id(input: &str) -> (Option<u64>, &str) {
    let digit_count = input.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return (None, input);
    }
    let (id, rest) = input.split_at(digit_count);
    (id.parse().ok(), rest)
}

pub(super) fn parse_json(input: &str, description: &str) -> Result<Value> {
    serde_json::from_str(input).with_context(|| format!("invalid {description} JSON"))
}

pub(super) fn parse_array(input: &str, description: &str) -> Result<Vec<Value>> {
    parse_json(input, description)?
        .as_array()
        .cloned()
        .with_context(|| format!("{description} payload is not an array"))
}

pub(super) fn gzip_sdp(sdp: &str) -> Result<Vec<u8>> {
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

pub(super) fn gunzip_sdp(attachment: &[u8]) -> Result<String> {
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

pub(super) fn resolve_binary_event(
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

pub(super) fn hydrate_gzip_sdp(value: &mut Value, attachments: &[Vec<u8>]) -> Result<()> {
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
