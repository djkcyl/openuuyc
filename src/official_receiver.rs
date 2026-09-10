//! Official-compatible UU/WebRTC video receive state machine.
//!
//! The same depacketizer/FEC/PacketBuffer/reference/FrameBuffer implementation
//! serves the live viewer and deterministic decrypted-RTP capture replay.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::util::marshal::Unmarshal;

use crate::codec_parameters::{
    NaluInfo, ParameterTracker, h264_nalu_info, h265_nalu_info, rewrite_h264_sps,
};
use crate::decoder_result::VideoDecodeResult;
use crate::rsfec::{RsFecConfig, RsFecReceiver, normalize_rtx_source};
use crate::rtc::PlayoutDelay;
use crate::rtp_capture::{CapturedCodec, load_capture};
use crate::timing::{DecodeSchedule, PlayoutSchedule, VideoPlayoutTiming};
use crate::video_color::{VideoColorHistory, VideoColorSpace};
use crate::video_format::VideoFormatSignature;

#[cfg(test)]
mod tests;

const PACKET_BUFFER_INITIAL_SIZE: usize = 2_048;
const PACKET_BUFFER_MAX_SIZE: usize = 16_384;
const FRAME_BUFFER_MAX_SIZE: usize = 800;
const DECODED_HISTORY_SIZE: usize = 1 << 13;
const DECODER_ADMISSION_TOKENS: usize = 1;
const MAX_STASHED_FRAMES: usize = 100;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VideoCodecKind {
    H264,
    H265,
}

/// WebRTC/UU video-timing header extension. All six values are milliseconds
/// from the frame capture timestamp. A flags value of 0xff invalidates the
/// whole extension; UU currently also uses bit 2 in addition to upstream's
/// timer/size bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VideoSendTiming {
    pub flags: u8,
    pub encode_start_delta_ms: u16,
    pub encode_finish_delta_ms: u16,
    pub packetization_finish_delta_ms: u16,
    pub pacer_exit_delta_ms: u16,
    pub network_timestamp_delta_ms: u16,
    pub network2_timestamp_delta_ms: u16,
}

#[derive(Default)]
struct ReplayStats {
    media_packets: u64,
    rtx_packets: u64,
    padding_packets: u64,
    fec_packets: u64,
    fec_recovered_packets: u64,
    malformed_packets: u64,
    parameter_requests: u64,
    duplicate_packets: u64,
    packet_buffer_expansions: u64,
    packet_buffer_clears: u64,
    completed_frames: u64,
    keyframes: u64,
    reference_stashed: u64,
    reference_dropped: u64,
    frame_buffer_dropped: u64,
    decoded_frames: u64,
    decoded_bytes: u64,
    injected_drops: u64,
}

#[derive(Default)]
struct ReplayTimeline {
    current_received_micros: u64,
    last_released_micros: Option<u64>,
    first_decoded_timestamp: Option<u32>,
    last_decoded_timestamp: Option<u32>,
    first_decoded_arrival: Option<u64>,
    last_decoded_arrival: Option<u64>,
    keyframe_points: Vec<(u16, u16, u32, u64)>,
    packet_buffer_clear_points: Vec<(u16, u32, u64)>,
    malformed_points: Vec<(u16, u32, Option<u8>, usize, u64)>,
    frame_digest: Sha256,
}

pub(crate) struct ParsedVideoPacket {
    codec: VideoCodecKind,
    sequence_number: u16,
    timestamp: u32,
    marker: bool,
    is_first_packet_in_frame: bool,
    sps: bool,
    pps: bool,
    idr: bool,
    packet_keyframe: bool,
    nalus: Vec<NaluInfo>,
    parameter_format: Option<VideoFormatSignature>,
    color_space: Option<VideoColorSpace>,
    rotation: Option<u16>,
    content_type: Option<u8>,
    video_capture_index: Option<u16>,
    is_new_picture: Option<bool>,
    video_timing: Option<VideoSendTiming>,
    frame_sending_delay_ms: Option<u16>,
    playout_delay: Option<PlayoutDelay>,
    nack_count: u8,
    payload: Vec<u8>,
    received_micros: u64,
    received_at: Instant,
}

impl ParsedVideoPacket {
    pub(crate) fn set_receive_timing(&mut self, delay: Option<PlayoutDelay>, nack_count: u8) {
        self.playout_delay = delay;
        self.nack_count = nack_count;
    }

    pub(crate) fn sequence_number(&self) -> u16 {
        self.sequence_number
    }

    pub(crate) fn starts_keyframe(&self) -> bool {
        self.is_first_packet_in_frame && self.packet_keyframe
    }
}

struct BufferedPacket {
    packet: ParsedVideoPacket,
    continuous: bool,
}

struct EncodedFrame {
    codec: VideoCodecKind,
    id: i64,
    references: Vec<i64>,
    first_sequence_number: u16,
    last_sequence_number: u16,
    timestamp: u32,
    keyframe: bool,
    color_space: Option<VideoColorSpace>,
    rotation: u16,
    content_type: u8,
    video_capture_index: Option<u16>,
    is_new_picture: Option<bool>,
    video_timing: Option<VideoSendTiming>,
    frame_sending_delay_ms: Option<u16>,
    playout_delay: Option<PlayoutDelay>,
    nack_count: u8,
    received_micros: u64,
    received_at: Instant,
    last_received_at: Instant,
    assembled_at: Instant,
    data: Vec<u8>,
    parameter_format: Option<VideoFormatSignature>,
}

struct PacketBuffer {
    buffer: Vec<Option<BufferedPacket>>,
    first_sequence_number: u16,
    first_packet_received: bool,
    cleared_to_first_sequence: bool,
    newest_inserted_sequence: Option<u16>,
    missing_packets: HashSet<u16>,
    received_padding: HashSet<u16>,
    last_keyframe_first_sequence: Option<u16>,
    parameters: ParameterTracker,
}

struct SequenceUnwrapper {
    last: Option<i64>,
}

struct GoP {
    key_sequence: u16,
    last_picture_sequence: u16,
    last_with_padding: u16,
}

struct SeqNumOnlyRefFinder {
    gops: Vec<GoP>,
    stashed: VecDeque<EncodedFrame>,
    stashed_padding: HashSet<u16>,
    unwrapper: SequenceUnwrapper,
}

struct FrameInfo {
    frame: EncodedFrame,
    continuous: bool,
}

struct FrameBuffer {
    frames: BTreeMap<i64, FrameInfo>,
    decoded: HashSet<i64>,
    decoded_order: VecDeque<i64>,
    last_decoded_id: Option<i64>,
    last_decoded_timestamp: Option<u32>,
    last_continuous_frame_id: Option<i64>,
    keyframe_required: bool,
    dropped: u64,
}

enum FrameDecision {
    HandOff(EncodedFrame),
    Stash(EncodedFrame),
    Drop,
}

#[derive(Default)]
struct ReferenceResult {
    frames: Vec<EncodedFrame>,
    stashed: u64,
    dropped: u64,
}

#[derive(Clone, Copy, Default)]
pub struct ReplayOptions {
    pub drop_repairable_originals: bool,
}

pub(crate) struct ReceivedVideoFrame {
    pub codec: VideoCodecKind,
    pub frame_id: i64,
    pub data: Vec<u8>,
    pub parameter_format: Option<VideoFormatSignature>,
    pub rtp_timestamp: u32,
    pub keyframe: bool,
    pub color_space: Option<VideoColorSpace>,
    pub rotation: u16,
    pub content_type: u8,
    pub video_capture_index: Option<u16>,
    pub is_new_picture: Option<bool>,
    pub video_timing: Option<VideoSendTiming>,
    pub frame_sending_delay_ms: Option<u16>,
    pub received_at: Instant,
    pub last_received_at: Instant,
    pub assembled_at: Instant,
    pub schedule: PlayoutSchedule,
}

#[derive(Default)]
pub(crate) struct ReceiverResult {
    pub frames: Vec<ReceivedVideoFrame>,
    pub request_keyframe: bool,
    pub force_keyframe: bool,
    pub accepted_packet: bool,
    pub continuous_sequence: Option<u16>,
    pub predecode_drops: usize,
    pub frame_buffer_frames: usize,
}

pub(crate) struct VideoHeaderExtensions {
    pub orientation: Option<u8>,
    pub content_type: Option<u8>,
    pub capture_index: Option<u8>,
    pub is_new_picture: Option<u8>,
    pub timing: Option<u8>,
    pub sending_delay: Option<u8>,
    pub color_space: Option<u8>,
}

pub(crate) struct OfficialVideoReceiver {
    packet_buffer: PacketBuffer,
    reference_finder: SeqNumOnlyRefFinder,
    frame_buffer: FrameBuffer,
    in_flight: HashMap<i64, InFlightFrame>,
    decoder_requires_keyframe: bool,
    has_decoded_frame: bool,
    last_decode_keyframe_request: Option<Instant>,
    last_sequence_by_frame: BTreeMap<i64, u16>,
    pending_continuous_sequence: Option<u16>,
    pending_predecode_drops: usize,
    timing: VideoPlayoutTiming,
    scheduled: Option<(u32, DecodeSchedule)>,
    decode_timeout: ReceiveDecodeTimeout,
    extensions: VideoHeaderExtensions,
    color_history: VideoColorHistory,
    current_codec: Option<VideoCodecKind>,
    last_assembled_timestamp: Option<u32>,
    last_completed_picture_id: i64,
    has_received_frame: bool,
}

