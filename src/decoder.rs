pub(crate) mod platform;

use crate::decoder::platform::{
    DecodeError, VideoDecoder, VideoDecoderConfig, VideoOutputPreference,
};
use anyhow::{Context, Result, anyhow, bail};
use mediaway_common::{
    Bytes, CodecKind, Packet, PixelFormat, Rational, VideoFrame, VideoFrameStorage,
};
use yuv::{YuvBiPlanarImage, YuvConversionMode, YuvRange, YuvStandardMatrix, yuv_nv12_to_rgba};

use crate::capability::{CodecCapability, DeviceCapability, DisplayCapability, QUALITY_DIMENSIONS};
use crate::media::{CodecPreference, ConnectionMediaProfile, VideoCodec};
use crate::rtc::EncodedVideoFrame;
use crate::video_color::{ColorMatrix, RenderColor};

#[cfg(windows)]
pub(crate) mod software_slot;
#[cfg(windows)]
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
    #[cfg(any(windows, target_os = "macos"))]
    Dropped(i64),
    Failed {
        token: Option<i64>,
        error: anyhow::Error,
    },
}

#[derive(Debug)]
pub(crate) enum DecodedSurface {
    CpuNv12(Bytes),
    #[cfg(windows)]
    CpuI444(Bytes),
    #[cfg(windows)]
    D3D11(windows_surface::D3D11Surface),
}

#[derive(Debug)]
pub(crate) enum RenderSurface {
    CpuRgba8(Vec<Rgba8>),
    #[cfg(windows)]
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
            #[cfg(windows)]
            Self::CpuI444(data) => {
                i444_to_rgba_pixels(width, height, &data, color).map(RenderSurface::CpuRgba8)
            }
            Self::CpuNv12(data) => {
                // The VideoToolbox adapter requests and validates the VideoRange
                // destination pixel format, regardless of the source range.
                #[cfg(target_os = "macos")]
                let color = RenderColor {
                    full_range: false,
                    ..color
                };
                nv12_to_rgba_pixels(width, height, &data, color).map(RenderSurface::CpuRgba8)
            }
            #[cfg(windows)]
            Self::D3D11(surface) => Ok(RenderSurface::D3D11(surface)),
        }
    }
}

pub(crate) struct NativeVideoDecoder {
    backend: DecoderBackend,
    candidate: DecoderCandidate,
    label: String,
    frame_duration: u64,
    #[cfg(windows)]
    software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
}

/// Local implementation choices, never serialized as invented UU decoder IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecoderCandidate {
    #[cfg(windows)]
    WindowsD3d11,
    #[cfg(target_os = "linux")]
    VulkanH264,
    PlatformMemory,
}

impl DecoderCandidate {
    pub(crate) fn available(codec: VideoCodec, prefer_hardware: bool) -> Vec<Self> {
        let mut candidates = Vec::new();
        if prefer_hardware {
            #[cfg(windows)]
            candidates.push(Self::WindowsD3d11);
            #[cfg(target_os = "linux")]
            if codec == VideoCodec::H264 {
                candidates.push(Self::VulkanH264);
            }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        let _ = codec;
        #[cfg(windows)]
        if codec == VideoCodec::H264 {
            candidates.push(Self::PlatformMemory);
        }
        #[cfg(not(windows))]
        candidates.push(Self::PlatformMemory);
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
            #[cfg(windows)]
            None,
        )
    }

