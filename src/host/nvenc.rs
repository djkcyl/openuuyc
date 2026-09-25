//! NVENC on the capture adapter. Raw desktop pixels never leave D3D11.
use super::{
    format::{Backend, Codec, Format, Rate},
    gpu_conversion::Conversion,
};
use anyhow::{Context, Result, bail, ensure};
use openuuyc_nvenc_sys as nv;
use std::{
    ffi::{CStr, c_void},
    ptr,
};
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

struct Api {
    functions: nv::NV_ENCODE_API_FUNCTION_LIST,
    level: u32,
    version: u32,
    _library: libloading::os::windows::Library,
}

impl Api {
    fn load() -> Result<Self> {
        unsafe {
            // Driver installation is the only DLL source; never search the working directory.
            let library =
                libloading::os::windows::Library::load_with_flags("nvEncodeAPI64.dll", 0x00000800)
                    .context("未找到 NVIDIA NVENC 驱动")?;
            let maximum = library.get::<unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS>(
                b"NvEncodeAPIGetMaxSupportedVersion\0",
            )?;
            let mut supported = 0;
            ensure!(
                maximum(&mut supported) == nv::NVENCSTATUS::NV_ENC_SUCCESS,
                "查询 NVENC API 失败"
            );
            ensure!(supported >= 0x80, "NVIDIA 驱动不支持NVENC API 8.0");
            let level = supported.min(0xc1);
            let version = (level >> 4) | ((level & 15) << 24);
            let create = library.get::<unsafe extern "C" fn(
                *mut nv::NV_ENCODE_API_FUNCTION_LIST,
            ) -> nv::NVENCSTATUS>(b"NvEncodeAPICreateInstance\0")?;
            let mut functions = nv::NV_ENCODE_API_FUNCTION_LIST::default();
            functions.version = version | 0x70020000;
            ensure!(
                create(&mut functions) == nv::NVENCSTATUS::NV_ENC_SUCCESS,
                "创建 NVENC API 失败"
            );
            ensure!(
                functions.nvEncOpenEncodeSessionEx.is_some()
                    && (if level >= 0xa0 {
                        functions.nvEncGetEncodePresetConfigEx.is_some()
                    } else {
                        functions.nvEncGetEncodePresetConfig.is_some()
                    })
                    && functions.nvEncGetEncodeCaps.is_some()
                    && functions.nvEncInitializeEncoder.is_some()
                    && functions.nvEncCreateBitstreamBuffer.is_some()
                    && functions.nvEncDestroyBitstreamBuffer.is_some()
                    && functions.nvEncRegisterResource.is_some()
                    && functions.nvEncUnregisterResource.is_some()
                    && functions.nvEncMapInputResource.is_some()
                    && functions.nvEncUnmapInputResource.is_some()
                    && functions.nvEncEncodePicture.is_some()
                    && functions.nvEncLockBitstream.is_some()
                    && functions.nvEncUnlockBitstream.is_some()
                    && functions.nvEncReconfigureEncoder.is_some()
                    && functions.nvEncDestroyEncoder.is_some(),
                "NVENC 驱动接口不完整"
            );
            Ok(Self {
                functions,
                level,
                version,
                _library: library,
            })
        }
    }
    fn structure(&self, revision: u32, high: bool) -> u32 {
        self.version | 0x70000000 | (revision << 16) | if high { 1 << 31 } else { 0 }
    }
    fn config_version(&self) -> u32 {
        self.structure(
            if self.level == 0x80 {
                6
            } else if self.level < 0xc0 {
                7
            } else {
                8
            },
            true,
        )
    }
    fn init_version(&self) -> u32 {
        self.structure(if self.level < 0xc1 { 5 } else { 6 }, true)
    }
    fn preset(&self, quality: i32) -> (nv::GUID, nv::NV_ENC_TUNING_INFO) {
        if self.level < 0xa0 {
            // T C4DC10/C4DC50 both select the legacy low-latency-HQ preset.
            (
                nv::GUID {
                    Data1: 0xc5f733b9,
                    Data2: 0xea97,
                    Data3: 0x4cf9,
                    Data4: [0xbe, 0xc2, 0xbf, 0x78, 0xa7, 0x4f, 0xd1, 0x05],
                },
                nv::NV_ENC_TUNING_INFO(0),
            )
        } else if quality == 4 {
            (
                nv::NV_ENC_PRESET_P4_GUID,
                nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_HIGH_QUALITY,
            )
        } else {
            (
                nv::NV_ENC_PRESET_P1_GUID,
                nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            )
        }
    }
    fn check(&self, session: *mut c_void, status: nv::NVENCSTATUS, operation: &str) -> Result<()> {
        if status == nv::NVENCSTATUS::NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = if session.is_null() {
            String::new()
        } else {
            self.functions
                .nvEncGetLastErrorString
                .and_then(|get| unsafe {
                    let p = get(session);
                    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
                })
                .unwrap_or_default()
        };
        bail!("NVENC {operation} 失败（{}）：{detail}", status.0)
    }
}

