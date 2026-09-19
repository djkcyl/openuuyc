pub(crate) mod platform;

use crate::decoder::platform::{
    DecodeError, VideoDecoder, VideoDecoderConfig, VideoOutputPreference,
};
use anyhow::{Context, Result, anyhow, bail};
use mediaway_common::{Bytes, CodecKind, Packet, PixelFormat, Rational};
use yuv::{YuvBiPlanarImage, YuvConversionMode, YuvRange, YuvStandardMatrix, yuv_nv12_to_rgba};

use crate::capability::{CodecCapability, DeviceCapability, QUALITY_DIMENSIONS};
use crate::media::{CodecPreference, ConnectionMediaProfile, VideoCodec};
use crate::rtc::EncodedVideoFrame;
use crate::video_color::{ColorMatrix, RenderColor};

pub(crate) mod software_slot;

#[cfg(windows)]
pub(crate) mod windows_surface;
#[cfg(not(windows))]
#[path = "decoder/windows_surface_stub.rs"]
pub(crate) mod windows_surface;

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub surface: DecodedSurface,
    pub ready_at: std::time::Instant,
}

#[derive(Default)]
pub(crate) struct DecodedBatch {
    pub frames: Vec<DecodedFrame>,
    pub input_error: Option<anyhow::Error>,
    pub output_issues: Vec<DecoderOutputIssue>,
}

pub(crate) enum DecoderOutputIssue {
    Dropped(i64),
    Failed {
        token: Option<i64>,
        error: anyhow::Error,
    },
}

#[derive(Debug)]
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows decoder produces GPU surfaces.")
)]
pub(crate) enum DecodedSurface {
    CpuNv12(Bytes),

    CpuI444(Bytes),

    D3D11(windows_surface::D3D11Surface),
}

#[derive(Debug)]
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows presenter draws GPU surfaces.")
)]
pub(crate) enum RenderSurface {
    CpuRgba8(Vec<Rgba8>),

    /// Packed NV12 handed to the renderer as-is; the shader converts it.
    /// Doing that on the GPU saves a full-frame CPU conversion per picture.
    #[cfg(not(windows))]
    CpuNv12 {
        data: Bytes,
        color: RenderColor,
    },

    D3D11(windows_surface::D3D11Surface),
}

#[repr(C, align(4))]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Rgba8(pub [u8; 4]);

impl DecodedSurface {
    // Associate the decoder token with its input frame before choosing color.
    // CPU conversion remains on the decoder worker, outside the frame-queue lock.
    pub(crate) fn prepare(
        self,
        width: u32,
        height: u32,
        color: RenderColor,
    ) -> Result<RenderSurface> {
        match self {
            Self::CpuI444(data) => {
                i444_to_rgba_pixels(width, height, &data, color).map(RenderSurface::CpuRgba8)
            }
            #[cfg(windows)]
            Self::CpuNv12(data) => {
                nv12_to_rgba_pixels(width, height, &data, color).map(RenderSurface::CpuRgba8)
            }
            // The Linux renderer samples NV12 directly; only validate the layout.
            #[cfg(not(windows))]
            Self::CpuNv12(data) => {
                nv12_layout(width, height, &data)?;
                Ok(RenderSurface::CpuNv12 { data, color })
            }

            Self::D3D11(surface) => Ok(RenderSurface::D3D11(surface)),
        }
    }
}

pub(crate) struct NativeVideoDecoder {
    backend: DecoderBackend,
    candidate: DecoderCandidate,
    label: String,
    frame_duration: u64,

    software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
}

/// Local implementation choices, never serialized as invented UU decoder IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecoderCandidate {
    #[cfg(windows)]
    WindowsD3d11,
    /// VA-API through the local driver: NVDEC, Intel or AMD.
    #[cfg(not(windows))]
    LinuxVaapi,

    SoftwareH264,
}

impl DecoderCandidate {
    pub(crate) fn available(codec: VideoCodec, prefer_hardware: bool) -> Vec<Self> {
        let mut candidates = Vec::new();
        if prefer_hardware {
            #[cfg(windows)]
            candidates.push(Self::WindowsD3d11);
            #[cfg(not(windows))]
            candidates.push(Self::LinuxVaapi);
        }

        if codec == VideoCodec::H264 {
            candidates.push(Self::SoftwareH264);
        }

        candidates
    }
}

