use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use webrtc::interceptor::stream_info::StreamInfo;
use webrtc::interceptor::{
    Attributes, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader, RTPWriter,
};
use webrtc::util::marshal::{Marshal, Unmarshal};

pub(crate) const CAPTURE_PATH_ENV: &str = "OPENUUYC_RTP_CAPTURE";
const MAGIC: &[u8; 8] = b"UURTP002";
const CHANNEL_CAPACITY: usize = 16_384;
type CaptureWriterTask = Arc<Mutex<Option<std::thread::JoinHandle<std::io::Result<()>>>>>;

#[derive(Clone)]
pub(crate) struct RtpCaptureBuilder {
    sender: SyncSender<CaptureRecord>,
    started_at: Instant,
    writer_task: CaptureWriterTask,
}

#[derive(Clone)]
struct RtpCaptureInterceptor {
    sender: SyncSender<CaptureRecord>,
    started_at: Instant,
    writer_task: CaptureWriterTask,
}

struct CaptureRtpReader {
    inner: Arc<dyn RTPReader + Send + Sync>,
    sender: SyncSender<CaptureRecord>,
    started_at: Instant,
}

enum CaptureRecord {
    Finish,
    Stream(StreamMetadata),
    Codecs(NegotiatedCodecs),
    Packet {
        elapsed_micros: u64,
        ssrc: u32,
        payload_type: u8,
        bytes: Vec<u8>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapturedCodec {
    pub payload_type: u8,
    pub mime_type: String,
    pub fmtp: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct NegotiatedCodecs {
    ssrc: u32,
    codecs: Vec<CapturedCodec>,
    extmap_allow_mixed: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct StreamMetadata {
    id: String,
    ssrc: u32,
    payload_type: u8,
    mime_type: String,
    clock_rate: u32,
    channels: u16,
    fmtp: String,
    associated_ssrc: Option<u32>,
    associated_payload_type: Option<u8>,
    header_extensions: Vec<(isize, String)>,
}

#[derive(Clone, Debug)]
pub struct CapturedStream {
    pub ssrc: u32,
    pub payload_type: u8,
    pub mime_type: String,
    pub fmtp: String,
    pub associated_ssrc: Option<u32>,
    pub header_extensions: Vec<(isize, String)>,
}

#[derive(Debug)]
pub struct CapturedRtpPacket {
    pub elapsed_micros: u64,
    pub bytes: Vec<u8>,
}

pub struct LoadedCapture {
    pub streams: Vec<CapturedStream>,
    pub codecs: BTreeMap<u32, Vec<CapturedCodec>>,
    pub extmap_allow_mixed: BTreeMap<u32, bool>,
    pub packets: Vec<CapturedRtpPacket>,
    pub truncated_tail: bool,
}

#[derive(Default)]
struct StreamSummary {
    packets: u64,
    bytes: u64,
    first_sequence: Option<u16>,
    last_sequence: Option<u16>,
}

impl RtpCaptureBuilder {
    pub(crate) fn record_codecs(
        &self,
        ssrc: u32,
        parameters: &webrtc::rtp_transceiver::rtp_codec::RTCRtpParameters,
        extmap_allow_mixed: bool,
    ) {
        let codecs = parameters
            .codecs
            .iter()
            .map(|codec| CapturedCodec {
                payload_type: codec.payload_type,
                mime_type: codec.capability.mime_type.clone(),
                fmtp: codec.capability.sdp_fmtp_line.clone(),
            })
            .collect();
        if let Err(error) = self
            .sender
            .try_send(CaptureRecord::Codecs(NegotiatedCodecs {
                ssrc,
                codecs,
                extmap_allow_mixed,
            }))
        {
            tracing::warn!(%error, ssrc, "failed to capture negotiated RTP codec mapping");
        }
    }

    pub(crate) fn from_environment() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(CAPTURE_PATH_ENV) else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        let file = File::create(&path)
            .with_context(|| format!("create decrypted RTP capture {}", path.display()))?;
        let (sender, receiver) = sync_channel(CHANNEL_CAPACITY);
        let writer_task = std::thread::Builder::new()
            .name("uu-rtp-capture".to_owned())
            .spawn(move || -> std::io::Result<()> {
                let mut writer = BufWriter::new(file);
                writer.write_all(MAGIC)?;
                let mut records_since_flush = 0_u16;
                while let Ok(record) = receiver.recv() {
                    if matches!(record, CaptureRecord::Finish) {
                        break;
                    }
                    write_record(&mut writer, record)?;
                    records_since_flush += 1;
                    if records_since_flush == 1024 {
                        writer.flush()?;
                        records_since_flush = 0;
                    }
                }
                writer.flush()
            })
            .context("spawn decrypted RTP capture writer")?;
        tracing::info!(path = %path.display(), "decrypted RTP capture enabled");
        Ok(Some(Self {
            sender,
            started_at: Instant::now(),
            writer_task: Arc::new(Mutex::new(Some(writer_task))),
        }))
    }
}

impl InterceptorBuilder for RtpCaptureBuilder {
    fn build(
        &self,
        _id: &str,
    ) -> std::result::Result<Arc<dyn Interceptor + Send + Sync>, webrtc::interceptor::Error> {
        Ok(Arc::new(RtpCaptureInterceptor {
            sender: self.sender.clone(),
            started_at: self.started_at,
            writer_task: Arc::clone(&self.writer_task),
        }))
    }
}

#[async_trait]
impl Interceptor for RtpCaptureInterceptor {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        reader
    }

    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }

    async fn bind_local_stream(
        &self,
        _info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        writer
    }

    async fn unbind_local_stream(&self, _info: &StreamInfo) {}

    async fn bind_remote_stream(
        &self,
        info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        let metadata = StreamMetadata {
            id: info.id.clone(),
            ssrc: info.ssrc,
            payload_type: info.payload_type,
            mime_type: info.mime_type.clone(),
            clock_rate: info.clock_rate,
            channels: info.channels,
            fmtp: info.sdp_fmtp_line.clone(),
            associated_ssrc: info.associated_stream.as_ref().map(|value| value.ssrc),
            associated_payload_type: info
                .associated_stream
                .as_ref()
                .map(|value| value.payload_type),
            header_extensions: info
                .rtp_header_extensions
                .iter()
                .map(|extension| (extension.id, extension.uri.clone()))
                .collect(),
        };
        let _ = self.sender.try_send(CaptureRecord::Stream(metadata));

        Arc::new(CaptureRtpReader {
            inner: reader,
            sender: self.sender.clone(),
            started_at: self.started_at,
        })
    }

    async fn unbind_remote_stream(&self, _info: &StreamInfo) {}

    async fn close(&self) -> std::result::Result<(), webrtc::interceptor::Error> {
        let task = self
            .writer_task
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(task) = task else {
            return Ok(());
        };
        let sender = self.sender.clone();
        tokio::task::spawn_blocking(move || {
            // The FIFO barrier includes every previously accepted record.
            // Other interceptor handles may still own sender clones, so EOF
            // alone cannot be the capture writer's shutdown protocol.
            let _ = sender.send(CaptureRecord::Finish);
            task.join()
                .map_err(|_| std::io::Error::other("RTP capture writer panicked"))?
        })
        .await
        .map_err(|error| webrtc::interceptor::Error::Other(error.to_string()))?
        .map_err(|error| webrtc::interceptor::Error::Other(format!("finish RTP capture: {error}")))
    }
}

#[async_trait]
impl RTPReader for CaptureRtpReader {
    async fn read(
        &self,
        buffer: &mut [u8],
        attributes: &Attributes,
    ) -> std::result::Result<(webrtc::rtp::packet::Packet, Attributes), webrtc::interceptor::Error>
    {
        let (packet, output_attributes) = self.inner.read(buffer, attributes).await?;
        let elapsed_micros = output_attributes
            .rtp_received_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started_at)
            .as_micros()
            .min(u128::from(u64::MAX));
        if let Ok(bytes) = packet.marshal() {
            let _ = self.sender.try_send(CaptureRecord::Packet {
                elapsed_micros: elapsed_micros as u64,
                ssrc: packet.header.ssrc,
                payload_type: packet.header.payload_type,
                bytes: bytes.to_vec(),
            });
        }
        Ok((packet, output_attributes))
    }
}

