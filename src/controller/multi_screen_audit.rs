//! Opt-in real UU service/decoder check; never runs with the local regressions.
use super::*;
use crate::media::{CodecPreference, FrameRateChoice, TransportChoice};
use crate::stream_control::{StreamControlSettings, StreamQuality};

async fn applied(control: &StreamControlHandle, settings: StreamControlSettings) -> Result<()> {
    let sequence = control.apply(settings)?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = control.snapshot();
            if let Some(error) = snapshot.last_error {
                bail!("{error}");
            }
            if snapshot.last_applied_sequence == Some(sequence) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .context("real shared capture-setting ACK timeout")?
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires saved login and explicitly selected LAN multi-monitor device"]
async fn real_multi_screen_settings() -> Result<()> {
    let alias = std::env::var("OPENUUYC_MULTI_SCREEN_AUDIT_DEVICE")
        .context("select an authorized device with OPENUUYC_MULTI_SCREEN_AUDIT_DEVICE")?;
    let _logging = crate::logging::init(
        "warn,openuuyc::stream_control=debug,openuuyc::viewer=info,openuuyc::rtc=info",
        std::path::Path::new("logs/multi-screen-settings-audit.log"),
    )?;
    let (mut connection, _) = connect_saved_alias(
        &alias,
        ConnectionMediaOptions {
            muted: true,
            frame_rate: FrameRateChoice::Fps144,
            codec: CodecPreference::Auto,
            hardware_decode: true,
            transport: TransportChoice::Auto,
        },
    )
    .await?;
    // Exercise real control/decoder paths without saving test choices to keyring.
    if let Some(mut writer) = connection.preference_writer.take() {
        writer.finish().await;
    }
    let control = connection.stream_control_handle();
    let initial = control.snapshot().settings;
    let result = async {
        let (mut first, _) = connection.start_native_viewer(&alias).await?;
        let factory = first.take_screen_playback().context("screen owner missing")?;
        let first = Arc::new(first);
        factory.register(Arc::clone(&first));
        let run = async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !connection.performance_monitor().snapshot().connection.ends_with(" LAN") {
                bail!("this high-quality comparison requires the selected LAN setup");
            }
            let first_screen = control.snapshot().remote_display.context("primary screen missing")?;
            let second_screen = factory.screens().into_iter().find(|screen| screen.id != first_screen.screen_id)
                .context("second screen missing")?;
            factory.set_visible(vec![first_screen.screen_id, second_screen.id]);
            let (open, second) = factory.open(second_screen.id, ViewerDisplayHandle::default(), None, None);
            let second = second.await.context("secondary decoder task stopped")??;
            open.await.context("secondary open task panicked")?;
            let tracks = [first_screen.screen_id, second_screen.id].map(|id| {
                control.snapshot().screens.into_iter().find(|screen| screen.id == id)
                    .and_then(|screen| connection.peer.video_tracks().get(screen.video_track_index))
                    .context("screen-to-track mapping missing")
            });
            let [first_track, second_track] = tracks;
            let tracks = [first_track?, second_track?];
            for (quality, bitrate, fps) in [
                (StreamQuality::Custom, 8, FrameRateChoice::Fps30),
                (StreamQuality::Custom, 20, FrameRateChoice::Fps60),
                (StreamQuality::Clear, 20, FrameRateChoice::Fps60),
                (StreamQuality::Original, 20, FrameRateChoice::Fps144),
                (StreamQuality::Auto, 20, FrameRateChoice::Fps144),
            ] {
                let mut settings = initial;
                settings.quality = quality;
                settings.custom_bitrate_mbps = bitrate;
                settings.frame_rate = fps;
                applied(&control, settings).await?;
                let before = tracks.each_ref().map(|track| track.performance.snapshot().total_decoded_frames);
                for track in &tracks { connection.peer.request_keyframe(track.metadata.ssrc).await?; }
                tokio::time::sleep(Duration::from_secs(5)).await;
                first.ensure_running()?; second.ensure_running()?;
                for (track, before) in tracks.iter().zip(before) {
                    let stats = track.performance.snapshot();
                    let decoded = stats.total_decoded_frames.saturating_sub(before);
                    println!("quality={quality:?} custom={bitrate} fps={fps:?} track={} decoded_delta={decoded} size={:?} receive_fps={:.1} video_rtp_mbps={:.2} codec={}",
                        track.index, stats.decoded_resolution, stats.receive_fps, stats.bitrate_mbps, stats.video_codec);
                    if decoded == 0 { bail!("track {} stopped decoding after shared settings", track.index); }
                }
            }
            // Hidden screens must inherit the current shared settings when the
            // official server assigns a track again; no local cached config.
            factory.set_visible(vec![first_screen.screen_id]);
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if factory.screens().iter().any(|screen| screen.id == second_screen.id && screen.video_track_index < 0) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }).await.context("hidden screen capture did not stop")?;
            let mut settings = initial;
            settings.quality = StreamQuality::Custom;
            settings.custom_bitrate_mbps = 8;
            settings.frame_rate = FrameRateChoice::Fps30;
            applied(&control, settings).await?;
            let (open, resumed) = factory.open(second_screen.id, ViewerDisplayHandle::default(), None, None);
            let resumed = resumed.await.context("resume task stopped")??;
            open.await.context("resume task panicked")?;
            let mapping = factory.screens().into_iter().find(|screen| screen.id == second_screen.id)
                .context("resumed screen missing")?;
            let track = connection.peer.video_tracks().get(mapping.video_track_index)
                .context("resumed track missing")?;
            let before = track.performance.snapshot().total_decoded_frames;
            tokio::time::sleep(Duration::from_secs(5)).await;
            resumed.ensure_running()?;
            let stats = track.performance.snapshot();
            println!("resumed screen={} track={} decoded_delta={} receive_fps={:.1} size={:?}",
                second_screen.id, track.index, stats.total_decoded_frames.saturating_sub(before), stats.receive_fps, stats.decoded_resolution);
            if stats.total_decoded_frames <= before || stats.receive_fps > 35.0 {
                bail!("resumed screen did not produce frames under the current 30 FPS setting");
            }
            let audio = control.audio().snapshot();
            println!("audio output_samples={} concealed_samples={} callbacks={} peak={:.5} device={} error={:?}",
                audio.output_samples, audio.concealed_samples, audio.output_callbacks, audio.peak, audio.device, audio.error);
            if audio.error.is_some() || audio.output_samples == 0 || audio.output_callbacks == 0 {
                bail!("real native audio path did not decode and render");
            }
            Ok(())
        }.await;
        let restore = applied(&control, initial).await;
        factory.close_decoders();
        run.and(restore)
    }.await;
    let closed = connection.close().await;
    result.and(closed)
}
