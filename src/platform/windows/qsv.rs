//! Intel hardware session selected by D3D11 LUID through the oneVPL dispatcher.
use super::{
    encoder::Encoded,
    format::{Codec, Format, Rate},
    gpu_conversion::Conversion,
    qsv_allocator::Allocator,
};
use anyhow::{Context, Result, ensure};
use openuuyc_vpl_sys as v;
use std::{mem::size_of, ptr};
use windows::{
    Win32::Graphics::{Direct3D11::*, Dxgi::IDXGIDevice},
    core::Interface,
};

fn check(status: v::mfxStatus, operation: &str) -> Result<()> {
    ensure!(status >= 0, "QSV {operation} 失败（{status}）");
    if status > 0 {
        tracing::debug!(status, operation, "QSV adjusted driver parameters");
    }
    Ok(())
}
fn header<T>(id: i32) -> v::mfxExtBuffer {
    v::mfxExtBuffer {
        BufferId: id as u32,
        BufferSz: size_of::<T>() as u32,
    }
}
fn codec(format: Format) -> u32 {
    (if format.codec == Codec::H264 {
        v::MFX_CODEC_AVC
    } else {
        v::MFX_CODEC_HEVC
    }) as u32
}
fn fourcc(format: Format) -> u32 {
    (match (format.chroma, format.depth) {
        (1, 8) => v::MFX_FOURCC_NV12,
        (1, 10) => v::MFX_FOURCC_P010,
        (3, 8) => v::MFX_FOURCC_AYUV,
        (3, 10) => v::MFX_FOURCC_Y410,
        _ => 0,
    }) as u32
}
struct Session {
    loader: v::mfxLoader,
    session: v::mfxSession,
}
impl Session {
    fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let mut result = Self {
                loader: v::MFXLoad(),
                session: ptr::null_mut(),
            };
            ensure!(!result.loader.is_null(), "无法创建oneVPL调度器");
            let dxgi: IDXGIDevice = device.cast()?;
            let mut luid = dxgi.GetAdapter()?.GetDesc()?.AdapterLuid;
            for (name, kind, data) in [
                (
                    b"mfxImplDescription.Impl\0".as_slice(),
                    5,
                    v::mfxVariant_data { U32: 2 },
                ),
                (
                    b"mfxImplDescription.AccelerationMode\0",
                    5,
                    v::mfxVariant_data { U32: 0x300 },
                ),
                (
                    b"mfxExtendedDeviceId.DeviceLUID\0",
                    11,
                    v::mfxVariant_data {
                        Ptr: ptr::from_mut(&mut luid).cast(),
                    },
                ),
            ] {
                let config = v::MFXCreateConfig(result.loader);
                ensure!(!config.is_null(), "无法创建QSV过滤条件");
                check(
                    v::MFXSetConfigFilterProperty(
                        config,
                        name.as_ptr(),
                        v::mfxVariant {
                            Version: v::mfxStructVersion { Version: 0x101 },
                            Type: kind,
                            Data: data,
                        },
                    ),
                    "SetConfigFilterProperty",
                )?;
            }
            check(
                v::MFXCreateSession(result.loader, 0, &mut result.session),
                "CreateSession",
            )?;
            ensure!(!result.session.is_null(), "oneVPL未返回编码会话");
            check(
                v::MFXVideoCORE_SetHandle(result.session, 3, device.as_raw()),
                "SetD3D11Device",
            )?;
            check(v::MFXSetPriority(result.session, 2), "SetPriority")?;
            Ok(result)
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                v::MFXClose(self.session);
            }
            if !self.loader.is_null() {
                v::MFXUnload(self.loader);
            }
        }
    }
}
struct Extensions {
    one: Box<v::mfxExtCodingOption>,
    two: Box<v::mfxExtCodingOption2>,
    three: Box<v::mfxExtCodingOption3>,
    color: Box<v::mfxExtVideoSignalInfo>,
    hevc: Box<v::mfxExtHEVCParam>,
    pointers: Vec<*mut v::mfxExtBuffer>,
}
impl Clone for Extensions {
    fn clone(&self) -> Self {
        let mut result = Self {
            one: self.one.clone(),
            two: self.two.clone(),
            three: self.three.clone(),
            color: self.color.clone(),
            hevc: self.hevc.clone(),
            pointers: Vec::new(),
        };
        for &pointer in &self.pointers {
            let id = unsafe { (*pointer).BufferId };
            for header in [
                &mut result.one.Header,
                &mut result.two.Header,
                &mut result.three.Header,
                &mut result.color.Header,
                &mut result.hevc.Header,
            ] {
                if header.BufferId == id {
                    result.pointers.push(ptr::from_mut(header));
                    break;
                }
            }
        }
        result
    }
}