impl NativeVideoDecoder {
    pub(crate) fn open(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        prefer_hardware: bool,
        extra_data: Bytes,
    ) -> Result<Self> {
        Self::open_inner(
            codec,
            width,
            height,
            frame_rate,
            prefer_hardware,
            extra_data,
            None,
        )
    }

    pub(crate) fn open_with_surface_writer(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        prefer_hardware: bool,
        extra_data: Bytes,
        surface_writer: windows_surface::D3D11SurfaceWriter,
    ) -> Result<Self> {
        Self::open_inner(
            codec,
            width,
            height,
            frame_rate,
            prefer_hardware,
            extra_data,
            Some(surface_writer),
        )
    }

    fn open_inner(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        prefer_hardware: bool,
        extra_data: Bytes,
        surface_writer: Option<windows_surface::D3D11SurfaceWriter>,
    ) -> Result<Self> {
        let mut last_error = None;
        for candidate in DecoderCandidate::available(codec, prefer_hardware) {
            match Self::open_candidate(
                candidate,
                codec,
                width,
                height,
                frame_rate,
                extra_data.clone(),
                surface_writer.clone(),
                None,
            ) {
                Ok(decoder) => return Ok(decoder),
                Err(error) => {
                    tracing::debug!(?candidate, %error, "native decoder candidate unavailable");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| DecodeError::NoBackend.into()))
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(
        not(windows),
        allow(
            unused_variables,
            reason = "The D3D11 writer and its format probe are Windows-only."
        )
    )]
    pub(crate) fn open_candidate(
        candidate: DecoderCandidate,
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        extra_data: Bytes,
        surface_writer: Option<windows_surface::D3D11SurfaceWriter>,
        software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
    ) -> Result<Self> {
        if width == 0 || height == 0 || frame_rate == 0 {
            return Err(DecodeError::InvalidInput.into());
        }
        let frame_duration = (90_000 / u64::from(frame_rate)).max(1);
        let format = crate::video_format::parse_annex_b_format(codec, &extra_data);
        let depth = format.map_or(8, |f| f.bit_depth_luma);
        let chroma = format.map_or(1, |f| f.chroma_format_idc);
        match candidate {
            #[cfg(not(windows))]
            DecoderCandidate::LinuxVaapi => {
                let config = decoder_config(
                    codec_kind(codec),
                    width,
                    height,
                    VideoOutputPreference::ZeroCopyGpu,
                    None,
                    extra_data,
                );
                let decoder = open_platform_decoder(&config)?;
                Ok(Self {
                    backend: DecoderBackend::Platform {
                        decoder: Box::new(decoder),
                        frame_reader: FrameReader::Cpu,
                    },
                    candidate,
                    label: format!("{} 硬解", platform_label()),
                    frame_duration,
                    software_slot: None,
                })
            }
            #[cfg(windows)]
            DecoderCandidate::WindowsD3d11 => {
                let supports = |writer: &windows_surface::D3D11SurfaceWriter| {
                    PlatformDecoder::probe_format(
                        writer.device_handle(),
                        codec_kind(codec),
                        width,
                        height,
                        depth,
                        chroma,
                    )
                };
                let writer = match surface_writer.filter(supports) {
                    Some(writer) => writer,
                    None => windows_surface::D3D11SurfaceWriter::available()?
                        .into_iter()
                        .find(supports)
                        .ok_or(DecodeError::Unsupported)?,
                };
                Self::open_windows_hardware(
                    codec_kind(codec),
                    width,
                    height,
                    frame_duration,
                    writer,
                    extra_data,
                )
            }

            DecoderCandidate::SoftwareH264 => {
                let software_slot =
                    Some(software_slot.map_or_else(software_slot::SoftwareSlot::acquire, Ok)?);
                let config = decoder_config(
                    codec_kind(codec),
                    width,
                    height,
                    VideoOutputPreference::CpuFramesOk,
                    None,
                    extra_data,
                );
                let decoder = open_platform_decoder(&config)?;
                let label = { "Rust H.264 软件解码" };
                Ok(Self {
                    backend: DecoderBackend::Platform {
                        decoder: Box::new(decoder),
                        frame_reader: FrameReader::Cpu,
                    },
                    candidate,
                    label: label.to_owned(),
                    frame_duration,

                    software_slot,
                })
            }
        }
    }

    pub(crate) const fn candidate(&self) -> DecoderCandidate {
        self.candidate
    }

    pub(crate) fn is_software(&self) -> bool {
        self.software_slot.is_some()
    }

    pub(crate) fn software_slot(&self) -> Option<std::rc::Rc<software_slot::SoftwareSlot>> {
        self.software_slot.clone()
    }

    pub(crate) fn surface_writer(&self) -> Option<windows_surface::D3D11SurfaceWriter> {
        match &self.backend {
            DecoderBackend::Platform {
                frame_reader: FrameReader::Windows(writer),
                ..
            } => Some(writer.clone()),
            _ => None,
        }
    }

    #[cfg(windows)]
    fn open_windows_hardware(
        codec: CodecKind,
        width: u32,
        height: u32,
        frame_duration: u64,
        reader: windows_surface::D3D11SurfaceWriter,
        extra_data: Bytes,
    ) -> Result<Self> {
        let config = decoder_config(
            codec,
            width,
            height,
            VideoOutputPreference::ZeroCopyGpu,
            Some(reader.device_handle()),
            extra_data,
        );
        let backend = open_platform_decoder(&config)?;
        Ok(Self {
            backend: DecoderBackend::Platform {
                decoder: Box::new(backend),
                frame_reader: FrameReader::Windows(reader),
            },
            candidate: DecoderCandidate::WindowsD3d11,
            label: format!("{} D3D11 硬解", platform_label()),
            frame_duration,
            software_slot: None,
        })
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn set_notification(
        &mut self,
        notification: crate::decoder::platform::DecoderNotification,
    ) {
        match &mut self.backend {
            DecoderBackend::Platform { decoder, .. } => decoder.set_notification(notification),
        }
    }

    pub(crate) fn reset_for_keyframe(&mut self, _hard_reset: bool) -> Result<()> {
        match &mut self.backend {
            DecoderBackend::Platform { decoder, .. } => decoder
                .reset_for_keyframe()
                .with_context(|| format!("reset {} decoder for keyframe cutover", self.label)),
        }
    }

    pub(crate) fn push(&mut self, frame: EncodedVideoFrame, decode_token: i64) -> DecodedBatch {
        match &mut self.backend {
            DecoderBackend::Platform {
                decoder,
                frame_reader,
            } => {
                let packet = Packet {
                    stream_id: 0,
                    pts: decode_token,
                    dts: decode_token,
                    duration: self.frame_duration,
                    is_keyframe: frame.keyframe,
                    is_discard: false,
                    payload: frame.data,
                };
                let input_error = decoder
                    .push_packet(&packet)
                    .context("submit Annex-B frame to native decoder")
                    .err();
                let mut batch = poll_platform_decoder(decoder, frame_reader);
                batch.input_error = input_error;
                batch
            }
        }
    }

    pub(crate) fn poll(&mut self) -> DecodedBatch {
        match &mut self.backend {
            DecoderBackend::Platform {
                decoder,
                frame_reader,
            } => poll_platform_decoder(decoder, frame_reader),
        }
    }
}

#[cfg_attr(
    not(windows),
    allow(
        unused_variables,
        reason = "Only the GPU arm, which is Windows-only, reads the frame reader."
    )
)]
fn poll_platform_decoder(
    decoder: &mut PlatformDecoder,
    frame_reader: &mut FrameReader,
) -> DecodedBatch {
    let mut batch = DecodedBatch::default();

    use crate::decoder::platform::{CpuFormat, PlatformDecodedFrame};
    loop {
        let frame = match decoder
            .poll_owned_frame()
            .context("poll owning Windows decoded frame")
        {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                batch
                    .output_issues
                    .push(DecoderOutputIssue::Failed { token: None, error });
                break;
            }
        };
        let ready_at = std::time::Instant::now();
        let (pts, width, height, surface) = match frame {
            #[cfg(windows)]
            PlatformDecodedFrame::Gpu(frame) => (
                frame.pts(),
                frame.width(),
                frame.height(),
                match frame_reader {
                    FrameReader::Windows(writer) => writer
                        .wrap_decoded_surface(frame)
                        .map(DecodedSurface::D3D11),
                    _ => Err(anyhow!("GPU decoder output has no owning device")),
                },
            ),
            PlatformDecodedFrame::Cpu(frame) => {
                let surface = match frame.format {
                    CpuFormat::Nv12 => nv12_layout(frame.width, frame.height, &frame.data)
                        .map(|_| DecodedSurface::CpuNv12(frame.data)),
                    CpuFormat::I444 => Ok(DecodedSurface::CpuI444(frame.data)),
                };
                (frame.pts, frame.width, frame.height, surface)
            }
        };
        match surface {
            Ok(surface) => batch.frames.push(DecodedFrame {
                pts,
                width,
                height,
                surface,
                ready_at,
            }),
            Err(error) => batch.output_issues.push(DecoderOutputIssue::Failed {
                token: Some(pts),
                error,
            }),
        }
    }
    while let Some(token) = decoder.poll_dropped_token() {
        batch.output_issues.push(DecoderOutputIssue::Dropped(token));
    }
    batch
}

