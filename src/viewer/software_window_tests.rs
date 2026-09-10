use super::*;
use crate::media::{CodecPreference, ConnectionMediaProfile, LocalDisplayInfo};

fn first_frame(id: i64) -> EncodedVideoFrame {
    let bytes = include_bytes!("../decoder/fixtures/h264.annexb");
    let mut aud = 0;
    let mut end = bytes.len();
    let mut i = 0;
    while i + 4 < bytes.len() {
        let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if bytes[i + prefix] & 31 == 9 {
            aud += 1;
            if aud == 2 {
                end = i;
                break;
            }
        }
        i += prefix + 1;
    }
    assert_eq!(aud, 2);
    EncodedVideoFrame {
        frame_id: id,
        data: Bytes::copy_from_slice(&bytes[..end]),
        rtp_timestamp: id as u32 * 1500,
        received_at: Instant::now(),
        assembled_at: Instant::now(),
        keyframe: true,
        rotation: 0,
        content_type: 0,
        video_capture_index: None,
        is_new_picture: None,
        sender_timing: FrameSenderTiming::default(),
        codec: VideoCodec::H264,
        parameter_format: None,
        color_space: None,
    }
}

async fn launch() -> NativeViewerSession {
    let performance = PerformanceMonitor::new("software window test");
    let (stream_control, _control, _echo) = StreamControlHandle::new(
        ConnectionMediaProfile {
            muted: true,
            local_display: LocalDisplayInfo::FALLBACK,
            stream_fps: 60,
            decoder_fps_cap: 60,
            codec: CodecPreference::H264,
            hardware_decode: false,
        },
        performance.clone(),
    );
    let (feedback, _feedback) = mpsc::unbounded_channel();
    let session = NativeViewerSession::launch(ViewerLaunchConfig {
        codec: VideoCodec::H264,
        hardware_decode: false,
        title: "software window test".to_owned(),
        initial_width: 256,
        initial_height: 256,
        frame_rate: 60,
        receiver_feedback: feedback,
        performance,
        stream_control,
        display: ViewerDisplayHandle::default(),
    })
    .await
    .unwrap();
    session.frame_wake.visible.store(true, Ordering::Release);
    session
}

fn feed(session: &NativeViewerSession, id: i64) {
    let VideoFrameSink::Unbounded { sender, wake } = session.video_sink();
    sender.send(first_frame(id)).unwrap();
    wake.unpark();
}

async fn displayed(session: &NativeViewerSession) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !mutex_lock(&session.frame_queue).is_empty() {
                return;
            }
            assert!(mutex_lock(&session.fatal_error).is_none());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("software decoder did not produce a frame");
}

#[test]
fn software_window_exclusion_pause_and_resume_real_decoder() {
    let _serial = crate::decoder::software_slot::TEST_SERIAL.lock().unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut first = launch().await;
        feed(&first, 1);
        tokio::time::timeout(Duration::from_secs(5), first.startup())
            .await
            .unwrap()
            .unwrap();
        displayed(&first).await;

        let mut blocked = launch().await;
        feed(&blocked, 1);
        let error = tokio::time::timeout(Duration::from_secs(5), blocked.startup())
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("整个客户端最多允许一个"),
            "{error:#}"
        );
        drop(blocked);

        tokio::time::timeout(Duration::from_secs(5), first.pause_software())
            .await
            .unwrap()
            .unwrap();
        assert!(mutex_lock(&first.frame_queue).is_empty());
        let mut second = launch().await;
        feed(&second, 1);
        tokio::time::timeout(Duration::from_secs(5), second.startup())
            .await
            .unwrap()
            .unwrap();
        displayed(&second).await;
        // RTP arriving for the hidden screen must not reacquire or decode.
        feed(&first, 2);
        drop(second);

        first.resume_decode();
        feed(&first, 3);
        displayed(&first).await;
        // Repeated immediate resume/pause must wait for the new stop request,
        // even if the worker has not yet consumed the preceding resume.
        for _ in 0..16 {
            first.resume_decode();
            tokio::time::timeout(Duration::from_secs(5), first.pause_software())
                .await
                .unwrap()
                .unwrap();
            drop(crate::decoder::software_slot::SoftwareSlot::acquire().unwrap());
        }
        drop(first);
    });
}