    #[cfg(windows)]
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
        #[cfg(windows)] surface_writer: Option<windows_surface::D3D11SurfaceWriter>,
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
                #[cfg(windows)]
                surface_writer.clone(),
                #[cfg(windows)]
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
    pub(crate) fn open_candidate(
        candidate: DecoderCandidate,
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        extra_data: Bytes,
        #[cfg(windows)] surface_writer: Option<windows_surface::D3D11SurfaceWriter>,
        #[cfg(windows)] software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
    ) -> Result<Self> {
        if width == 0 || height == 0 || frame_rate == 0 {
            return Err(DecodeError::InvalidInput.into());
        }
        let frame_duration = (90_000 / u64::from(frame_rate)).max(1);
        match candidate {
            #[cfg(windows)]
            DecoderCandidate::WindowsD3d11 => {
                let writer = surface_writer
                    .filter(|writer| writer.supports_codec(codec_kind(codec), width, height, 8))
                    .map_or_else(
                        || {
                            windows_surface::D3D11SurfaceWriter::for_codec(
                                codec_kind(codec),
                                width,
                                height,
                            )
                        },
                        Ok,
                    )?;
                Self::open_windows_hardware(
                    codec_kind(codec),
                    width,
                    height,
                    frame_duration,
                    writer,
                    extra_data,
                )
            }
            #[cfg(target_os = "linux")]
            DecoderCandidate::VulkanH264 => {
                if codec != VideoCodec::H264 {
                    return Err(DecodeError::Unsupported.into());
                }
                Ok(Self {
                    backend: DecoderBackend::VulkanH264(Box::new(open_vulkan_h264_decoder()?)),
                    candidate,
                    label: "Vulkan Video H.264 硬解".to_owned(),
                    frame_duration,
                })
            }
            DecoderCandidate::PlatformMemory => {
                #[cfg(windows)]
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
                let label = if cfg!(target_os = "macos") {
                    "VideoToolbox 硬解"
                } else if cfg!(target_os = "linux") {
                    "VA-API 硬解"
                } else {
                    "Rust H.264 软件解码"
                };
                Ok(Self {
                    backend: DecoderBackend::Platform {
                        decoder: Box::new(decoder),
                        frame_reader: FrameReader::Cpu,
                    },
                    candidate,
                    label: label.to_owned(),
                    frame_duration,
                    #[cfg(windows)]
                    software_slot,
                })
            }
        }
    }

    pub(crate) const fn candidate(&self) -> DecoderCandidate {
        self.candidate
    }

    pub(crate) fn is_software(&self) -> bool {
        #[cfg(windows)]
        {
            self.software_slot.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    #[cfg(windows)]
    pub(crate) fn software_slot(&self) -> Option<std::rc::Rc<software_slot::SoftwareSlot>> {
        self.software_slot.clone()
    }

    #[cfg(windows)]
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
            #[cfg(target_os = "linux")]
            DecoderBackend::VulkanH264(_) => {}
        }
    }

