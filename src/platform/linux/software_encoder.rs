//! CPU input for the unified Rust software codec. No codec algorithms here:
//! H.264 takes the captured BGRA as it is, AV1 takes NV12 (4:2:0) or AYUV
//! (4:4:4), converted here with the matrix and range the stream declares
//! (BT.601, limited range for SDR), as the Windows GPU conversion does.
use super::capture::Image;
use crate::media::encoding::{Encoded, Format, Rate};
use anyhow::{Result, ensure};
use openuuyc_codec::PixelFormat;
use openuuyc_codec::encoder::{Config, Encoder as Core};
use std::sync::{Arc, atomic::AtomicBool};
use yuv::{
    BufferStoreMut, YuvBiPlanarImageMut, YuvChromaSubsampling, YuvConversionMode,
    YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
};

pub(crate) struct Encoder {
    core: Core,
    /// The converted picture handed to the codec, reused between frames.
    input: Vec<u8>,
    size: (u32, u32),
    format: Format,
    rate: Rate,
}
impl Encoder {
    /// The formats this side can feed: Linux captures 8-bit SDR only.
    pub fn accepts(format: Format) -> bool {
        format.software().can_encode() && format.depth == 8
    }

    pub fn new(size: (u32, u32), format: Format, rate: Rate) -> Result<Self> {
        ensure!(Self::accepts(format), "软件编码不支持该格式");
        let core = Core::new(Config {
            width: size.0,
            height: size.1,
            format: format.software(),
            fps: rate.fps,
            bitrate: rate.target,
        })?;
        Ok(Self {
            core,
            input: Vec::new(),
            size,
            format,
            rate,
        })
    }
    pub fn request_keyframe(&mut self) {
        self.core.request_keyframe();
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        openuuyc_codec::encoder::maximum_size(self.format.codec.media())
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        if rate.target != self.rate.target || rate.fps != self.rate.fps {
            self.core.configure(rate.fps, rate.target)?;
        }
        let key = self.rate.quality != rate.quality;
        self.rate = rate;
        Ok(key)
    }
    pub fn encode(
        &mut self,
        image: &Image,
        timestamp: i64,
        key: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Vec<Encoded>> {
        ensure!(image.size() == self.size, "采集画面尺寸与编码器不一致");
        if key {
            self.core.request_keyframe();
        }
        let bgra = image.pixels()?;
        let width = self.size.0;
        let pitch = match self.core.input_format() {
            PixelFormat::Bgra => {
                self.core.prepare(&bgra, width as usize * 4, cancel)?;
                None
            }
            PixelFormat::Nv12 => Some(self.nv12(&bgra)?),
            PixelFormat::Ayuv => Some(self.ayuv(&bgra)?),
            other => anyhow::bail!("软件编码输入格式 {other:?} 没有转换"),
        };
        if let Some(pitch) = pitch {
            self.core.prepare(&self.input, pitch, cancel)?;
        }
        Ok(self
            .core
            .encode(timestamp, key, cancel)?
            .into_iter()
            .map(|p| Encoded {
                data: p.data,
                keyframe: p.keyframe,
                timestamp_100ns: p.timestamp_100ns,
                is_new: true,
                timing: None,
                format: self.format,
                color: self.format.color(None),
            })
            .collect())
    }

    /// Luma rows, then interleaved UV rows, at a pitch of the width.
    fn nv12(&mut self, bgra: &[u8]) -> Result<usize> {
        let (width, height) = self.size;
        let (w, h) = (width as usize, height as usize);
        self.input.resize(w * h * 3 / 2, 0);
        let (y, uv) = self.input.split_at_mut(w * h);
        let mut image = YuvBiPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(y),
            y_stride: width,
            uv_plane: BufferStoreMut::Borrowed(uv),
            uv_stride: width,
            width,
            height,
        };
        yuv::bgra_to_yuv_nv12(
            &mut image,
            bgra,
            width * 4,
            YuvRange::Limited,
            YuvStandardMatrix::Bt601,
            YuvConversionMode::Balanced,
        )?;
        Ok(w)
    }