struct InFlightFrame {
    last_sequence: u16,
    released_at: Instant,
    request_due: bool,
    previously_required_keyframe: bool,
}

impl OfficialVideoReceiver {
    pub(crate) fn new(
        mime_type: &str,
        extensions: VideoHeaderExtensions,
        codec_fmtp: &str,
    ) -> Result<Self> {
        let codec = if mime_type.eq_ignore_ascii_case("video/H264") {
            VideoCodecKind::H264
        } else if mime_type.eq_ignore_ascii_case("video/H265")
            || mime_type.eq_ignore_ascii_case("video/HEVC")
        {
            VideoCodecKind::H265
        } else {
            anyhow::bail!("unsupported official receiver codec {mime_type}");
        };
        let mut packet_buffer = PacketBuffer::new();
        if matches!(codec, VideoCodecKind::H264) {
            packet_buffer.install_h264_sprop(codec_fmtp);
        }
        Ok(Self {
            packet_buffer,
            reference_finder: SeqNumOnlyRefFinder::new(),
            frame_buffer: FrameBuffer::new(),
            in_flight: HashMap::new(),
            decoder_requires_keyframe: true,
            has_decoded_frame: false,
            last_decode_keyframe_request: None,
            last_sequence_by_frame: BTreeMap::new(),
            pending_continuous_sequence: None,
            pending_predecode_drops: 0,
            timing: VideoPlayoutTiming::new(Instant::now()),
            scheduled: None,
            decode_timeout: ReceiveDecodeTimeout::new(Instant::now()),
            extensions,
            color_history: VideoColorHistory::default(),
            current_codec: None,
            last_assembled_timestamp: None,
            last_completed_picture_id: -1,
            has_received_frame: false,
        })
    }

    pub(crate) fn configure_codec(&mut self, codec: VideoCodecKind, fmtp: &str) {
        if matches!(codec, VideoCodecKind::H264) {
            self.packet_buffer.install_h264_sprop(fmtp);
        }
    }

    pub(crate) fn parse_video_packet(
        &mut self,
        packet: &RtpPacket,
        received_at: Instant,
        codec: VideoCodecKind,
    ) -> Option<ParsedVideoPacket> {
        // RTP::Unmarshal already removes trailing padding. P=1 can still
        // carry a valid video payload (UU ReceivePacket, 0x18032D3F8).
        if packet.payload.is_empty() {
            return None;
        }
        let parsed = match codec {
            VideoCodecKind::H264 => depacketize_h264(packet, 0, received_at),
            VideoCodecKind::H265 => depacketize_h265(packet, 0, received_at),
        };
        let mut parsed = parsed?;
        let color_extension = self
            .extensions
            .color_space
            .and_then(|id| packet.header.get_extension(id));
        parsed.color_space = self.color_history.receive(
            packet.header.marker,
            parsed.packet_keyframe,
            color_extension.as_deref(),
        );
        parsed.rotation = self
            .extensions
            .orientation
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| payload.first().copied())
            .map(|value| u16::from(value & 0x03) * 90);
        parsed.content_type = self
            .extensions
            .content_type
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [value] if value & 0x80 == 0 => Some(value & 0x03),
                _ => None,
            });
        parsed.video_capture_index = self
            .extensions
            .capture_index
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [high, low] => Some(u16::from_be_bytes([*high, *low])),
                _ => None,
            });
        parsed.video_timing = self
            .extensions
            .timing
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| parse_video_timing(payload.as_ref()));
        parsed.is_new_picture = self
            .extensions
            .is_new_picture
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [value] => Some(*value != 0),
                _ => None,
            });
        parsed.frame_sending_delay_ms = self
            .extensions
            .sending_delay
            .and_then(|id| packet.header.get_extension(id))
            .and_then(|payload| match payload.as_ref() {
                [high, low] => Some(u16::from_be_bytes([*high, *low])),
                _ => None,
            });
        Some(parsed)
    }

    pub(crate) fn receive_parsed(&mut self, parsed: ParsedVideoPacket) -> ReceiverResult {
        let insert = self.packet_buffer.insert(parsed);
        self.accept_insert(insert)
    }

    pub(crate) fn receive_padding(&mut self, sequence_number: u16) -> ReceiverResult {
        let references = self.reference_finder.padding_received(sequence_number);
        self.insert_reference_result(references);
        let insert = self.packet_buffer.insert_padding(sequence_number);
        self.accept_insert(insert)
    }

    pub(crate) fn decoder_finished(
        &mut self,
        frame_id: i64,
        result: VideoDecodeResult,
    ) -> ReceiverResult {
        let Some(frame) = self.in_flight.remove(&frame_id) else {
            return ReceiverResult::default();
        };
        let accepted = result.accepted();
        let request_keyframe = if accepted {
            result == VideoDecodeResult::RequestKeyframe
        } else {
            frame.request_due || !self.has_decoded_frame || !frame.previously_required_keyframe
        };
        self.decoder_requires_keyframe = !accepted;
        self.frame_buffer.keyframe_required = !accepted;
        if accepted {
            self.has_decoded_frame = true;
            self.packet_buffer.clear_to(frame.last_sequence);
            self.reference_finder.clear_to(frame.last_sequence);
            self.last_sequence_by_frame
                .retain(|stored_id, _| *stored_id > frame_id);
        }
        if request_keyframe {
            self.last_decode_keyframe_request = Some(frame.released_at);
            self.decode_timeout.request_count = self.decode_timeout.request_count.saturating_add(1);
        }
        self.decode_timeout.update_waits();
        self.decode_timeout
            .start_next(self.decoder_requires_keyframe, Instant::now());
        // The official worker applies the complete Decode result before it
        // requests the next frame. There is no intermediate admission message.
        let frames = self.take_ready_frames(false);
        ReceiverResult {
            frames,
            request_keyframe,
            force_keyframe: request_keyframe,
            accepted_packet: false,
            continuous_sequence: self.pending_continuous_sequence.take(),
            predecode_drops: std::mem::take(&mut self.pending_predecode_drops),
            frame_buffer_frames: self.frame_buffer.frames.len(),
        }
    }

    pub(crate) fn record_decode(&mut self, duration: Duration, finished_at: Instant) {
        self.timing.record_decode(duration, finished_at);
        tracing::trace!(
            decode_p95_ms = self.timing.decode_estimate().as_millis(),
            "UU receive-stream decode timing sample"
        );
    }

    pub(crate) fn next_deadline(&self) -> Instant {
        self.scheduled
            .map_or(self.decode_timeout.deadline, |(_, scheduled)| {
                scheduled.deadline.min(self.decode_timeout.deadline)
            })
    }

    pub(crate) fn poll(&mut self, active: bool, receiving_keyframe: bool) -> ReceiverResult {
        let now = Instant::now();
        // Decode release precedes a timeout at the same instant, as its deadline
        // is normally clamped to one millisecond before the timeout.
        let frames = self.take_ready_frames(true);
        let mut request_keyframe = false;
        if now >= self.decode_timeout.deadline {
            self.decode_timeout.advance(now);
            request_keyframe = active && !receiving_keyframe;
            if request_keyframe {
                self.last_decode_keyframe_request = Some(now);
                self.decode_timeout.request_count =
                    self.decode_timeout.request_count.saturating_add(1);
            }
            self.decode_timeout.update_waits();
            self.decode_timeout
                .start_next(self.decoder_requires_keyframe, now);
        }
        ReceiverResult {
            frames,
            request_keyframe,
            force_keyframe: request_keyframe,
            predecode_drops: std::mem::take(&mut self.pending_predecode_drops),
            frame_buffer_frames: self.frame_buffer.frames.len(),
            ..ReceiverResult::default()
        }
    }

    fn accept_insert(&mut self, insert: PacketInsertResult) -> ReceiverResult {
        let mut request_keyframe = insert.cleared || insert.parameter_rejected;
        for frame in insert.frames {
            request_keyframe |= self.insert_assembled_frame(frame);
        }
        let frames = self.take_ready_frames(false);
        ReceiverResult {
            frames,
            request_keyframe,
            force_keyframe: false,
            accepted_packet: !insert.parameter_rejected,
            continuous_sequence: self.pending_continuous_sequence.take(),
            predecode_drops: std::mem::take(&mut self.pending_predecode_drops),
            frame_buffer_frames: self.frame_buffer.frames.len(),
        }
    }

    fn insert_assembled_frame(&mut self, frame: EncodedFrame) -> bool {
        let request_keyframe = !self.has_received_frame && !frame.keyframe;
        self.has_received_frame = true;

        if let Some(current_codec) = self.current_codec {
            if current_codec != frame.codec {
                let newer = self
                    .last_assembled_timestamp
                    .is_none_or(|last| rtp_timestamp_ahead_of(frame.timestamp, last));
                if !newer {
                    return request_keyframe;
                }
                self.reference_finder = SeqNumOnlyRefFinder::with_unwrap_anchor(
                    self.last_completed_picture_id + i64::from(u16::MAX),
                );
                self.current_codec = Some(frame.codec);
            }
        } else {
            self.current_codec = Some(frame.codec);
        }
        if self
            .last_assembled_timestamp
            .is_none_or(|last| rtp_timestamp_ahead_of(frame.timestamp, last))
        {
            self.last_assembled_timestamp = Some(frame.timestamp);
        }

        let references = self.reference_finder.manage(frame);
        self.insert_reference_result(references);
        request_keyframe
    }

    fn insert_reference_result(&mut self, result: ReferenceResult) {
        for frame in result.frames {
            if let Some(delay) = frame.playout_delay {
                self.timing.set_playout_delay(delay);
            }
            let frame_id = frame.id;
            let timestamp = frame.timestamp;
            let received_at = frame.last_received_at;
            let nack_delayed = frame.nack_count > 0;
            let was_present = self.frame_buffer.frames.contains_key(&frame_id);
            self.last_completed_picture_id = self.last_completed_picture_id.max(frame_id);
            self.last_sequence_by_frame
                .insert(frame_id, frame.last_sequence_number);
            if let Some(continuous_id) = self.frame_buffer.insert(frame)
                && let Some(sequence_number) =
                    self.last_sequence_by_frame.get(&continuous_id).copied()
            {
                self.pending_continuous_sequence = Some(sequence_number);
            }
            if !was_present && self.frame_buffer.frames.contains_key(&frame_id) && !nack_delayed {
                self.timing.incoming_timestamp(timestamp, received_at);
            }
        }
    }

    fn take_ready_frames(&mut self, release_scheduled: bool) -> Vec<ReceivedVideoFrame> {
        let mut output = Vec::new();
        while self.in_flight.len() < DECODER_ADMISSION_TOKENS {
            let Some((next_id, last_id)) = self.frame_buffer.next_and_last_decodable() else {
                self.scheduled = None;
                break;
            };
            let next = &self.frame_buffer.frames[&next_id].frame;
            let timestamp = next.timestamp;
            let now = Instant::now();
            let schedule = if self.frame_buffer.keyframe_required {
                self.scheduled = None;
                if !next.references.is_empty() {
                    self.pending_predecode_drops += self.frame_buffer.drop_next_decodable();
                    continue;
                }
                self.timing.keyframe_schedule(timestamp, now)
            } else if let Some((scheduled_timestamp, schedule)) = self.scheduled
                && scheduled_timestamp == timestamp
            {
                if !release_scheduled || schedule.deadline > now {
                    break;
                }
                schedule
            } else {
                let last_timestamp = self.frame_buffer.frames[&last_id].frame.timestamp;
                let Some(schedule) = self.timing.decode_schedule(
                    timestamp,
                    last_timestamp,
                    self.frame_buffer.frames.len(),
                    now,
                    self.decode_timeout.deadline.saturating_duration_since(now),
                ) else {
                    self.scheduled = None;
                    self.pending_predecode_drops += self.frame_buffer.drop_next_decodable();
                    continue;
                };
                self.scheduled = Some((timestamp, schedule));
                // 3304B0 posts even a zero-delay callback. Do not decode inline
                // during packet insertion, or replace this timestamp's task
                // just because another independently decodable frame arrives.
                break;
            };
            self.scheduled = None;
            let Some(frame) = self.frame_buffer.extract_next_decodable() else {
                break;
            };
            self.decode_timeout.frame_released(now, frame.keyframe);
            let request_wait = Duration::from_secs(u64::from(
                self.decode_timeout.request_count.saturating_add(1).min(3),
            ));
            let request_due = self
                .last_decode_keyframe_request
                .is_none_or(|last| now >= last + request_wait);
            let schedule = self.timing.frame_released(
                frame.timestamp,
                frame.last_received_at,
                frame.data.len(),
                frame.nack_count > 0,
                schedule,
                now,
            );
            self.in_flight.insert(
                frame.id,
                InFlightFrame {
                    last_sequence: frame.last_sequence_number,
                    released_at: now,
                    request_due,
                    previously_required_keyframe: self.decoder_requires_keyframe,
                },
            );
            output.push(ReceivedVideoFrame {
                codec: frame.codec,
                frame_id: frame.id,
                data: frame.data,
                parameter_format: frame.parameter_format,
                rtp_timestamp: frame.timestamp,
                keyframe: frame.keyframe,
                color_space: frame.color_space,
                rotation: frame.rotation,
                content_type: frame.content_type,
                video_capture_index: frame.video_capture_index,
                is_new_picture: frame.is_new_picture,
                video_timing: frame.video_timing,
                frame_sending_delay_ms: frame.frame_sending_delay_ms,
                received_at: frame.received_at,
                last_received_at: frame.last_received_at,
                assembled_at: frame.assembled_at,
                schedule,
            });
        }
        output
    }
}

