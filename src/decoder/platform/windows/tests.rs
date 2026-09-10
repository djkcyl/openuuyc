use super::*;
use mediaway_common::{GpuDeviceHandle, NativeHandle, Rational};
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::*,
        Dxgi::Common::{DXGI_FORMAT_P010, DXGI_SAMPLE_DESC},
    },
};
use windows::core::Interface;

const H264: &[u8] = include_bytes!("../../fixtures/h264.annexb");
const HEVC: &[u8] = include_bytes!("../../fixtures/hevc.annexb");
const MAIN10: &[u8] = include_bytes!("../../fixtures/main10.annexb");
const I444: &[u8] = include_bytes!("../../fixtures/h264_444.annexb");

fn access_units(bytes: &[u8], hevc: bool) -> Vec<&[u8]> {
    let mut starts = Vec::new();
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
        let kind = if hevc {
            (bytes[i + prefix] >> 1) & 63
        } else {
            bytes[i + prefix] & 31
        };
        if kind == if hevc { 35 } else { 9 } {
            starts.push(i);
        }
        i += prefix + 1;
    }
    starts.push(bytes.len());
    starts
        .windows(2)
        .map(|pair| &bytes[pair[0]..pair[1]])
        .collect()
}
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14695981039346656037, |state, byte| {
        (state ^ u64::from(*byte)).wrapping_mul(1099511628211)
    })
}
fn device() -> ID3D11Device {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            None,
        )
    }
    .unwrap();
    device.unwrap()
}
fn config(codec: CodecKind, device: Option<&ID3D11Device>) -> VideoDecoderConfig {
    VideoDecoderConfig {
        codec,
        width: 256,
        height: 256,
        time_base: Rational::new(1, 90000),
        pixel_format: PixelFormat::Nv12,
        output: if device.is_some() {
            VideoOutputPreference::ZeroCopyGpu
        } else {
            VideoOutputPreference::CpuFramesOk
        },
        gpu_device: device.map(|device| {
            GpuDeviceHandle::DirectX11(NativeHandle::new(device.as_raw() as usize).unwrap())
        }),
        extra_data: Bytes::new(),
    }
}
fn packet(bytes: &[u8], index: usize) -> Packet {
    Packet {
        stream_id: 0,
        pts: index as i64,
        dts: index as i64,
        duration: 1500,
        is_keyframe: index.is_multiple_of(16),
        is_discard: false,
        payload: Bytes::copy_from_slice(bytes),
    }
}
fn gpu_pixels(frame: &WindowsGpuVideoFrame) -> Vec<u8> {
    unsafe {
        let device = frame.texture().GetDevice().unwrap();
        let context = device.GetImmediateContext().unwrap();
        let sync = frame
            .texture()
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex>()
            .ok();
        if let Some(sync) = &sync {
            sync.AcquireSync(0, 200).unwrap();
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        frame.texture().GetDesc(&raw mut desc);
        let bytes_per_pixel = if desc.Format == DXGI_FORMAT_P010 {
            2
        } else {
            1
        };
        let mut staging = None;
        device
            .CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..desc
                },
                None,
                Some(&raw mut staging),
            )
            .unwrap();
        let staging = staging.unwrap();
        context.CopySubresourceRegion(
            &staging,
            0,
            0,
            0,
            0,
            frame.texture(),
            frame.subresource(),
            None,
        );
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
            .unwrap();
        let pitch = mapped.RowPitch as usize;
        let mut bytes = Vec::new();
        let width = frame.width() as usize * bytes_per_pixel;
        let top = frame.visible_y() as usize;
        let left = frame.visible_x() as usize * bytes_per_pixel;
        for row in 0..frame.height() as usize {
            bytes.extend_from_slice(std::slice::from_raw_parts(
                (mapped.pData as *const u8).add((top + row) * pitch + left),
                width,
            ));
        }
        for row in 0..frame.height() as usize / 2 {
            bytes.extend_from_slice(std::slice::from_raw_parts(
                (mapped.pData as *const u8)
                    .add((desc.Height as usize + top / 2 + row) * pitch + left),
                width,
            ));
        }
        context.Unmap(&staging, 0);
        if let Some(sync) = sync {
            sync.ReleaseSync(0).unwrap();
        }
        bytes
    }
}
fn run(bytes: &[u8], hashes: &str, codec: CodecKind, hardware: bool) {
    let device = hardware.then(device);
    let cfg = config(codec, device.as_ref());
    let mut decoder = WindowsVideoDecoder::open(&cfg).unwrap();
    let units = access_units(bytes, codec == CodecKind::Hevc);
    let expected = hashes
        .lines()
        .map(|line| u64::from_str_radix(line, 16).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(units.len(), expected.len());
    let mut held = None;
    for (index, bytes) in units.iter().enumerate() {
        decoder.push_packet(&packet(bytes, index)).unwrap();
        let output = decoder
            .poll_owned_frame()
            .unwrap()
            .expect("each complete AU must output without a future packet");
        let (token, checksum) = match output {
            WindowsDecodedFrame::Gpu(frame) => {
                let result = (frame.pts(), hash(&gpu_pixels(&frame)));
                if index == 0 {
                    held = Some(frame);
                }
                result
            }
            WindowsDecodedFrame::Cpu(frame) => (frame.pts, hash(&frame.data)),
        };
        assert_eq!(token, index as i64, "output token mismatch");
        assert_eq!(
            checksum, expected[index],
            "visible pixel mismatch for frame {index}"
        );
        assert!(decoder.poll_owned_frame().unwrap().is_none());
    }
    decoder.reset_for_keyframe().unwrap();
    decoder.push_packet(&packet(units[0], 0)).unwrap();
    assert!(decoder.poll_owned_frame().unwrap().is_some());
    drop(decoder);
    if let Some(frame) = held {
        assert_eq!(
            hash(&gpu_pixels(&frame)),
            expected[0],
            "frame lease must survive reset and decoder destruction"
        );
    }
}
#[test]
fn software_h264_multislice_and_references() {
    run(
        H264,
        include_str!("../../fixtures/h264.fnv64"),
        CodecKind::H264,
        false,
    );
}
#[test]
fn software_h264_preserves_444() {
    run(
        I444,
        include_str!("../../fixtures/h264_444.fnv64"),
        CodecKind::H264,
        false,
    );
}
#[test]
#[ignore = "requires a D3D11 video device"]
fn dxva11_h264_multislice_reference_and_lease() {
    run(
        H264,
        include_str!("../../fixtures/h264.fnv64"),
        CodecKind::H264,
        true,
    );
}
#[test]
#[ignore = "requires a D3D11 HEVC video device"]
fn dxva11_hevc_reference_and_lease() {
    run(
        HEVC,
        include_str!("../../fixtures/hevc.fnv64"),
        CodecKind::Hevc,
        true,
    );
}
#[test]
#[ignore = "requires a D3D11 HEVC Main10 video device"]
fn dxva11_main10_preserves_pixels() {
    run(
        MAIN10,
        include_str!("../../fixtures/main10.fnv64"),
        CodecKind::Hevc,
        true,
    );
}

#[test]
#[ignore = "requires a D3D11 H264/HEVC video device"]
fn reset_after_bad_input_and_cancellation() {
    for (codec, data) in [(CodecKind::H264, H264), (CodecKind::Hevc, HEVC)] {
        let device = device();
        let mut decoder = WindowsVideoDecoder::open(&config(codec, Some(&device))).unwrap();
        let units = access_units(data, codec == CodecKind::Hevc);
        decoder.push_packet(&packet(units[0], 0)).unwrap();
        let first = decoder.poll_owned_frame().unwrap().unwrap();
        let invalid = packet(&[0, 0, 1, 0xff, 0xff, 0xff], 1);
        assert!(decoder.push_packet(&invalid).is_err());
        decoder.reset_for_keyframe().unwrap();
        decoder.push_packet(&packet(units[0], 2)).unwrap();
        let second = decoder.poll_owned_frame().unwrap().unwrap();
        let (WindowsDecodedFrame::Gpu(first), WindowsDecodedFrame::Gpu(second)) = (first, second)
        else {
            panic!("hardware output required")
        };
        assert_eq!(hash(&gpu_pixels(&first)), hash(&gpu_pixels(&second)));
        assert_eq!(second.pts(), 2);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        decoder.set_notification(DecoderNotification::new(std::thread::current(), cancel));
        assert_eq!(
            decoder.push_packet(&packet(units[0], 3)),
            Err(DecodeError::Closed)
        );
        drop(decoder);
        assert_eq!(hash(&gpu_pixels(&first)), hash(&gpu_pixels(&second)));
    }
}

#[test]
#[ignore = "requires a D3D11 H264/HEVC video device"]
fn resize_crop_and_return_to_previous_size() {
    let cases: [(CodecKind, &[u8], &[u8], &str); 2] = [
        (
            CodecKind::H264,
            H264,
            include_bytes!("../../fixtures/h264_resize.annexb"),
            include_str!("../../fixtures/h264_resize.fnv64"),
        ),
        (
            CodecKind::Hevc,
            HEVC,
            include_bytes!("../../fixtures/hevc_resize.annexb"),
            include_str!("../../fixtures/hevc_resize.fnv64"),
        ),
    ];
    for (codec, original, changed, expected) in cases {
        let device = device();
        let mut decoder = WindowsVideoDecoder::open(&config(codec, Some(&device))).unwrap();
        let original = access_units(original, codec == CodecKind::Hevc);
        let changed = access_units(changed, codec == CodecKind::Hevc);
        decoder.push_packet(&packet(original[0], 0)).unwrap();
        let WindowsDecodedFrame::Gpu(held) = decoder.poll_owned_frame().unwrap().unwrap() else {
            panic!("GPU frame required")
        };
        let initial_hash = hash(&gpu_pixels(&held));
        decoder.reset_for_keyframe().unwrap();
        decoder.push_packet(&packet(changed[0], 16)).unwrap();
        let WindowsDecodedFrame::Gpu(resized) = decoder.poll_owned_frame().unwrap().unwrap() else {
            panic!("GPU frame required")
        };
        assert_eq!((resized.width(), resized.height()), (640, 360));
        assert_eq!(
            hash(&gpu_pixels(&resized)),
            u64::from_str_radix(expected.lines().next().unwrap(), 16).unwrap()
        );
        decoder.reset_for_keyframe().unwrap();
        decoder.push_packet(&packet(original[0], 32)).unwrap();
        let WindowsDecodedFrame::Gpu(restored) = decoder.poll_owned_frame().unwrap().unwrap()
        else {
            panic!("GPU frame required")
        };
        assert_eq!(hash(&gpu_pixels(&restored)), initial_hash);
        drop(decoder);
        assert_eq!(hash(&gpu_pixels(&held)), initial_hash);
    }
}

#[test]
#[ignore = "requires a D3D11 HEVC Main/Main10 device"]
fn same_size_bit_depth_switch_preserves_held_outputs() {
    let device = device();
    let mut decoder = WindowsVideoDecoder::open(&config(CodecKind::Hevc, Some(&device))).unwrap();
    let mut held = Vec::new();
    for (data, hashes) in [
        (HEVC, include_str!("../../fixtures/hevc.fnv64")),
        (MAIN10, include_str!("../../fixtures/main10.fnv64")),
        (HEVC, include_str!("../../fixtures/hevc.fnv64")),
    ] {
        // In-band IDR and SPS changes must work without an external reset.
        let units = access_units(data, true);
        for (index, (unit, expected)) in units.iter().zip(hashes.lines()).enumerate() {
            decoder.push_packet(&packet(unit, index)).unwrap();
            let WindowsDecodedFrame::Gpu(frame) = decoder.poll_owned_frame().unwrap().unwrap()
            else {
                panic!("GPU output required")
            };
            let expected = u64::from_str_radix(expected, 16).unwrap();
            assert_eq!(hash(&gpu_pixels(&frame)), expected);
            if index == 0 {
                held.push((frame, expected));
            }
        }
    }
    drop(decoder);
    for (frame, expected) in held {
        assert_eq!(hash(&gpu_pixels(&frame)), expected);
    }
}

#[test]
#[ignore = "requires a D3D11 H264 device with texture array support"]
fn array_output_pool_is_acquired_after_reordering() {
    let _array = rust_dxva::dxva::array_surfaces_for_test();
    let device = device();
    let hashes: Vec<u64> = include_str!("../../fixtures/h264.fnv64")
        .lines()
        .map(|s| u64::from_str_radix(s, 16).unwrap())
        .collect();
    for (data, delay) in [
        (
            include_bytes!("../../fixtures/h264_no_vui.annexb").as_slice(),
            16,
        ),
        (
            include_bytes!("../../fixtures/h264_no_vui_low_delay.annexb").as_slice(),
            0,
        ),
    ] {
        let mut decoder =
            WindowsVideoDecoder::open(&config(CodecKind::H264, Some(&device))).unwrap();
        let mut emitted = 0;
        let mut held = None;
        for (index, unit) in access_units(data, false).iter().enumerate() {
            decoder.push_packet(&packet(unit, index)).unwrap();
            while let Some(frame) = decoder.poll_owned_frame().unwrap() {
                let WindowsDecodedFrame::Gpu(frame) = frame else {
                    panic!("GPU output required")
                };
                assert_eq!(frame.pts(), emitted as i64);
                assert_eq!(hash(&gpu_pixels(&frame)), hashes[emitted]);
                if emitted == 0 {
                    assert_eq!(index, delay);
                    held = Some(frame);
                }
                emitted += 1;
            }
            if delay == 0 {
                assert_eq!(emitted, index + 1);
            }
        }
        assert!(emitted >= hashes.len() - delay);
        drop(decoder);
        assert_eq!(hash(&gpu_pixels(&held.unwrap())), hashes[0]);
    }
}

#[test]
#[ignore = "requires a D3D11 HEVC device"]
fn cra_leading_inputs_are_explicitly_retired() {
    use sha2::{Digest, Sha256};
    let _array = rust_dxva::dxva::array_surfaces_for_test();
    let device = device();
    let mut decoder = WindowsVideoDecoder::open(&config(CodecKind::Hevc, Some(&device))).unwrap();
    let case: serde_json::Value =
        serde_json::from_str(include_str!("../../fixtures/hevc_cra.json")).unwrap();
    let expected = case["frames"].as_array().unwrap();
    let units = access_units(include_bytes!("../../fixtures/hevc_cra.annexb"), true);
    let mut inflight = std::collections::HashSet::new();
    let mut dropped = 0;
    for (index, unit) in units.iter().enumerate() {
        inflight.insert(index as i64);
        decoder.push_packet(&packet(unit, index)).unwrap();
        while let Some(frame) = decoder.poll_owned_frame().unwrap() {
            let WindowsDecodedFrame::Gpu(frame) = frame else {
                panic!("GPU output required")
            };
            let row = expected
                .iter()
                .find(|f| f["input_index"].as_i64() == Some(frame.pts()))
                .unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(gpu_pixels(&frame))),
                row["sha256"].as_str().unwrap()
            );
            assert!(inflight.remove(&frame.pts()));
        }
        while let Some(token) = decoder.poll_dropped_token() {
            assert!(
                !expected
                    .iter()
                    .any(|f| f["input_index"].as_i64() == Some(token))
            );
            assert!(inflight.remove(&token));
            dropped += 1;
        }
    }
    assert_eq!(dropped, units.len() - expected.len());
    // Only the real output-reorder tail remains; discarded RASL input tokens
    // must not accumulate and trigger the viewer's inflight-pressure recovery.
    assert!(inflight.len() <= 2);
    assert!(inflight.iter().all(|&t| {
        expected
            .iter()
            .any(|f| f["input_index"].as_i64() == Some(t))
    }));
}
