use openuuyc_h264::{Error, stream::Decoder};
use std::sync::atomic::{AtomicBool, Ordering};

fn aus(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    for i in 0..data.len().saturating_sub(4) {
        if data[i..].starts_with(&[0, 0, 0, 1]) && data[i + 4] & 31 == 9 {
            starts.push(i);
        }
    }
    assert!(!starts.is_empty());
    starts[0] = 0;
    starts.push(data.len());
    starts.windows(2).map(|r| &data[r[0]..r[1]]).collect()
}
fn without_parameters(au: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    for nal in oxideav_h264::nal::AnnexBSplitter::new(au) {
        if !matches!(nal[0] & 31, 7 | 8) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    }
    out
}
fn pixels(output: &openuuyc_h264::stream::Output) -> Vec<u8> {
    let mut p = vec![];
    output.picture.pack_into(&mut p).unwrap();
    p
}

#[test]
fn failed_parameter_change_recovers_committed_idr_and_held_output() {
    let input = aus(include_bytes!("../../fixtures/h264.annexb"));
    let resize = aus(include_bytes!("../../fixtures/h264_resize.annexb"));
    let mut decoder = Decoder::new();
    let mut first = decoder.submit(input[0], 11).unwrap();
    assert_eq!(first.len(), 1);
    let held = first.pop().unwrap();
    let expected = pixels(&held);
    let mut bad = vec![];
    for nal in oxideav_h264::nal::AnnexBSplitter::new(resize[0]) {
        if matches!(nal[0] & 31, 7 | 8) {
            bad.extend_from_slice(&[0, 0, 0, 1]);
            bad.extend_from_slice(nal);
        }
    }
    bad.extend_from_slice(&[0, 0, 0, 1, 0x65]);
    assert!(decoder.submit(&bad, 12).is_err());
    assert!(matches!(
        decoder.submit(input[1], 13),
        Err(Error::NeedKeyframe)
    ));
    let restored = decoder.submit(&without_parameters(input[0]), 14).unwrap();
    assert_eq!(pixels(&restored[0]), expected);
    assert_eq!(restored[0].token, 14);
    decoder.reset().unwrap();
    let resized = decoder.submit(resize[0], 15).unwrap();
    assert_ne!(resized[0].picture.crop.width, held.picture.crop.width);
    decoder.close();
    drop(decoder);
    assert_eq!(pixels(&held), expected);
}

#[test]
fn cancellation_eos_and_close_have_distinct_recovery() {
    let input = aus(include_bytes!("../../fixtures/h264.annexb"));
    let cancel = AtomicBool::new(false);
    let mut decoder = Decoder::new();
    let held = decoder
        .submit_with_cancel(input[0], 1, &cancel)
        .unwrap()
        .pop()
        .unwrap();
    let expected = pixels(&held);
    cancel.store(true, Ordering::Release);
    assert!(matches!(
        decoder.submit_with_cancel(input[1], 2, &cancel),
        Err(Error::Cancelled)
    ));
    decoder.reset().unwrap();
    assert!(cancel.load(Ordering::Acquire));
    assert!(matches!(
        decoder.submit_with_cancel(input[0], 3, &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Release);
    assert!(matches!(
        decoder.submit_with_cancel(input[1], 4, &cancel),
        Err(Error::NeedKeyframe)
    ));
    assert_eq!(
        pixels(
            &decoder
                .submit_with_cancel(&without_parameters(input[0]), 5, &cancel)
                .unwrap()[0]
        ),
        expected
    );
    assert!(decoder.finish().unwrap().is_empty());
    assert!(decoder.finish().unwrap().is_empty());
    assert!(matches!(decoder.submit(input[0], 6), Err(Error::Closed)));
    decoder.reset().unwrap();
    assert_eq!(decoder.submit(input[0], 7).unwrap().len(), 1);
    decoder.close();
    assert!(matches!(decoder.reset(), Err(Error::Closed)));
    assert!(matches!(decoder.submit(input[0], 8), Err(Error::Closed)));
    assert_eq!(pixels(&held), expected);
}

#[test]
fn avcc_seed_and_rejected_seed_do_not_decode_or_poison_a_picture() {
    let input = aus(include_bytes!("../../fixtures/h264.annexb"));
    let nals: Vec<_> = oxideav_h264::nal::AnnexBSplitter::new(input[0]).collect();
    let sps = *nals.iter().find(|n| n[0] & 31 == 7).unwrap();
    let pps = *nals.iter().find(|n| n[0] & 31 == 8).unwrap();
    let mut config = vec![1, sps[1], sps[2], sps[3], 255, 225];
    config.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    config.extend_from_slice(sps);
    config.push(1);
    config.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    config.extend_from_slice(pps);
    let mut seeded = Decoder::new();
    seeded.seed(&config).unwrap();
    let mut bad = config.clone();
    bad.truncate(bad.len() - 2);
    assert!(seeded.seed(&bad).is_err());
    let output = seeded.submit(&without_parameters(input[0]), 42).unwrap();
    let mut direct = Decoder::new();
    let reference = direct.submit(input[0], 99).unwrap();
    assert_eq!(pixels(&output[0]), pixels(&reference[0]));
    assert_eq!(output[0].token, 42);
}
