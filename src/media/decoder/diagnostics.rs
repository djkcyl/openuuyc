//! Exhaustive current-backend checks. No automatic candidate or adapter fallback.
use super::software_slot::SoftwareSlot;
use crate::media::decode_api::{
    DecoderNotification, VideoDecoder, VideoDecoderConfig, VideoOutputPreference,
};
use crate::media::{LocalDisplayInfo, VideoCodec};
use crate::platform::{
    decoder::{WindowsDecodedFrame, WindowsVideoDecoder},
    surface::D3D11SurfaceWriter,
};
use anyhow::{Context, Result, ensure};
use mediaway_common::{Bytes, CodecKind, Packet, PixelFormat, Rational};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) struct Format {
    codec: VideoCodec,
    chroma: u8,
    depth: u8,
}
impl Format {
    fn label(self) -> String {
        format!(
            "{} · {} · {}位",
            if self.codec == VideoCodec::H264 {
                "H.264"
            } else {
                "H.265"
            },
            if self.chroma == 1 { "420" } else { "444" },
            self.depth
        )
    }
    fn kind(self) -> CodecKind {
        if self.codec == VideoCodec::H264 {
            CodecKind::H264
        } else {
            CodecKind::Hevc
        }
    }
    fn sample(self, size: (u32, u32)) -> Option<&'static [u8]> {
        macro_rules! sample {
            ($name:literal) => {
                match size {
                    (1280, 720) => Some(
                        include_bytes!(concat!("fixtures/matrix/", $name, "_1280x720.annexb"))
                            .as_slice(),
                    ),
                    (1920, 1080) => Some(
                        include_bytes!(concat!("fixtures/matrix/", $name, "_1920x1080.annexb"))
                            .as_slice(),
                    ),
                    (2560, 1440) => Some(
                        include_bytes!(concat!("fixtures/matrix/", $name, "_2560x1440.annexb"))
                            .as_slice(),
                    ),
                    (3840, 2160) => Some(
                        include_bytes!(concat!("fixtures/matrix/", $name, "_3840x2160.annexb"))
                            .as_slice(),
                    ),
                    _ => None,
                }
            };
        }
        match (self.codec, self.chroma, self.depth) {
            (VideoCodec::H264, 1, 8) => sample!("h264"),
            (VideoCodec::H264, 3, 8) => sample!("h264_444"),
            (VideoCodec::H265, 1, 8) => sample!("hevc"),
            (VideoCodec::H265, 1, 10) => sample!("main10"),
            (VideoCodec::H265, 3, 8) => sample!("hevc_444"),
            (VideoCodec::H265, 3, 10) => sample!("hevc_444_10"),
            _ => None,
        }
    }
}
fn formats() -> Vec<Format> {
    [VideoCodec::H264, VideoCodec::H265]
        .into_iter()
        .flat_map(|codec| {
            [1, 3].into_iter().flat_map(move |chroma| {
                [8, 10].map(|depth| Format {
                    codec,
                    chroma,
                    depth,
                })
            })
        })
        .collect()
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Pending,
    Passed,
    Failed,
    Unsupported,
    Busy,
    Unchecked,
}
impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待检查",
            Self::Passed => "通过",
            Self::Failed => "失败",
            Self::Unsupported => "未实现",
            Self::Busy => "资源占用",
            Self::Unchecked => "未检查",
        }
    }
}
#[derive(Clone)]
pub(crate) struct Cell {
    pub status: Status,
    pub detail: String,
}
pub(crate) struct Row {
    pub format: String,
    pub cells: Vec<Cell>,
}
pub(crate) struct Backend {
    pub name: String,
    pub device: Option<String>,
    pub sizes: Vec<(u32, u32)>,
    pub rows: Vec<Row>,
}
#[derive(Default)]
pub(crate) struct Report {
    pub backends: Vec<Backend>,
    pub message: String,
}
pub(crate) enum Event {
    Backend(Backend),
    Cell(usize, usize, usize, Cell),
    Finished(String),
}