/// UU TimeoutTracker for this receiver's negotiated 1000ms NACK history:
/// initial keyframe/frame limits are 3s; decode-return and timeout callbacks
/// select min(1s * (keyframe request count + 1), 3s). No 50ms polling loop.
struct ReceiveDecodeTimeout {
    deadline: Instant,
    keyframe_wait: Duration,
    frame_wait: Duration,
    waiting_keyframe: bool,
    request_count: u32,
}

impl ReceiveDecodeTimeout {
    fn new(now: Instant) -> Self {
        Self {
            deadline: now + Duration::from_secs(3),
            keyframe_wait: Duration::from_secs(3),
            frame_wait: Duration::from_secs(3),
            waiting_keyframe: true,
            request_count: 0,
        }
    }

    fn update_waits(&mut self) {
        let wait = Duration::from_secs(u64::from(self.request_count.saturating_add(1).min(3)));
        self.keyframe_wait = wait;
        self.frame_wait = wait;
    }

    fn start_next(&mut self, keyframe: bool, now: Instant) {
        if keyframe {
            self.waiting_keyframe = true;
            self.deadline = self.deadline.min(now + self.keyframe_wait);
        }
    }

    fn frame_released(&mut self, now: Instant, keyframe: bool) {
        self.waiting_keyframe = false;
        self.deadline = now + self.frame_wait;
        if keyframe {
            self.request_count = 0;
        }
    }

    fn advance(&mut self, now: Instant) {
        self.deadline = now
            + if self.waiting_keyframe {
                self.keyframe_wait
            } else {
                self.frame_wait
            };
    }
}

