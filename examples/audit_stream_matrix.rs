//! Temporary real-service verification driver, not a mock or a playback policy.
//! One process/route: reuse the product connection, settings, decoder and window.
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use openuuyc::controller;
use openuuyc::media::{CodecPreference, ConnectionMediaOptions, FrameRateChoice, TransportChoice};
use openuuyc::performance::{CadenceMetrics, PerformanceMonitor};
use openuuyc::stream_control::{StreamControlHandle, StreamControlSettings, StreamQuality};
use serde_json::json;

fn cadence(value: CadenceMetrics) -> serde_json::Value {
    json!({"average_ms":value.average_ms,"p95_ms":value.p95_ms,"max_ms":value.max_ms})
}

async fn run_cases(
    control: StreamControlHandle,
    performance: PerformanceMonitor,
    output: PathBuf,
    steady: bool,
    adaptive: Option<(u32, bool, usize)>,
) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut output = std::io::BufWriter::new(file);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let state = control.snapshot();
            if let Some(error) = state.last_error {
                bail!("initial setting failed: {error}");
            }
            if state.ready
                && state.pending_count == 0
                && performance.snapshot().total_rendered_frames >= 30
            {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("initial real playback timeout")??;

    // Finish at Auto/144. Original immediately precedes Auto so its official
    // remembered automatic sub-tier remains Original, not the preceding Clear.
    let cases = [
        (StreamQuality::Auto, FrameRateChoice::Fps144, None),
        (StreamQuality::Auto, FrameRateChoice::Fps60, None),
        (StreamQuality::Auto, FrameRateChoice::Fps90, None),
        (StreamQuality::Auto, FrameRateChoice::Fps30, None),
        (StreamQuality::High, FrameRateChoice::Fps60, None),
        (StreamQuality::Clear, FrameRateChoice::Fps60, None),
        (StreamQuality::Custom, FrameRateChoice::Fps60, Some(8)),
        (StreamQuality::Custom, FrameRateChoice::Fps60, Some(20)),
        (StreamQuality::Original, FrameRateChoice::Fps60, None),
        (StreamQuality::Auto, FrameRateChoice::Fps144, None),
    ];
    let adaptive_case = [(StreamQuality::Adaptive, FrameRateChoice::Fps144, None)];
    let selected = if adaptive.is_some() {
        &adaptive_case[..]
    } else if steady {
        &cases[..1]
    } else {
        &cases[..]
    };
    for &(quality, frame_rate, bitrate) in selected {
        let mut settings = control.snapshot().settings;
        settings.quality = quality;
        settings.frame_rate = frame_rate;
        if let Some((ceiling, automatic, _)) = adaptive {
            settings.adaptive_ceiling_mbps = ceiling;
            settings.stability_priority = automatic;
        }
        if let Some(bitrate) = bitrate {
            settings.custom_bitrate_mbps = bitrate;
        }
        let requested_at = Instant::now();
        let sequence = control.apply(settings)?;
        println!("setting sequence={sequence} quality={quality:?} fps={frame_rate:?}");
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = control.snapshot();
                if let Some(error) = state.last_error {
                    bail!("setting {sequence} failed: {error}");
                }
                if state.last_applied_sequence == Some(sequence) && state.pending_count == 0 {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("real setting acknowledgment timeout")??;
        let ack_ms = requested_at.elapsed().as_secs_f64() * 1000.0;
        // Observation warmup, never applied to the media processing pipeline.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let before = performance.snapshot();
        let measured_at = Instant::now();
        let mut routes = vec![before.connection.clone()];
        let observation_seconds =
            adaptive.map_or(if steady { 90 } else { 12 }, |(_, _, seconds)| seconds);
        let mut windows = Vec::with_capacity(observation_seconds);
        let mut previous = before.clone();
        for _ in 0..observation_seconds {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let current = performance.snapshot();
            if routes.last() != Some(&current.connection) {
                routes.push(current.connection.clone());
            }
            let span = current.uptime.saturating_sub(previous.uptime).as_secs_f64();
            let budget = control.snapshot().adaptive.map(|budget| {
                json!({
                    "phase":format!("{:?}", budget.phase),"applied_mbps":budget.applied_mbps,
                    "pending_mbps":budget.pending_mbps,"suggested_mbps":budget.suggested_mbps,
                    "recovery_ceiling_mbps":budget.recovery_ceiling_mbps,
                "received_video_rtp_mbps":budget.received_video_rtp_mbps,
                "reference_video_mbps":budget.reference_video_mbps,"reference_rtp_mbps":budget.reference_rtp_mbps,
                })
            });
            windows.push(json!({
                "elapsed_seconds":span,
                "submitted_frames":current.total_rendered_frames.saturating_sub(previous.total_rendered_frames),
                "rtx_received":current.rtx_packets_received.saturating_sub(previous.rtx_packets_received),
                "fec_recovered":current.fec_packets_recovered.saturating_sub(previous.fec_packets_recovered),
                "received_rtp_bytes":current.total_received_rtp_bytes.saturating_sub(previous.total_received_rtp_bytes),
                "budget":budget,
            }));
            previous = current;
        }
        let after = performance.snapshot();
        let wall_seconds = measured_at.elapsed().as_secs_f64();
        // Performance snapshots may be cached for 250 ms. Pair the cumulative
        // counters with their own sample times, not the observer's sleep clock.
        let seconds = after
            .uptime
            .checked_sub(before.uptime)
            .context("performance snapshot clock moved backwards")?
            .as_secs_f64();
        if seconds <= 0.0 {
            bail!("performance snapshot did not advance");
        }
        let frames = after
            .total_rendered_frames
            .saturating_sub(before.total_rendered_frames);
        let current = control.snapshot();
        let record = json!({
            "requested_quality":format!("{quality:?}"),"requested_fps":frame_rate.value(current.local_display),"requested_custom_mbps":bitrate,
            "ack_ms":ack_ms,"notice":current.last_notice,"route_history":routes,
            "elapsed_seconds":seconds,"wall_seconds":wall_seconds,"received_frames":after.total_received_frames.saturating_sub(before.total_received_frames),
            "decoded_frames":after.total_decoded_frames.saturating_sub(before.total_decoded_frames),
            "submitted_frames":frames,"submitted_fps":frames as f64/seconds,
            "mean_received_rtp_mbps":after.total_received_rtp_bytes.saturating_sub(before.total_received_rtp_bytes) as f64 * 8.0 / seconds / 1_000_000.0,
            "actual_quality_label":after.quality,"stream_format":after.video_format,"decoded_resolution":after.decoded_resolution,
            "last_rate_mbps":after.bitrate_mbps,"frm_ms":after.frame_delay_ms,
            "recent_300_source":cadence(after.source_cadence),"recent_300_assembly":cadence(after.receive_cadence),
            "recent_300_submit":cadence(after.render_cadence),
            "recent_300_local_ms":{"average":after.local_frame_delay_average_ms,"p95":after.local_frame_delay_p95_ms},
            "rtx_received":after.rtx_packets_received.saturating_sub(before.rtx_packets_received),
            "fec_recovered":after.fec_packets_recovered.saturating_sub(before.fec_packets_recovered),
            "predecode_drops":after.predecode_dropped_frames.saturating_sub(before.predecode_dropped_frames),
            "presentation_drops":after.dropped_present_frames.saturating_sub(before.dropped_present_frames),
            "pending_nacks":after.outstanding_nacks,"presentation_queue":after.presentation_queue_frames,
            "measurement_windows":windows,
            "clock_e2e_provided":after.pipeline_stats.as_ref().is_some_and(|p|p.e2e.is_some()),
            "source_display_baseline":current.remote_display.map(|d|json!({"width":d.width,"height":d.height,"hz":d.refresh_hz})),
            "switch":after.stream_switch.as_ref().map(|s|json!({"stage":s.stage,"ack_ms":s.request_to_ack_ms,"presentation_ms":s.request_to_present_ms})),
        });
        writeln!(output, "{record}")?;
        output.flush()?;
        println!("{record}");
        if frames == 0 {
            bail!("no fresh presentation during setting {sequence}");
        }
    }
    if adaptive.is_some() {
        // Exercise explicit exit from the opt-in policy on the real channel.
        let mut settings = control.snapshot().settings;
        settings.quality = StreamQuality::Auto;
        let sequence = control.apply(settings)?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let state = control.snapshot();
                if let Some(error) = state.last_error {
                    bail!("exit adaptive failed: {error}");
                }
                if state.last_applied_sequence == Some(sequence) && state.adaptive.is_none() {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("exit adaptive ACK timeout")??;
        println!("Explicit Auto restored and acknowledged; adaptive controller removed.");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let alias = args.next().context("device alias required")?;
    let transport: TransportChoice = args.next().context("transport required")?.parse()?;
    let codec: CodecPreference = args.next().context("codec required")?.parse()?;
    let output = PathBuf::from(args.next().context("new JSONL output path required")?);
    let mode = args.next();
    let (steady, adaptive) = match mode.as_deref() {
        None => (false, None),
        Some("steady" | "screens" | "screen-capture" | "software") => (true, None),
        Some(mode @ ("adaptive" | "advice")) => {
            let cap: u32 = args.next().context("video ceiling required")?.parse()?;
            let seconds: usize = args
                .next()
                .context("observation seconds required")?
                .parse()?;
            if !(1..=500).contains(&cap) || !(10..=600).contains(&seconds) {
                bail!("invalid budget or observation duration");
            }
            (false, Some((cap, mode == "adaptive", seconds)))
        }
        Some(_) => bail!(
            "optional mode must be steady, adaptive <Mbps> <seconds> or advice <Mbps> <seconds>"
        ),
    };
    let log_path = output.with_extension("log");
    let _logging = openuuyc::logging::init(
        if steady {
            "warn,webrtc_srtp=info,openuuyc::nack_audit=debug,openuuyc::controller=info,openuuyc::stream_control=debug,openuuyc::viewer=info,openuuyc::rtc=info"
        } else {
            "warn,openuuyc::controller=info,openuuyc::stream_control=debug,openuuyc::viewer=info,openuuyc::rtc=info"
        },
        &log_path,
    )?;
    let (mut connection, _) = controller::connect_saved_alias(
        &alias,
        ConnectionMediaOptions {
            muted: true,
            frame_rate: FrameRateChoice::Fps144,
            codec,
            hardware_decode: mode.as_deref() != Some("software"),
            transport,
        },
    )
    .await?;
    let control = connection.stream_control_handle();
    let restore_settings: StreamControlSettings =
        match std::env::var("OPENUUYC_AUDIT_RESTORE_SETTINGS") {
            Ok(json) => serde_json::from_str(&json).context("parse audit restoration settings")?,
            Err(_) => control.snapshot().settings,
        };
    let performance = connection.performance_monitor();
    let (viewer, _) = connection.start_native_viewer(&alias).await?;
    let close = viewer.close_handle();
    let cases = tokio::spawn(async move {
        let restore_control = control.clone();
        let result = async {
            if mode.as_deref() == Some("screen-capture") {
                let second = control
                    .snapshot()
                    .screens
                    .into_iter()
                    .find(|screen| screen.video_track_index < 0)
                    .context("no inactive second display to verify")?;
                control.set_screen_capture(second.id, true).await?;
                tokio::time::sleep(Duration::from_secs(3)).await;
                let active = control.snapshot().screens;
                println!("after start: {active:?}");
                let observed = active
                    .iter()
                    .any(|screen| screen.id == second.id && screen.video_track_index >= 0);
                control.set_screen_capture(second.id, false).await?;
                if observed {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "secondary screen has no assigned track after start"
                    ))
                }
            } else if mode.as_deref() == Some("screens") {
                tokio::time::sleep(Duration::from_secs(10)).await;
                println!("screens={:?}", control.snapshot().screens);
                println!(
                    "received={} rendered={}",
                    performance.snapshot().total_received_frames,
                    performance.snapshot().total_rendered_frames
                );
                Ok(())
            } else {
                run_cases(control, performance, output, steady, adaptive).await
            }
        }
        .await;
        // Exercise settings without permanently replacing the user's choices.
        // Also attempt restoration when a case fails before closing playback.
        let restored = async {
            if restore_control.snapshot().settings != restore_settings {
                let sequence = restore_control.apply(restore_settings)?;
                tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        let state = restore_control.snapshot();
                        if let Some(error) = state.last_error {
                            bail!("restore settings failed: {error}");
                        }
                        if state.last_applied_sequence == Some(sequence) && state.pending_count == 0
                        {
                            return Ok::<(), anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .context("restore settings acknowledgement timeout")??;
                println!("Previous viewing settings restored and acknowledged.");
            }
            Ok(())
        }
        .await;
        close.close();
        result.and(restored)
    });
    let playback = controller::run_native_viewer_session(connection, viewer).await;
    if !cases.is_finished() {
        cases.abort();
    }
    let result = cases
        .await
        .context("matrix window closed before verification finished")?;
    playback?;
    result
}
