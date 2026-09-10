//! Wire regression for wrap/duplicate/delta bugs found in the real receive path.
//! This tests our encoder, not a mock UU server or proof of remote acceptance.
use webrtc::interceptor::twcc::Recorder;
use webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc;
use webrtc::util::marshal::Unmarshal;

#[test]
fn transport_feedback_preserves_arrival_across_wrap_late_packets_and_report_split() {
    let mut recorder = Recorder::new(1);
    let period = (1_i64 << 24) * 64_000;
    let origin = period - 1000;
    for (seq, offset) in [(65534, 0), (0, 80), (65535, 40), (0, 9999), (1, 120)] {
        recorder.record(42, seq, origin + offset);
    }
    for seq in 2..3000_u16 {
        recorder.record(42, seq, origin + 120 + i64::from(seq - 1) * 137);
    }
    let reports = recorder.build_feedback_packet();
    assert_eq!(reports.len(), 1);
    let wire = reports[0].marshal().unwrap();
    let feedback = TransportLayerCc::unmarshal(&mut wire.as_ref()).unwrap();
    assert_eq!(feedback.base_sequence_number, 65534);
    assert_eq!(feedback.packet_status_count, 3002);
    assert_eq!(feedback.recv_deltas.len(), 3002);
    let mut reconstructed = i64::from(feedback.reference_time) * 64_000;
    for (index, delta) in feedback.recv_deltas.iter().enumerate() {
        reconstructed += delta.delta;
        let offset = if index < 4 {
            index as i64 * 40
        } else {
            120 + (index as i64 - 3) * 137
        };
        assert!(
            (reconstructed - (origin + offset)).abs() <= 125,
            "index={index}"
        );
    }
    assert!(recorder.build_feedback_packet().is_empty());

    // A delta > i16 ticks starts a new packet with its own base/reference.
    let mut recorder = Recorder::new(1);
    recorder.record(42, 10, 1);
    recorder.record(42, 11, 10_000_001);
    let reports = recorder.build_feedback_packet();
    assert_eq!(reports.len(), 2);
    for (index, report) in reports.iter().enumerate() {
        let wire = report.marshal().unwrap();
        let feedback = TransportLayerCc::unmarshal(&mut wire.as_ref()).unwrap();
        assert_eq!(feedback.base_sequence_number, 10 + index as u16);
        assert_eq!(feedback.packet_status_count, 1);
        let time = i64::from(feedback.reference_time) * 64_000 + feedback.recv_deltas[0].delta;
        assert!((time - (1 + index as i64 * 10_000_000)).abs() <= 125);
    }
    recorder.record(42, 12, 10_100_001);
    assert_eq!(recorder.build_feedback_packet().len(), 1);
}

// Exercise budget updates while a report is pending, then close while the
// writer is blocked. This is the local timer/cancellation contract, not UU BWE.
#[tokio::test]
async fn feedback_budget_update_keeps_deadline_and_cancels_blocked_write() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio::time::{sleep, timeout};
    use webrtc::interceptor::stream_info::{RTPHeaderExtension, StreamInfo};
    use webrtc::interceptor::twcc::receiver::Receiver;
    use webrtc::interceptor::{Attributes, InterceptorBuilder, RTCPWriterFn, RTPReaderFn};

    let (budget, interval) = watch::channel(Duration::from_secs(2));
    let interceptor = Receiver::builder()
        .with_interval_updates(interval)
        .build("budget-regression")?;
    let (reports, mut observed) = mpsc::unbounded_channel();
    let blocked = Arc::new(AtomicBool::new(false));
    let blocked_writer = blocked.clone();
    let _writer = interceptor
        .bind_rtcp_writer(Arc::new(RTCPWriterFn(Box::new(move |packets, _| {
            let sequences = packets
                .iter()
                .filter_map(|p| p.as_any().downcast_ref::<TransportLayerCc>())
                .map(|p| (p.base_sequence_number, p.packet_status_count))
                .collect::<Vec<_>>();
            reports.send(sequences).unwrap();
            let blocked = blocked_writer.load(Ordering::Relaxed);
            Box::pin(async move {
                if blocked {
                    std::future::pending::<()>().await;
                }
                Ok(1)
            })
        }))))
        .await;
    let sequence = Arc::new(AtomicU16::new(0));
    let input = Arc::new(RTPReaderFn(Box::new(move |_, _| {
        let sequence = sequence.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let mut packet = webrtc::rtp::packet::Packet::default();
            packet.header.ssrc = 42;
            packet
                .header
                .set_extension(3, bytes::Bytes::copy_from_slice(&sequence.to_be_bytes()))?;
            let mut attributes = Attributes::new();
            attributes.rtp_received_at = Some(std::time::Instant::now());
            Ok((packet, attributes))
        })
    })));
    let stream = interceptor
        .bind_remote_stream(
            &StreamInfo {
                ssrc: 42,
                rtp_header_extensions: vec![RTPHeaderExtension {
                    id: 3,
                    uri:
                        "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
                            .to_owned(),
                }],
                ..Default::default()
            },
            input,
        )
        .await;
    let mut buf = [0; 128];
    stream.read(&mut buf, &Attributes::new()).await?;
    assert_eq!(
        timeout(Duration::from_secs(3), observed.recv())
            .await?
            .unwrap(),
        vec![(0, 1)]
    );
    sleep(Duration::from_millis(700)).await;
    stream.read(&mut buf, &Attributes::new()).await?;
    // New 500 ms deadline is already due, rather than 500 ms from this update.
    budget.send_replace(Duration::from_millis(500));
    assert_eq!(
        timeout(Duration::from_millis(300), observed.recv())
            .await?
            .unwrap(),
        vec![(1, 1)]
    );
    budget.send_replace(Duration::from_millis(800));
    stream.read(&mut buf, &Attributes::new()).await?;
    assert!(
        timeout(Duration::from_millis(150), observed.recv())
            .await
            .is_err()
    );
    assert_eq!(
        timeout(Duration::from_secs(2), observed.recv())
            .await?
            .unwrap(),
        vec![(2, 1)]
    );
    blocked.store(true, Ordering::Relaxed);
    stream.read(&mut buf, &Attributes::new()).await?;
    assert_eq!(
        timeout(Duration::from_secs(2), observed.recv())
            .await?
            .unwrap(),
        vec![(3, 1)]
    );
    timeout(Duration::from_millis(300), interceptor.close()).await??;
    Ok(())
}