pub fn replay_capture(path: PathBuf, options: ReplayOptions) -> Result<String> {
    let mut capture = load_capture(path.clone())?;
    // Capture writers are fed by independent SSRC interceptor drainers. File
    // order is not receive order: replay by the retained packet arrival before
    // comparing recovery behavior. Stable sorting preserves ties at µs precision.
    capture.packets.sort_by_key(|packet| packet.elapsed_micros);
    let mut observed_packets = BTreeMap::<u32, u64>::new();
    for captured in &capture.packets {
        let mut raw = captured.bytes.as_slice();
        if let Ok(packet) = RtpPacket::unmarshal(&mut raw) {
            *observed_packets.entry(packet.header.ssrc).or_default() += 1;
        }
    }
    let media = capture
        .streams
        .iter()
        .filter(|stream| {
            (stream.mime_type.eq_ignore_ascii_case("video/H265")
                || stream.mime_type.eq_ignore_ascii_case("video/H264"))
                && stream.associated_ssrc.is_none()
        })
        .max_by_key(|stream| observed_packets.get(&stream.ssrc).copied().unwrap_or(0))
        .context("capture contains no video media stream")?;
    let media_ssrc = media.ssrc;
    let payload_codecs = capture
        .codecs
        .get(&media_ssrc)
        .context("capture has no negotiated PT-to-codec mapping; capture a new session")?;
    let rsfec_config = payload_codecs
        .iter()
        .find(|codec| codec.mime_type.eq_ignore_ascii_case("video/rs-fec-cm256"))
        .map(|codec| RsFecConfig::from_fmtp(&codec.fmtp));
    let allow_mixed = *capture
        .extmap_allow_mixed
        .get(&media_ssrc)
        .context("capture lacks receive extension policy; capture a new session")?;
    let extension_id = |uri: &str| {
        media
            .header_extensions
            .iter()
            .find(|(_, value)| value == uri)
            .and_then(|(id, _)| u8::try_from(*id).ok())
    };
    let rid = extension_id("urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id");
    let repaired_rid = extension_id("urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id");
    let color_extension_id = extension_id(crate::video_color::COLOR_SPACE_URI);
    let mut color_history = VideoColorHistory::default();
    let rtx_ssrcs = capture
        .streams
        .iter()
        .filter(|stream| {
            stream.mime_type.eq_ignore_ascii_case("video/rtx")
                && stream.associated_ssrc == Some(media_ssrc)
        })
        .map(|stream| stream.ssrc)
        .collect::<HashSet<_>>();
    let fec_ssrcs = capture
        .streams
        .iter()
        .filter(|stream| {
            stream.mime_type.eq_ignore_ascii_case("video/rs-fec-cm256")
                && stream.associated_ssrc == Some(media_ssrc)
        })
        .map(|stream| stream.ssrc)
        .collect::<HashSet<_>>();
    let repairable_originals = capture
        .packets
        .iter()
        .filter_map(|captured| {
            let mut raw = captured.bytes.as_slice();
            let packet = RtpPacket::unmarshal(&mut raw).ok()?;
            if !rtx_ssrcs.contains(&packet.header.ssrc)
                || packet.payload.len() < 2
                || (packet.header.padding && captured.bytes.last() == Some(&0))
            {
                return None;
            }
            Some((
                u16::from_be_bytes([packet.payload[0], packet.payload[1]]),
                packet.header.timestamp,
            ))
        })
        .collect::<HashSet<_>>();

    let mut stats = ReplayStats::default();
    let mut packet_buffer = PacketBuffer::new();
    let mut reference_finder = SeqNumOnlyRefFinder::new();
    let mut frame_buffer = FrameBuffer::new();
    let mut last_payload_type = None;
    let mut timeline = ReplayTimeline::default();
    let capture_origin = Instant::now();
    let mutable_extensions = media
        .header_extensions
        .iter()
        .filter_map(|(id, uri)| Some((u8::try_from(*id).ok()?, fec_mutable_preserve_bytes(uri)?)))
        .collect::<Vec<_>>();
    let mut fec_receiver = rsfec_config
        .filter(|_| !fec_ssrcs.is_empty())
        .map(|config| RsFecReceiver::new(media_ssrc, config.max_k, mutable_extensions));

    for captured in capture.packets {
        timeline.current_received_micros = captured.elapsed_micros;
        let mut raw = captured.bytes.as_slice();
        let Ok(packet) = RtpPacket::unmarshal(&mut raw) else {
            stats.malformed_packets += 1;
            continue;
        };
        let outer_is_rtx = rtx_ssrcs.contains(&packet.header.ssrc);
        if outer_is_rtx && packet.header.padding && captured.bytes.last() == Some(&0) {
            stats.malformed_packets += 1;
            continue;
        }
        let received_at = capture_origin + Duration::from_micros(captured.elapsed_micros);
        let mut pending = VecDeque::<(RtpPacket, bool, Option<Vec<u8>>)>::new();
        if fec_ssrcs.contains(&packet.header.ssrc) {
            stats.fec_packets += 1;
            let Some(receiver) = fec_receiver.as_mut() else {
                continue;
            };
            match receiver.receive_repair(&packet.payload) {
                Ok(recovery) => {
                    stats.fec_recovered_packets += recovery.recovered_packets.len() as u64;
                    for recovered in recovery.recovered_packets {
                        let mut raw = recovered.as_slice();
                        let Ok(packet) = RtpPacket::unmarshal(&mut raw) else {
                            stats.malformed_packets += 1;
                            continue;
                        };
                        let is_rtx = rtx_ssrcs.contains(&packet.header.ssrc);
                        pending.push_back((packet, is_rtx, None));
                    }
                }
                Err(error) => {
                    stats.malformed_packets += 1;
                    tracing::debug!(%error, "offline RSFEC repair rejected");
                }
            }
        } else {
            let is_rtx = outer_is_rtx;
            if packet.header.ssrc == media_ssrc {
                stats.media_packets += 1;
                if options.drop_repairable_originals
                    && repairable_originals
                        .contains(&(packet.header.sequence_number, packet.header.timestamp))
                {
                    stats.injected_drops += 1;
                    continue;
                }
            } else if is_rtx {
                stats.rtx_packets += 1;
            } else {
                continue;
            }
            pending.push_back((packet, is_rtx, Some(captured.bytes)));
        }

        while let Some((packet, is_rtx, source)) = pending.pop_front() {
            let Some((sequence_number, source)) = process_media_packet(
                packet,
                is_rtx,
                source,
                captured.elapsed_micros,
                received_at,
                media_ssrc,
                payload_codecs,
                &mut last_payload_type,
                color_extension_id,
                &mut color_history,
                (
                    rsfec_config.is_some_and(|config| config.rtx_as_source),
                    rid,
                    repaired_rid,
                    allow_mixed,
                ),
                &mut stats,
                &mut timeline,
                &mut packet_buffer,
                &mut reference_finder,
                &mut frame_buffer,
            ) else {
                continue;
            };
            let Some(receiver) = fec_receiver.as_mut() else {
                continue;
            };
            match receiver.remember_media(sequence_number, &source) {
                Ok(recovery) => {
                    stats.fec_recovered_packets += recovery.recovered_packets.len() as u64;
                    for recovered in recovery.recovered_packets {
                        let mut raw = recovered.as_slice();
                        let Ok(packet) = RtpPacket::unmarshal(&mut raw) else {
                            stats.malformed_packets += 1;
                            continue;
                        };
                        let is_rtx = rtx_ssrcs.contains(&packet.header.ssrc);
                        pending.push_back((packet, is_rtx, None));
                    }
                }
                Err(error) => {
                    stats.malformed_packets += 1;
                    tracing::debug!(%error, "offline RSFEC media source rejected");
                }
            }
        }
    }
    stats.frame_buffer_dropped = frame_buffer.dropped;
    let frame_digest = timeline.frame_digest.finalize_reset();
    let frame_digest = frame_digest
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect::<String>();

    Ok(format!(
        "replay: {}\ntruncated tail: {}\nmedia ssrc: {}\nRTX/FEC streams: {}/{}\nmedia packets: {}\nRTX packets: {}\ninjected repairable-original drops: {}\nFEC repair/recovered: {}/{}\npadding packets: {}\nmalformed packets: {} {:?}\nparameter dependency requests: {}\nduplicate packets: {}\nPacketBuffer expansions/clears: {}/{}\nPacketBuffer clear points: {:?}\ncomplete frames/keyframes: {}/{}\nkeyframe points: {:?}\nreference stashed/dropped: {}/{}\nFrameBuffer dropped: {}\ndecodable frames: {}\ndecodable bytes: {}\ndecodable SHA-256: {}\ndecodable timestamp range: {:?}..{:?}\ndecodable arrival range: {:?}..{:?} us\n",
        path.display(),
        capture.truncated_tail,
        media_ssrc,
        rtx_ssrcs.len(),
        fec_ssrcs.len(),
        stats.media_packets,
        stats.rtx_packets,
        stats.injected_drops,
        stats.fec_packets,
        stats.fec_recovered_packets,
        stats.padding_packets,
        stats.malformed_packets,
        timeline.malformed_points,
        stats.parameter_requests,
        stats.duplicate_packets,
        stats.packet_buffer_expansions,
        stats.packet_buffer_clears,
        timeline.packet_buffer_clear_points,
        stats.completed_frames,
        stats.keyframes,
        timeline.keyframe_points,
        stats.reference_stashed,
        stats.reference_dropped,
        stats.frame_buffer_dropped,
        stats.decoded_frames,
        stats.decoded_bytes,
        frame_digest,
        timeline.first_decoded_timestamp,
        timeline.last_decoded_timestamp,
        timeline.first_decoded_arrival,
        timeline.last_decoded_arrival,
    ))
}

#[allow(clippy::too_many_arguments)]
fn process_media_packet(
    mut packet: RtpPacket,
    is_rtx: bool,
    mut rsfec_source: Option<Vec<u8>>,
    elapsed_micros: u64,
    received_at: Instant,
    media_ssrc: u32,
    payload_codecs: &[CapturedCodec],
    last_payload_type: &mut Option<u8>,
    color_extension_id: Option<u8>,
    color_history: &mut VideoColorHistory,
    rtx_source: (bool, Option<u8>, Option<u8>, bool),
    stats: &mut ReplayStats,
    timeline: &mut ReplayTimeline,
    packet_buffer: &mut PacketBuffer,
    reference_finder: &mut SeqNumOnlyRefFinder,
    frame_buffer: &mut FrameBuffer,
) -> Option<(u16, Vec<u8>)> {
    if is_rtx {
        let payload_type = payload_codecs
            .iter()
            .find(|codec| {
                codec.payload_type == packet.header.payload_type
                    && codec.mime_type.eq_ignore_ascii_case("video/rtx")
            })?
            .fmtp
            .split(';')
            .find_map(|part| part.trim().strip_prefix("apt="))?
            .parse::<u8>()
            .ok()?;
        if packet.payload.len() < 2 {
            if packet.payload.is_empty() && packet.header.padding {
                stats.padding_packets += 1;
            } else {
                stats.malformed_packets += 1;
            }
            return None;
        }
        packet.header.sequence_number = u16::from_be_bytes([packet.payload[0], packet.payload[1]]);
        packet.header.payload_type = payload_type;
        packet.header.ssrc = media_ssrc;
        packet.header.padding = false;
        packet.payload = packet.payload.slice(2..);
        rsfec_source = if rtx_source.0 {
            normalize_rtx_source(&packet, rtx_source.1, rtx_source.2, rtx_source.3)
                .ok()
                .map(|source| source.to_vec())
        } else {
            None
        };
    }
    if packet.header.ssrc != media_ssrc {
        return None;
    }
    let payload_codec = payload_codecs
        .iter()
        .find(|codec| codec.payload_type == packet.header.payload_type)?;
    let codec = if payload_codec.mime_type.eq_ignore_ascii_case("video/H264") {
        VideoCodecKind::H264
    } else if payload_codec.mime_type.eq_ignore_ascii_case("video/H265") {
        VideoCodecKind::H265
    } else {
        return None;
    };
    if *last_payload_type != Some(packet.header.payload_type) {
        if codec == VideoCodecKind::H264 {
            packet_buffer.install_h264_sprop(&payload_codec.fmtp);
        }
        *last_payload_type = Some(packet.header.payload_type);
    }
    let source = rsfec_source.map(|source| (packet.header.sequence_number, source));
    if packet.payload.is_empty() {
        stats.padding_packets += 1;
        let decisions = reference_finder.padding_received(packet.header.sequence_number);
        accept_reference_decisions(decisions, stats, frame_buffer);
        drain_decodable(
            stats,
            timeline,
            packet_buffer,
            reference_finder,
            frame_buffer,
        );
        let insert = packet_buffer.insert_padding(packet.header.sequence_number);
        accept_completed_frames(
            insert.frames,
            stats,
            timeline,
            reference_finder,
            frame_buffer,
        );
        drain_decodable(
            stats,
            timeline,
            packet_buffer,
            reference_finder,
            frame_buffer,
        );
        return source;
    }
    let parsed = match codec {
        VideoCodecKind::H264 => depacketize_h264(&packet, elapsed_micros, received_at),
        VideoCodecKind::H265 => depacketize_h265(&packet, elapsed_micros, received_at),
    };
    let Some(mut parsed) = parsed else {
        stats.malformed_packets += 1;
        timeline.malformed_points.push((
            packet.header.sequence_number,
            packet.header.timestamp,
            packet.payload.first().map(|value| match codec {
                VideoCodecKind::H264 => value & 0x1f,
                VideoCodecKind::H265 => (value >> 1) & 0x3f,
            }),
            packet.payload.len(),
            elapsed_micros,
        ));
        return source;
    };
    let color_extension = color_extension_id.and_then(|id| packet.header.get_extension(id));
    parsed.color_space = color_history.receive(
        packet.header.marker,
        parsed.packet_keyframe,
        color_extension.as_deref(),
    );
    let insert = packet_buffer.insert(parsed);
    stats.parameter_requests += u64::from(insert.parameter_rejected);
    stats.duplicate_packets += u64::from(insert.duplicate);
    stats.packet_buffer_expansions += insert.expansions as u64;
    stats.packet_buffer_clears += u64::from(insert.cleared);
    if insert.cleared {
        timeline.packet_buffer_clear_points.push((
            packet.header.sequence_number,
            packet.header.timestamp,
            elapsed_micros,
        ));
    }
    accept_completed_frames(
        insert.frames,
        stats,
        timeline,
        reference_finder,
        frame_buffer,
    );
    drain_decodable(
        stats,
        timeline,
        packet_buffer,
        reference_finder,
        frame_buffer,
    );
    source
}

