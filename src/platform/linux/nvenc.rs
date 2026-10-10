//! NVENC with CUDA input. The session (presets, rate control, encoding) is
//! the one Windows uses, from `media::encoding::nvenc`; this side owns a
//! pitched device buffer registered as the encoder's input. NvFBC frames are
//! copied into it on the GPU, frames from other capture methods are uploaded.
//!
//! The input is 8-bit BGRA ("ARGB" in NVENC's word order), which NVENC
//! converts to the negotiated 4:2:0 or 4:4:4 itself.
use super::capture::Image;
use super::cuda::{Buffer, Context};
use crate::media::encoding::nvenc::{
    NV_ENC_BUFFER_FORMAT, NV_ENC_DEVICE_TYPE, NV_ENC_INPUT_RESOURCE_TYPE, Registered, Session,
};
use crate::media::encoding::{Color, Encoded, Format, Rate};
use anyhow::{Result, ensure};
use std::sync::Arc;

pub(crate) struct Encoder {
    /// Destroyed in `drop` while the CUDA context is current.
    session: std::mem::ManuallyDrop<Session>,
    registered: Option<Registered>,
    input: Buffer,
    context: Arc<Context>,
}

impl Encoder {
    /// The formats NVENC encodes from BGRA input.
    pub fn accepts(format: Format) -> bool {
        format.valid() && format.depth == 8
    }

    pub fn new(
        context: &Arc<Context>,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        ensure!(Self::accepts(format), "NVENC 的 CUDA 输入只支持 8 位格式");
        let input = Buffer::new(context, size.0, size.1)?;
        let _current = context.enter()?;
        let session = Session::open(
            NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            context.raw(),
            size.0,
            size.1,
            rate,
            format,
            Color::Sdr,
        )?;
        let registered = session.register(
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            input.ptr as *mut std::ffi::c_void,
            u32::try_from(input.pitch)?,
            NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
        )?;
        Ok(Self {
            session: std::mem::ManuallyDrop::new(session),
            registered: Some(registered),
            input,
            context: context.clone(),
        })
    }

    pub fn maximum_size(&self) -> (u32, u32) {
        self.session.maximum_size()
    }

    pub fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        let _current = self.context.enter()?;
        self.session.configure_rate(rate)
    }

    pub fn encode(
        &mut self,
        image: &Image,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        let (width, height) = image.size();
        ensure!(
            (width, height) == (self.input.width, self.input.height),
            "采集画面尺寸与编码器不一致"
        );
        match image {
            Image::Cpu { pixels, .. } => self.input.upload(pixels)?,
            Image::Cuda(buffer) => self.input.copy_from_device(buffer.ptr, buffer.pitch)?,
        }
        let _current = self.context.enter()?;
        let registered = self.registered.as_ref().expect("registered until drop");
        self.session.encode(registered, timestamp, keyframe)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _current = self.context.enter();
        // The input is unregistered before its session is destroyed, both
        // with the session's CUDA context current.
        if let Some(registered) = self.registered.take() {
            self.session.unregister(registered);
        }
        // SAFETY: the session is not used again.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.session) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an NVIDIA GPU: `cargo test -- --ignored nvenc_colour`.
    /// NVENC converts the BGRA input itself; its matrix and range must be the
    /// ones the stream's VUI declares (BT.601, limited range for SDR).
    #[test]
    #[ignore]
    fn nvenc_colour_matches_the_declared_matrix() {
        let context = super::super::cuda::display_context().unwrap();
        let (width, height) = (256u32, 256u32);
        let rate = Rate {
            target: 20_000_000,
            peak: 20_000_000,
            fps: 30,
            quality: 4,
            quality_target: crate::media::encoding::QualityTarget {
                bitrate: 20_000_000,
                fps: 60,
            },
        };
        let colour = Color::Sdr.space(None);
        println!("declared VUI: {colour:?}");
        for (name, bgr, bt601, bt709) in [
            ("red", [0u8, 0, 255], (81, 90, 240), (63, 102, 240)),
            ("green", [0, 255, 0], (145, 54, 34), (173, 42, 26)),
            ("blue", [255, 0, 0], (41, 240, 110), (32, 240, 118)),
            ("white", [255, 255, 255], (235, 128, 128), (235, 128, 128)),
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
            let mut encoder = Encoder::new(
                &context,
                (width, height),
                crate::media::encoding::Format::AVC,
                rate,
            )
            .unwrap();
            let encoded = encoder.encode(&image, 0, true).unwrap();
            let mut decoder = openuuyc_h264::stream::Decoder::new();
            let outputs = decoder.submit(&encoded[0].data, 0).unwrap();
            let outputs = if outputs.is_empty() {
                decoder.finish().unwrap()
            } else {
                outputs
            };
            let mut nv12 = Vec::new();
            outputs[0].picture.pack_into(&mut nv12).unwrap();
            let luma = (width * height) as usize;
            let (y, u, v) = (nv12[luma / 2 + 128], nv12[luma + 1000], nv12[luma + 1001]);
            println!("{name}: Y {y} U {u} V {v}   BT.601 {bt601:?}   BT.709 {bt709:?}");
            let near = |a: u8, b: i32| (i32::from(a) - b).abs() <= 3;
            assert!(
                near(y, bt601.0) && near(u, bt601.1) && near(v, bt601.2),
                "{name} is not BT.601 limited"
            );
        }
    }
}

#[cfg(test)]
mod memory_tests {
    /// Stage by stage, so an outside `nvidia-smi` can attribute video memory.
    #[test]
    #[ignore]
    fn video_memory_by_stage() {
        let pause = |stage: &str| {
            println!("STAGE {stage}");
            std::thread::sleep(std::time::Duration::from_secs(4));
        };
        pause("start");
        let context = super::super::cuda::display_context().unwrap();
        pause("cuda-context");
        let (w, h) = (2560, 1440);
        let buffer = super::super::cuda::Buffer::new(&context, w, h).unwrap();
        pause("cuda-buffer");
        let rate = crate::media::encoding::Rate {
            target: 20_000_000,
            peak: 20_000_000,
            fps: 60,
            quality: 4,
            quality_target: crate::media::encoding::QualityTarget {
                bitrate: 20_000_000,
                fps: 60,
            },
        };
        let encoder =
            super::Encoder::new(&context, (w, h), crate::media::encoding::Format::AVC, rate)
                .unwrap();
        pause("nvenc-h264-1440p");
        let screens = super::super::capture::screens().unwrap();
        let desktop = super::super::capture::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend {}", desktop.backend_name());
        pause("nvfbc");
        drop((desktop, encoder, buffer));
        pause("dropped");
    }
}
