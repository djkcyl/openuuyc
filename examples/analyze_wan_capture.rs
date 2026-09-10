use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use openuuyc::rtp_capture::load_capture;
use serde_json::json;
use webrtc::util::marshal::Unmarshal;

#[derive(Default)]
struct Frame {
    first_us: u64,
    last_us: u64,
    send_ticks: Option<i64>,
    marker: bool,
    packets: usize,
}

fn quantiles(mut samples: Vec<f64>) -> serde_json::Value {
    samples.sort_by(f64::total_cmp);
    if samples.is_empty() {
        return json!(null);
    }
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    json!({"count":samples.len(),"p50_ms":at(0.5),"p95_ms":at(0.95),"max_ms":at(1.0)})
}

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("capture path required")?;
    let capture = load_capture(path.into())?;
    let mut counts = HashMap::<(u32, u8), usize>::new();
    for record in &capture.packets {
        if record.bytes.len() >= 12 {
            let ssrc = u32::from_be_bytes(record.bytes[8..12].try_into()?);
            *counts.entry((ssrc, record.bytes[1] & 0x7f)).or_default() += 1;
        }
    }
    let (stream, payload_type) = capture
        .streams
        .iter()
        .filter(|s| s.associated_ssrc.is_none())
        .flat_map(|s| {
            capture
                .codecs
                .get(&s.ssrc)
                .into_iter()
                .flatten()
                .filter(|c| c.mime_type.eq_ignore_ascii_case("video/h265"))
                .map(move |c| (s, c.payload_type))
        })
        .max_by_key(|(s, pt)| counts.get(&(s.ssrc, *pt)).copied().unwrap_or(0))
        .context("populated primary H265 stream")?;
    let ast = stream
        .header_extensions
        .iter()
        .find(|(_, uri)| uri.ends_with("abs-send-time"))
        .map(|(id, _)| *id as u8);
    let mut frames = HashMap::<u32, Frame>::new();
    let mut last_send = None::<i64>;
    let mut primary_packets = 0;
    let mut rate_bins = BTreeMap::<u64, (u64, u64)>::new();
    let timing_id = stream
        .header_extensions
        .iter()
        .find(|(_, uri)| uri.ends_with("video-timing"))
        .map(|(id, _)| *id as u8);
    let mut timing_flags = BTreeMap::<u8, usize>::new();
    for record in &capture.packets {
        let packet = webrtc::rtp::packet::Packet::unmarshal(&mut record.bytes.as_slice())?;
        rate_bins
            .entry(record.elapsed_micros / 1_000_000)
            .or_default()
            .0 += record.bytes.len() as u64;
        if packet.header.ssrc != stream.ssrc {
            continue;
        }
        // PT 100 RED carries both video and ULPFEC on this negotiated stream.
        // Exclude FEC/RTX from source frame cadence; this is not an assembly test.
        if packet.header.payload_type != payload_type
            && !(packet.header.payload_type == 100 && packet.payload.first() == Some(&payload_type))
        {
            continue;
        }
        primary_packets += 1;
        if let Some(timing) = timing_id.and_then(|id| packet.header.get_extension(id))
            && timing.len() == 13
        {
            *timing_flags.entry(timing[0]).or_default() += 1;
        }
        rate_bins
            .entry(record.elapsed_micros / 1_000_000)
            .or_default()
            .1 += record.bytes.len() as u64;
        let sent = ast
            .and_then(|id| packet.header.get_extension(id))
            .filter(|v| v.len() == 3)
            .map(|v| {
                let raw = (i64::from(v[0]) << 16) | (i64::from(v[1]) << 8) | i64::from(v[2]);
                let value = last_send.map_or(raw, |last| {
                    let delta =
                        (raw - (last & 0xff_ffff) + (1 << 23)).rem_euclid(1 << 24) - (1 << 23);
                    last + delta
                });
                last_send = Some(last_send.map_or(value, |last| last.max(value)));
                value
            });
        let frame = frames
            .entry(packet.header.timestamp)
            .or_insert_with(|| Frame {
                first_us: record.elapsed_micros,
                ..Default::default()
            });
        frame.first_us = frame.first_us.min(record.elapsed_micros);
        frame.last_us = frame.last_us.max(record.elapsed_micros);
        frame.send_ticks = match (frame.send_ticks, sent) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        frame.marker |= packet.header.marker;
        frame.packets += 1;
    }
    let mut frames: Vec<_> = frames.into_iter().collect();
    frames.sort_by_key(|(_, f)| f.first_us);
    let mut source = Vec::new();
    let mut sent = Vec::new();
    let mut received = Vec::new();
    let mut spread = Vec::new();
    let mut gaps = Vec::new();
    for (_, frame) in &frames {
        spread.push((frame.last_us - frame.first_us) as f64 / 1000.0);
    }
    for pair in frames.windows(2) {
        let (previous_ts, previous) = &pair[0];
        let (next_ts, next) = &pair[1];
        let source_ms = f64::from(next_ts.wrapping_sub(*previous_ts) as i32) / 90.0;
        let received_ms = (next.last_us as i64 - previous.last_us as i64) as f64 / 1000.0;
        let sent_ms = next
            .send_ticks
            .zip(previous.send_ticks)
            .map(|(a, b)| (a - b) as f64 * 1000.0 / 262144.0);
        source.push(source_ms);
        received.push(received_ms);
        if let Some(v) = sent_ms {
            sent.push(v);
        }
        if received_ms > 60.0 {
            gaps.push(json!({"at_s":next.last_us as f64/1e6,"rtp_ms":source_ms,"send_ms":sent_ms,"receive_ms":received_ms,"marker":next.marker,"packets":next.packets}));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ssrc":stream.ssrc,"payload_type":payload_type,"abs_send_time_extension":ast,
            "primary_packets":primary_packets,"timestamp_groups":frames.len(),"truncated_tail":capture.truncated_tail,
            "video_timing_flags_packet_counts":timing_flags,
            "received_rtp_mbps_by_second":rate_bins.iter().map(|(second,(total,primary))|
                json!({"second":second,"all_rtp_mbps":*total as f64*8.0/1e6,"primary_video_rtp_mbps":*primary as f64*8.0/1e6})).collect::<Vec<_>>(),
            "rtp_interval":quantiles(source),"sender_last_packet_interval":quantiles(sent),
            "receiver_last_packet_interval":quantiles(received),"frame_packet_spread":quantiles(spread),
            "gaps_over_60ms":gaps,
            "scope":"Primary RTP groups only, no claim of complete AU recovery or actual display scanout"
        }))?
    );
    Ok(())
}