fn accept_completed_frames(
    frames: Vec<EncodedFrame>,
    stats: &mut ReplayStats,
    timeline: &mut ReplayTimeline,
    reference_finder: &mut SeqNumOnlyRefFinder,
    frame_buffer: &mut FrameBuffer,
) {
    for frame in frames {
        stats.completed_frames += 1;
        stats.keyframes += u64::from(frame.keyframe);
        if frame.keyframe {
            timeline.keyframe_points.push((
                frame.first_sequence_number,
                frame.last_sequence_number,
                frame.timestamp,
                frame.received_micros,
            ));
        }
        accept_reference_decisions(reference_finder.manage(frame), stats, frame_buffer);
    }
}

fn accept_reference_decisions(
    result: ReferenceResult,
    stats: &mut ReplayStats,
    frame_buffer: &mut FrameBuffer,
) {
    stats.reference_stashed += result.stashed;
    stats.reference_dropped += result.dropped;
    for frame in result.frames {
        frame_buffer.insert(frame);
    }
}

fn drain_decodable(
    stats: &mut ReplayStats,
    timeline: &mut ReplayTimeline,
    packet_buffer: &mut PacketBuffer,
    reference_finder: &mut SeqNumOnlyRefFinder,
    frame_buffer: &mut FrameBuffer,
) {
    while let Some(frame) = frame_buffer.extract_next_decodable() {
        let gap_us = timeline.last_released_micros.map_or(0, |last| {
            timeline.current_received_micros.saturating_sub(last)
        });
        if gap_us > 50_000 {
            tracing::debug!(target: "openuuyc::replay_recovery",
                at_us = timeline.current_received_micros,
                gap_us, first_packet_us = frame.received_micros,
                first_sequence = frame.first_sequence_number,
                last_sequence = frame.last_sequence_number,
                timestamp = frame.timestamp, keyframe = frame.keyframe,
                stashed_frames = reference_finder.stashed.len(),
                "offline recovered-frame release gap");
        }
        timeline.last_released_micros = Some(timeline.current_received_micros);
        timeline
            .first_decoded_timestamp
            .get_or_insert(frame.timestamp);
        timeline.last_decoded_timestamp = Some(frame.timestamp);
        timeline
            .first_decoded_arrival
            .get_or_insert(frame.received_micros);
        timeline.last_decoded_arrival = Some(frame.received_micros);
        stats.decoded_frames += 1;
        stats.decoded_bytes += frame.data.len() as u64;
        timeline.frame_digest.update(frame.timestamp.to_be_bytes());
        timeline
            .frame_digest
            .update((frame.data.len() as u64).to_be_bytes());
        timeline.frame_digest.update(&frame.data);
        packet_buffer.clear_to(frame.last_sequence_number);
        reference_finder.clear_to(frame.last_sequence_number);
    }
}

struct PacketInsertResult {
    frames: Vec<EncodedFrame>,
    duplicate: bool,
    expansions: usize,
    cleared: bool,
    parameter_rejected: bool,
}

impl PacketBuffer {
    fn new() -> Self {
        let mut buffer = Vec::with_capacity(PACKET_BUFFER_INITIAL_SIZE);
        buffer.resize_with(PACKET_BUFFER_INITIAL_SIZE, || None);
        Self {
            buffer,
            first_sequence_number: 0,
            first_packet_received: false,
            cleared_to_first_sequence: false,
            newest_inserted_sequence: None,
            missing_packets: HashSet::new(),
            received_padding: HashSet::new(),
            last_keyframe_first_sequence: None,
            parameters: ParameterTracker::default(),
        }
    }

    fn install_h264_sprop(&mut self, fmtp: &str) {
        self.parameters.install_h264_sprop(fmtp);
    }

    fn insert(&mut self, mut packet: ParsedVideoPacket) -> PacketInsertResult {
        let tracked = match packet.codec {
            VideoCodecKind::H264 => self.parameters.h264(
                &mut packet.nalus,
                packet.is_first_packet_in_frame,
                &mut packet.payload,
            ),
            VideoCodecKind::H265 => self
                .parameters
                .h265(&packet.nalus, packet.is_first_packet_in_frame),
        };
        match tracked {
            Ok(format) => packet.parameter_format = format.or(packet.parameter_format),
            Err(reason) => {
                tracing::debug!(
                    reason,
                    sequence_number = packet.sequence_number,
                    "request keyframe for missing codec parameter dependency"
                );
                return PacketInsertResult {
                    parameter_rejected: true,
                    ..PacketInsertResult::empty()
                };
            }
        }
        let sequence_number = packet.sequence_number;
        if !self.first_packet_received {
            self.first_sequence_number = sequence_number;
            self.first_packet_received = true;
        } else if ahead_of(self.first_sequence_number, sequence_number) {
            if self.cleared_to_first_sequence {
                return PacketInsertResult::empty();
            }
            self.first_sequence_number = sequence_number;
        }

        let mut index = sequence_number as usize % self.buffer.len();
        // UU's collision cleanup precedes duplicate detection and compares
        // raw uint16_t values (4F850B cmp/jnb), not AheadOf. Preserve that
        // branch even across sequence wrap; other sequence gates still use
        // their independently verified modular ordering.
        if self.buffer[index].as_ref().is_some_and(|stored| {
            self.last_keyframe_first_sequence
                .is_some_and(|keyframe| stored.packet.sequence_number < keyframe)
        }) {
            self.buffer[index] = None;
        }
        if self.buffer[index]
            .as_ref()
            .is_some_and(|stored| stored.packet.sequence_number == sequence_number)
        {
            let mut result = PacketInsertResult::empty();
            result.duplicate = true;
            return result;
        }
        let mut expansions = 0;
        while self.buffer[index].is_some() {
            let collided_sequence = self.buffer[index]
                .as_ref()
                .map(|stored| stored.packet.sequence_number);
            if !self.expand() {
                break;
            }
            expansions += 1;
            index = sequence_number as usize % self.buffer.len();
            tracing::debug!(
                sequence_number,
                collided_sequence,
                buffer_size = self.buffer.len(),
                first_sequence_number = self.first_sequence_number,
                cleared_to_first_sequence = self.cleared_to_first_sequence,
                "PacketBuffer size expanded"
            );
        }
        if self.buffer[index].is_some() {
            let occupied_slots = self.buffer.iter().filter(|slot| slot.is_some()).count();
            tracing::warn!(
                sequence_number,
                occupied_sequence = self.buffer[index]
                    .as_ref()
                    .map(|stored| stored.packet.sequence_number),
                first_sequence_number = self.first_sequence_number,
                cleared_to_first_sequence = self.cleared_to_first_sequence,
                last_keyframe_first_sequence = self.last_keyframe_first_sequence,
                buffer_size = self.buffer.len(),
                occupied_slots,
                "Clear PacketBuffer and request key frame"
            );
            self.clear();
            return PacketInsertResult {
                frames: Vec::new(),
                duplicate: false,
                expansions,
                cleared: true,
                parameter_rejected: false,
            };
        }
        self.buffer[index] = Some(BufferedPacket {
            packet,
            continuous: false,
        });
        self.update_missing(sequence_number);
        let padding_age = (self.buffer.len() / 4).min(usize::from(u16::MAX)) as u16;
        let oldest_padding = sequence_number.wrapping_sub(padding_age);
        self.received_padding
            .retain(|padding| sequence_at_or_ahead(*padding, oldest_padding));
        PacketInsertResult {
            frames: self.find_frames(sequence_number),
            duplicate: false,
            expansions,
            cleared: false,
            parameter_rejected: false,
        }
    }

