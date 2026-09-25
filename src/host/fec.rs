//! UU RS-FEC sender, using the already validated systematic CM256 implementation.
use crate::rsfec::{RsFecConfig, cm256_encode_repairs, zero_mutable_extensions};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use webrtc::{interceptor::stream_info::StreamInfo, rtp::packet::Packet, util::marshal::Marshal};

// T542CF4 tables in the current V4.7.3 SDK. Only reachable low-RTT rows are kept.
const HIGH: &[u8; 109 * 256] = include_bytes!("fec-high-rtt.bin");
const LOW: &[u8; 70 * 256] = include_bytes!("fec-low-rtt.bin");
pub(crate) struct Sender {
    max_k: usize,
    mutable: HashMap<u8, usize>,
    sources: Vec<(u16, Vec<u8>)>,
    block: u16,
    frames: u32,
    keyframe: bool,
}
pub(crate) struct Block {
    id: u16,
    sources: Vec<(u16, Vec<u8>)>,
    protected_ssrc: u32,
    pub desired: usize,
    pub shard_size: usize,
    pub keyframe: bool,
}
impl Sender {
    pub fn new(info: &StreamInfo) -> Self {
        let config = RsFecConfig::from_fmtp(&info.sdp_fmtp_line);
        let mutable = info
            .rtp_header_extensions
            .iter()
            .filter_map(|e| {
                let preserve = match e.uri.as_str() {
                    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
                    | "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
                    | "urn:ietf:params:rtp-hdrext:toffset"
                    | "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay" => 0,
                    "http://www.webrtc.org/experiments/rtp-hdrext/video-timing" => 7,
                    _ => return None,
                };
                u8::try_from(e.id).ok().map(|id| (id, preserve))
            })
            .collect();
        Self {
            max_k: config.max_k as usize,
            mutable,
            sources: Vec::new(),
            block: rand::random(),
            frames: 0,
            keyframe: false,
        }
    }
    pub fn clear(&mut self) {
        self.sources.clear();
        self.frames = 0;
        self.keyframe = false;
    }
    pub fn push(
        &mut self,
        packet: &Packet,
        keyframe: bool,
        loss: f64,
        rtt_ms: u64,
        fps: u32,
    ) -> Result<Option<Block>> {
        let raw = packet.marshal()?;
        ensure!(raw.len() + 2 <= 1500, "RSFEC source exceeds shard capacity");
        let raw = zero_mutable_extensions(&raw, &self.mutable)
            .context("RSFEC source extensions invalid")?;
        if self.sources.first().is_some_and(|(base, _)| {
            packet.header.sequence_number.wrapping_sub(*base) as usize >= self.max_k
        }) {
            self.clear();
        }
        let mut shard = Vec::with_capacity(raw.len() + 2);
        shard.extend_from_slice(&(raw.len() as u16).to_be_bytes());
        shard.extend_from_slice(&raw);
        self.sources.push((packet.header.sequence_number, shard));
        self.keyframe |= keyframe;
        self.frames += u32::from(packet.header.marker);
        let max_frames =
            ((2.0 * rtt_ms as f64 * fps.max(1) as f64 / 1000.0).round() as u32).clamp(1, 6);
        let factor = (loss.clamp(0.0, 0.35) * 256.0) as usize;
        let min_group = if factor < 81 { 10 } else { 8 };
        if self.sources.len() < self.max_k
            && !(packet.header.marker
                && (self.frames >= max_frames || self.sources.len() >= min_group))
        {
            return Ok(None);
        }
        let sources = std::mem::take(&mut self.sources);
        let k = sources.len();
        let shard_size = sources.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        let loss = (loss.clamp(0.0, 1.0) * 255.0).round() as usize;
        let table = if rtt_ms > 120 || self.keyframe || k >= 71 {
            &HIGH[..]
        } else {
            &LOW[..]
        };
        let mut desired = table[(k - 1) * 256 + loss] as usize;
        if !self.keyframe && rtt_ms < 20 {
            desired = 0;
        }
        // Thresholds are quantized to /256 by T1EAD1A before T542CF4.
        let floor = ((k * 12 + 128) >> 8).max(1);
        let ceiling = ((k * 89 + 128) >> 8).max(1);
        desired = desired.clamp(floor, ceiling).min(k).min(256 - k);
        let block = Block {
            id: self.block,
            sources,
            protected_ssrc: packet.header.ssrc,
            desired,
            shard_size,
            keyframe: self.keyframe,
        };
        self.block = self.block.wrapping_add(1);
        self.frames = 0;
        self.keyframe = false;
        Ok(Some(block))
    }
}
impl Block {
    pub fn payloads(mut self, count: usize) -> Result<Vec<Vec<u8>>> {
        let k = self.sources.len();
        let count = count.min(self.desired).min(k).min(256 - k);
        if count == 0 {
            return Ok(Vec::new());
        }
        let base = self.sources[0].0;
        let span = self.sources.last().unwrap().0.wrapping_sub(base) as usize + 1;
        ensure!(span <= 109, "RSFEC sequence span exceeds mask");
        let mask_size = if span <= 15 {
            2
        } else if span <= 46 {
            6
        } else {
            14
        };
        let mut header = vec![0u8; 13 + mask_size];
        header[..2].copy_from_slice(&self.id.to_be_bytes());
        header[2] = k as u8;
        header[3] = count as u8;
        header[5..9].copy_from_slice(&self.protected_ssrc.to_be_bytes());
        header[9..11].copy_from_slice(&(self.shard_size as u16).to_be_bytes());
        header[11..13].copy_from_slice(&base.to_be_bytes());
        header[13
            + match mask_size {
                2 => 0,
                6 => 2,
                _ => 6,
            }] = 0x80;
        for (seq, shard) in &mut self.sources {
            let bit = seq.wrapping_sub(base) as usize;
            let wire = bit
                + if bit < 15 {
                    1
                } else if bit < 46 {
                    2
                } else {
                    3
                };
            header[13 + wire / 8] |= 0x80 >> (wire % 8);
            shard.resize(self.shard_size, 0);
        }
        let sources: Vec<_> = self.sources.into_iter().map(|(_, s)| s).collect();
        let repairs = cm256_encode_repairs(&sources, count as u8, self.shard_size)?;
        Ok(repairs
            .into_iter()
            .enumerate()
            .map(|(i, repair)| {
                let mut payload = header.clone();
                payload[4] = (k + i) as u8;
                payload.extend(repair);
                payload
            })
            .collect())
    }
}
