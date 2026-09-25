//! Offline capture replay; no live transport ownership.
use super::depacketize::fec_mutable_preserve_bytes;
use super::packets::PacketBuffer;
use super::process_media_packet;
use super::references::{FrameBuffer, SeqNumOnlyRefFinder};
use crate::diagnostics::rtp_capture::load_capture;
use crate::media::video_color::VideoColorHistory;
use crate::transport::rsfec::{RsFecConfig, RsFecReceiver};
use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::util::marshal::Unmarshal;

#[derive(Default)]
pub(super) struct ReplayStats {
    pub(super) media_packets: u64,
    pub(super) rtx_packets: u64,
    pub(super) padding_packets: u64,
    pub(super) fec_packets: u64,
    pub(super) fec_recovered_packets: u64,
    pub(super) malformed_packets: u64,
    pub(super) parameter_requests: u64,
    pub(super) duplicate_packets: u64,
    pub(super) packet_buffer_expansions: u64,
    pub(super) packet_buffer_clears: u64,
    pub(super) completed_frames: u64,
    pub(super) keyframes: u64,
    pub(super) reference_stashed: u64,
    pub(super) reference_dropped: u64,
    pub(super) frame_buffer_dropped: u64,
    pub(super) decoded_frames: u64,
    pub(super) decoded_bytes: u64,
    pub(super) injected_drops: u64,
}

#[derive(Default)]
pub(super) struct ReplayTimeline {
    pub(super) current_received_micros: u64,
    pub(super) last_released_micros: Option<u64>,
    pub(super) first_decoded_timestamp: Option<u32>,
    pub(super) last_decoded_timestamp: Option<u32>,
    pub(super) first_decoded_arrival: Option<u64>,
    pub(super) last_decoded_arrival: Option<u64>,
    pub(super) keyframe_points: Vec<(u16, u16, u32, u64)>,
    pub(super) packet_buffer_clear_points: Vec<(u16, u32, u64)>,
    pub(super) malformed_points: Vec<(u16, u32, Option<u8>, usize, u64)>,
    pub(super) frame_digest: Sha256,
}

#[derive(Clone, Copy, Default)]
pub struct ReplayOptions {
    pub drop_repairable_originals: bool,
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
    let color_extension_id = extension_id(crate::media::video_color::COLOR_SPACE_URI);
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
