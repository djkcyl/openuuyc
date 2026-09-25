//! Rust AVC software fallback, sharing the project's H.264 core.
use super::{
    encoder::Encoded,
    format::{Format, Rate},
};
use anyhow::{Result, ensure};
use openuuyc_h264::encoder::{Config, Encoder as Core};
pub(super) const MAXIMUM: (u32, u32) = (3840, 2160);
pub(super) const MAX_FPS: u32 = 144;
pub(crate) struct Encoder {
    core: Core,
    pub width: u32,
    pub height: u32,
    rate: Rate,
    planar: Vec<u8>,
}
impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let core = Core::new(Config {
            width,
            height,
            fps,
            bitrate,
        })?;
        Ok(Self {
            core,
            width,
            height,
            rate: Rate {
                target: bitrate,
                peak: bitrate,
                fps,
                quality: 0,
            },
            planar: vec![0; width as usize * height as usize * 3 / 2],
        })
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        self.core.configure(rate.fps, rate.target)?;
        let keyframe = self.rate.quality != rate.quality;
        self.rate = rate;
        Ok(keyframe)
    }
    pub fn encode(&mut self, bgra: &[u8], timestamp: i64, keyframe: bool) -> Result<Vec<Encoded>> {
        let luma = self.width as usize * self.height as usize;
        ensure!(bgra.len() == luma * 4, "Rust H264输入尺寸不符");
        let (y, uv) = self.planar.split_at_mut(luma);
        let (u, v) = uv.split_at_mut(luma / 4);
        yuv::bgra_to_yuv420(
            &mut yuv::YuvPlanarImageMut {
                y_plane: yuv::BufferStoreMut::Borrowed(y),
                y_stride: self.width,
                u_plane: yuv::BufferStoreMut::Borrowed(u),
                u_stride: self.width / 2,
                v_plane: yuv::BufferStoreMut::Borrowed(v),
                v_stride: self.width / 2,
                width: self.width,
                height: self.height,
            },
            bgra,
            self.width * 4,
            yuv::YuvRange::Limited,
            yuv::YuvStandardMatrix::Bt601,
            yuv::YuvConversionMode::Balanced,
        )?;

        let Some(unit) = self.core.encode(
            &self.planar[..luma],
            &self.planar[luma..luma + luma / 4],
            &self.planar[luma + luma / 4..],
            timestamp,
            keyframe,
        )?
        else {
            return Ok(Vec::new());
        };
        Ok(vec![Encoded {
            data: unit.data,
            keyframe: unit.keyframe,
            timestamp_100ns: unit.timestamp_100ns,
            is_new: true,
            timing: None,
            format: Format::AVC,
            color: Format::AVC.color(None),
        }])
    }
}
