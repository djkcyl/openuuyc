//! Hardware first; the Rust H.264 core is the negotiated software fallback.
use super::rust_h264::Encoder as SoftwareEncoder;
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{Graphics::Direct3D11::*, System::Com::*},
    core::Interface,
};

pub(crate) fn probe(
    desktop: &mut super::capture::Desktop,
    lease: &super::Lease,
) -> Result<Vec<super::format::Capability>> {
    use super::format::{Backend, Capability, Codec, Format, Rate};
    let desc = unsafe {
        desktop
            .device
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
            .GetAdapter()?
            .GetDesc()?
    };
    let adapter =
        (u64::from(desc.AdapterLuid.HighPart as u32) << 32) | u64::from(desc.AdapterLuid.LowPart);
    let mut candidates = Vec::new();
    let mut adapters = super::capture::adapters()?;
    adapters.sort_by_key(|(luid, _)| *luid != adapter);
    for (luid, vendor) in adapters {
        let backend = match vendor {
            0x10de => Backend::Nvidia,
            0x1002 => Backend::Amd,
            0x8086 => Backend::Intel,
            _ => continue,
        };
        let device = if luid == adapter {
            desktop.device.clone()
        } else {
            match super::capture::create_device(luid) {
                Ok((device, _)) => device,
                Err(error) => {
                    tracing::debug!(%error,luid,"encoder adapter unavailable");
                    continue;
                }
            }
        };
        for codec in [Codec::H264, Codec::H265] {
            for chroma in [1, 3] {
                for depth in [8, 10] {
                    let format = Format {
                        codec,
                        chroma,
                        depth,
                    };
                    if backend.accepts(format) {
                        candidates.push((luid, device.clone(), backend, format));
                    }
                }
            }
        }
    }
    candidates.push((
        adapter,
        desktop.device.clone(),
        Backend::Software,
        Format::AVC,
    ));
    let mut cached = None::<super::capture::Frame>;
    let mut cached_hdr = None;
    let mut result = Vec::new();
    for (adapter, device, backend, format) in candidates {
        if !lease.requested() {
            anyhow::bail!("共享已取消");
        }
        let probe = (|| -> Result<Capability> {
            if cached_hdr != Some(format.hdr()) {
                cached = None;
            }
            for _ in 0..20 {
                if !lease.requested() {
                    anyhow::bail!("共享已取消");
                }
                if let Some(frame) = desktop.next(50, 1, false, format.hdr(), (1280, 720))? {
                    cached = Some(frame);
                    break;
                }
                if cached.is_some() {
                    break;
                }
            }
            cached_hdr = Some(format.hdr());
            let frame = cached.as_ref().context("尚未取得所选桌面画面")?;
            let rate = Rate {
                target: 2_000_000,
                peak: 2_000_000,
                fps: 30,
                quality: 1,
            };
            let mut transfer = if device != desktop.device {
                Some(super::transfer::Transfer::new(
                    &desktop.device,
                    &device,
                    frame,
                )?)
            } else {
                None
            };
            let mut encoder = if backend == Backend::Software {
                Encoder::software(&device, frame.width, frame.height, 30, rate.target)?
            } else {
                Encoder::hardware_format(&device, (frame.width, frame.height), format, rate)?
            };
            let mut output = None;
            for i in 0..20 {
                if !lease.requested() {
                    anyhow::bail!("共享已取消");
                }
                let delivery = if let Some(transfer) = transfer.as_mut() {
                    match transfer.copy(frame)? {
                        Some(frame) => Some(frame),
                        None => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            continue;
                        }
                    }
                } else {
                    None
                };
                let frames = encoder.encode(
                    delivery
                        .as_ref()
                        .map_or(&frame.texture, |d| &d.frame.texture),
                    i * 333333,
                    true,
                )?;
                drop(delivery);
                output = frames.iter().find_map(|frame| {
                    crate::video_format::parse_annex_b_format(format.codec.media(), &frame.data)
                });
                if output.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let output = output.context("编码器未输出可验证的SPS")?;
            ensure!(
                output.chroma_format_idc == format.chroma
                    && output.bit_depth_luma == format.depth
                    && output.bit_depth_chroma == format.depth
                    && (output.visible_width, output.visible_height) == (frame.width, frame.height),
                "编码器实际码流与请求格式不一致"
            );
            Ok(Capability {
                adapter,
                backend,
                format,
                maximum: encoder.maximum_size(),
            })
        })();
        match probe {
            Ok(cap) => {
                tracing::debug!(?cap, "host encoder capability verified from SPS");
                result.push(cap)
            }
            Err(error) => {
                tracing::debug!(?backend,?format,%error,"host encoder capability rejected")
            }
        }
    }
    ensure!(!result.is_empty(), "所选桌面没有可用的视频编码器");
    Ok(result)
}