pub(crate) struct Encoder {
    frame_rate: super::encoder_rate::Controller,
    api: Api,
    session: *mut c_void,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
    registered: nv::NV_ENC_REGISTERED_PTR,
    config: nv::NV_ENC_CONFIG,
    init: nv::NV_ENC_INITIALIZE_PARAMS,
    conversion: Conversion,
    index: u32,
    quality: i32,
    maximum: (u32, u32),
    format: Format,
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
        let Rate {
            target: bitrate,
            peak,
            fps,
            quality: _,
        } = rate;
        ensure!(
            Backend::Nvidia.accepts(format),
            "NVENC当前D3D输入路径不支持该格式"
        );
        ensure!(
            width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0,
            "NVENC 尺寸必须为正偶数"
        );
        ensure!(
            (1..=144).contains(&fps) && bitrate > 0,
            "NVENC 编码参数无效"
        );
        let api = Api::load()?;
        // T C49E10 always starts from the P1 configuration. Frame controls may
        // subsequently change preset/tuning while retaining those base fields.
        let (preset_guid, tuning) = api.preset(3);
        let mut encoder = Self {
            frame_rate: super::encoder_rate::Controller::new(Rate { quality: 0, ..rate }),
            api,
            session: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            registered: ptr::null_mut(),
            config: nv::NV_ENC_CONFIG::default(),
            init: nv::NV_ENC_INITIALIZE_PARAMS::default(),
            conversion,
            index: 0,
            quality: 0,
            maximum: (0, 0),
            format,
        };
        unsafe {
            let mut open = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS::default();
            open.version = encoder.api.structure(1, false);
            open.apiVersion = encoder.api.version;
            open.deviceType = nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = device.as_raw();
            let f = &encoder.api.functions;
            let status = f.nvEncOpenEncodeSessionEx.unwrap()(&mut open, &mut encoder.session);
            encoder
                .api
                .check(encoder.session, status, "OpenEncodeSessionEx")?;
            ensure!(!encoder.session.is_null(), "NVENC 未返回会话");
            let dimension = |cap| -> Result<u32> {
                let mut query = nv::NV_ENC_CAPS_PARAM::default();
                query.version = encoder.api.structure(1, false);
                query.capsToQuery = cap;
                let mut value = 0;
                encoder.api.check(
                    encoder.session,
                    f.nvEncGetEncodeCaps.unwrap()(
                        encoder.session,
                        codec_guid(format),
                        &mut query,
                        &mut value,
                    ),
                    "GetEncodeCaps",
                )?;
                ensure!(value > 0 && value <= 16384, "NVENC 返回无效尺寸能力");
                Ok(value as u32)
            };
            encoder.maximum = (
                dimension(nv::NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX)?,
                dimension(nv::NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX)?,
            );
            ensure!(
                width <= encoder.maximum.0 && height <= encoder.maximum.1,
                "所选画面超出 NVENC 尺寸能力"
            );
            let mut preset = nv::NV_ENC_PRESET_CONFIG::default();
            preset.version = encoder.api.structure(4, true);
            preset.presetCfg.version = encoder.api.config_version();
            let status = if encoder.api.level >= 0xa0 {
                f.nvEncGetEncodePresetConfigEx.unwrap()(
                    encoder.session,
                    codec_guid(format),
                    preset_guid,
                    tuning,
                    &mut preset,
                )
            } else {
                f.nvEncGetEncodePresetConfig.unwrap()(
                    encoder.session,
                    codec_guid(format),
                    preset_guid,
                    &mut preset,
                )
            };
            encoder
                .api
                .check(encoder.session, status, "GetEncodePresetConfigEx")?;
            let mut config = preset.presetCfg;
            config.version = encoder.api.config_version();
            // Retain the queried driver preset's profile for ordinary H264.
            // Chroma/depth use explicit profiles only where the codec requires it.
            if format.chroma == 3 {
                config.profileGUID = if format.codec == Codec::H264 {
                    nv::NV_ENC_H264_PROFILE_HIGH_444_GUID
                } else {
                    nv::NV_ENC_HEVC_PROFILE_FREXT_GUID
                };
            } else if format.codec == Codec::H265 {
                config.profileGUID = if format.depth == 10 {
                    nv::NV_ENC_HEVC_PROFILE_MAIN10_GUID
                } else {
                    nv::NV_ENC_HEVC_PROFILE_MAIN_GUID
                };
            }
            config.gopLength = u32::MAX;
            config.frameIntervalP = 1;
            config.frameFieldMode =
                nv::NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
            config.mvPrecision = nv::NV_ENC_MV_PRECISION::NV_ENC_MV_PRECISION_QUARTER_PEL;
            config.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
            config.rcParams.set_enableAQ(1);
            config.rcParams.set_enableTemporalAQ(0);
            config.rcParams.set_enableLookahead(0);
            config.rcParams.set_disableIadapt(1);
            config.rcParams.set_zeroReorderDelay(1);
            config.rcParams.lookaheadDepth = 0;
            config.rcParams.multiPass = nv::NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
            config.rcParams.lowDelayKeyFrameScale = u8::from(encoder.api.level >= 0xa0);
            set_rate(&mut config, bitrate, peak, fps);
            let vui = if format.codec == Codec::H264 {
                let h264 = &mut config.encodeCodecConfig.h264Config;
                h264.idrPeriod = u32::MAX;
                h264.maxNumRefFrames = 1;
                h264.chromaFormatIDC = u32::from(format.chroma);
                h264.sliceMode = 3;
                h264.sliceModeData = 1;
                h264.set_repeatSPSPPS(0);
                h264.set_outputAUD(0);
                h264.set_enableFillerDataInsertion(0);
                &mut h264.h264VUIParameters
            } else {
                let hevc = &mut config.encodeCodecConfig.hevcConfig;
                hevc.idrPeriod = u32::MAX;
                hevc.maxNumRefFramesInDPB = 1;
                hevc.set_chromaFormatIDC(u32::from(format.chroma));
                hevc.set_pixelBitDepthMinus8(u32::from(format.depth - 8));
                hevc.sliceMode = 3;
                hevc.sliceModeData = 1;
                hevc.set_repeatSPSPPS(0);
                hevc.set_outputAUD(0);
                hevc.set_enableFillerDataInsertion(0);
                &mut hevc.hevcVUIParameters
            };
            let color = format.color(None);
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFormat = nv::NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            vui.videoFullRangeFlag = u32::from(color.range == 2);
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = nv::NV_ENC_VUI_COLOR_PRIMARIES(color.primaries.into());
            vui.transferCharacteristics =
                nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC(color.transfer.into());
            vui.colourMatrix = nv::NV_ENC_VUI_MATRIX_COEFFS(color.matrix.into());
            encoder.config = config;
            let mut init = nv::NV_ENC_INITIALIZE_PARAMS::default();
            init.version = encoder.api.init_version();
            init.encodeGUID = codec_guid(format);
            init.presetGUID = preset_guid;
            init.encodeWidth = width;
            init.encodeHeight = height;
            init.darWidth = width;
            init.darHeight = height;
            init.frameRateNum = fps;
            init.frameRateDen = 1;
            init.enablePTD = 1;
            init.enableEncodeAsync = 0;
            init.maxEncodeWidth = width;
            init.maxEncodeHeight = height;
            init.tuningInfo = tuning;
            init.encodeConfig = &mut encoder.config;
            encoder.api.check(
                encoder.session,
                f.nvEncInitializeEncoder.unwrap()(encoder.session, &mut init),
                "InitializeEncoder",
            )?;
            init.encodeConfig = ptr::null_mut();
            encoder.init = init;
            let mut buffer = nv::NV_ENC_CREATE_BITSTREAM_BUFFER::default();
            buffer.version = encoder.api.structure(1, false);
            encoder.api.check(
                encoder.session,
                f.nvEncCreateBitstreamBuffer.unwrap()(encoder.session, &mut buffer),
                "CreateBitstreamBuffer",
            )?;
            encoder.bitstream = buffer.bitstreamBuffer;
            ensure!(!encoder.bitstream.is_null(), "NVENC未返回码流缓冲");
            let mut resource = nv::NV_ENC_REGISTER_RESOURCE::default();
            resource.version = encoder
                .api
                .structure(if encoder.api.level < 0xc0 { 3 } else { 4 }, false);
            resource.resourceType =
                nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            resource.width = width;
            resource.height = height;
            resource.resourceToRegister = encoder.conversion.output.as_raw();
            resource.bufferFormat = match (format.chroma, format.depth) {
                (1, 10) => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
                (3, 8) => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_AYUV,
                _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            };
            resource.bufferUsage = nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
            encoder
                .api
                .check(
                    encoder.session,
                    f.nvEncRegisterResource.unwrap()(encoder.session, &mut resource),
                    "RegisterResource",
                )
                .map_err(|error| InputRejected(format!("{error:#}")))?;
            encoder.registered = resource.registeredResource;
            if encoder.registered.is_null() {
                return Err(InputRejected("驱动未返回注册资源".into()).into());
            }
        }
        tracing::info!(
            width,
            height,
            fps,
            bitrate,
            backend = "NVENC",
            ?format,
            api = encoder.api.level,
            "host hardware encoder initialized"
        );
        Ok(encoder)
    }
    pub(crate) fn maximum_size(&self) -> (u32, u32) {
        self.maximum
    }

    pub(crate) fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        ensure!(
            (1..=144).contains(&rate.fps) && rate.target > 0,
            "NVENC 重配参数无效"
        );
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        let Rate {
            target: bitrate,
            peak,
            fps,
            quality,
        } = update.rate;
        let changed_quality = self.quality != quality;
        let mut config = self.config;
        set_rate(&mut config, bitrate, peak, update.buffer_fps);
        let mut params = nv::NV_ENC_RECONFIGURE_PARAMS::default();
        params.version = self.api.structure(1, true);
        params.reInitEncodeParams = self.init;
        params.reInitEncodeParams.frameRateNum = fps;
        let (preset, tuning) = self.api.preset(quality);
        params.reInitEncodeParams.presetGUID = preset;
        params.reInitEncodeParams.tuningInfo = tuning;
        params.reInitEncodeParams.encodeConfig = &mut config;
        let result = unsafe {
            self.api.check(
                self.session,
                self.api.functions.nvEncReconfigureEncoder.unwrap()(self.session, &mut params),
                "ReconfigureEncoder",
            )
        };
        if let Err(error) = result {
            if update.control_changed {
                return Err(error);
            }
            // T C37450: rejected feedback leaves the committed configuration
            // intact; unlike failed frame controls it does not discard a frame.
            tracing::warn!(%error, "NVENC rate feedback rejected; retaining configured rate");
            return Ok(false);
        }
        self.config = config;
        self.init = params.reInitEncodeParams;
        self.init.encodeConfig = ptr::null_mut();
        self.quality = quality;
        self.frame_rate.commit(update);
        Ok(changed_quality)
    }

    pub(crate) fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_100ns: i64,
        keyframe: bool,
    ) -> Result<Vec<super::encoder::Encoded>> {
        self.frame_rate.input(timestamp_100ns);
        self.conversion.convert(texture)?;
        unsafe {
            let f = &self.api.functions;
            let mut map = nv::NV_ENC_MAP_INPUT_RESOURCE::default();
            map.version = self.api.structure(4, false);
            map.registeredResource = self.registered;
            self.api.check(
                self.session,
                f.nvEncMapInputResource.unwrap()(self.session, &mut map),
                "MapInputResource",
            )?;
            let _mapped = Mapped {
                api: &self.api,
                session: self.session,
                input: map.mappedResource,
            };
            let mut picture = nv::NV_ENC_PIC_PARAMS::default();
            picture.version = self
                .api
                .structure(if self.api.level < 0xc0 { 4 } else { 6 }, true);
            picture.inputWidth = self.init.encodeWidth;
            picture.inputHeight = self.init.encodeHeight;
            picture.inputBuffer = map.mappedResource;
            picture.bufferFmt = map.mappedBufferFmt;
            picture.outputBitstream = self.bitstream;
            picture.pictureStruct = nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            picture.inputTimeStamp = timestamp_100ns.max(0) as u64;
            picture.inputDuration = 10_000_000 / u64::from(self.init.frameRateNum);
            picture.frameIdx = self.index;
            self.index = self.index.wrapping_add(1);
            if keyframe {
                picture.encodePicFlags = 6;
            }
            let status = f.nvEncEncodePicture.unwrap()(self.session, &mut picture);
            if status == nv::NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
                return Ok(Vec::new());
            }
            self.api.check(self.session, status, "EncodePicture")?;
            let mut bitstream = nv::NV_ENC_LOCK_BITSTREAM::default();
            bitstream.version = self.api.structure(
                if self.api.level == 0xc0 { 2 } else { 1 },
                self.api.level >= 0xc1,
            );
            bitstream.outputBitstream = self.bitstream;
            self.api.check(
                self.session,
                f.nvEncLockBitstream.unwrap()(self.session, &mut bitstream),
                "LockBitstream",
            )?;
            let _locked = Locked {
                api: &self.api,
                session: self.session,
                output: self.bitstream,
            };
            ensure!(
                !bitstream.bitstreamBufferPtr.is_null()
                    && bitstream.bitstreamSizeInBytes > 0
                    && bitstream.bitstreamSizeInBytes <= 64 * 1024 * 1024,
                "NVENC 输出码流无效"
            );
            let bytes = std::slice::from_raw_parts(
                bitstream.bitstreamBufferPtr.cast::<u8>(),
                bitstream.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            Ok(vec![super::encoder::Encoded {
                format: self.format,
                color: self.format.color(None),
                data: bytes,
                keyframe: bitstream.pictureType == nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR,
                is_new: true,
                timing: None,
                timestamp_100ns: bitstream
                    .outputTimeStamp
                    .try_into()
                    .context("NVENC 时间戳溢出")?,
            }])
        }
    }
}
fn codec_guid(format: Format) -> nv::GUID {
    if format.codec == Codec::H264 {
        nv::NV_ENC_CODEC_H264_GUID
    } else {
        nv::NV_ENC_CODEC_HEVC_GUID
    }
}
fn set_rate(config: &mut nv::NV_ENC_CONFIG, bitrate: u32, peak: u32, fps: u32) {
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = peak.max(bitrate);
    config.rcParams.vbvBufferSize = ((u64::from(bitrate)
        * if bitrate < 120_000_000 { 5 } else { 1 })
        / u64::from(fps.max(1))) as u32;
    config.rcParams.vbvInitialDelay = 0;
}
struct Mapped<'a> {
    api: &'a Api,
    session: *mut c_void,
    input: nv::NV_ENC_INPUT_PTR,
}
impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.functions.nvEncUnmapInputResource.unwrap())(self.session, self.input);
        }
    }
}
struct Locked<'a> {
    api: &'a Api,
    session: *mut c_void,
    output: nv::NV_ENC_OUTPUT_PTR,
}
impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.functions.nvEncUnlockBitstream.unwrap())(self.session, self.output);
        }
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let f = &self.api.functions;
            if !self.registered.is_null() {
                f.nvEncUnregisterResource.unwrap()(self.session, self.registered);
            }
            if !self.bitstream.is_null() {
                f.nvEncDestroyBitstreamBuffer.unwrap()(self.session, self.bitstream);
            }
            if !self.session.is_null() {
                f.nvEncDestroyEncoder.unwrap()(self.session);
            }
        }
    }
}