    fn insert_padding(&mut self, sequence_number: u16) -> PacketInsertResult {
        self.update_missing(sequence_number);
        self.received_padding.insert(sequence_number);
        PacketInsertResult {
            frames: self.find_frames(sequence_number.wrapping_add(1)),
            duplicate: false,
            expansions: 0,
            cleared: false,
            parameter_rejected: false,
        }
    }

    fn expand(&mut self) -> bool {
        if self.buffer.len() == PACKET_BUFFER_MAX_SIZE {
            return false;
        }
        let new_size = (self.buffer.len() * 2).min(PACKET_BUFFER_MAX_SIZE);
        let mut expanded = Vec::with_capacity(new_size);
        expanded.resize_with(new_size, || None);
        for slot in &mut self.buffer {
            if let Some(packet) = slot.take() {
                let index = packet.packet.sequence_number as usize % new_size;
                expanded[index] = Some(packet);
            }
        }
        self.buffer = expanded;
        true
    }

    fn clear(&mut self) {
        for slot in &mut self.buffer {
            *slot = None;
        }
        self.first_packet_received = false;
        self.cleared_to_first_sequence = false;
        self.newest_inserted_sequence = None;
        self.missing_packets.clear();
        self.received_padding.clear();
        self.last_keyframe_first_sequence = None;
    }

    fn clear_to(&mut self, sequence_number: u16) {
        if self.cleared_to_first_sequence && ahead_of(self.first_sequence_number, sequence_number) {
            return;
        }
        if !self.first_packet_received {
            return;
        }
        let target = sequence_number.wrapping_add(1);
        let forward_difference = target.wrapping_sub(self.first_sequence_number) as usize;
        let iterations = forward_difference.min(self.buffer.len());
        for _ in 0..iterations {
            let index = self.first_sequence_number as usize % self.buffer.len();
            if self.buffer[index]
                .as_ref()
                .is_some_and(|stored| ahead_of(target, stored.packet.sequence_number))
            {
                self.buffer[index] = None;
            }
            self.first_sequence_number = self.first_sequence_number.wrapping_add(1);
        }
        self.first_sequence_number = target;
        self.cleared_to_first_sequence = true;
        self.missing_packets
            .retain(|missing| sequence_at_or_ahead(*missing, target));
        self.received_padding
            .retain(|padding| sequence_at_or_ahead(*padding, target));
    }

    fn potential_new_frame(&self, sequence_number: u16) -> bool {
        let index = sequence_number as usize % self.buffer.len();
        let previous_index = if index == 0 {
            self.buffer.len() - 1
        } else {
            index - 1
        };
        let Some(entry) = self.buffer[index].as_ref() else {
            return false;
        };
        if entry.packet.sequence_number != sequence_number {
            return false;
        }
        if entry.packet.is_first_packet_in_frame {
            return true;
        }
        let Some(previous) = self.buffer[previous_index].as_ref() else {
            return false;
        };
        previous.packet.sequence_number == sequence_number.wrapping_sub(1)
            && previous.packet.timestamp == entry.packet.timestamp
            && previous.continuous
    }

    fn find_frames(&mut self, mut sequence_number: u16) -> Vec<EncodedFrame> {
        let mut frames = Vec::new();
        for _ in 0..self.buffer.len() {
            if self.received_padding.contains(&sequence_number) {
                sequence_number = sequence_number.wrapping_add(1);
                continue;
            }
            if !self.potential_new_frame(sequence_number) {
                break;
            }
            let index = sequence_number as usize % self.buffer.len();
            let is_last = {
                let entry = self.buffer[index].as_mut().expect("potential frame slot");
                entry.continuous = true;
                entry.packet.marker
            };
            if is_last {
                let mut start = sequence_number;
                let mut tested = 0_usize;
                loop {
                    tested += 1;
                    if tested == self.buffer.len() {
                        break;
                    }
                    let previous = start.wrapping_sub(1);
                    let previous_index = previous as usize % self.buffer.len();
                    let Some(entry) = self.buffer[previous_index].as_ref() else {
                        break;
                    };
                    if entry.packet.sequence_number != previous
                        || entry.packet.timestamp
                            != self.buffer[index]
                                .as_ref()
                                .expect("last frame packet")
                                .packet
                                .timestamp
                    {
                        break;
                    }
                    start = previous;
                }
                let end = sequence_number.wrapping_add(1);
                let mut validate = start;
                while validate != end {
                    if self.buffer[validate as usize % self.buffer.len()]
                        .as_ref()
                        .is_some_and(|entry| entry.packet.nalus.len() > 9)
                    {
                        return frames;
                    }
                    validate = validate.wrapping_add(1);
                }
                let mut cursor = start;
                let mut data = Vec::new();
                let mut sps = false;
                let mut pps = false;
                let mut idr = false;
                let mut parameter_format = None;
                let mut color_space = None;
                let mut rotation = None;
                let mut content_type = None;
                let mut video_capture_index = None;
                let mut is_new_picture = None;
                let mut video_timing = None;
                let mut frame_sending_delay_ms = None;
                let mut playout_delay = None;
                let mut nack_count = 0;
                let mut received_micros = u64::MAX;
                let mut received_at: Option<Instant> = None;
                let mut last_received_at: Option<Instant> = None;
                let timestamp = self.buffer[index]
                    .as_ref()
                    .expect("last frame packet")
                    .packet
                    .timestamp;
                let codec = self.buffer[index]
                    .as_ref()
                    .expect("last frame packet")
                    .packet
                    .codec;
                while cursor != end {
                    if self.received_padding.remove(&cursor) {
                        cursor = cursor.wrapping_add(1);
                        continue;
                    }
                    let slot = cursor as usize % self.buffer.len();
                    let Some(packet) = self.buffer[slot].take() else {
                        break;
                    };
                    if cursor == start {
                        playout_delay = packet.packet.playout_delay;
                    }
                    nack_count = nack_count.max(packet.packet.nack_count);
                    sps |= packet.packet.sps;
                    pps |= packet.packet.pps;
                    idr |= packet.packet.idr;
                    parameter_format = packet.packet.parameter_format.or(parameter_format);
                    if packet.packet.marker {
                        color_space = packet.packet.color_space;
                    }
                    rotation = rotation.or(packet.packet.rotation);
                    content_type = content_type.or(packet.packet.content_type);
                    video_capture_index = video_capture_index.or(packet.packet.video_capture_index);
                    // First valid marker in sequence order, including a false
                    // value; never inherit the previous frame's marker.
                    is_new_picture = is_new_picture.or(packet.packet.is_new_picture);
                    video_timing = video_timing.or(packet.packet.video_timing);
                    frame_sending_delay_ms =
                        frame_sending_delay_ms.or(packet.packet.frame_sending_delay_ms);
                    received_micros = received_micros.min(packet.packet.received_micros);
                    received_at = Some(received_at.map_or(packet.packet.received_at, |current| {
                        current.min(packet.packet.received_at)
                    }));
                    last_received_at = Some(
                        last_received_at.map_or(packet.packet.received_at, |current| {
                            current.max(packet.packet.received_at)
                        }),
                    );
                    data.extend_from_slice(&packet.packet.payload);
                    cursor = cursor.wrapping_add(1);
                }
                let keyframe = match codec {
                    VideoCodecKind::H264 => idr,
                    VideoCodecKind::H265 => sps && pps && idr,
                };
                if keyframe {
                    self.last_keyframe_first_sequence = Some(start);
                }
                frames.push(EncodedFrame {
                    codec,
                    id: 0,
                    references: Vec::new(),
                    first_sequence_number: start,
                    last_sequence_number: sequence_number,
                    timestamp,
                    keyframe,
                    color_space,
                    rotation: rotation.unwrap_or(0),
                    content_type: content_type.unwrap_or(0),
                    video_capture_index,
                    is_new_picture,
                    video_timing,
                    frame_sending_delay_ms,
                    playout_delay,
                    nack_count,
                    received_micros,
                    received_at: received_at.expect("complete frame has at least one packet"),
                    last_received_at: last_received_at
                        .expect("complete frame has at least one packet"),
                    assembled_at: Instant::now(),
                    data,
                    parameter_format,
                });
                self.missing_packets
                    .retain(|missing| *missing > sequence_number);
            }
            sequence_number = sequence_number.wrapping_add(1);
        }
        frames
    }

    fn update_missing(&mut self, sequence_number: u16) {
        let Some(mut newest) = self.newest_inserted_sequence else {
            self.newest_inserted_sequence = Some(sequence_number);
            return;
        };
        if ahead_of(sequence_number, newest) {
            let old = sequence_number.wrapping_sub(10_000);
            self.missing_packets
                .retain(|value| sequence_at_or_ahead(*value, old));
            if ahead_of(old, newest) {
                newest = old;
            }
            newest = newest.wrapping_add(1);
            while ahead_of(sequence_number, newest) {
                self.missing_packets.insert(newest);
                newest = newest.wrapping_add(1);
            }
            self.newest_inserted_sequence = Some(sequence_number);
        } else {
            self.missing_packets.remove(&sequence_number);
        }
    }
}

impl PacketInsertResult {
    fn empty() -> Self {
        Self {
            frames: Vec::new(),
            duplicate: false,
            expansions: 0,
            cleared: false,
            parameter_rejected: false,
        }
    }
}