fn write_record(writer: &mut impl Write, record: CaptureRecord) -> std::io::Result<()> {
    let payload = match record {
        CaptureRecord::Finish => return Ok(()),
        CaptureRecord::Stream(metadata) => {
            let json = serde_json::to_vec(&metadata).map_err(std::io::Error::other)?;
            let mut payload = Vec::with_capacity(1 + json.len());
            payload.push(1);
            payload.extend_from_slice(&json);
            payload
        }
        CaptureRecord::Codecs(codecs) => {
            let json = serde_json::to_vec(&codecs).map_err(std::io::Error::other)?;
            let mut payload = Vec::with_capacity(1 + json.len());
            payload.push(3);
            payload.extend_from_slice(&json);
            payload
        }
        CaptureRecord::Packet {
            elapsed_micros,
            ssrc,
            payload_type,
            bytes,
        } => {
            let mut payload = Vec::with_capacity(18 + bytes.len());
            payload.push(2);
            payload.extend_from_slice(&elapsed_micros.to_le_bytes());
            payload.extend_from_slice(&ssrc.to_le_bytes());
            payload.push(payload_type);
            payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(&bytes);
            payload
        }
    };
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(&payload)
}

pub fn inspect_capture(path: PathBuf) -> Result<String> {
    let file = File::open(&path).with_context(|| format!("open RTP capture {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut magic = [0_u8; MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .context("read RTP capture header")?;
    if &magic != MAGIC {
        anyhow::bail!("unsupported RTP capture format");
    }

    let mut metadata = BTreeMap::<(u32, u8), StreamMetadata>::new();
    let mut negotiated = BTreeMap::<u32, Vec<CapturedCodec>>::new();
    let mut streams = BTreeMap::<(u32, u8), StreamSummary>::new();
    let mut invalid_packets = 0_u64;
    let mut record_header_mismatches = 0_u64;
    let mut remarshal_mismatches = 0_u64;
    let mut elapsed_micros = 0_u64;
    let mut truncated_tail = false;
    loop {
        let mut length = [0_u8; 4];
        match reader.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error).context("read RTP capture record length"),
        }
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > 2 * 1024 * 1024 {
            anyhow::bail!("invalid RTP capture record length {length}");
        }
        let mut payload = vec![0_u8; length];
        if let Err(error) = reader.read_exact(&mut payload) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                truncated_tail = true;
                break;
            }
            return Err(error).context("read RTP capture record");
        }
        match payload[0] {
            1 => {
                let stream: StreamMetadata =
                    serde_json::from_slice(&payload[1..]).context("decode RTP stream metadata")?;
                metadata.insert((stream.ssrc, stream.payload_type), stream);
            }
            3 => {
                let value: NegotiatedCodecs = serde_json::from_slice(&payload[1..])
                    .context("decode negotiated RTP codecs")?;
                negotiated.insert(value.ssrc, value.codecs);
            }
            2 if payload.len() >= 18 => {
                let record_elapsed =
                    u64::from_le_bytes(payload[1..9].try_into().expect("fixed record field"));
                let ssrc =
                    u32::from_le_bytes(payload[9..13].try_into().expect("fixed record field"));
                let payload_type = payload[13];
                let raw_len =
                    u32::from_le_bytes(payload[14..18].try_into().expect("fixed record field"))
                        as usize;
                if payload.len() != 18 + raw_len {
                    anyhow::bail!("RTP capture packet length does not match its record");
                }
                elapsed_micros = elapsed_micros.max(record_elapsed);
                let raw = &payload[18..];
                let mut input = raw;
                match webrtc::rtp::packet::Packet::unmarshal(&mut input) {
                    Ok(packet) => {
                        if packet.header.ssrc != ssrc || packet.header.payload_type != payload_type
                        {
                            record_header_mismatches += 1;
                        }
                        let summary = streams
                            .entry((packet.header.ssrc, packet.header.payload_type))
                            .or_default();
                        summary.packets += 1;
                        summary.bytes += raw_len as u64;
                        summary
                            .first_sequence
                            .get_or_insert(packet.header.sequence_number);
                        summary.last_sequence = Some(packet.header.sequence_number);
                        if packet.marshal().ok().as_deref() != Some(raw) {
                            remarshal_mismatches += 1;
                        }
                    }
                    _ => invalid_packets += 1,
                }
            }
            tag => anyhow::bail!("unknown RTP capture record tag {tag}"),
        }
    }

    let mut output = format!(
        "capture: {}\nduration: {:.3}s\nstreams: {}\ninvalid packets: {}\nrecord header mismatches: {}\nraw/remarshal mismatches: {}\ntruncated tail: {}\n",
        path.display(),
        elapsed_micros as f64 / 1_000_000.0,
        streams.len(),
        invalid_packets,
        record_header_mismatches,
        remarshal_mismatches,
        truncated_tail
    );
    for ((ssrc, payload_type), summary) in streams {
        let stream = metadata
            .get(&(ssrc, payload_type))
            .or_else(|| metadata.values().find(|value| value.ssrc == ssrc));
        let media_ssrc = stream
            .and_then(|value| value.associated_ssrc)
            .unwrap_or(ssrc);
        let codec = negotiated.get(&media_ssrc).and_then(|codecs| {
            codecs
                .iter()
                .find(|codec| codec.payload_type == payload_type)
        });
        output.push_str(&format!(
            "  ssrc={ssrc} pt={payload_type} mime={} packets={} bytes={} seq={:?}..{:?} associated={:?} extensions={:?}\n",
            codec.map_or("unknown (no negotiated PT mapping)", |value| value.mime_type.as_str()),
            summary.packets,
            summary.bytes,
            summary.first_sequence,
            summary.last_sequence,
            stream.and_then(|value| value.associated_ssrc),
            stream.map(|value| value.header_extensions.as_slice()).unwrap_or_default(),
        ));
    }
    Ok(output)
}

