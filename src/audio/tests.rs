use super::*;
use opusic_c::Channels;

#[test]
fn output_meter_is_shared_decays_and_clears_on_mute_or_source_change() {
    let audio = AudioPlayback::new();
    let second_window = audio.clone();
    let meter = &audio.0.shared.output_level;
    meter.record([0.5, 0.125]);
    let sampled_at = (meter.sample.load(Ordering::Relaxed) >> 32) as u32;
    let initial = meter.levels_at(sampled_at);
    assert!((initial[0] - 0.5).abs() < 0.0001);
    assert!((initial[1] - 0.125).abs() < 0.0001);
    assert_eq!(meter.levels_at(sampled_at), initial); // Reads do not consume peaks.
    assert!(meter.levels_at(sampled_at + 100)[0] < initial[0]);
    assert_eq!(meter.levels_at(sampled_at + 251), [0.0; 2]); // Output has stopped.
    assert!(second_window.output_levels()[0] > 0.0);
    audio.set_settings(AudioSettings {
        volume: 100,
        muted: true,
    });
    assert_eq!(second_window.output_levels(), [0.0; 2]);
    audio.set_settings(AudioSettings {
        volume: 100,
        muted: false,
    });
    assert_eq!(second_window.output_levels(), [0.0; 2]); // No stale peak on unmute.
    meter.record([0.0, 0.4]);
    assert_eq!(second_window.output_levels()[0], 0.0);
    assert!(second_window.output_levels()[1] > 0.0);
    audio.select_source("audio/opus", RATE, 2).unwrap();
    assert_eq!(second_window.output_levels(), [0.0; 2]);
}

pub(crate) fn tone_packets(count: usize) -> Vec<Bytes> {
    tone_packets_with_samples(count, 960)
}

pub(crate) fn tone_packets_with_samples(count: usize, packet_samples: usize) -> Vec<Bytes> {
    let mut encoder = opusic_c::Encoder::new(
        Channels::Stereo,
        SampleRate::Hz48000,
        opusic_c::Application::Audio,
    )
    .unwrap();
    encoder.set_inband_fec(opusic_c::InbandFec::Mode1).unwrap();
    encoder.set_packet_loss(10).unwrap();
    let mut encoded = vec![0; MAX_PACKET];
    (0..count)
        .map(|packet| {
            let input = (0..packet_samples)
                .flat_map(|sample| {
                    let time = (packet * packet_samples + sample) as f32 / RATE as f32;
                    [
                        0.2 * (std::f32::consts::TAU * 440.0 * time).sin(),
                        0.1 * (std::f32::consts::TAU * 880.0 * time).sin(),
                    ]
                })
                .collect::<Vec<_>>();
            let size = encoder.encode_float_to_slice(&input, &mut encoded).unwrap();
            Bytes::copy_from_slice(&encoded[..size])
        })
        .collect()
}