    pub(crate) fn reset_for_keyframe(&mut self, hard_reset: bool) -> Result<()> {
        match &mut self.backend {
            DecoderBackend::Platform { decoder, .. } => decoder
                .reset_for_keyframe(hard_reset)
                .with_context(|| format!("reset {} decoder for keyframe cutover", self.label)),
            #[cfg(target_os = "linux")]
            DecoderBackend::VulkanH264(decoder) => {
                let _ = hard_reset;
                // The adapter generation was retired before this call. Flush
                // parser/sorter output from that generation; the complete IDR
                // submitted next performs the H264 reference-picture reset.
                let _ = decoder
                    .flush()
                    .context("flush Vulkan parser and decoded output")?;
                Ok(())
            }
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
            #[cfg(target_os = "linux")]
            DecoderBackend::VulkanH264(decoder) => {
                let input_pts = decode_token as u64;
                let mut batch = DecodedBatch::default();
                let mut output = match decoder
                    .decode(gpu_video::EncodedInputChunk {
                        data: &frame.data,
                        pts: Some(input_pts),
                    })
                    .context("submit Annex-B frame to Vulkan Video decoder")
                {
                    Ok(output) => output,
                    Err(error) => {
                        batch.input_error = Some(error);
                        return batch;
                    }
                };
                match decoder
                    .process_event(gpu_video::DecoderEvent::SignalFrameEnd)
                    .context("finish complete Vulkan H264 access unit")
                {
                    Ok(complete) => output.extend(complete),
                    Err(error) => batch.input_error = Some(error),
                }
                for frame in output {
                    let token = frame.metadata.pts.unwrap_or(input_pts) as i64;
                    match nv12_decoded_frame(
                        token,
                        frame.data.width,
                        frame.data.height,
                        frame.data.frame.into(),
                    ) {
                        Ok(frame) => batch.frames.push(frame),
                        Err(error) => batch.output_issues.push(DecoderOutputIssue::Failed {
                            token: Some(token),
                            error,
                        }),
                    }
                }
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
            #[cfg(target_os = "linux")]
            DecoderBackend::VulkanH264(_) => DecodedBatch::default(),
        }
    }
}

fn poll_platform_decoder(
    decoder: &mut PlatformDecoder,
    frame_reader: &mut FrameReader,
) -> DecodedBatch {
    let mut batch = DecodedBatch::default();
    #[cfg(windows)]
    #[allow(irrefutable_let_patterns)]
    // Windows has one platform variant; Linux/Apple use the loop below.
    if let PlatformDecoder::Windows(decoder) = &mut *decoder {
        use crate::decoder::platform::windows::{WindowsCpuFormat, WindowsDecodedFrame};
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
                WindowsDecodedFrame::Gpu(frame) => (
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
                WindowsDecodedFrame::Cpu(frame) => {
                    let surface = match frame.format {
                        WindowsCpuFormat::Nv12 => {
                            nv12_layout(frame.width, frame.height, &frame.data)
                                .map(|_| DecodedSurface::CpuNv12(frame.data))
                        }
                        WindowsCpuFormat::I444 => Ok(DecodedSurface::CpuI444(frame.data)),
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
        return batch;
    }
    loop {
        let output = match decoder.poll_output().context("poll native decoded output") {
            Ok(Some(output)) => output,
            Ok(None) => break,
            Err(error) => {
                batch
                    .output_issues
                    .push(DecoderOutputIssue::Failed { token: None, error });
                break;
            }
        };
        match output {
            crate::decoder::platform::VideoDecoderOutput::Frame { frame, ready_at } => {
                let token = frame.pts;
                match frame_reader.read_surface(&frame) {
                    Ok(surface) => batch.frames.push(DecodedFrame {
                        pts: token,
                        width: frame.width,
                        height: frame.height,
                        surface,
                        ready_at,
                    }),
                    Err(error) => batch.output_issues.push(DecoderOutputIssue::Failed {
                        token: Some(token),
                        error,
                    }),
                }
            }
            #[cfg(target_os = "macos")]
            crate::decoder::platform::VideoDecoderOutput::Dropped { pts } => {
                batch.output_issues.push(DecoderOutputIssue::Dropped(pts));
            }
            #[cfg(target_os = "macos")]
            crate::decoder::platform::VideoDecoderOutput::Failed { pts, error } => {
                batch.output_issues.push(DecoderOutputIssue::Failed {
                    token: pts,
                    error: error.into(),
                });
            }
        }
    }
    batch
}

#[cfg(windows)]
pub(crate) fn detect_native_decoder_support(
    profile: ConnectionMediaProfile,
) -> Result<DeviceCapability> {
    std::thread::spawn(move || {
        let mut capabilities = Vec::new();
        if profile.hardware_decode
            && let Ok(writers) = windows_surface::D3D11SurfaceWriter::available()
        {
            for (codec, id) in [(VideoCodec::H264, 1), (VideoCodec::H265, 2)] {
                if matches!(
                    (profile.codec, codec),
                    (CodecPreference::H264, VideoCodec::H265)
                        | (CodecPreference::H265, VideoCodec::H264)
                ) {
                    continue;
                }
                for depth in [8, 10] {
                    if codec == VideoCodec::H264 && depth != 8 {
                        continue;
                    }
                    for &(width, height) in QUALITY_DIMENSIONS[1..].iter().rev() {
                        if writers.iter().any(|writer| {
                            writer.supports_codec(
                                codec_kind(codec),
                                width as u32,
                                height as u32,
                                depth,
                            )
                        }) {
                            capabilities.push(CodecCapability {
                                video_codec: id,
                                width,
                                height,
                                chroma_sampling: 1,
                                bit_depth: depth,
                                codec_impl: 32,
                            });
                            break;
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
            display_info: vec![DisplayCapability {
                id: 0,
                fps: profile.local_display.refresh_hz,
                kind: 0,
                hdr: -1,
            }],
            video_codec_capability: capabilities,
        })
    })
    .join()
    .map_err(|_| anyhow!("native capability probe thread panicked"))?
}

#[cfg(not(windows))]
pub(crate) fn detect_native_decoder_support(
    profile: ConnectionMediaProfile,
) -> Result<DeviceCapability> {
    // This is a backend configuration probe, not a measured decode throughput.
    // Keep COM/native backend calls on a dedicated thread, not a Tokio worker.
    std::thread::spawn(move || {
        let mut video_codec_capability = Vec::new();
        for (codec, video_codec) in [(VideoCodec::H264, 1), (VideoCodec::H265, 2)] {
            if matches!(
                (profile.codec, codec),
                (CodecPreference::H264, VideoCodec::H265)
                    | (CodecPreference::H265, VideoCodec::H264)
            ) {
                continue;
            }
            for &(width, height) in QUALITY_DIMENSIONS.iter().rev() {
                let Ok(decoder) = NativeVideoDecoder::open(
                    codec,
                    width as u32,
                    height as u32,
                    profile.stream_fps,
                    profile.hardware_decode,
                    Bytes::new(),
                ) else {
                    continue;
                };
                let codec_impl = match decoder.candidate() {
                    #[cfg(target_os = "macos")]
                    DecoderCandidate::PlatformMemory => 34,
                    // UU has no Vulkan/VA-API identifier. Its generic decoder
                    // classification is 37; never invent a new wire enum.
                    _ => 37,
                };
                video_codec_capability.push(CodecCapability {
                    video_codec,
                    width,
                    height,
                    chroma_sampling: 1,
                    bit_depth: 8,
                    codec_impl,
                });
                break;
            }
        }
        if video_codec_capability.is_empty() {
            bail!(
                "no native decoder accepted a UU-compatible format for the selected decoding mode"
            );
        }
        Ok(DeviceCapability {
            ice_id: String::new(),
            display_info: vec![DisplayCapability {
                id: 0,
                fps: profile.local_display.refresh_hz,
                kind: 0,
                // No HDR output/color-space pipeline is exposed yet. 0 means
                // HDR ready in UU, not false; -1 is unavailable/unknown.
                hdr: -1,
            }],
            video_codec_capability,
        })
    })
    .join()
    .map_err(|_| anyhow!("native capability probe thread panicked"))?
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

#[cfg(target_os = "linux")]
fn nv12_decoded_frame(pts: i64, width: u32, height: u32, nv12: Bytes) -> Result<DecodedFrame> {
    let ready_at = std::time::Instant::now();
    nv12_layout(width, height, &nv12)?;
    Ok(DecodedFrame {
        pts,
        width,
        height,
        surface: DecodedSurface::CpuNv12(nv12),
        ready_at,
    })
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

#[cfg(windows)]
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
    #[cfg(target_os = "linux")]
    VulkanH264(Box<gpu_video::BytesDecoder>),
}

#[cfg(target_os = "linux")]
fn open_vulkan_h264_decoder() -> Result<gpu_video::BytesDecoder> {
    use gpu_video::parameters::{
        DecoderParameters, DecoderUsageFlags, VulkanAdapterDescriptor, VulkanDeviceDescriptor,
    };

    let instance = gpu_video::VulkanInstance::new().context("create Vulkan Video instance")?;
    let adapter = instance
        .create_adapter(&VulkanAdapterDescriptor::default())
        .context("find a Vulkan Video adapter")?;
    let device = adapter
        .create_device(&VulkanDeviceDescriptor::default())
        .context("create Vulkan Video device")?;
    device
        .create_bytes_decoder_h264(DecoderParameters {
            usage_flags: DecoderUsageFlags::STREAMING,
            ..DecoderParameters::default()
        })
        .context("create Vulkan Video H.264 decoder")
}

enum PlatformDecoder {
    #[cfg(windows)]
    Windows(crate::decoder::platform::windows::WindowsVideoDecoder),
    #[cfg(target_os = "macos")]
    Apple(crate::decoder::platform::apple::AppleVideoDecoder),
    #[cfg(target_os = "linux")]
    Linux(crate::decoder::platform::linux::LinuxVideoDecoder),
}

impl PlatformDecoder {
    fn reset_for_keyframe(&mut self, hard: bool) -> std::result::Result<(), DecodeError> {
        #[cfg(not(target_os = "macos"))]
        let _ = hard;
        match self {
            #[cfg(windows)]
            Self::Windows(decoder) => decoder.reset_for_keyframe(),
            // VideoToolbox callbacks carry the generation-packed decode token;
            // VA-API is synchronous and its IDR path clears the DPB. Their stale
            // outputs are rejected by the shared token/generation map.
            #[cfg(target_os = "macos")]
            Self::Apple(decoder) => decoder.reset_for_keyframe(hard),
            #[cfg(target_os = "linux")]
            Self::Linux(_) => Ok(()),
        }
    }
}

impl VideoDecoder for PlatformDecoder {
    fn set_notification(&mut self, notification: crate::decoder::platform::DecoderNotification) {
        match self {
            #[cfg(windows)]
            Self::Windows(decoder) => decoder.set_notification(notification),
            #[cfg(target_os = "macos")]
            Self::Apple(decoder) => decoder.set_notification(notification),
            #[cfg(target_os = "linux")]
            Self::Linux(decoder) => decoder.set_notification(notification),
        }
    }

    fn poll_output(
        &mut self,
    ) -> std::result::Result<Option<crate::decoder::platform::VideoDecoderOutput>, DecodeError>
    {
        match self {
            #[cfg(windows)]
            Self::Windows(decoder) => decoder.poll_output(),
            #[cfg(target_os = "macos")]
            Self::Apple(decoder) => decoder.poll_output(),
            #[cfg(target_os = "linux")]
            Self::Linux(decoder) => decoder.poll_output(),
        }
    }

    fn push_packet(
        &mut self,
        packet: &Packet,
    ) -> std::result::Result<(), crate::decoder::platform::DecodeError> {
        match self {
            #[cfg(windows)]
            Self::Windows(decoder) => decoder.push_packet(packet),
            #[cfg(target_os = "macos")]
            Self::Apple(decoder) => decoder.push_packet(packet),
            #[cfg(target_os = "linux")]
            Self::Linux(decoder) => decoder.push_packet(packet),
        }
    }

    fn poll_frame(
        &mut self,
    ) -> std::result::Result<Option<VideoFrame>, crate::decoder::platform::DecodeError> {
        match self {
            #[cfg(windows)]
            Self::Windows(decoder) => decoder.poll_frame(),
            #[cfg(target_os = "macos")]
            Self::Apple(decoder) => decoder.poll_frame(),
            #[cfg(target_os = "linux")]
            Self::Linux(decoder) => decoder.poll_frame(),
        }
    }
}

fn open_platform_decoder(config: &VideoDecoderConfig) -> Result<PlatformDecoder> {
    #[cfg(windows)]
    return crate::decoder::platform::windows::WindowsVideoDecoder::open(config)
        .map(PlatformDecoder::Windows)
        .map_err(Into::into);
    #[cfg(target_os = "macos")]
    return crate::decoder::platform::apple::AppleVideoDecoder::open(config)
        .map(PlatformDecoder::Apple)
        .map_err(Into::into);
    #[cfg(target_os = "linux")]
    return crate::decoder::platform::linux::LinuxVideoDecoder::open(config)
        .map(PlatformDecoder::Linux)
        .map_err(Into::into);
    #[allow(unreachable_code)]
    Err(anyhow!(
        "native video decoding is unsupported on this platform"
    ))
}

fn platform_label() -> &'static str {
    if cfg!(windows) {
        "DXVA11"
    } else if cfg!(target_os = "macos") {
        "VideoToolbox"
    } else if cfg!(target_os = "linux") {
        "VA-API"
    } else {
        "原生平台"
    }
}

enum FrameReader {
    Cpu,
    #[cfg(windows)]
    Windows(windows_surface::D3D11SurfaceWriter),
}

impl FrameReader {
    fn read_surface(&mut self, frame: &VideoFrame) -> Result<DecodedSurface> {
        if frame.format != PixelFormat::Nv12 {
            bail!(
                "native decoder returned unsupported pixel format: {:?}",
                frame.format
            );
        }
        match (&mut *self, &frame.storage) {
            (Self::Cpu, VideoFrameStorage::Cpu { .. }) => cpu_surface(frame),
            #[cfg(windows)]
            (Self::Windows(_), VideoFrameStorage::Gpu(_)) => {
                bail!("non-owning Windows GPU frame reached the renderer; use poll_owned_frame")
            }
            _ => bail!("native decoder returned an output storage type this renderer cannot read"),
        }
    }
}

fn cpu_surface(frame: &VideoFrame) -> Result<DecodedSurface> {
    if frame.format != PixelFormat::Nv12 {
        bail!(
            "native CPU decoder returned unsupported pixel format: {:?}",
            frame.format
        );
    }
    let VideoFrameStorage::Cpu { data } = &frame.storage else {
        bail!("native decoder returned a GPU frame to the CPU renderer");
    };
    nv12_layout(frame.width, frame.height, data)?;
    Ok(DecodedSurface::CpuNv12(data.clone()))
}
