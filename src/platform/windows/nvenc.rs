//! NVENC on the capture adapter. Raw desktop pixels never leave D3D11.
//! The session itself (presets, rate control, encoding) is shared with the
//! other platforms in `media::encoding::nvenc`.
use super::{
    format::{Backend, Format, Rate},
    gpu_conversion::Conversion,
};
use crate::media::encoding::nvenc::{
    NV_ENC_BUFFER_FORMAT, NV_ENC_DEVICE_TYPE, NV_ENC_INPUT_RESOURCE_TYPE, Registered, Session,
};
use anyhow::{Result, ensure};
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

pub(crate) struct Encoder {
    session: Session,
    registered: Option<Registered>,
    conversion: Conversion,
}

#[derive(Debug, thiserror::Error)]
#[error("NVENC输入候选被拒绝：{0}")]
struct InputRejected(String);

impl Encoder {
    pub(crate) fn new_format(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        rate: Rate,
        format: Format,
    ) -> Result<Self> {
        let conversion = Conversion::new(device, (width, height), format)?;
        let compute = conversion.is_compute();
        match Self::create(device, width, height, rate, format, conversion) {
            Err(error) if compute && error.downcast_ref::<InputRejected>().is_some() => {
                tracing::debug!(%error, "NVENC registered compute input rejected; trying pixel candidate");
                Self::create(
                    device,
                    width,
                    height,
                    rate,
                    format,
                    Conversion::pixel(device, (width, height), format, 1)?,
                )
            }
            result => result,
        }
    }
    fn create(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        rate: Rate,
        format: Format,
        conversion: Conversion,
    ) -> Result<Self> {
        ensure!(
            Backend::Nvidia.accepts(format),
            "NVENC当前D3D输入路径不支持该格式"
        );
        let session = Session::open(
            NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device.as_raw(),
            width,
            height,
            rate,
            format,
        )?;
        let registered = session
            .register(
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                conversion.output.as_raw(),
                0,
                match (format.chroma, format.depth) {
                    (1, 10) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
                    (3, 8) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_AYUV,
                    _ => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
                },
            )
            .map_err(|error| InputRejected(format!("{error:#}")))?;
        Ok(Self {
            session,
            registered: Some(registered),
            conversion,
        })
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        self.session.maximum_size()
    }

    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        self.session.configure_rate(rate)
    }

    pub(crate) fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_100ns: i64,
        keyframe: bool,
    ) -> Result<Vec<super::encoder::Encoded>> {
        self.conversion.convert(texture)?;
        let registered = self.registered.as_ref().expect("registered until drop");
        self.session.encode(registered, timestamp_100ns, keyframe)
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        // The input is unregistered before its session is destroyed.
        if let Some(registered) = self.registered.take() {
            self.session.unregister(registered);
        }
    }
}