// Driver busy paths use the SDK's bounded spin/yield/backoff schedule.
pub(super) fn retry_pause(attempt: usize) {
    match attempt {
        0..=3 => std::hint::spin_loop(),
        4..=7 => std::thread::yield_now(),
        _ => std::thread::sleep(std::time::Duration::from_millis(
            1u64 << (attempt - 8).min(3),
        )),
    }
}

pub(crate) struct Runtime;
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct SwitchCandidate(pub String);
impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

pub(crate) struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_100ns: i64,
    pub is_new: bool,
    pub timing: Option<FrameTiming>,
    pub format: super::format::Format,
    pub color: crate::video_color::VideoColorSpace,
}
#[derive(Clone, Copy)]
pub(crate) struct FrameTiming {
    pub captured: std::time::Instant,
    pub encode_started: std::time::Instant,
    pub encode_finished: std::time::Instant,
}

pub(crate) enum Encoder {
    Nvidia(super::nvenc::Encoder),
    Amd(super::amf::Encoder),
    Intel(super::qsv::Encoder),
    Software {
        encoder: SoftwareEncoder,
        context: ID3D11DeviceContext,
        staging: ID3D11Texture2D,
        pixels: Vec<u8>,
    },
}
impl Encoder {
    pub(crate) fn hardware_format(
        device: &ID3D11Device,
        size: (u32, u32),
        format: super::format::Format,
        rate: super::format::Rate,
    ) -> Result<Self> {
        unsafe {
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast()?;
            let desc = dxgi.GetAdapter()?.GetDesc()?;
            match desc.VendorId {
                0x10de => Ok(Self::Nvidia(super::nvenc::Encoder::new_format(
                    device, size.0, size.1, rate, format,
                )?)),
                0x1002 => Ok(Self::Amd(super::amf::Encoder::new(
                    device, size, format, rate,
                )?)),
                0x8086 => Ok(Self::Intel(super::qsv::Encoder::new(
                    device, size, format, rate,
                )?)),
                _ => anyhow::bail!("当前采集适配器没有可用的硬件编码候选"),
            }
        }
    }
    pub(crate) fn software(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        let encoder = SoftwareEncoder::new(width, height, fps, bitrate)?;
        unsafe {
            let mut staging = None;
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut staging),
            )?;
            Ok(Self::Software {
                encoder,
                context: device.GetImmediateContext()?,
                staging: staging.context("软件编码读回纹理")?,
                pixels: vec![0; width as usize * height as usize * 4],
            })
        }
    }
    pub(crate) fn implementation(&self) -> i32 {
        match self {
            Self::Nvidia(_) => 0,
            Self::Amd(_) => 1,
            Self::Intel(_) => 2,
            Self::Software { .. } => 5,
        }
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        match self {
            Self::Nvidia(e) => e.maximum_size(),
            Self::Amd(e) => e.maximum_size(),
            Self::Intel(e) => e.maximum_size(),
            Self::Software { .. } => super::rust_h264::MAXIMUM,
        }
    }
    pub(crate) fn configure_rate(&mut self, rate: super::format::Rate) -> Result<bool> {
        match self {
            Self::Nvidia(e) => e.configure_rate(rate),
            Self::Amd(e) => e.configure(rate),
            Self::Intel(e) => e.configure(rate),
            Self::Software { encoder, .. } => encoder.configure(rate),
        }
    }
    pub(crate) fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        match self {
            Self::Nvidia(e) => e.encode(texture, timestamp, keyframe),
            Self::Amd(e) => e.encode(texture, timestamp, keyframe),
            Self::Intel(e) => e.encode(texture, timestamp, keyframe),
            Self::Software {
                encoder,
                context,
                staging,
                pixels,
            } => unsafe {
                context.CopyResource(&*staging, texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(&*staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let result = (|| {
                    let row = encoder.width as usize * 4;
                    ensure!(
                        !mapped.pData.is_null() && mapped.RowPitch as usize >= row,
                        "软件编码读回行跨度无效"
                    );
                    debug_assert_eq!(pixels.len(), row * encoder.height as usize);
                    for y in 0..encoder.height as usize {
                        std::ptr::copy_nonoverlapping(
                            (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                            pixels.as_mut_ptr().add(y * row),
                            row,
                        );
                    }
                    Ok(())
                })();
                context.Unmap(&*staging, 0);
                result?;
                encoder.encode(pixels, timestamp, keyframe)
            },
        }
    }
}