pub(crate) fn run(
    display: LocalDisplayInfo,
    tx: &Sender<Event>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let devices = match D3D11SurfaceWriter::diagnostic_adapters() {
        Ok(devices) if !devices.is_empty() => devices,
        Ok(_) => vec![(
            "DXVA11".into(),
            Err(anyhow::anyhow!("没有可用的硬件适配器")),
        )],
        Err(error) => vec![("DXVA11 枚举".into(), Err(error))],
    };
    let mut all: Vec<_> = devices
        .into_iter()
        .map(|(name, writer)| (format!("DXVA11 · {name}"), Some(name), Some(writer)))
        .collect();
    all.push(("OpenUUYC H264 · 软件".into(), None, None));
    let mut sizes: Vec<_> = crate::protocol::capability::QUALITY_DIMENSIONS
        .iter()
        .map(|&(w, h)| (w as u32, h as u32))
        .collect();
    let local = (display.width, display.height);
    if local.0 > 0 && local.1 > 0 && !sizes.contains(&local) {
        sizes.push(local);
    }
    let matrix = formats();
    for (name, device, _) in &all {
        if tx
            .send(Event::Backend(Backend {
                name: name.clone(),
                device: device.clone(),
                sizes: sizes.clone(),
                rows: matrix
                    .iter()
                    .map(|f| Row {
                        format: f.label(),
                        cells: sizes
                            .iter()
                            .map(|_| Cell {
                                status: Status::Pending,
                                detail: String::new(),
                            })
                            .collect(),
                    })
                    .collect(),
            }))
            .is_err()
        {
            return Ok(());
        }
    }
    for (backend, (name, _, device)) in all.iter().enumerate() {
        for (row, format) in matrix.iter().copied().enumerate() {
            for (column, &size) in sizes.iter().enumerate() {
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                let supported = if device.is_some() {
                    format.codec == VideoCodec::H265 || (format.chroma == 1 && format.depth == 8)
                } else {
                    format.codec == VideoCodec::H264 && format.depth == 8
                };
                let mut cell = Cell {
                    status: Status::Unchecked,
                    detail: String::new(),
                };
                if !supported {
                    cell.status = Status::Unsupported;
                    cell.detail = "本程序此后端未实现该格式".into();
                } else if let Some(Err(error)) = device {
                    cell.status = Status::Failed;
                    cell.detail = format!("设备不可用：{error:#}");
                } else if let Some(sample) = format.sample(size) {
                    let writer = device.as_ref().and_then(|d| d.as_ref().ok());
                    let result = (|| {
                        if let Some(writer) = writer {
                            WindowsVideoDecoder::check_format(
                                writer.device_handle(),
                                format.kind(),
                                size.0,
                                size.1,
                                format.depth,
                                format.chroma,
                            )
                            .context("驱动配置创建失败")?;
                            cell.detail = "驱动配置：可创建\n".into();
                        }
                        decode_sample(format, size, sample, writer, cancel.clone())
                    })();
                    match result {
                        Ok(()) => {
                            cell.status = Status::Passed;
                            cell.detail
                                .push_str("实际解码出帧，尺寸、像素格式及合成图案采样校验通过");
                        }
                        Err(error) => {
                            cell.status = if error
                                .downcast_ref::<super::software_slot::SoftwarePlaybackBusy>()
                                .is_some()
                            {
                                Status::Busy
                            } else {
                                Status::Failed
                            };
                            cell.detail.push_str(&format!("{error:#}"));
                        }
                    }
                } else {
                    cell.detail = "没有此尺寸的内置样本，未执行实际解码".into();
                }
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                cell.detail = format!(
                    "{} · {}×{}\n{}",
                    format.label(),
                    size.0,
                    size.1,
                    cell.detail
                );
                tracing::info!(
                    backend = name,
                    format = format.label(),
                    width = size.0,
                    height = size.1,
                    result = cell.status.label(),
                    detail = cell.detail,
                    "decoder diagnostic result"
                );
                if tx.send(Event::Cell(backend, row, column, cell)).is_err() {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Fixtures contain multiple access units; submit the first complete picture,
/// including all its parameter sets and slices, not the whole stream as one frame.
fn first_picture(format: Format, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut picture = false;
    for nal in crate::media::video_format::annex_b_units(data) {
        let (vcl, first, aud) = if format.codec == VideoCodec::H264 {
            let kind = nal[0] & 31;
            (
                matches!(kind, 1 | 5),
                nal.get(1).is_some_and(|v| v & 128 != 0),
                kind == 9,
            )
        } else {
            let kind = (nal[0] >> 1) & 63;
            (
                kind <= 31,
                nal.get(2).is_some_and(|v| v & 128 != 0),
                kind == 35,
            )
        };
        if picture && ((vcl && first) || aud) {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        picture |= vcl;
    }
    out
}
fn decode_sample(
    format: Format,
    size: (u32, u32),
    sample: &[u8],
    writer: Option<&D3D11SurfaceWriter>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let sample = first_picture(format, sample);
    let signature = crate::media::video_format::parse_annex_b_format(format.codec, &sample)
        .context("样本SPS无效")?;
    ensure!(
        signature.chroma_format_idc == format.chroma && signature.bit_depth_luma == format.depth,
        "样本格式不匹配"
    );
    ensure!(
        (signature.visible_width, signature.visible_height) == size,
        "样本尺寸与目标不一致"
    );
    let _slot = if writer.is_none() {
        Some(SoftwareSlot::acquire()?)
    } else {
        None
    };
    let (w, h) = (signature.visible_width, signature.visible_height);
    let points = [
        (w / 8, h / 8),
        (w * 7 / 8, h / 8),
        (w / 2, h / 2),
        (w / 8, h * 7 / 8),
        (w * 7 / 8, h * 7 / 8),
    ];
    let mut decoder = WindowsVideoDecoder::open(&VideoDecoderConfig {
        codec: format.kind(),
        width: w,
        height: h,
        time_base: Rational::new(1, 90000),
        pixel_format: PixelFormat::Nv12,
        output: if writer.is_some() {
            VideoOutputPreference::ZeroCopyGpu
        } else {
            VideoOutputPreference::CpuFramesOk
        },
        gpu_device: writer.map(|w| w.device_handle()),
        extra_data: Bytes::new(),
    })
    .context("创建指定后端")?;
    decoder.set_notification(DecoderNotification::new(cancel.clone()));
    decoder
        .push_packet(&Packet {
            stream_id: 0,
            pts: 1,
            dts: 1,
            duration: 3000,
            is_keyframe: true,
            is_discard: false,
            payload: sample.into(),
        })
        .context("提交样本码流")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        ensure!(!cancel.load(Ordering::Acquire), "检查已取消");
        if let Some(frame) = decoder.poll_owned_frame().context("读取解码输出")? {
            let actual = match frame {
                WindowsDecodedFrame::Gpu(frame) => {
                    use windows::Win32::Graphics::{
                        Direct3D11::D3D11_TEXTURE2D_DESC, Dxgi::Common::*,
                    };
                    let expected = match (format.chroma, format.depth) {
                        (1, 8) => DXGI_FORMAT_NV12,
                        (1, 10) => DXGI_FORMAT_P010,
                        (3, 8) => DXGI_FORMAT_AYUV,
                        (3, 10) => DXGI_FORMAT_Y410,
                        _ => unreachable!(),
                    };
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe {
                        frame.texture().GetDesc(&mut desc);
                    }
                    ensure!(desc.Format == expected, "GPU输出像素格式与样本不一致");
                    check_pattern(&frame.diagnostic_pixels(&points, &cancel)?)?;
                    let actual = (frame.width(), frame.height());
                    let _surface = writer
                        .context("缺少D3D11输出拥有者")?
                        .wrap_decoded_surface(frame)?;
                    actual
                }
                WindowsDecodedFrame::Cpu(frame) => {
                    use crate::platform::decoder::WindowsCpuFormat;
                    let expected = if format.chroma == 3 {
                        WindowsCpuFormat::I444
                    } else {
                        WindowsCpuFormat::Nv12
                    };
                    ensure!(frame.format == expected, "软件输出像素格式与样本不一致");
                    let plane = frame.width as usize * frame.height as usize;
                    ensure!(
                        frame.data.len()
                            == if format.chroma == 3 {
                                plane * 3
                            } else {
                                plane + plane / 2
                            },
                        "软件输出平面长度不符"
                    );
                    ensure!(
                        (frame.width, frame.height) == (w, h),
                        "软件输出尺寸与样本不一致"
                    );
                    let pixels: Vec<_> = points
                        .iter()
                        .map(|&(x, y)| {
                            let offset = (y * w + x) as usize;
                            let (u, v) = if format.chroma == 3 {
                                (plane + offset, plane * 2 + offset)
                            } else {
                                let u = plane + (y / 2 * w + x / 2 * 2) as usize;
                                (u, u + 1)
                            };
                            [
                                frame.data[offset] as u16,
                                frame.data[u] as u16,
                                frame.data[v] as u16,
                            ]
                        })
                        .collect();
                    check_pattern(&pixels)?;
                    (frame.width, frame.height)
                }
            };
            ensure!(actual == (w, h), "解码输出尺寸与样本不一致");
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "创建成功，但2秒内没有解码输出");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn check_pattern(pixels: &[[u16; 3]]) -> Result<()> {
    for (index, actual) in pixels.iter().enumerate() {
        let expected = [if index == 2 { 192 } else { 64 }, 96, 160];
        ensure!(
            actual
                .iter()
                .zip(expected)
                .all(|(&a, e)| a.abs_diff(e) <= 8),
            "合成图案第{}点像素不符：实际{:?}，预期{:?}",
            index + 1,
            actual,
            expected
        );
    }
    Ok(())
}