#[derive(Debug, thiserror::Error)]
#[error("QSV编码阶段失败：{0:#}")]
struct EncodeFailure(anyhow::Error);

#[derive(Debug, PartialEq, Eq)]
enum SubmitAction {
    Output,
    Grow,
    Retry,
    Failed,
}
fn submit_action(status: v::mfxStatus, has_sync: bool) -> SubmitAction {
    if status >= 0 && has_sync {
        SubmitAction::Output
    } else if status == v::mfxStatus_MFX_ERR_NOT_ENOUGH_BUFFER {
        SubmitAction::Grow
    } else if status > 0 {
        SubmitAction::Retry
    } else {
        SubmitAction::Failed
    }
}

pub(crate) struct Encoder {
    active: Option<Active>,
    device: ID3D11Device,
    size: (u32, u32),
    format: Format,
}
impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        Ok(Self {
            active: Some(Active::new(device, size, format, rate)?),
            device: device.clone(),
            size,
            format,
        })
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        self.active.as_ref().map_or(self.size, Active::maximum_size)
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        self.active
            .as_mut()
            .context("QSV编码器已释放")?
            .configure(rate)
    }
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        let active = self.active.as_mut().context("QSV编码器已释放")?;
        let compute = active.conversion.is_compute();
        let result = active.encode(texture, timestamp, keyframe);
        match result {
            Err(error) if compute && error.is::<EncodeFailure>() => {
                let restored = (
                    active.params,
                    active.extensions.clone(),
                    active.frame_rate.clone(),
                );
                let rate = active.rate;
                // C449A0 closes the whole failed encoder BEFORE opening its
                // replacement; the old allocator/callbacks outlive that close.
                drop(self.active.take());
                tracing::debug!(%error, "QSV compute encode failed; rebuilding complete pixel path once");
                let replacement = (|| {
                    Active::create(
                        &self.device,
                        self.size,
                        self.format,
                        rate,
                        Conversion::pixel(&self.device, self.size, self.format, 16)?,
                        Some(restored),
                    )
                })()
                .map_err(|error: anyhow::Error| {
                    super::encoder::SwitchCandidate(format!("QSV像素重建失败：{error:#}"))
                })?;
                self.active = Some(replacement);
                self.active
                    .as_mut()
                    .unwrap()
                    .encode(texture, timestamp, true)
                    .map_err(unwrap_encode_error)
            }
            result => result.map_err(unwrap_encode_error),
        }
    }
}
fn unwrap_encode_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<EncodeFailure>() {
        Ok(failure) => failure.0,
        Err(error) => error,
    }
}
impl Extensions {
    fn new(version: (u16, u16), size: (u32, u32), format: Format, low_power: bool) -> Self {
        let color = format.color(None);
        let mut result = Self {
            one: Box::new(v::mfxExtCodingOption {
                Header: header::<v::mfxExtCodingOption>(v::MFX_EXTBUFF_CODING_OPTION),
                NalHrdConformance: 32,
                PicTimingSEI: 32,
                VuiNalHrdParameters: 32,
                MaxDecFrameBuffering: 1,
                AUDelimiter: 32,
                ..Default::default()
            }),
            two: Box::new(v::mfxExtCodingOption2 {
                Header: header::<v::mfxExtCodingOption2>(v::MFX_EXTBUFF_CODING_OPTION2),
                MBBRC: 16,
                RepeatPPS: 32,
                LookAheadDepth: 0,
                ..Default::default()
            }),
            three: Box::new(v::mfxExtCodingOption3 {
                Header: header::<v::mfxExtCodingOption3>(v::MFX_EXTBUFF_CODING_OPTION3),
                ScenarioInfo: 8,
                LowDelayBRC: if format.codec == Codec::H265 {
                    if low_power { 16 } else { 32 }
                } else {
                    0
                },
                TargetChromaFormatPlus1: if format.chroma == 3 { 4 } else { 0 },
                TargetBitDepthLuma: format.depth.into(),
                TargetBitDepthChroma: format.depth.into(),
                ..Default::default()
            }),
            color: Box::new(v::mfxExtVideoSignalInfo {
                Header: header::<v::mfxExtVideoSignalInfo>(v::MFX_EXTBUFF_VIDEO_SIGNAL_INFO),
                VideoFormat: 5,
                VideoFullRange: u16::from(color.range == 2),
                ColourDescriptionPresent: 1,
                ColourPrimaries: color.primaries.into(),
                TransferCharacteristics: color.transfer.into(),
                MatrixCoefficients: color.matrix.into(),
            }),
            hevc: Box::new(v::mfxExtHEVCParam {
                Header: header::<v::mfxExtHEVCParam>(v::MFX_EXTBUFF_HEVC_PARAM),
                PicWidthInLumaSamples: size.0.div_ceil(16) as u16 * 16,
                PicHeightInLumaSamples: size.1.div_ceil(16) as u16 * 16,
                ..Default::default()
            }),
            pointers: Vec::new(),
        };
        result.pointers.push(ptr::from_mut(&mut result.one.Header));
        if version >= (1, 8) {
            result.pointers.push(ptr::from_mut(&mut result.two.Header));
        }
        if version >= (1, 16) {
            result
                .pointers
                .push(ptr::from_mut(&mut result.three.Header));
        }
        result
            .pointers
            .push(ptr::from_mut(&mut result.color.Header));
        if format.codec == Codec::H265 && (size.0 % 16 != 0 || size.1 % 16 != 0) {
            result.pointers.push(ptr::from_mut(&mut result.hevc.Header));
        }
        result
    }
    fn bind(&mut self, param: &mut v::mfxVideoParam) {
        param.ExtParam = self.pointers.as_mut_ptr();
        param.NumExtParam = self.pointers.len() as u16;
    }
}
struct Active {
    size: (u32, u32),
    session: Option<Session>,
    _allocator: Box<Allocator>,
    _callbacks: Box<v::mfxFrameAllocator>,
    extensions: Extensions,
    params: v::mfxVideoParam,
    surface: Box<v::mfxFrameSurface1>,
    conversion: Conversion,
    buffer: Vec<u8>,
    rate: Rate,
    frame_rate: super::encoder_rate::Controller,
    maximum: (u32, u32),
    order: u32,
    initialized: bool,
    timestamps: std::collections::VecDeque<(u64, i64)>,
    format: Format,
}
impl Active {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        let conversion = Conversion::aligned(device, size, format, 16)?;
        let compute = conversion.is_compute();
        match Self::create(device, size, format, rate, conversion, None) {
            Ok(encoder) => Ok(encoder),
            Err(error) if compute => {
                tracing::debug!(%error,"QSV compute path rejected; trying pixel path");
                Self::create(
                    device,
                    size,
                    format,
                    rate,
                    Conversion::pixel(device, size, format, 16)?,
                    None,
                )
            }
            Err(error) => Err(error),
        }
    }
    fn create(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        conversion: Conversion,
        restored: Option<(
            v::mfxVideoParam,
            Extensions,
            super::encoder_rate::Controller,
        )>,
    ) -> Result<Self> {
        ensure!(
            format.valid() && size.0 > 0 && size.1 > 0 && size.0 <= 16384 && size.1 <= 16384,
            "QSV输入格式无效"
        );
        unsafe {
            let session = Session::new(device)?;
            let mut allocator = Allocator::new(device, conversion.bind_flags())?;
            let mut callbacks = Box::new(allocator.callbacks());
            check(
                v::MFXVideoCORE_SetFrameAllocator(session.session, &mut *callbacks),
                "SetFrameAllocator",
            )?;
            let mut version = v::mfxVersion::default();
            check(
                v::MFXQueryVersion(session.session, &mut version),
                "QueryVersion",
            )?;
            let version = (
                version.__bindgen_anon_1.Major,
                version.__bindgen_anon_1.Minor,
            );
            let mut params = v::mfxVideoParam {
                AsyncDepth: 1,
                IOPattern: 1,
                ..Default::default()
            };
            let mfx = &mut params.__bindgen_anon_1.mfx;
            mfx.CodecId = codec(format);
            mfx.LowPower = if format.codec == Codec::H265 { 16 } else { 0 };
            mfx.FrameInfo = v::mfxFrameInfo {
                FourCC: fourcc(format),
                ChromaFormat: format.chroma.into(),
                BitDepthLuma: format.depth.into(),
                BitDepthChroma: format.depth.into(),
                Shift: u16::from(format.depth == 10),
                FrameRateExtN: rate.fps,
                FrameRateExtD: 1,
                PicStruct: 1,
                AspectRatioW: 1,
                AspectRatioH: 1,
                ..Default::default()
            };
            mfx.FrameInfo.__bindgen_anon_1.__bindgen_anon_1 =
                v::mfxFrameInfo__bindgen_ty_1__bindgen_ty_1 {
                    Width: size.0.div_ceil(16) as u16 * 16,
                    Height: size.1.div_ceil(16) as u16 * 16,
                    CropW: size.0 as u16,
                    CropH: size.1 as u16,
                    ..Default::default()
                };
            mfx.__bindgen_anon_1.__bindgen_anon_1 = v::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1 {
                TargetUsage: 7,
                GopRefDist: 1,
                GopOptFlag: 1,
                RateControlMethod: 2,
                NumSlice: 1,
                ..Default::default()
            };
            set_rate(&mut params, rate);
            let mut extensions = Extensions::new(version, size, format, true);
            let mut frame_rate = super::encoder_rate::Controller::new(rate);
            if let Some((previous, previous_extensions, previous_rate)) = restored {
                params = previous;
                extensions = previous_extensions;
                frame_rate = previous_rate;
            }
            extensions.bind(&mut params);
            let mut queried = params;
            let mut status = v::MFXVideoENCODE_Query(session.session, &mut params, &mut queried);
            if (status < 0 || validate(&queried, size, format).is_err())
                && format.codec == Codec::H265
            {
                params.__bindgen_anon_1.mfx.LowPower = 32;
                extensions.three.LowDelayBRC = 32;
                queried = params;
                status = v::MFXVideoENCODE_Query(session.session, &mut params, &mut queried);
            }
            check(status, "Query")?;
            validate(&queried, size, format)?;
            params = queried;
            let mut request = v::mfxFrameAllocRequest::default();
            check(
                v::MFXVideoENCODE_QueryIOSurf(session.session, &mut params, &mut request),
                "QueryIOSurf",
            )?;
            ensure!(request.NumFrameSuggested > 0, "QSV未返回输入纹理要求");
            let mut surface = Box::<v::mfxFrameSurface1>::default();
            surface.Info = params.__bindgen_anon_1.mfx.FrameInfo;
            surface.Data.MemId = allocator.register(conversion.output.clone());
            let mut this = Self {
                size,
                session: Some(session),
                _allocator: allocator,
                _callbacks: callbacks,
                extensions,
                params,
                surface,
                conversion,
                buffer: vec![0; 1024 * 1024],
                rate,
                frame_rate,
                maximum: size,
                order: 0,
                initialized: false,
                timestamps: Default::default(),
                format,
            };
            let mut status = v::MFXVideoENCODE_Init(this.raw(), &mut this.params);
            if status < 0
                && format.codec == Codec::H265
                && this.params.__bindgen_anon_1.mfx.LowPower == 16
            {
                v::MFXVideoENCODE_Close(this.raw());
                this.params.__bindgen_anon_1.mfx.LowPower = 32;
                this.extensions.three.LowDelayBRC = 32;
                let mut queried = this.params;
                check(
                    v::MFXVideoENCODE_Query(this.raw(), &mut this.params, &mut queried),
                    "Query without LowPower",
                )?;
                validate(&queried, size, format)?;
                this.params = queried;
                check(
                    v::MFXVideoENCODE_QueryIOSurf(this.raw(), &mut this.params, &mut request),
                    "QueryIOSurf without LowPower",
                )?;
                this.surface.Info = this.params.__bindgen_anon_1.mfx.FrameInfo;
                status = v::MFXVideoENCODE_Init(this.raw(), &mut this.params);
            }
            check(status, "Init")?;
            this.initialized = true;
            check(
                v::MFXVideoENCODE_GetVideoParam(this.raw(), &mut this.params),
                "GetVideoParam",
            )?;
            validate(&this.params, size, format)?;
            this.maximum = this.maximum_for_format(format).unwrap_or(size);
            Ok(this)
        }
    }
    fn raw(&self) -> v::mfxSession {
        self.session.as_ref().unwrap().session
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        self.maximum
    }
    fn maximum_for_format(&self, format: Format) -> Result<(u32, u32)> {
        unsafe {
            let loader = self.session.as_ref().unwrap().loader;
            let mut description = ptr::null_mut();
            check(
                v::MFXEnumImplementations(loader, 0, 1, &mut description),
                "EnumImplementations",
            )?;
            ensure!(!description.is_null(), "QSV能力描述为空");
            let result = (|| -> Result<(u32, u32)> {
                let description = &*description.cast::<v::mfxImplDescription>();
                let mut maximum = (0, 0);
                for entry in slice(description.Enc.Codecs, description.Enc.NumCodecs)? {
                    if entry.CodecID != codec(format) {
                        continue;
                    }
                    for profile in slice(entry.Profiles, entry.NumProfiles)? {
                        for memory in slice(profile.MemDesc, profile.NumMemTypes)? {
                            if memory.MemHandleType == v::mfxResourceType_MFX_RESOURCE_DX11_TEXTURE
                                && slice(memory.ColorFormats, memory.NumColorFormats)?
                                    .contains(&fourcc(format))
                            {
                                if u64::from(memory.Width.Max) * u64::from(memory.Height.Max)
                                    > u64::from(maximum.0) * u64::from(maximum.1)
                                {
                                    maximum = (memory.Width.Max, memory.Height.Max);
                                }
                            }
                        }
                    }
                }
                ensure!(
                    maximum.0 > 0 && maximum.1 > 0 && maximum.0 <= 16384 && maximum.1 <= 16384,
                    "QSV没有有效格式上限"
                );
                Ok(maximum)
            })();
            v::MFXDispReleaseImplDescription(loader, description);
            result
        }
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        ensure!(self.session.is_some(), "QSV编码会话已关闭");
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        if self.surface.Data.Locked != 0 {
            return Ok(false);
        }
        let mut params = self.params;
        set_rate(&mut params, update.rate);
        self.extensions.bind(&mut params);
        unsafe {
            let status = v::MFXVideoENCODE_Reset(self.raw(), &mut params);
            if status != 0 && status != 5 {
                if update.control_changed {
                    anyhow::bail!("QSV Reset 失败（{status}）");
                }
                tracing::warn!(
                    status,
                    "QSV rate feedback rejected; retaining configured rate"
                );
                return Ok(false);
            }
            let mut effective = params;
            let status = v::MFXVideoENCODE_GetVideoParam(self.raw(), &mut effective);
            if status == 0 {
                validate(&effective, self.size, self.format)?;
                params = effective;
            } else {
                tracing::debug!(
                    status,
                    "QSV rate query failed; keeping requested parameters"
                );
            }
        }
        let changed_quality = self.rate.quality != rate.quality;
        self.params = params;
        self.surface.Info = unsafe { params.__bindgen_anon_1.mfx.FrameInfo };
        self.rate = rate;
        self.frame_rate.commit(update);
        Ok(changed_quality)
    }
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        ensure!(self.session.is_some(), "QSV编码会话已关闭");
        for attempt in 0..50 {
            if self.surface.Data.Locked == 0 {
                break;
            }
            super::encoder::retry_pause(attempt);
        }
        if self.surface.Data.Locked != 0 {
            return Ok(Vec::new());
        }
        self.frame_rate.input(timestamp);
        self.conversion.convert(texture)?;
        self.submit(timestamp, keyframe)
            .map_err(|error| EncodeFailure(error).into())
    }
    fn submit(&mut self, timestamp: i64, keyframe: bool) -> Result<Vec<Encoded>> {
        self.surface.Data.TimeStamp = (timestamp.max(0) as u64) * 9 / 1000;
        self.timestamps
            .push_back((self.surface.Data.TimeStamp, timestamp));
        while self.timestamps.len() > 128 {
            self.timestamps.pop_front();
        }
        self.surface.Data.FrameOrder = self.order;
        self.order = self.order.wrapping_add(1);
        let mut control = v::mfxEncodeCtrl {
            FrameType: if keyframe {
                (v::MFX_FRAMETYPE_I | v::MFX_FRAMETYPE_IDR | v::MFX_FRAMETYPE_REF) as u16
            } else {
                0
            },
            ..Default::default()
        };
        let mut stream = v::mfxBitstream::default();
        let mut sync = ptr::null_mut();
        let mut accepted = false;
        unsafe {
            for attempt in 0..50 {
                stream.Data = self.buffer.as_mut_ptr();
                stream.MaxLength = self.buffer.len() as u32;
                let status = v::MFXVideoENCODE_EncodeFrameAsync(
                    self.raw(),
                    &mut control,
                    &mut *self.surface,
                    &mut stream,
                    &mut sync,
                );
                // A positive warning WITH a sync point owns real output.
                // Busy/not-ready without a sync point is bounded, not success.
                match submit_action(status, !sync.is_null()) {
                    SubmitAction::Output => {
                        accepted = true;
                        break;
                    }
                    SubmitAction::Grow => {
                        ensure!(self.buffer.len() < 64 * 1024 * 1024, "QSV码流超过64MiB");
                        self.buffer.resize(self.buffer.len() * 2, 0);
                        continue;
                    }
                    SubmitAction::Retry => {
                        super::encoder::retry_pause(attempt);
                        continue;
                    }
                    SubmitAction::Failed => {}
                }
                return Err(super::encoder::SwitchCandidate(format!(
                    "QSV EncodeFrameAsync失败或无同步点（{status}）"
                ))
                .into());
            }
            if !accepted {
                return Err(super::encoder::SwitchCandidate("QSV提交重试耗尽".into()).into());
            }
            let status = v::MFXVideoCORE_SyncOperation(self.raw(), sync, 8000);
            if status != 0 {
                // mfxBitstream is owned by the outstanding task. Quiesce the
                // session while this stack object and its buffer are still alive.
                v::MFXVideoENCODE_Close(self.raw());
                self.initialized = false;
                self.session.take();
                return Err(
                    super::encoder::SwitchCandidate(format!("QSV同步未完成（{status}）")).into(),
                );
            }
        }
        let start = stream.DataOffset as usize;
        let end = start
            .checked_add(stream.DataLength as usize)
            .context("QSV码流长度溢出")?;
        ensure!(end <= self.buffer.len() && start < end, "QSV返回无效码流");
        let output_time = self
            .timestamps
            .iter()
            .position(|(clock, _)| *clock == stream.TimeStamp)
            .and_then(|index| self.timestamps.remove(index))
            .map_or(timestamp, |(_, original)| original);
        Ok(vec![Encoded {
            data: self.buffer[start..end].to_vec(),
            keyframe: stream.FrameType & v::MFX_FRAMETYPE_IDR as u16 != 0,
            timestamp_100ns: output_time,
            is_new: true,
            timing: None,
            format: self.format,
            color: self.format.color(None),
        }])
    }
}
unsafe fn slice<'a, T>(data: *const T, count: u16) -> Result<&'a [T]> {
    unsafe {
        ensure!(
            count <= 1024 && (count == 0 || !data.is_null()),
            "QSV能力数组无效"
        );
        Ok(if count == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data, count as usize)
        })
    }
}
fn set_rate(params: &mut v::mfxVideoParam, rate: Rate) {
    unsafe {
        let mfx = &mut params.__bindgen_anon_1.mfx;
        let peak = rate.peak.max(rate.target) / 1000;
        let factor = peak.div_ceil(u16::MAX as u32).max(1);
        mfx.BRCParamMultiplier = factor as u16;
        let encoding = &mut mfx.__bindgen_anon_1.__bindgen_anon_1;
        encoding.__bindgen_anon_2.TargetKbps = (rate.target / 1000).div_ceil(factor).max(1) as u16;
        encoding.__bindgen_anon_3.MaxKbps = peak.div_ceil(factor) as u16;
        mfx.FrameInfo.FrameRateExtN = rate.fps;
        mfx.FrameInfo.FrameRateExtD = 1;
    }
}
fn validate(params: &v::mfxVideoParam, size: (u32, u32), format: Format) -> Result<()> {
    unsafe {
        let mfx = params.__bindgen_anon_1.mfx;
        let info = mfx.FrameInfo;
        let dimensions = info.__bindgen_anon_1.__bindgen_anon_1;
        ensure!(
            mfx.CodecId == codec(format)
                && info.FourCC == fourcc(format)
                && info.ChromaFormat == u16::from(format.chroma)
                && (info.BitDepthLuma == u16::from(format.depth)
                    || (format.depth == 8 && info.BitDepthLuma == 0))
                && (info.BitDepthChroma == u16::from(format.depth)
                    || (format.depth == 8 && info.BitDepthChroma == 0))
                && (u32::from(dimensions.CropW), u32::from(dimensions.CropH)) == size
                && mfx.__bindgen_anon_1.__bindgen_anon_1.GopRefDist == 1,
            "QSV驱动修改了必要编码契约"
        );
        Ok(())
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                v::MFXVideoENCODE_Close(self.raw());
            }
            // Close the session before freeing callback state and its surface handles.
            self.session.take();
        }
    }
}