impl SequenceUnwrapper {
    fn unwrap(&mut self, value: u16) -> i64 {
        let value = i64::from(value);
        let Some(last) = self.last else {
            self.last = Some(value);
            return value;
        };
        let base = last & !0xffff;
        let candidates = [base + value, base + value - 65_536, base + value + 65_536];
        let unwrapped = *candidates
            .iter()
            .min_by_key(|candidate| (*candidate - last).abs())
            .expect("sequence candidates");
        self.last = Some(unwrapped);
        unwrapped
    }
}

impl SeqNumOnlyRefFinder {
    fn new() -> Self {
        Self::with_unwrap_anchor_opt(None)
    }

    fn with_unwrap_anchor(anchor: i64) -> Self {
        Self::with_unwrap_anchor_opt(Some(anchor))
    }

    fn with_unwrap_anchor_opt(anchor: Option<i64>) -> Self {
        Self {
            gops: Vec::new(),
            stashed: VecDeque::new(),
            stashed_padding: HashSet::new(),
            unwrapper: SequenceUnwrapper { last: anchor },
        }
    }

    fn manage(&mut self, frame: EncodedFrame) -> ReferenceResult {
        let decision = self.manage_internal(frame);
        let mut output = ReferenceResult::default();
        match decision {
            FrameDecision::HandOff(frame) => {
                output.frames.push(frame);
                self.retry_stashed(&mut output);
            }
            FrameDecision::Stash(frame) => {
                if self.stashed.len() > MAX_STASHED_FRAMES {
                    self.stashed.pop_back();
                    output.dropped += 1;
                }
                self.stashed.push_front(frame);
                output.stashed += 1;
            }
            FrameDecision::Drop => output.dropped += 1,
        }
        output
    }

    fn manage_internal(&mut self, mut frame: EncodedFrame) -> FrameDecision {
        if frame.keyframe {
            if let Some(gop) = self
                .gops
                .iter_mut()
                .find(|gop| gop.key_sequence == frame.last_sequence_number)
            {
                gop.last_picture_sequence = frame.last_sequence_number;
                gop.last_with_padding = frame.last_sequence_number;
            } else {
                self.gops.push(GoP {
                    key_sequence: frame.last_sequence_number,
                    last_picture_sequence: frame.last_sequence_number,
                    last_with_padding: frame.last_sequence_number,
                });
            }
        }
        if self.gops.is_empty() {
            return FrameDecision::Stash(frame);
        }
        let keep_key = self
            .find_gop_index(frame.last_sequence_number)
            .map(|index| self.gops[index].key_sequence);
        if self.gops.len() > 1 {
            self.gops.retain(|gop| {
                Some(gop.key_sequence) == keep_key
                    || ahead_of(gop.key_sequence, frame.last_sequence_number)
                    || frame.last_sequence_number.wrapping_sub(gop.key_sequence) <= 100
            });
        }
        let Some(gop_index) = self.find_gop_index(frame.last_sequence_number) else {
            return FrameDecision::Drop;
        };
        let gop = &mut self.gops[gop_index];
        if !frame.keyframe && frame.first_sequence_number.wrapping_sub(1) != gop.last_with_padding {
            return FrameDecision::Stash(frame);
        }
        let last_reference = gop.last_picture_sequence;
        if !frame.keyframe {
            let reference = self.unwrapper.unwrap(last_reference);
            frame.references.push(reference);
        }
        frame.id = self.unwrapper.unwrap(frame.last_sequence_number);
        if ahead_of(frame.last_sequence_number, gop.last_picture_sequence) {
            gop.last_picture_sequence = frame.last_sequence_number;
            gop.last_with_padding = frame.last_sequence_number;
        }
        self.update_last_picture_with_padding(frame.last_sequence_number);
        FrameDecision::HandOff(frame)
    }

    fn find_gop_index(&self, sequence_number: u16) -> Option<usize> {
        self.gops
            .iter()
            .enumerate()
            .filter(|(_, gop)| !ahead_of(gop.key_sequence, sequence_number))
            .min_by_key(|(_, gop)| sequence_number.wrapping_sub(gop.key_sequence))
            .map(|(index, _)| index)
    }

    fn padding_received(&mut self, sequence_number: u16) -> ReferenceResult {
        self.stashed_padding.retain(|value| {
            ahead_of(*value, sequence_number) || sequence_number.wrapping_sub(*value) <= 100
        });
        self.stashed_padding.insert(sequence_number);
        self.update_last_picture_with_padding(sequence_number);
        let mut output = ReferenceResult::default();
        self.retry_stashed(&mut output);
        output
    }

    fn update_last_picture_with_padding(&mut self, sequence_number: u16) {
        let Some(gop_index) = self.find_gop_index(sequence_number) else {
            return;
        };
        loop {
            let next = self.gops[gop_index].last_with_padding.wrapping_add(1);
            if !self.stashed_padding.remove(&next) {
                break;
            }
            self.gops[gop_index].last_with_padding = next;
        }
        if sequence_number.wrapping_sub(self.gops[gop_index].key_sequence) > 10_000
            && !ahead_of(self.gops[gop_index].key_sequence, sequence_number)
        {
            let last_picture_sequence = self.gops[gop_index].last_picture_sequence;
            let last_with_padding = self.gops[gop_index].last_with_padding;
            self.gops.clear();
            self.gops.push(GoP {
                key_sequence: sequence_number,
                last_picture_sequence,
                last_with_padding,
            });
        }
    }

    fn retry_stashed(&mut self, output: &mut ReferenceResult) {
        loop {
            let mut progress = false;
            let mut remaining = VecDeque::new();
            while let Some(frame) = self.stashed.pop_front() {
                match self.manage_internal(frame) {
                    FrameDecision::HandOff(frame) => {
                        output.frames.push(frame);
                        progress = true;
                    }
                    FrameDecision::Stash(frame) => remaining.push_back(frame),
                    FrameDecision::Drop => output.dropped += 1,
                }
            }
            self.stashed = remaining;
            if !progress {
                break;
            }
        }
    }

    fn clear_to(&mut self, sequence_number: u16) {
        self.stashed
            .retain(|frame| !ahead_of(sequence_number, frame.first_sequence_number));
    }
}

impl FrameBuffer {
    fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            decoded: HashSet::new(),
            decoded_order: VecDeque::with_capacity(DECODED_HISTORY_SIZE),
            last_decoded_id: None,
            last_decoded_timestamp: None,
            last_continuous_frame_id: None,
            keyframe_required: true,
            dropped: 0,
        }
    }

    fn insert(&mut self, frame: EncodedFrame) -> Option<i64> {
        let mut references = HashSet::with_capacity(frame.references.len());
        if frame
            .references
            .iter()
            .any(|reference| *reference >= frame.id || !references.insert(*reference))
        {
            self.dropped += 1;
            return self.last_continuous_frame_id;
        }
        if self.last_decoded_id.is_some_and(|last| frame.id <= last) {
            let newer_keyframe = frame.keyframe
                && self
                    .last_decoded_timestamp
                    .is_some_and(|last| rtp_timestamp_ahead_of(frame.timestamp, last));
            if newer_keyframe {
                self.clear();
            } else {
                self.dropped += 1;
                return self.last_continuous_frame_id;
            }
        }
        if self.frames.contains_key(&frame.id) {
            return self.last_continuous_frame_id;
        }
        if self.frames.len() == FRAME_BUFFER_MAX_SIZE {
            if frame.keyframe {
                self.dropped += self.frames.len() as u64;
                self.clear();
            } else {
                self.dropped += 1;
                return self.last_continuous_frame_id;
            }
        }
        let continuous = self.references_continuous(&frame);
        let id = frame.id;
        self.frames.insert(id, FrameInfo { frame, continuous });
        self.propagate_continuity(id);
        self.last_continuous_frame_id
    }

    fn references_continuous(&self, frame: &EncodedFrame) -> bool {
        frame.references.iter().all(|reference| {
            self.decoded.contains(reference)
                || self
                    .frames
                    .get(reference)
                    .is_some_and(|value| value.continuous)
        })
    }

    fn propagate_continuity(&mut self, start: i64) {
        let ids = self
            .frames
            .range(start..)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ids {
            let continuous = self
                .frames
                .get(&id)
                .is_some_and(|info| info.continuous || self.references_continuous(&info.frame));
            if continuous && let Some(info) = self.frames.get_mut(&id) {
                info.continuous = true;
                self.last_continuous_frame_id = Some(
                    self.last_continuous_frame_id
                        .map_or(id, |last| last.max(id)),
                );
            }
        }
    }

    fn extract_next_decodable(&mut self) -> Option<EncodedFrame> {
        loop {
            let id = self.frames.iter().find_map(|(id, info)| {
                (info.continuous
                    && info
                        .frame
                        .references
                        .iter()
                        .all(|reference| self.decoded.contains(reference)))
                .then_some(*id)
            })?;
            let frame = self.frames.remove(&id)?.frame;
            if self.keyframe_required && !frame.keyframe {
                self.dropped += 1;
                continue;
            }
            if frame.keyframe {
                self.keyframe_required = false;
            }
            self.mark_decoded(id, frame.timestamp);
            return Some(frame);
        }
    }

    fn next_and_last_decodable(&self) -> Option<(i64, i64)> {
        let mut decodable = self.frames.iter().filter_map(|(id, info)| {
            (info.continuous
                && info
                    .frame
                    .references
                    .iter()
                    .all(|reference| self.decoded.contains(reference)))
            .then_some(*id)
        });
        let first = decodable.next()?;
        Some((first, decodable.next_back().unwrap_or(first)))
    }

    fn drop_next_decodable(&mut self) -> usize {
        let Some((next, _)) = self.next_and_last_decodable() else {
            return 0;
        };
        let remove = self
            .frames
            .range(..=next)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut dropped = 0;
        for id in remove {
            if self.frames.remove(&id).is_some() {
                self.dropped += 1;
                dropped += 1;
            }
        }
        self.recompute_last_continuous();
        dropped
    }

    fn recompute_last_continuous(&mut self) {
        self.last_continuous_frame_id = self
            .frames
            .iter()
            .filter_map(|(id, info)| info.continuous.then_some(*id))
            .next_back();
    }

    fn mark_decoded(&mut self, id: i64, timestamp: u32) {
        self.decoded.insert(id);
        self.decoded_order.push_back(id);
        self.last_decoded_id = Some(id);
        self.last_decoded_timestamp = Some(timestamp);
        while self.decoded_order.len() > DECODED_HISTORY_SIZE {
            if let Some(oldest) = self.decoded_order.pop_front() {
                self.decoded.remove(&oldest);
            }
        }
        self.propagate_continuity(id);
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.decoded.clear();
        self.decoded_order.clear();
        self.last_decoded_id = None;
        self.last_decoded_timestamp = None;
        self.last_continuous_frame_id = None;
        self.keyframe_required = true;
    }
}

