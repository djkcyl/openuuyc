//! Local receive/close contract regressions, not a mock UU service or proof of
//! WAN compatibility. UDP/DTLS/SRTP exchanges use real loopback sockets.
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use webrtc::dtls::{cipher_suite::CipherSuiteId, config::Config as DtlsConfig, conn::DTLSConn};
use webrtc::mux::{Config as MuxConfig, Mux, mux_func::match_all};
use webrtc::srtp::{
    config::Config, config::SessionKeys, protection_profile::ProtectionProfile, session::Session,
};
use webrtc::util::{self, Conn, marshal::Marshal};

const LIMIT: Duration = Duration::from_secs(3);

// Inject the two packet-consumption errors exposed by AgentConn and DTLSConn,
// then a terminal error. Nothing here impersonates UU authentication or media.
type TimedPacket = (Vec<u8>, Instant);

struct PacketInput {
    packets: Mutex<mpsc::Receiver<util::Result<TimedPacket>>>,
    closes: AtomicUsize,
}

#[async_trait]
impl Conn for PacketInput {
    async fn connect(&self, _: SocketAddr) -> util::Result<()> {
        unreachable!()
    }
    async fn recv(&self, buf: &mut [u8]) -> util::Result<usize> {
        Ok(self.recv_with_timestamp(buf).await?.0)
    }
    async fn recv_with_timestamp(&self, buf: &mut [u8]) -> util::Result<(usize, Instant)> {
        let (bytes, received_at) = self
            .packets
            .lock()
            .await
            .recv()
            .await
            .ok_or(util::Error::ErrBufferClosed)??;
        let copied = bytes.len().min(buf.len());
        buf[..copied].copy_from_slice(&bytes[..copied]);
        if copied != bytes.len() {
            return Err(util::Error::ErrBufferShort);
        }
        Ok((copied, received_at))
    }
    async fn recv_from(&self, _: &mut [u8]) -> util::Result<(usize, SocketAddr)> {
        unreachable!()
    }
    async fn send(&self, bytes: &[u8]) -> util::Result<usize> {
        Ok(bytes.len())
    }
    async fn send_to(&self, _: &[u8], _: SocketAddr) -> util::Result<usize> {
        unreachable!()
    }
    fn local_addr(&self) -> util::Result<SocketAddr> {
        Ok("127.0.0.1:0".parse().unwrap())
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }
    async fn close(&self) -> util::Result<()> {
        self.closes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

#[tokio::test]
async fn mux_discards_failed_reads_and_closes_only_owned_endpoints() -> anyhow::Result<()> {
    timeout(LIMIT, async {
        let (tx, rx) = mpsc::channel(8);
        let input = Arc::new(PacketInput {
            packets: Mutex::new(rx),
            closes: AtomicUsize::new(0),
        });
        let mut mux = Mux::new(MuxConfig {
            conn: input.clone(),
            buffer_size: 4,
        });
        let mut cloned = mux.clone();
        let one = mux.new_endpoint(Box::new(|b| b.first() == Some(&1))).await;
        let two = mux.new_endpoint(Box::new(|b| b.first() == Some(&2))).await;
        let mut bytes = [0; 8];
        tx.send(Ok((vec![1, 7], Instant::now()))).await?;
        let size = one.recv(&mut bytes).await?;
        assert_eq!(&bytes[..size], &[1, 7]);
        tx.send(Ok((vec![1, 99, 99, 99, 99], Instant::now())))
            .await?; // consumed but truncated
        tx.send(Err(util::Error::from_std(
            webrtc::dtls::Error::ErrBufferTooSmall,
        )))
        .await?;
        tx.send(Ok((vec![1, 8], Instant::now()))).await?;
        let size = one.recv(&mut bytes).await?;
        assert_eq!(&bytes[..size], &[1, 8]); // neither stale nor partial bytes

        let as_conn: Arc<dyn Conn + Send + Sync> = one.clone();
        as_conn.close().await?;
        assert_eq!(input.closes.load(Ordering::Relaxed), 0);
        assert!(as_conn.send(&[1]).await.is_err());
        tx.send(Ok((vec![1, 9], Instant::now()))).await?;
        tx.send(Ok((vec![2, 10], Instant::now()))).await?;
        let size = two.recv(&mut bytes).await?;
        assert_eq!(&bytes[..size], &[2, 10]);

        drop(tx); // terminal read, with the previous packet still in buf
        assert_eq!(
            two.recv(&mut bytes).await,
            Err(util::Error::ErrBufferClosed)
        );
        cloned.close().await;
        mux.close().await;
        let late = mux.new_endpoint(Box::new(match_all)).await;
        assert_eq!(
            late.recv(&mut bytes).await,
            Err(util::Error::ErrBufferClosed)
        );
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

async fn udp_pair() -> anyhow::Result<(Arc<UdpSocket>, Arc<UdpSocket>)> {
    let a = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let b = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    a.connect(b.local_addr()?).await?;
    b.connect(a.local_addr()?).await?;
    Ok((a, b))
}

fn srtp_config() -> Config {
    Config {
        profile: ProtectionProfile::Aes128CmHmacSha1_80,
        keys: SessionKeys {
            local_master_key: vec![0x31; 16],
            remote_master_key: vec![0x31; 16],
            local_master_salt: vec![0x42; 14],
            remote_master_salt: vec![0x42; 14],
        },
        ..Default::default()
    }
}

fn rtp_packet(ssrc: u32, sequence_number: u16) -> Bytes {
    webrtc::rtp::packet::Packet {
        header: webrtc::rtp::header::Header {
            version: 2,
            payload_type: 96,
            ssrc,
            sequence_number,
            timestamp: u32::from(sequence_number) * 750,
            ..Default::default()
        },
        payload: Bytes::from_static(b"receive lifecycle"),
    }
    .marshal()
    .unwrap()
}

#[tokio::test]
async fn arrival_metadata_survives_queued_short_reads_and_srtp_demux() -> anyhow::Result<()> {
    timeout(LIMIT, async {
        let first = Instant::now() - Duration::from_millis(30);
        let second = first + Duration::from_millis(8);
        let third = second + Duration::from_millis(8);
        let buffer = util::Buffer::new(0, 4096);
        // Grow and wrap the existing byte ring, mix legacy/timestamped writes,
        // and consume a truncated packet without shifting the next timestamp.
        buffer
            .write_with_timestamp(&vec![1; 1500], Some(first))
            .await?;
        buffer
            .write_with_timestamp(&vec![2; 700], Some(second))
            .await?;
        let mut short = [0; 8];
        assert_eq!(
            buffer.read_with_timestamp(&mut short, None).await,
            Err(util::Error::ErrBufferShort)
        );
        buffer.write(b"untimed").await?;
        buffer
            .write_with_timestamp(&vec![3; 2100], Some(third))
            .await?;
        let mut bytes = [0; 4096];
        assert_eq!(
            buffer.read_with_timestamp(&mut bytes, None).await?,
            (700, Some(second))
        );
        assert!(bytes[..700].iter().all(|&b| b == 2));
        assert_eq!(
            buffer.read_with_timestamp(&mut bytes, None).await?,
            (7, None)
        );
        buffer.close().await;
        assert_eq!(
            buffer.read_with_timestamp(&mut bytes, None).await?,
            (2100, Some(third))
        );
        assert!(bytes[..2100].iter().all(|&b| b == 3));
        assert_eq!(
            buffer.read_with_timestamp(&mut bytes, None).await,
            Err(util::Error::ErrBufferClosed)
        );

        let (a, b) = udp_pair().await?;
        let sender = Session::new(a, srtp_config(), true).await?;
        let (tx, rx) = mpsc::channel(8);
        let mut mux = Mux::new(MuxConfig {
            conn: Arc::new(PacketInput {
                packets: Mutex::new(rx),
                closes: AtomicUsize::new(0),
            }),
            buffer_size: 8192,
        });
        let receiver = Session::new(
            mux.new_endpoint(Box::new(match_all)).await,
            srtp_config(),
            true,
        )
        .await?;
        let mut ordered = receiver.subscribe_incoming_rtp().unwrap();
        tx.send(Ok((vec![0; 8193], first))).await?;
        for (sequence, arrival) in [(1, second), (2, third)] {
            let packet = rtp_packet(42, sequence);
            sender.write(&packet, true).await?;
            let n = b.recv(&mut bytes).await?;
            // Carry an upstream timestamp older than both async consumers.
            tx.send(Ok((bytes[..n].to_vec(), arrival))).await?;
            let event = ordered.recv().await?;
            assert_eq!(event.data, packet);
            assert_eq!(event.received_at, arrival);
            let stream = receiver.open(42).await;
            let (parsed, stream_arrival) = stream.read_rtp_with_arrival(&mut bytes).await?;
            assert_eq!(parsed.header.sequence_number, sequence);
            assert_eq!(stream_arrival, arrival);
        }
        mux.close().await;
        receiver.close().await?;
        sender.close().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn encrypted_streams_survive_bad_packets_and_end_on_transport_close() -> anyhow::Result<()> {
    timeout(LIMIT, async {
        for is_rtp in [true, false] {
            let (a, b) = udp_pair().await?;
            let mut mux = Mux::new(MuxConfig {
                conn: b,
                buffer_size: 8192,
            });
            let endpoint = mux.new_endpoint(Box::new(match_all)).await;
            let sender = Session::new(a.clone(), srtp_config(), is_rtp).await?;
            let receiver = Session::new(endpoint, srtp_config(), is_rtp).await?;
            let mut ordered = receiver.subscribe_incoming_rtp();
            let packet = if is_rtp {
                rtp_packet(42, 1)
            } else {
                webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                    sender_ssrc: 77,
                    media_ssrc: 42,
                }
                .marshal()?
            };
            a.send(&[0x80, 96, 0]).await?; // malformed/authentication failure
            sender.write(&packet, is_rtp).await?;
            let (stream, _) = receiver.accept().await?;
            let mut bytes = [0; 256];
            let size = stream.read(&mut bytes).await?;
            assert_eq!(&bytes[..size], packet.as_ref());
            if let Some(ordered) = &mut ordered {
                assert_eq!(ordered.recv().await?.data, packet);
            }
            // The next valid packet must still work after another malformed one.
            a.send(&[0x80, 96, 1]).await?;
            let next = if is_rtp { rtp_packet(42, 2) } else { packet };
            sender.write(&next, is_rtp).await?;
            let size = stream.read(&mut bytes).await?;
            assert_eq!(&bytes[..size], next.as_ref());
            if let Some(ordered) = &mut ordered {
                assert_eq!(ordered.recv().await?.data, next);
            }

            mux.close().await;
            assert!(matches!(
                stream.read(&mut bytes).await,
                Err(webrtc::srtp::Error::Util(util::Error::ErrBufferClosed))
            ));
            assert!(receiver.accept().await.is_err());
            if let Some(ordered) = &mut ordered {
                assert!(matches!(
                    ordered.recv().await,
                    Err(tokio::sync::broadcast::error::RecvError::Closed)
                ));
            }
            receiver.close().await?;
            receiver.close().await?;
            let late = receiver.open(99).await;
            assert!(late.read(&mut bytes).await.is_err());
            assert!(receiver.write(&next, is_rtp).await.is_err());
            sender.close().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn session_shutdown_does_not_wait_on_full_stream_announcements() -> anyhow::Result<()> {
    timeout(LIMIT, async {
        let (a, b) = udp_pair().await?;
        let mut mux = Mux::new(MuxConfig {
            conn: b,
            buffer_size: 8192,
        });
        let receiver = Session::new(
            mux.new_endpoint(Box::new(match_all)).await,
            srtp_config(),
            true,
        )
        .await?;
        let sender = Session::new(a, srtp_config(), true).await?;
        let mut ordered = receiver.subscribe_incoming_rtp().unwrap();
        let mut streams = Vec::new();
        for ssrc in 1000..1024 {
            streams.push(receiver.open(ssrc).await);
        }
        // No accept consumer: the ninth new stream blocks its bounded announcement.
        for ssrc in 1..=9 {
            sender.write(&rtp_packet(ssrc, 1), true).await?;
        }
        for _ in 0..9 {
            ordered.recv().await?;
        }
        // The owner's transceiver stops precede its session stop. Neither side
        // may wait on a notification the blocked reader cannot currently take.
        for stream in streams {
            stream.close().await?;
        }
        receiver.close().await?;
        while let Ok((stream, _)) = receiver.accept().await {
            let mut bytes = [0; 256];
            while stream.read(&mut bytes).await.is_ok() {}
        }
        assert!(matches!(
            ordered.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Closed)
        ));
        sender.close().await?;
        mux.close().await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn dtls_application_mux_handles_truncation_and_underlying_close() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    timeout(LIMIT, async {
        let (a, b) = udp_pair().await?;
        let mut mux_a = Mux::new(MuxConfig {
            conn: a,
            buffer_size: 8192,
        });
        let mut mux_b = Mux::new(MuxConfig {
            conn: b,
            buffer_size: 8192,
        });
        let config = DtlsConfig {
            psk: Some(Arc::new(|_| Box::pin(async { Ok(vec![0x53; 16]) }))),
            psk_identity_hint: Some(b"local-contract-regression".to_vec()),
            cipher_suites: vec![CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8],
            ..Default::default()
        };
        let endpoint_a = mux_a.new_endpoint(Box::new(match_all)).await;
        let endpoint_b = mux_b.new_endpoint(Box::new(match_all)).await;
        let (a, b) = tokio::try_join!(
            DTLSConn::new(endpoint_a, config.clone(), true, None),
            DTLSConn::new(endpoint_b, config, false, None)
        )?;
        let b = Arc::new(b);
        let mut data_mux = Mux::new(MuxConfig {
            conn: b.clone(),
            buffer_size: 4,
        });
        let application = data_mux.new_endpoint(Box::new(match_all)).await;
        a.write(b"too-long", None).await?;
        a.write(b"ok", None).await?;
        let mut bytes = [0; 16];
        let size = application.recv(&mut bytes).await?;
        assert_eq!(&bytes[..size], b"ok");
        mux_b.close().await;
        assert_eq!(
            application.recv(&mut bytes).await,
            Err(util::Error::ErrBufferClosed)
        );
        data_mux.close().await;
        // Close-notify now fails on the closed endpoint, but local cleanup
        // still executes and a repeated close is harmless.
        let _ = b.close().await;
        b.close().await?;
        a.close().await?;
        mux_a.close().await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