pub(crate) fn detect_native_decoder_support(
    profile: ConnectionMediaProfile,
) -> Result<DeviceCapability> {
    std::thread::spawn(move || {
        let mut capabilities = Vec::new();
        if profile.hardware_decode
            && let Ok(probe) = hardware_probe()
        {
            for (codec, id) in [(VideoCodec::H264, 1), (VideoCodec::H265, 2)] {
                if matches!(
                    (profile.codec, codec),
                    (CodecPreference::H264, VideoCodec::H265)
                        | (CodecPreference::H265, VideoCodec::H264)
                ) {
                    continue;
                }
                for chroma in [1, 3] {
                    for depth in [8, 10] {
                        for &(width, height) in QUALITY_DIMENSIONS[1..].iter().rev() {
                            if probe(codec, width as u32, height as u32, depth, chroma) {
                                capabilities.push(CodecCapability {
                                    video_codec: id,
                                    width,
                                    height,
                                    chroma_sampling: chroma,
                                    bit_depth: depth,
                                    codec_impl: 32,
                                });
                                break;
                            }
                        }
                    }
                }
            }
        }
        if profile.codec != CodecPreference::H265 {
            // streamer 958290: this is the advertised software ceiling, not
            // an arbitrary decoder rejection of a larger hardware-fallback AU.
            for chroma_sampling in [1, 3] {
                capabilities.push(CodecCapability {
                    video_codec: 1,
                    width: 1920,
                    height: 1080,
                    chroma_sampling,
                    bit_depth: 8,
                    codec_impl: 37,
                });
            }
        }
        if capabilities.is_empty() {
            bail!("no decoder supports the selected codec and decoding mode");
        }
        Ok(DeviceCapability {
            ice_id: String::new(),
            display_info: crate::display_hdr::capabilities(profile.local_display.refresh_hz),
            video_codec_capability: capabilities,
        })
    })
    .join()
    .map_err(|_| anyhow!("native capability probe thread panicked"))?
}