#[test]
fn opus_playout_waveform_remains_continuous_with_variable_arrivals() -> Result<()> {
    let packets = tone_packets(1_100);
    for scenario in 0..5 {
        let audio = AudioPlayback::new();
        let generation = audio.select_source("audio/opus", RATE, 2).unwrap();
        audio.0.shared.started.store(true, Ordering::Release);
        let mut engine = Engine::new(Arc::clone(&audio.0.shared))?;
        let mut next = 0;
        let mut previous = 0.0_f32;
        let mut discontinuities = 0;
        let mut max_step = 0.0_f32;
        for tick in 0..2_000_u64 {
            let now = tick * 10_000;
            while next < packets.len() {
                let interval = match scenario {
                    2 => 20_020,
                    3 => 19_980,
                    _ => 20_000,
                };
                let arrival = next as u64 * interval
                    + if scenario == 1 {
                        // A delay change and recovery, without losing any packet.
                        (if next / 200 % 3 == 1 { 30_000 } else { 0 })
                            + [0, 6_000, 12_000, 3_000][next % 4]
                    } else {
                        0
                    };
                if arrival > now {
                    break;
                }
                if scenario != 4 || !(500..550).contains(&next) {
                    audio.receive(
                        generation,
                        packets[next].clone(),
                        12_000 + next as u32 * 960,
                        next as u16,
                    );
                }
                next += 1;
            }
            let mut block = [0.0; BLOCK * 2];
            engine.block(&mut block)?;
            for pair in block.chunks_exact(2) {
                let step = (pair[0] - previous).abs();
                if tick > 100 {
                    max_step = max_step.max(step);
                    if step > 0.06 {
                        discontinuities += 1;
                    }
                }
                previous = pair[0];
            }
        }
        eprintln!(
            "scenario={scenario} discontinuities={discontinuities} max_step={max_step} output={} concealed={} neteq={:?}",
            audio.snapshot().output_samples,
            audio.snapshot().concealed_samples,
            engine.stats,
        );
        ensure!(
            discontinuities == 0,
            "continuous Opus tone contains abrupt waveform steps"
        );
        ensure!(
            engine.stats.buffer_ms < 200,
            "playout did not shed excess delay after recovery"
        );
    }
    Ok(())
}

#[test]
fn opus_clock_reorder_loss_and_resampling() -> Result<()> {
    for packet_samples in [BLOCK, BLOCK * 2] {
        let blocks_per_packet = packet_samples / BLOCK;
        let packets = tone_packets_with_samples(200 / blocks_per_packet, packet_samples);
        for rate in [44_100, 48_000, 96_000] {
            let audio = AudioPlayback::new();
            let generation = audio.select_source("audio/opus", RATE, 2).unwrap();
            audio.0.shared.started.store(true, Ordering::Release);
            let mut renderer = Renderer::new(Arc::clone(&audio.0.shared), rate)?;
            let send = |index: usize| {
                audio.receive(
                    generation,
                    packets[index].clone(),
                    (u32::MAX - 4_799).wrapping_add(index as u32 * packet_samples as u32),
                    (u16::MAX - 7).wrapping_add(index as u16),
                )
            };
            let mut energy = [0.0_f64; 2];
            // Two packets arrive as a reversed pair; duplicates must not replay.
            // One packet is lost at each of two points, then normal traffic resumes.
            for block in 0..200 {
                if block % (2 * blocks_per_packet) == 0 {
                    let packet = block / blocks_per_packet;
                    send(packet + 1);
                    if packet != 20 && packet != 54 {
                        send(packet);
                        send(packet);
                    }
                }
                for _ in 0..rate / 100 {
                    let pair = renderer.next()?;
                    for (channel, sample) in pair.into_iter().enumerate() {
                        ensure!(
                            sample.is_finite() && sample.abs() < 1.0,
                            "invalid decoded sample"
                        );
                        energy[channel] += f64::from(sample).powi(2);
                    }
                }
            }
            let stats = audio.snapshot();
            ensure!(
                stats.output_samples > 80_000 && stats.output_samples <= 96_000,
                "duplicate/lost sample accounting: {}",
                stats.output_samples
            );
            ensure!(
                stats.concealed_samples > 0,
                "missing packets did not exercise concealment"
            );
            ensure!(
                energy[0] > 100.0 && energy[1] > 10.0 && energy[0] > energy[1] * 2.0,
                "stereo waveform lost or channels mixed: {energy:?}"
            );
            // Replacing the RTP source must discard every old packet and PCM tail.
            let next = audio.select_source("audio/opus", RATE, 2).unwrap();
            audio.receive(generation, packets[0].clone(), 12, 12);
            audio.receive(next, packets[0].clone(), 72, 1);
            let mut block = [0.0; BLOCK * 2];
            renderer.engine.block(&mut block)?;
            ensure!(
                renderer.engine.generation == next && renderer.engine.seen.len() == 1,
                "old source survived replacement"
            );
        }
    }
    Ok(())
}
