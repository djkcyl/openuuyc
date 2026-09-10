//! Rust D3D11 decoding for all Windows hardware playback.
//! Hardware preparation never calls the native video bridge.
mod avc;
pub(super) mod dxva;
mod hevc;
mod output;
mod params;

use super::*;
use std::ffi::c_void;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::core::Interface;

enum Decoder {
    Avc(avc::Avc),
    Hevc(hevc::Hevc),
}
pub(super) struct Session {
    decoder: Decoder,
    output: output::Output,
    extra: Option<Bytes>,
}
impl Session {
    pub(super) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        if config.width == 0 || config.height == 0 || config.width > 16384 || config.height > 16384
        {
            return Err(DecodeError::InvalidInput);
        }
        let Some(GpuDeviceHandle::DirectX11(handle)) = config.gpu_device else {
            return Err(DecodeError::InvalidInput);
        };
        let raw = handle.get() as *mut c_void;
        let device = unsafe { ID3D11Device::from_raw_borrowed(&raw) }
            .ok_or(DecodeError::InvalidInput)?
            .clone();
        if !config.extra_data.is_empty()
            && !config.extra_data.starts_with(&[0, 0, 1])
            && !config.extra_data.starts_with(&[0, 0, 0, 1])
        {
            return Err(DecodeError::Unsupported);
        }
        let decoder = match config.codec {
            CodecKind::H264 => Decoder::Avc(avc::Avc::new(device)),
            CodecKind::Hevc => Decoder::Hevc(hevc::Hevc::new(device)),
            _ => return Err(DecodeError::Unsupported),
        };
        tracing::info!(codec=?config.codec, "Rust DXVA session created");
        Ok(Self {
            decoder,
            output: output::Output::default(),
            extra: Some(config.extra_data.clone()),
        })
    }
    pub(super) fn probe(
        handle: GpuDeviceHandle,
        codec: CodecKind,
        w: u32,
        h: u32,
        depth: u8,
    ) -> bool {
        let GpuDeviceHandle::DirectX11(handle) = handle else {
            return false;
        };
        let raw = handle.get() as *mut c_void;
        let Some(device) = (unsafe { ID3D11Device::from_raw_borrowed(&raw) }) else {
            return false;
        };
        let codec = match codec {
            CodecKind::H264 => dxva::Codec::H264,
            CodecKind::Hevc => dxva::Codec::Hevc,
            _ => return false,
        };
        dxva::Pool::probe(device, codec, w, h, depth).is_ok()
    }
    pub(super) fn push(
        &mut self,
        packet: &Packet,
        notification: &DecoderNotification,
    ) -> Result<(), DecodeError> {
        if notification.is_cancelled() {
            return Err(DecodeError::Closed);
        }
        let extra = self.extra.take().filter(|e| !e.is_empty());
        let combined;
        let data = if let Some(extra) = extra {
            combined = [extra.as_ref(), packet.payload.as_ref()].concat();
            &combined[..]
        } else {
            packet.payload.as_ref()
        };
        let decoded = match &mut self.decoder {
            Decoder::Avc(d) => d.decode(data, notification.cancellation()).map(Some),
            Decoder::Hevc(d) => d.decode(data, notification.cancellation()),
        };
        let result = decoded.and_then(|picture| {
            if let Some(picture) = picture {
                self.output.push(output::Frame {
                    picture,
                    token: packet.pts,
                })
            } else {
                self.output.discard_token(packet.pts);
                Ok(())
            }
        });
        result.map_err(|error| {
            self.reset();
            if notification.is_cancelled() {
                return DecodeError::Closed;
            }
            tracing::warn!(detail=%error, "Rust DXVA decode failed");
            match error.downcast_ref::<dxva::Failure>() {
                Some(dxva::Failure::Unsupported) => DecodeError::Unsupported,
                Some(dxva::Failure::HardwareFailure) => DecodeError::HardwareFailure,
                None if error.downcast_ref::<windows::core::Error>().is_some() => {
                    DecodeError::HardwareFailure
                }
                None => DecodeError::NeedKeyframe,
            }
        })
    }
    pub(super) fn poll(&mut self) -> Result<Option<WindowsDecodedFrame>, DecodeError> {
        let Some(frame) = self.output.poll() else {
            return Ok(None);
        };
        let picture = frame.picture;
        // Keep queued pictures in the decode DPB. The smaller shared display
        // pool is acquired only once the picture is actually ready for output.
        let pool = std::sync::Arc::clone(&picture.surface.pool);
        let surface = pool.display(picture.surface).map_err(|error| {
            tracing::warn!(detail=%error, "Rust DXVA output transfer failed");
            self.reset();
            DecodeError::HardwareFailure
        })?;
        Ok(Some(WindowsDecodedFrame::Gpu(WindowsGpuVideoFrame {
            texture: surface.texture().clone(),
            subresource: surface.subresource(),
            pts: frame.token,
            visible_x: picture.left,
            visible_y: picture.top,
            width: picture.width,
            height: picture.height,
            _lease: surface,
        })))
    }
    pub(super) fn reset(&mut self) {
        self.output.reset();
        match &mut self.decoder {
            Decoder::Avc(d) => d.reset(),
            Decoder::Hevc(d) => d.reset(),
        }
    }
    pub(super) fn poll_dropped(&mut self) -> Option<i64> {
        self.output.poll_dropped()
    }
}