/// A closure that answers whether the platform decodes a format in hardware.
/// On Windows every D3D11 device is asked; on Linux the VA-API driver is.
#[cfg(windows)]
fn hardware_probe() -> Result<impl Fn(VideoCodec, u32, u32, u8, u8) -> bool> {
    let writers = windows_surface::D3D11SurfaceWriter::available()?;
    Ok(move |codec, width, height, depth, chroma| {
        writers.iter().any(|writer| {
            PlatformDecoder::probe_format(
                writer.device_handle(),
                codec_kind(codec),
                width,
                height,
                depth,
                chroma,
            )
        })
    })
}

#[cfg(not(windows))]
fn hardware_probe() -> Result<impl Fn(VideoCodec, u32, u32, u8, u8) -> bool> {
    Ok(|codec, width, height, depth, chroma| {
        crate::decoder::platform::linux::probe_hardware(
            codec_kind(codec),
            width,
            height,
            depth,
            chroma,
        )
    })
}

fn decoder_config(
    codec: CodecKind,
    width: u32,
    height: u32,
    output: VideoOutputPreference,
    gpu_device: Option<mediaway_common::GpuDeviceHandle>,
    extra_data: Bytes,
) -> VideoDecoderConfig {
    VideoDecoderConfig {
        codec,
        width,
        height,
        time_base: Rational::new(1, 90_000),
        pixel_format: PixelFormat::Nv12,
        output,
        gpu_device,
        extra_data,
    }
}

fn codec_kind(codec: VideoCodec) -> CodecKind {
    match codec {
        VideoCodec::H264 => CodecKind::H264,
        VideoCodec::H265 => CodecKind::Hevc,
    }
}

