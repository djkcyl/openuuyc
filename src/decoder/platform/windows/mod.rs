//! Pure Rust D3D11 Video and H.264 software decoding.
//! No MFT, native video bridge or implicit backend fallback.
#![cfg(windows)]
use crate::decoder::platform::{
    DecodeError, DecoderNotification, VideoDecoder, VideoDecoderConfig, VideoOutputPreference,
};
use mediaway_common::{
    Bytes, CodecKind, GpuDeviceHandle, Packet, PixelFormat, VideoFrame, VideoFrameStorage,
};
use std::collections::VecDeque;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
mod rust_dxva;
pub struct WindowsGpuVideoFrame {
    texture: ID3D11Texture2D,
    subresource: u32,
    pts: i64,
    visible_x: u32,
    visible_y: u32,
    width: u32,
    height: u32,
    _lease: std::sync::Arc<rust_dxva::dxva::Lease>,
}
unsafe impl Send for WindowsGpuVideoFrame {}
impl WindowsGpuVideoFrame {
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }
    pub const fn subresource(&self) -> u32 {
        self.subresource
    }
    pub const fn pts(&self) -> i64 {
        self.pts
    }

    pub const fn width(&self) -> u32 {
        self.width
    }
    pub const fn height(&self) -> u32 {
        self.height
    }
    pub const fn visible_x(&self) -> u32 {
        self.visible_x
    }
    pub const fn visible_y(&self) -> u32 {
        self.visible_y
    }
}
impl std::fmt::Debug for WindowsGpuVideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsGpuVideoFrame")
            .field("pts", &self.pts)
            .field("subresource", &self.subresource)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsCpuFormat {
    Nv12,
    I444,
}
pub struct WindowsCpuVideoFrame {
    pub pts: i64,
    pub duration: u64,
    pub width: u32,
    pub height: u32,
    pub format: WindowsCpuFormat,
    pub data: Bytes,
}
pub enum WindowsDecodedFrame {
    Gpu(WindowsGpuVideoFrame),
    Cpu(WindowsCpuVideoFrame),
}
pub struct WindowsVideoDecoder {
    backend: Backend,
    pending: VecDeque<WindowsDecodedFrame>,
    notification: DecoderNotification,
}
enum Backend {
    Hardware(Box<rust_dxva::Session>),
    Software(Box<openuuyc_h264::stream::Decoder>),
}
fn software_error(error: openuuyc_h264::Error) -> DecodeError {
    use openuuyc_h264::Error;
    match error {
        Error::Cancelled | Error::Closed => DecodeError::Closed,
        Error::Unsupported(_) => DecodeError::Unsupported,
        Error::Allocation => DecodeError::Backend,
        Error::NeedKeyframe | Error::Truncated | Error::Invalid(_) => {
            tracing::debug!(%error,"Rust H264 input requires a new keyframe");
            DecodeError::NeedKeyframe
        }
    }
}
impl WindowsVideoDecoder {
    pub fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        let backend = match config.output {
            VideoOutputPreference::CpuFramesOk => {
                if config.codec != CodecKind::H264 {
                    return Err(DecodeError::Unsupported);
                }
                if config.width == 0
                    || config.height == 0
                    || config.width > 16384
                    || config.height > 16384
                {
                    return Err(DecodeError::InvalidInput);
                }
                let mut decoder = openuuyc_h264::stream::Decoder::new();
                decoder.seed(&config.extra_data).map_err(software_error)?;
                Backend::Software(Box::new(decoder))
            }
            VideoOutputPreference::ZeroCopyGpu => {
                Backend::Hardware(Box::new(rust_dxva::Session::open(config)?))
            }
        };
        Ok(Self {
            backend,
            pending: VecDeque::new(),
            notification: DecoderNotification::default(),
        })
    }
    pub fn probe(
        device: GpuDeviceHandle,
        codec: CodecKind,
        width: u32,
        height: u32,
        depth: u8,
    ) -> bool {
        rust_dxva::Session::probe(device, codec, width, height, depth)
    }
    pub fn poll_owned_frame(&mut self) -> Result<Option<WindowsDecodedFrame>, DecodeError> {
        if self.notification.is_cancelled() {
            self.pending.clear();
            return Err(DecodeError::Closed);
        }
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }
        match &mut self.backend {
            Backend::Hardware(session) => session.poll(),
            Backend::Software(_) => Ok(None),
        }
    }
    pub fn poll_dropped_token(&mut self) -> Option<i64> {
        match &mut self.backend {
            Backend::Hardware(session) => session.poll_dropped(),
            Backend::Software(_) => None,
        }
    }
    pub fn reset_for_keyframe(&mut self) -> Result<(), DecodeError> {
        self.pending.clear();
        match &mut self.backend {
            Backend::Hardware(session) => {
                session.reset();
                Ok(())
            }
            Backend::Software(session) => session.reset().map_err(software_error),
        }
    }
}
impl VideoDecoder for WindowsVideoDecoder {
    fn set_notification(&mut self, notification: DecoderNotification) {
        self.notification = notification;
    }
    fn push_packet(&mut self, packet: &Packet) -> Result<(), DecodeError> {
        match &mut self.backend {
            Backend::Hardware(session) => session.push(packet, &self.notification),
            Backend::Software(session) => {
                if packet.payload.len() > i32::MAX as usize || packet.duration > i64::MAX as u64 {
                    return Err(DecodeError::InvalidInput);
                }
                let outputs = session
                    .submit_with_cancel(
                        &packet.payload,
                        packet.pts as u64,
                        self.notification.cancellation(),
                    )
                    .map_err(software_error)?;
                for output in outputs {
                    let picture = output.picture;
                    let mut packed = Vec::new();
                    picture.pack_into(&mut packed).map_err(software_error)?;
                    if self.notification.is_cancelled() {
                        self.pending.clear();
                        session.reset().map_err(software_error)?;
                        return Err(DecodeError::Closed);
                    }
                    self.pending
                        .push_back(WindowsDecodedFrame::Cpu(WindowsCpuVideoFrame {
                            pts: output.token as i64,
                            duration: packet.duration,
                            width: picture.crop.width as u32,
                            height: picture.crop.height as u32,
                            format: if picture.chroma == openuuyc_h264::picture::Chroma::Yuv444 {
                                WindowsCpuFormat::I444
                            } else {
                                WindowsCpuFormat::Nv12
                            },
                            data: Bytes::from(packed),
                        }));
                }
                Ok(())
            }
        }
    }
    fn poll_frame(&mut self) -> Result<Option<VideoFrame>, DecodeError> {
        match self.poll_owned_frame()? {
            None => Ok(None),
            Some(WindowsDecodedFrame::Cpu(frame)) if frame.format == WindowsCpuFormat::Nv12 => {
                Ok(Some(VideoFrame {
                    pts: frame.pts,
                    duration: frame.duration,
                    width: frame.width,
                    height: frame.height,
                    format: PixelFormat::Nv12,
                    storage: VideoFrameStorage::Cpu { data: frame.data },
                }))
            }
            _ => Err(DecodeError::Unsupported),
        }
    }
}
impl Drop for WindowsVideoDecoder {
    fn drop(&mut self) {
        self.pending.clear();
    }
}
#[cfg(test)]
mod tests;