    /// Packed V, U, Y, A bytes per pixel, the order AYUV has in memory.
    fn ayuv(&mut self, bgra: &[u8]) -> Result<usize> {
        let (width, height) = self.size;
        let mut planar =
            YuvPlanarImageMut::<u8>::alloc(width, height, YuvChromaSubsampling::Yuv444);
        yuv::bgra_to_yuv444(
            &mut planar,
            bgra,
            width * 4,
            YuvRange::Limited,
            YuvStandardMatrix::Bt601,
            YuvConversionMode::Balanced,
        )?;
        let pixels = width as usize * height as usize;
        self.input.resize(pixels * 4, 0);
        let (y, u, v) = (
            planar.y_plane.borrow(),
            planar.u_plane.borrow(),
            planar.v_plane.borrow(),
        );
        for (i, pixel) in self.input.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[v[i], u[i], y[i], 255]);
        }
        Ok(width as usize * 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::encoding::{Codec, QualityTarget};

    /// The converted input must carry the matrix and range the stream
    /// declares: BT.601, limited range. Solid colours go through encoding and
    /// the shared software decoder, then the decoded samples are compared.
    #[test]
    fn software_colour_matches_the_declared_matrix() {
        let (width, height) = (128u32, 128u32);
        let rate = Rate {
            target: 8_000_000,
            peak: 8_000_000,
            fps: 30,
            quality: 4,
            quality_target: QualityTarget {
                bitrate: 8_000_000,
                fps: 30,
            },
        };
        for format in [
            Format::AVC,
            Format {
                codec: Codec::Av1,
                chroma: 1,
                depth: 8,
            },
            Format {
                codec: Codec::Av1,
                chroma: 3,
                depth: 8,
            },
        ] {
            for (name, bgr, expected) in [
                ("red", [0u8, 0, 255], [81u8, 90, 240]),
                ("green", [0, 255, 0], [145, 54, 34]),
                ("blue", [255, 0, 0], [41, 240, 110]),
                ("white", [255, 255, 255], [235, 128, 128]),
            ] {
                let mut pixels = Vec::with_capacity((width * height * 4) as usize);
                for _ in 0..width * height {
                    pixels.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
                }
                let image = Image::Cpu {
                    pixels: Arc::new(pixels),
                    width,
                    height,
                };
                let mut encoder = Encoder::new((width, height), format, rate).unwrap();
                let cancel = Arc::new(AtomicBool::new(false));
                let encoded = encoder.encode(&image, 0, true, &cancel).unwrap();
                let mut decoder =
                    openuuyc_codec::decoder::Decoder::new(format.software().codec, &[]).unwrap();
                let frames = decoder.push(&encoded[0].data, 0).unwrap();
                let frame = &frames[0];
                let (w, h) = (frame.coded_width as usize, frame.coded_height as usize);
                let centre = (h / 2) * w + w / 2;
                let actual = match frame.format {
                    PixelFormat::Nv12 => {
                        let uv = w * h + (h / 4) * w + (w / 2 & !1);
                        [frame.data[centre], frame.data[uv], frame.data[uv + 1]]
                    }
                    PixelFormat::I444 => [
                        frame.data[centre],
                        frame.data[w * h + centre],
                        frame.data[2 * w * h + centre],
                    ],
                    PixelFormat::Ayuv => {
                        let p = &frame.data[centre * 4..centre * 4 + 4];
                        [p[2], p[1], p[0]]
                    }
                    other => panic!("unexpected decoded format {other:?}"),
                };
                println!("{format:?} {name}: {actual:?}, BT.601 limited {expected:?}");
                assert!(
                    actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 4),
                    "{format:?} {name} is not BT.601 limited"
                );
            }
        }
    }
}