fn nv12_layout(width: u32, height: u32, nv12: &[u8]) -> Result<(usize, usize)> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        bail!("packed NV12 requires positive even dimensions, got {width}x{height}");
    }
    let width_usize = usize::try_from(width).context("decoded width does not fit usize")?;
    let height_usize = usize::try_from(height).context("decoded height does not fit usize")?;
    let luma_len = width_usize
        .checked_mul(height_usize)
        .context("decoded luma dimensions overflow")?;
    let expected_len = luma_len
        .checked_add(luma_len / 2)
        .context("decoded NV12 dimensions overflow")?;
    if nv12.len() < expected_len {
        bail!(
            "native decoder returned short NV12 frame: {} bytes for {}x{} (need {})",
            nv12.len(),
            width,
            height,
            expected_len
        );
    }
    Ok((luma_len, expected_len))
}

fn i444_to_rgba_pixels(
    width: u32,
    height: u32,
    data: &[u8],
    color: RenderColor,
) -> Result<Vec<Rgba8>> {
    let plane = (width as usize)
        .checked_mul(height as usize)
        .context("I444 dimensions overflow")?;
    if plane == 0 || plane.checked_mul(3) != Some(data.len()) {
        bail!(
            "invalid packed I444 frame {width}x{height}: {} bytes",
            data.len()
        );
    }
    let mut pixels = vec![Rgba8([0, 0, 0, 255]); plane];
    let image = yuv::YuvPlanarImage {
        y_plane: &data[..plane],
        y_stride: width,
        u_plane: &data[plane..2 * plane],
        u_stride: width,
        v_plane: &data[2 * plane..],
        v_stride: width,
        width,
        height,
    };
    yuv::yuv444_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        width.checked_mul(4).context("RGBA stride overflow")?,
        if color.full_range {
            YuvRange::Full
        } else {
            YuvRange::Limited
        },
        match color.matrix {
            ColorMatrix::Bt601 => YuvStandardMatrix::Bt601,
            ColorMatrix::Bt709 => YuvStandardMatrix::Bt709,
            ColorMatrix::Bt2020 => YuvStandardMatrix::Bt2020,
        },
    )
    .map_err(|error| anyhow!("convert I444 to RGBA: {error}"))?;
    Ok(pixels)
}

#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "The Linux renderer converts NV12 on the GPU.")
)]
fn nv12_to_rgba_pixels(
    width: u32,
    height: u32,
    nv12: &[u8],
    color: RenderColor,
) -> Result<Vec<Rgba8>> {
    let (luma_len, expected_len) = nv12_layout(width, height, nv12)?;
    let pixel_count = luma_len;
    let mut pixels = vec![Rgba8([0, 0, 0, 255]); pixel_count];
    let image = YuvBiPlanarImage {
        y_plane: &nv12[..luma_len],
        y_stride: width,
        uv_plane: &nv12[luma_len..expected_len],
        uv_stride: width,
        width,
        height,
    };
    yuv_nv12_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        width
            .checked_mul(4)
            .context("decoded RGBA stride overflow")?,
        if color.full_range {
            YuvRange::Full
        } else {
            YuvRange::Limited
        },
        match color.matrix {
            ColorMatrix::Bt601 => YuvStandardMatrix::Bt601,
            ColorMatrix::Bt709 => YuvStandardMatrix::Bt709,
            ColorMatrix::Bt2020 => YuvStandardMatrix::Bt2020,
        },
        YuvConversionMode::Balanced,
    )
    .map_err(|error| anyhow!("convert native NV12 frame to RGBA: {error}"))?;
    Ok(pixels)
}

enum DecoderBackend {
    Platform {
        decoder: Box<PlatformDecoder>,
        frame_reader: FrameReader,
    },
}

type PlatformDecoder = crate::decoder::platform::PlatformVideoDecoder;

fn open_platform_decoder(config: &VideoDecoderConfig) -> Result<PlatformDecoder> {
    PlatformDecoder::open(config).map_err(Into::into)
}

fn platform_label() -> &'static str {
    #[cfg(windows)]
    {
        "DXVA11"
    }
    #[cfg(not(windows))]
    {
        "VA-API"
    }
}

enum FrameReader {
    Cpu,
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "Only the D3D11 decoder owns surfaces.")
    )]
    Windows(windows_surface::D3D11SurfaceWriter),
}
