use super::*;
use bytes::Bytes;
use webrtc::rtp::header::Header;
use webrtc::util::marshal::Marshal;

fn receiver(extension_id: u8) -> OfficialVideoReceiver {
    OfficialVideoReceiver::new(
        "video/H264",
        VideoHeaderExtensions {
            orientation: None,
            content_type: None,
            capture_index: None,
            is_new_picture: Some(extension_id),
            timing: None,
            sending_delay: None,
            color_space: None,
        },
        "",
    )
    .unwrap()
}

fn packet(id: u8, flag: &[u8], sequence: u16, timestamp: u32, payload: &[u8]) -> RtpPacket {
    let mut header = Header {
        version: 2,
        payload_type: 98,
        sequence_number: sequence,
        timestamp,
        marker: true,
        extension: true,
        extension_profile: if id > 14 { 0x1000 } else { 0xbede },
        ..Default::default()
    };
    header
        .set_extension(id, Bytes::copy_from_slice(flag))
        .unwrap();
    RtpPacket {
        header,
        payload: Bytes::copy_from_slice(payload),
    }
}

#[test]
fn new_picture_metadata_survives_wire_and_rtx_normalization() {
    for id in [14, 15] {
        let mut receiver = receiver(id);
        for (flag, expected) in [
            (&[0][..], Some(false)),
            (&[2][..], Some(true)),
            (&[0, 1][..], None),
        ] {
            let mut packet = packet(id, flag, 8, 9_000, &[0x41, 0xe0]);
            packet
                .header
                .set_extension(3, Bytes::from_static(b"video_0"))
                .unwrap();
            let wire = normalize_rtx_source(&packet, Some(2), Some(3), true).unwrap();
            let decoded = RtpPacket::unmarshal(&mut wire.as_ref()).unwrap();
            assert_eq!(decoded.header.get_extension(2).unwrap(), b"video_0"[..]);
            let parsed = receiver
                .parse_video_packet(&decoded, Instant::now(), VideoCodecKind::H264)
                .unwrap();
            assert_eq!(parsed.is_new_picture, expected);
        }
    }
}

#[test]
fn assembly_uses_first_present_marker_in_sequence_order_without_inheritance() {
    let mut receiver = receiver(15);
    let mut buffer = PacketBuffer::new();
    let mut start = packet(15, &[0], 10, 9_000, &[0x7c, 0x81, 0xe0]);
    start.header.marker = false;
    let end = packet(15, &[1], 11, 9_000, &[0x7c, 0x41, 0]);
    // Arrival order must not let the tail's true override the head's false.
    for packet in [&end, &start] {
        let wire = packet.marshal().unwrap();
        let packet = RtpPacket::unmarshal(&mut wire.as_ref()).unwrap();
        let parsed = receiver
            .parse_video_packet(&packet, Instant::now(), VideoCodecKind::H264)
            .unwrap();
        let result = buffer.insert(parsed);
        if packet.header.sequence_number == 10 {
            assert_eq!(result.frames.len(), 1);
            assert_eq!(result.frames[0].is_new_picture, Some(false));
            assert!(!result.frames[0].data.is_empty());
        } else {
            assert!(result.frames.is_empty());
        }
    }
    let mut next = packet(15, &[1], 12, 12_000, &[0x41, 0xe0]);
    next.header.extension = false;
    next.header.extensions.clear();
    let parsed = receiver
        .parse_video_packet(&next, Instant::now(), VideoCodecKind::H264)
        .unwrap();
    let result = buffer.insert(parsed);
    assert_eq!(result.frames.len(), 1);
    assert_eq!(result.frames[0].is_new_picture, None);
}