fn rtp_timestamp_ahead_of(newer: u32, older: u32) -> bool {
    let distance = newer.wrapping_sub(older);
    distance != 0 && distance < 0x8000_0000
}

fn depacketize_h265(
    packet: &RtpPacket,
    received_micros: u64,
    received_at: Instant,
) -> Option<ParsedVideoPacket> {
    let payload = packet.payload.as_ref();
    if payload.len() < 2 {
        return None;
    }
    let kind = (payload[0] >> 1) & 63;
    if kind == 50 {
        return None;
    }
    let mut first = true;
    let mut output = Vec::new();
    let mut nalus = Vec::new();
    let mut packet_keyframe = false;
    if kind == 49 {
        if payload.len() < 3 {
            return None;
        }
        first = payload[2] & 0x80 != 0;
        let original = payload[2] & 63;
        packet_keyframe = (19..=21).contains(&original);
        if first {
            let mut unit = vec![(payload[0] & 0x81) | (original << 1), payload[1]];
            unit.extend_from_slice(&payload[3..]);
            nalus.push(h265_nalu_info(&unit, true));
            output.extend_from_slice(&START_CODE);
            output.extend(unit);
        } else {
            output.extend_from_slice(&payload[3..]);
        }
    } else {
        let mut units = Vec::new();
        if kind == 48 {
            if payload.len() < 5 {
                return None;
            }
            let mut offset = 2;
            while offset < payload.len() {
                let length =
                    u16::from_be_bytes(payload.get(offset..offset + 2)?.try_into().ok()?) as usize;
                offset += 2;
                if length < 2 {
                    return None;
                }
                units.push(payload.get(offset..offset + length)?);
                offset += length;
            }
        } else {
            units.push(payload);
        }
        for unit in units {
            let info = h265_nalu_info(unit, false);
            if (48..=50).contains(&info.kind) {
                return None;
            }
            packet_keyframe |= (19..=21).contains(&info.kind) || info.kind == 33;
            if nalus.len() < 10 {
                nalus.push(info);
            }
            output.extend_from_slice(&START_CODE);
            output.extend_from_slice(unit);
        }
    }
    parsed_packet(
        packet,
        VideoCodecKind::H265,
        first,
        packet_keyframe,
        nalus,
        output,
        received_micros,
        received_at,
    )
}

#[allow(clippy::too_many_arguments)]
fn parsed_packet(
    packet: &RtpPacket,
    codec: VideoCodecKind,
    first: bool,
    packet_keyframe: bool,
    mut nalus: Vec<NaluInfo>,
    payload: Vec<u8>,
    received_micros: u64,
    received_at: Instant,
) -> Option<ParsedVideoPacket> {
    let sps_kind = if codec == VideoCodecKind::H264 { 7 } else { 33 };
    let pps_kind = if codec == VideoCodecKind::H264 { 8 } else { 34 };
    let parameter_format = nalus
        .iter()
        .filter(|n| n.kind == sps_kind)
        .filter_map(|n| n.format)
        .next_back();
    // The official RTP video header has one geometry slot per packet, which
    // all SPS entries in that packet copy into their tracker records.
    for nalu in &mut nalus {
        if nalu.kind == sps_kind {
            nalu.format = parameter_format;
        }
    }
    Some(ParsedVideoPacket {
        codec,
        sequence_number: packet.header.sequence_number,
        timestamp: packet.header.timestamp,
        marker: packet.header.marker,
        is_first_packet_in_frame: first,
        sps: nalus.iter().any(|n| n.kind == sps_kind && n.sps >= 0),
        pps: nalus.iter().any(|n| n.kind == pps_kind),
        idr: nalus.iter().any(|n| {
            if codec == VideoCodecKind::H264 {
                n.kind == 5
            } else {
                (19..=21).contains(&n.kind)
            }
        }),
        packet_keyframe,
        nalus,
        parameter_format,
        color_space: None,
        rotation: None,
        content_type: None,
        video_capture_index: None,
        is_new_picture: None,
        video_timing: None,
        frame_sending_delay_ms: None,
        playout_delay: None,
        nack_count: 0,
        payload,
        received_micros,
        received_at,
    })
}

fn ahead_of(newer: u16, older: u16) -> bool {
    let distance = newer.wrapping_sub(older);
    distance != 0 && (distance < 0x8000 || (distance == 0x8000 && newer > older))
}

fn sequence_at_or_ahead(value: u16, reference: u16) -> bool {
    value == reference || ahead_of(value, reference)
}

fn parse_video_timing(payload: &[u8]) -> Option<VideoSendTiming> {
    let (flags, deltas) = match payload.len() {
        // Legacy WebRTC wire format omitted the flags byte.
        12 => (0, payload),
        13 => (payload[0], &payload[1..]),
        _ => return None,
    };
    if flags == u8::MAX {
        return None;
    }
    let read = |offset: usize| u16::from_be_bytes([deltas[offset], deltas[offset + 1]]);
    Some(VideoSendTiming {
        flags,
        encode_start_delta_ms: read(0),
        encode_finish_delta_ms: read(2),
        packetization_finish_delta_ms: read(4),
        pacer_exit_delta_ms: read(6),
        network_timestamp_delta_ms: read(8),
        network2_timestamp_delta_ms: read(10),
    })
}

fn fec_mutable_preserve_bytes(uri: &str) -> Option<usize> {
    match uri {
        "urn:ietf:params:rtp-hdrext:toffset"
        | "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
        | "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
        | "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay" => Some(0),
        "http://www.webrtc.org/experiments/rtp-hdrext/video-timing" => Some(7),
        _ => None,
    }
}

fn depacketize_h264(
    packet: &RtpPacket,
    received_micros: u64,
    received_at: Instant,
) -> Option<ParsedVideoPacket> {
    let payload = packet.payload.as_ref();
    let kind = *payload.first()? & 31;
    let mut first = true;
    let mut nalus = Vec::new();
    let mut output = Vec::new();
    let mut packet_keyframe = false;
    if kind == 28 {
        if payload.len() < 2 {
            return None;
        }
        first = payload[1] & 0x80 != 0;
        let original = payload[1] & 31;
        packet_keyframe = original == 5;
        if first {
            let mut unit = vec![(payload[0] & 0xe0) | original];
            unit.extend_from_slice(&payload[2..]);
            nalus.push(h264_nalu_info(&unit, true));
            output.extend_from_slice(&START_CODE);
            output.extend(unit);
        } else {
            output.extend_from_slice(&payload[2..]);
        }
    } else {
        let mut units = Vec::new();
        if kind == 24 {
            if payload.len() < 4 {
                return None;
            }
            let mut offset = 1;
            while offset < payload.len() {
                let length =
                    u16::from_be_bytes(payload.get(offset..offset + 2)?.try_into().ok()?) as usize;
                offset += 2;
                if length == 0 {
                    return None;
                }
                units.push(payload.get(offset..offset + length)?);
                offset += length;
            }
        } else {
            units.push(payload);
        }
        let mut last_rewrite = None;
        for (index, unit) in units.iter().enumerate() {
            let info = h264_nalu_info(unit, false);
            if matches!(info.kind, 24 | 28) {
                return None;
            }
            packet_keyframe |= matches!(info.kind, 5 | 7);
            if info.kind == 7
                && let Some(rewritten) = rewrite_h264_sps(unit)
            {
                last_rewrite = Some((index, rewritten));
            }
            if nalus.len() < 10 {
                nalus.push(info);
            }
        }
        // UU's STAP-A rewrite preserves the original packet prefix and
        // replaces the last SPS needing rewriting; do not chain replacements.
        for (index, unit) in units.iter().enumerate() {
            output.extend_from_slice(&START_CODE);
            if let Some((changed, bytes)) = last_rewrite.as_ref()
                && *changed == index
            {
                output.extend_from_slice(bytes);
            } else {
                output.extend_from_slice(unit);
            }
        }
    }
    parsed_packet(
        packet,
        VideoCodecKind::H264,
        first,
        packet_keyframe,
        nalus,
        output,
        received_micros,
        received_at,
    )
}
