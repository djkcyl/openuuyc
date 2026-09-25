//! Capture/encoder worker lifecycle; owns driver objects on one thread.
use super::super::Lease;
use super::super::VideoConfig;
use super::super::capture;
use super::super::encoder;
use super::super::lock;
use super::super::output_size;
use super::Published;
use anyhow::Context as _;
use anyhow::Result;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) fn capture_loop(
    mut screen: capture::Screen,
    handle: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    config: Arc<Mutex<VideoConfig>>,
    keyframe: Arc<AtomicBool>,
    sender: mpsc::Sender<encoder::Encoded>,
    started: Instant,
    transport: crate::features::host::transport::Transport,
    negotiated: Arc<crate::features::host::format::Negotiated>,
    publication: tokio::sync::watch::Sender<Published>,
) -> Result<()> {
    let _runtime = encoder::Runtime::new()?;
    let mut desktop = None;
    let mut encoder = None;
    let mut current = None;
    let mut next = Instant::now();
    let mut paced_fps = 0;
    let mut cached = None::<capture::Frame>;
    let mut last_frame = None::<Instant>;
    let mut failed = std::collections::HashSet::new();
    let mut encode_errors = 0u32;
    let mut requested = None;
    let mut automatic = None::<crate::features::host::parameters::AutoQuality>;
    let mut initial_auto = true;
    let mut admission = crate::features::host::parameters::WindowAdmission::default();
    let mut generation = 0;
    let mut encoding_device = None::<(u64, windows::Win32::Graphics::Direct3D11::ID3D11Device)>;
    let mut transfer = None::<crate::features::host::transfer::Transfer>;
    let mut frame_metadata = std::collections::BTreeMap::new();
    let mut last_diagnostics = None;

    while !cancel.is_cancelled() && handle.requested() {
        let mut wanted = *lock(&config);
        if !negotiated.permits_codec(wanted.format.codec) {
            let previous = wanted.revision;
            let chroma = wanted.format.chroma;
            let hdr = wanted.format.hdr();
            negotiated.apply(
                &mut wanted,
                None,
                chroma,
                hdr,
                (screen.width, screen.height),
            )?;
            let mut active = lock(&config);
            if active.revision != previous {
                continue;
            }
            wanted.revision = previous.wrapping_add(1);
            *active = wanted;
        }

        let requested_hdr = wanted.format.hdr();
        if desktop.is_none()
            && connected.load(Ordering::Acquire)
            && transport.media_ready()
            && wanted.sending
            && wanted.capturing
        {
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
        }
        if let Some(desktop) = desktop.as_ref() {
            // T CCDCA0/CF3450 gate HDR on the actual source, not only the
            // requested setting. Keep the request so same-source HDR recovery
            // can re-enable the negotiated format without rewriting user intent.
            negotiated.apply_source(
                &mut wanted,
                (screen.width, screen.height),
                desktop.hdr_available(),
            )?;
        }
        let settings = (
            wanted.revision,
            wanted.quality,
            wanted.auto_quality,
            wanted.bitrate,
            wanted.fps,
        );
        let changed = requested != Some(settings);
        if changed {
            automatic = (wanted.quality == 5).then(|| {
                crate::features::host::parameters::AutoQuality::new(
                    wanted.auto_quality,
                    wanted.maximum_quality,
                )
            });
            initial_auto = true;
            requested = Some(settings);
        }
        let on_source = |codec| {
            negotiated.choices.iter().any(|c| {
                c.capability.adapter == screen.adapter
                    && c.capability.backend != crate::features::host::format::Backend::Software
                    && c.capability.format.chroma == wanted.format.chroma
                    && c.capability.format.depth == wanted.format.depth
                    && c.capability.format.codec == codec
            })
        };
        let skip_cross_hevc = on_source(crate::features::host::format::Codec::H264)
            && !on_source(crate::features::host::format::Codec::H265);
        let candidate = negotiated
            .choices
            .iter()
            .filter(|c| {
                negotiated.permits_codec(c.capability.format.codec)
                    && c.capability.format.chroma == wanted.format.chroma
                    && c.capability.format.depth == wanted.format.depth
                    && (c.capability.format.codec == wanted.format.codec
                        || c.capability.format.codec == crate::features::host::format::Codec::H264)
                    && !(skip_cross_hevc
                        && c.capability.adapter != screen.adapter
                        && c.capability.format.codec == crate::features::host::format::Codec::H265)
                    && !failed.contains(&(
                        c.capability.adapter,
                        c.capability.backend,
                        c.capability.format,
                    ))
            })
            .max_by_key(|c| {
                let maximum = c.maximum_for(wanted.requested_maximum);
                (
                    c.capability.format == wanted.format,
                    c.capability.backend != crate::features::host::format::Backend::Software,
                    c.fps.min(wanted.maximum_fps),
                    c.capability.adapter == screen.render_adapter.unwrap_or(screen.adapter),
                    maximum.0.max(maximum.1),
                )
            })
            .context("本会话的协商编码候选已全部失败")?;
        let hardware =
            candidate.capability.backend != crate::features::host::format::Backend::Software;
        let candidate_id = (
            candidate.capability.adapter,
            candidate.capability.backend,
            candidate.capability.format,
        );
        wanted.format = candidate.capability.format;
        wanted.maximum = candidate.maximum_for(wanted.requested_maximum);
        wanted.fps = wanted.fps.min(candidate.fps).min(screen.fps).max(1);
        let period = Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
        if paced_fps != wanted.fps {
            paced_fps = wanted.fps;
            next = Instant::now();
        }
        let maximum_quality = negotiated.maximum_quality(
            wanted.format,
            (screen.width, screen.height),
            wanted.maximum,
        );
        if let Some(auto) = automatic.as_mut() {
            auto.limit(maximum_quality);
            let (probe, lower, loss) = transport.automatic_rates();
            if wanted.sending
                && wanted.capturing
                && auto
                    .observe(
                        Instant::now(),
                        wanted.fps,
                        last_frame.is_some_and(|at| at.elapsed() < Duration::from_secs(1)),
                        probe,
                        lower,
                        loss,
                    )
                    .is_some()
            {
                initial_auto = false;
            }
            wanted.quality = auto.quality();
        } else if wanted.quality != 6 {
            // The fallback encoder's real size limit also limits the fixed
            // quality budget and QoS label, not only the GPU texture size.
            wanted.quality = wanted.quality.min(maximum_quality);
        }
        transport.quality(automatic.is_some(), wanted.quality, changed);
        transport.fps(wanted.fps);
        let bounds = if automatic.is_some() {
            crate::features::host::parameters::automatic(wanted.quality, wanted.fps, initial_auto)
        } else {
            crate::features::host::parameters::fixed(
                wanted.quality,
                wanted.bitrate,
                (screen.width, screen.height),
                wanted.fps,
            )
        };
        if wanted.sending && wanted.capturing {
            transport.configure(bounds, changed);
        } else {
            transport.pause();
        }
        if !connected.load(Ordering::Acquire)
            || !transport.media_ready()
            || !wanted.sending
            || !wanted.capturing
        {
            if !wanted.capturing {
                if last_diagnostics.take().is_some() {
                    handle.video(None);
                }
                desktop = None;
                encoder = None;
                current = None;
                transfer = None;
                encoding_device = None;
                publication.send_if_modified(|state| {
                    if state.capturing || state.visible {
                        state.capturing = false;
                        state.visible = false;
                        true
                    } else {
                        false
                    }
                });
            }
            cached = None;
            last_frame = None;
            keyframe.store(true, Ordering::Release);
            next = Instant::now();
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if Instant::now() < next {
            std::thread::sleep(
                next.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
            continue;
        }
        if sender.capacity() == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        if desktop.is_none() {
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
        }
        let wait_ms = ((1000.0 / f64::from(wanted.fps)).round() as u32).max(1);
        let capture_started = Instant::now();
        // Advance the planned deadline, not the (usually late) wake-up time.
        // Otherwise sleep/dispatch overhead is added to every frame period.
        // A whole missed period resets the clock instead of replaying a backlog.
        next = if capture_started.saturating_duration_since(next) >= period {
            capture_started + period
        } else {
            next + period
        };
        let captured = desktop.as_mut().context("缺少采集源")?.next(
            wait_ms,
            wanted.quality,
            wanted.cursor_capture,
            wanted.format.hdr(),
            wanted.maximum,
        );
        let captured = captured?;
        let capture_state = desktop.as_ref().context("缺少采集源")?;
        if generation != capture_state.generation {
            generation = capture_state.generation;
            encoder = None;
            cached = None;
            current = None;
            last_frame = None;
            transfer = None;
            keyframe.store(true, Ordering::Release);
        }
        screen = capture_state.screen.clone();
        // A selected source remains attached during temporary capture loss.
        // Removing its track here can make the receiver unsubscribe and prevent
        // the next capture attempt. Visibility has its own permission report.
        publication.send_if_modified(|state| {
            let changed = state.screen != screen || !state.capturing;
            if state.screen != screen {
                state.screen.clone_from(&screen);
            }
            state.capturing = true;
            changed
        });
        handle.source(&screen, capture_state.backend_name());
        if wanted.format.hdr() != (requested_hdr && capture_state.hdr_available()) {
            // Capture recovery or source refresh changed the effective format.
            // Rebuild preprocessing/encoder before admitting this frame.
            cached = None;
            continue;
        }
        if !capture_state.available {
            cached = None;
            publication.send_if_modified(|state| {
                if state.visible {
                    state.visible = false;
                    true
                } else {
                    false
                }
            });
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let frame = match captured {
            Some(frame) => {
                cached = Some(frame.clone());
                frame
            }
            None => {
                let Some(mut frame) = cached.clone() else {
                    continue;
                };
                if last_frame.is_some_and(|at| {
                    at.elapsed() < Duration::from_secs_f64(1.0 / f64::from(wanted.fps.min(30)))
                }) {
                    continue;
                }
                frame.captured = Instant::now();
                frame.is_new = false;
                frame
            }
        };
        let size = output_size(frame.width, frame.height, wanted.quality);
        let source_device = &desktop.as_ref().context("缺少采集源")?.device;
        let device = if !hardware || candidate.capability.adapter == screen.adapter {
            source_device.clone()
        } else {
            if encoding_device
                .as_ref()
                .is_none_or(|(adapter, _)| *adapter != candidate.capability.adapter)
            {
                match capture::create_device(candidate.capability.adapter) {
                    Ok((device, _)) => {
                        encoding_device = Some((candidate.capability.adapter, device))
                    }
                    Err(error) => {
                        tracing::warn!(%error,?candidate_id,"encoder adapter failed");
                        failed.insert(candidate_id);
                        continue;
                    }
                }
            }
            encoding_device.as_ref().unwrap().1.clone()
        };
        if current.is_none_or(|(old_size, old_candidate)| {
            old_size != size || old_candidate != candidate_id
        }) {
            // Release before replacement to avoid consuming a second driver
            // session solely for a size/candidate transition (T C2D840).
            drop(encoder.take());
            let created = if hardware {
                encoder::Encoder::hardware_format(
                    &device,
                    size,
                    wanted.format,
                    crate::features::host::format::Rate {
                        target: bounds.initial.max(300_000),
                        peak: bounds.initial.max(300_000),
                        fps: wanted.fps,
                        quality: wanted.quality,
                    },
                )
            } else {
                encoder::Encoder::software(
                    &device,
                    size.0,
                    size.1,
                    wanted.fps,
                    bounds.initial.max(300_000),
                )
            };
            encoder = match created {
                Ok(created) => {
                    handle.encoder(
                        created.implementation(),
                        created.maximum_size(),
                        &screen,
                        desktop.as_ref().context("缺少采集源")?.backend_name(),
                    );
                    Some(created)
                }
                Err(error) => {
                    tracing::warn!(%error,?candidate_id,"host encoder initialization failed; disabling this candidate");
                    failed.insert(candidate_id);
                    encoder = None;
                    cached = None;
                    current = None;
                    continue;
                }
            };
            current = Some((size, candidate_id));
            frame_metadata.clear();
            encode_errors = 0;
            keyframe.store(true, Ordering::Release);
        }
        let active_encoder = encoder.as_mut().context("缺少编码器")?;
        let peak = transport.media_rate().min(bounds.maximum);
        let codec_minimum = (bounds.minimum / 1000).max(30) * 1000;
        let (media_rate, discard_frame) =
            admission.next(peak, codec_minimum, transport.cwnd_ratio());
        if discard_frame {
            continue;
        }
        let needs_transfer = device != *source_device;
        let prepared = (|| -> Result<Option<crate::features::host::transfer::Delivery>> {
            if !needs_transfer {
                return Ok(None);
            }
            if transfer
                .as_ref()
                .is_none_or(|t| !t.matches(source_device, &device, &frame))
            {
                transfer = Some(crate::features::host::transfer::Transfer::new(
                    source_device,
                    &device,
                    &frame,
                )?);
            }
            transfer.as_mut().unwrap().copy(&frame)
        })();
        if needs_transfer && prepared.as_ref().is_ok_and(|frame| frame.is_none()) {
            continue;
        }
        let encoded = (|| -> Result<_> {
            // Delivery failures share the source-loss/candidate-recovery exits.
            let delivery = prepared?;
            if active_encoder.configure_rate(crate::features::host::format::Rate {
                target: media_rate,
                peak,
                fps: wanted.fps,
                quality: wanted.quality,
            })? {
                keyframe.store(true, Ordering::Release);
            }
            // Network requests are rate-limited at their RTCP entry. Explicit
            // quality/source controls and failed inputs must not wait 600ms.
            let force = keyframe.swap(false, Ordering::AcqRel);
            let encode_started = Instant::now();
            let timestamp = (frame.captured.duration_since(started).as_nanos() / 100) as i64;
            frame_metadata.insert(
                timestamp,
                (
                    frame.captured,
                    encode_started,
                    frame.is_new,
                    frame.hdr_metadata,
                ),
            );
            while frame_metadata.len() > 2048 {
                frame_metadata.pop_first();
            }
            let encoded = active_encoder.encode(
                delivery
                    .as_ref()
                    .map_or(&frame.texture, |d| &d.frame.texture),
                timestamp,
                force,
            )?;
            for output in &encoded {
                if let Some(actual) = crate::media::video_format::parse_annex_b_format(
                    output.format.codec.media(),
                    &output.data,
                ) {
                    anyhow::ensure!(
                        actual.chroma_format_idc == output.format.chroma
                            && actual.bit_depth_luma == output.format.depth
                            && actual.bit_depth_chroma == output.format.depth
                            && (actual.visible_width, actual.visible_height) == size,
                        "编码器实际输出偏离协商格式"
                    );
                }
            }
            Ok((encoded, force, Instant::now()))
        })();
        let (encoded, force, encode_finished) = match encoded {
            Ok(encoded) => {
                encode_errors = 0;
                encoded
            }
            Err(error) => {
                if unsafe { source_device.GetDeviceRemovedReason() }.is_err() {
                    tracing::warn!(%error,"capture graphics device was removed; recreating selected source");
                    encoder = None;
                    current = None;
                    cached = None;
                    desktop = None;
                    transfer = None;
                    encoding_device = None;
                    last_frame = None;
                    publication.send_if_modified(|state| {
                        if state.visible {
                            state.visible = false;
                            true
                        } else {
                            false
                        }
                    });
                    keyframe.store(true, Ordering::Release);
                    continue;
                }
                encode_errors += 1;
                tracing::warn!(%error,hardware,consecutive=encode_errors,"host encoding failed");
                keyframe.store(true, Ordering::Release);
                next = Instant::now() + Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
                if encode_errors >= 10
                    || error.downcast_ref::<encoder::SwitchCandidate>().is_some()
                    || unsafe { device.GetDeviceRemovedReason() }.is_err()
                {
                    // T C2C610: disable the failed candidate. Do not endlessly
                    // reopen that same encoder after its consecutive failures.
                    failed.insert(candidate_id);
                    encoder = None;
                    current = None;
                    cached = None;
                    last_frame = None;
                }
                continue;
            }
        };
        if force && !encoded.iter().any(|frame| frame.keyframe) {
            keyframe.store(true, Ordering::Release);
        }
        last_frame = Some(frame.captured);
        if !encoded.is_empty() {
            let diagnostics = super::super::ActiveEncoding {
                backend: candidate.capability.backend,
                adapter: candidate.capability.adapter,
                format: wanted.format,
                size,
                fps: wanted.fps,
                target_bps: media_rate,
            };
            if last_diagnostics != Some(diagnostics) {
                handle.video(Some(diagnostics));
                last_diagnostics = Some(diagnostics);
            }
            let mut active = lock(&config);
            if active.revision == wanted.revision {
                active.reported_quality = wanted.quality;
            }
            let state = Published {
                screen: screen.clone(),
                capturing: true,
                visible: true,
                quality: wanted.quality,
                fps: wanted.fps,
                encoder: Some(candidate.capability.backend),
                capture: desktop.as_ref().map_or("DXGI", |d| d.backend_name()).into(),
            };
            publication.send_if_modified(|old| {
                if *old != state {
                    *old = state;
                    true
                } else {
                    false
                }
            });
        }
        for mut frame in encoded {
            if let Some((captured, input_started, is_new, metadata)) =
                frame_metadata.remove(&frame.timestamp_100ns)
            {
                frame.is_new = is_new;
                frame.color = frame.format.color(metadata);
                frame.timing = Some(encoder::FrameTiming {
                    captured,
                    encode_started: input_started,
                    encode_finished,
                });
            }
            // No B frames: older metadata cannot belong to a later output.
            while frame_metadata
                .first_key_value()
                .is_some_and(|(&timestamp, _)| timestamp < frame.timestamp_100ns)
            {
                frame_metadata.pop_first();
            }
            sender.blocking_send(frame).context("画面发送队列已关闭")?;
        }
    }
    Ok(())
}
