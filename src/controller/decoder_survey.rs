//! Explicitly selected real-device AU capture for offline decoder comparisons.
use super::*;
use crate::media::{CodecPreference, FrameRateChoice, TransportChoice};
use std::{io::Write, path::PathBuf, time::Instant};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an explicitly authorized device and fresh local output directory"]
async fn capture_real_video() -> Result<()> {
    let alias = std::env::var("OPENUUYC_SURVEY_DEVICE").context("select the authorized device")?;
    let (preference, expected_codec, hardware_decode) = match std::env::var("OPENUUYC_SURVEY_CODEC")
        .as_deref()
        .unwrap_or("h264")
    {
        "h264" => (CodecPreference::H264, VideoCodec::H264, false),
        "h264-dxva" => (CodecPreference::H264, VideoCodec::H264, true),
        "hevc" => (CodecPreference::H265, VideoCodec::H265, true),
        _ => bail!("survey codec must be h264, h264-dxva or hevc"),
    };
    let dir = PathBuf::from(
        std::env::var_os("OPENUUYC_SURVEY_OUTPUT")
            .context("select fresh capture output directory")?,
    );
    std::fs::create_dir_all(&dir)?;
    let mut video = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("real.annexb"))?,
    );
    let mut metadata = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("frames.jsonl"))?,
    );
    let _logging = crate::logging::init(
        "warn,openuuyc::viewer=info,openuuyc::decoder=info",
        &dir.join("capture.log"),
    )?;
    let (mut connection, summary) = connect_saved_alias(
        &alias,
        ConnectionMediaOptions {
            muted: true,
            frame_rate: FrameRateChoice::Fps60,
            codec: preference,
            hardware_decode,
            transport: TransportChoice::Auto,
        },
    )
    .await?;
    if let Some(mut writer) = connection.preference_writer.take() {
        writer.finish().await;
    }
    let result=async {
        ensure_alias(&alias,&summary.alias)?;
        let (selected,codec)=connection.select_video_track().await?;
        if codec!=expected_codec{bail!("remote did not negotiate the selected codec")}
        let track_index=selected.id.strip_prefix("video_").and_then(|v|v.parse::<u64>().ok()).context("invalid video track id")?;
        let performance=connection.performance_monitor().for_video_track(track_index);
        let mut viewer=NativeViewerSession::launch(ViewerLaunchConfig {
            codec,hardware_decode,title:alias.clone(),
            initial_width:connection.profile.local_display.width,
            initial_height:connection.profile.local_display.height,
            frame_rate:connection.profile.stream_fps,
            receiver_feedback:connection.forwarder.video_receiver_feedback(),
            performance:performance.clone(),
            stream_control:connection.stream_control_handle(),
            display:ViewerDisplayHandle::default(),
        }).await?;
        let (sender,mut receiver)=tokio::sync::mpsc::unbounded_channel();
        connection.forwarder.add_video_sink(crate::rtc::VideoFrameSink::Unbounded{sender,wake:std::thread::current()}).await;
        connection.forwarder.add_video_sink(viewer.video_sink()).await;
        // Both consumers are attached before reception starts, so the observer
        // receives the same initial IDR as the real baseline decoder.
        connection.forwarder.start();
        connection.peer.request_keyframe(selected.ssrc).await?;
        tokio::time::timeout(Duration::from_secs(15),viewer.startup()).await.context("baseline decoder startup timed out")??;
        let captured=tokio::time::timeout(Duration::from_secs(30),async {
            let mut count=0;let mut started=None;let origin=Instant::now();
            while count<600 {
                let frame=receiver.recv().await.context("capture video channel closed")?;
                if started.is_none() {if !frame.keyframe{continue}started=Some(Instant::now());}
                if frame.codec!=expected_codec{bail!("unexpected negotiated video codec")}
                video.write_all(&frame.data)?;
                let format=frame.parameter_format.or_else(||crate::video_format::parse_annex_b_format(frame.codec,&frame.data));
                writeln!(metadata,"{}",serde_json::json!({"index":count,"bytes":frame.data.len(),"rtp_timestamp":frame.rtp_timestamp,"keyframe":frame.keyframe,"is_new_picture":frame.is_new_picture,"received_us":frame.received_at.saturating_duration_since(origin).as_micros(),"assembled_us":frame.assembled_at.saturating_duration_since(origin).as_micros(),"width":format.map(|f|f.visible_width),"height":format.map(|f|f.visible_height),"chroma":format.map(|f|f.chroma_format_idc),"depth":format.map(|f|f.bit_depth_luma)}))?;
                count+=1;viewer.ensure_running()?;
            }
            video.flush()?;metadata.flush()?;
            println!("captured {count} real {codec:?} AUs in {:?}; device={} track={} decoder={}",started.unwrap().elapsed(),summary.alias,selected.id,performance.snapshot().decoder);
            Ok::<_,anyhow::Error>(())
        }).await.context("real AU capture timed out")?;
        viewer.close_handle().close();
        drop(viewer);
        captured
    }.await;
    let closed = connection.close().await;
    result.and(closed)
}
fn ensure_alias(selected: &str, resolved: &str) -> Result<()> {
    if selected != resolved {
        bail!("resolved device differs from the explicitly selected alias")
    };
    Ok(())
}