pub fn load_capture(path: PathBuf) -> Result<LoadedCapture> {
    let file = File::open(&path).with_context(|| format!("open RTP capture {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut magic = [0_u8; MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .context("read RTP capture header")?;
    if &magic != MAGIC {
        anyhow::bail!("unsupported RTP capture format");
    }
    let mut streams = BTreeMap::<u32, CapturedStream>::new();
    let mut codecs = BTreeMap::<u32, Vec<CapturedCodec>>::new();
    let mut extmap_allow_mixed = BTreeMap::<u32, bool>::new();
    let mut packets = Vec::new();
    let mut truncated_tail = false;
    loop {
        let mut length = [0_u8; 4];
        match reader.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error).context("read RTP capture record length"),
        }
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > 2 * 1024 * 1024 {
            anyhow::bail!("invalid RTP capture record length {length}");
        }
        let mut payload = vec![0_u8; length];
        if let Err(error) = reader.read_exact(&mut payload) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                truncated_tail = true;
                break;
            }
            return Err(error).context("read RTP capture record");
        }
        match payload[0] {
            1 => {
                let value: StreamMetadata =
                    serde_json::from_slice(&payload[1..]).context("decode RTP stream metadata")?;
                streams.insert(
                    value.ssrc,
                    CapturedStream {
                        ssrc: value.ssrc,
                        payload_type: value.payload_type,
                        mime_type: value.mime_type,
                        fmtp: value.fmtp,
                        associated_ssrc: value.associated_ssrc,
                        header_extensions: value.header_extensions,
                    },
                );
            }
            3 => {
                let value: NegotiatedCodecs = serde_json::from_slice(&payload[1..])
                    .context("decode negotiated RTP codecs")?;
                codecs.insert(value.ssrc, value.codecs);
                extmap_allow_mixed.insert(value.ssrc, value.extmap_allow_mixed);
            }
            2 if payload.len() >= 18 => {
                let elapsed_micros =
                    u64::from_le_bytes(payload[1..9].try_into().expect("fixed record field"));
                let raw_len =
                    u32::from_le_bytes(payload[14..18].try_into().expect("fixed record field"))
                        as usize;
                if payload.len() != 18 + raw_len {
                    anyhow::bail!("RTP capture packet length does not match its record");
                }
                packets.push(CapturedRtpPacket {
                    elapsed_micros,
                    bytes: payload[18..].to_vec(),
                });
            }
            tag => anyhow::bail!("unknown RTP capture record tag {tag}"),
        }
    }
    Ok(LoadedCapture {
        streams: streams.into_values().collect(),
        codecs,
        extmap_allow_mixed,
        packets,
        truncated_tail,
    })
}
