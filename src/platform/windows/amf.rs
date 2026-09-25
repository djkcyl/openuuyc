//! AMF's D3D11 hardware producer. All COM/driver calls stay on its owner thread.
use super::{
    encoder::Encoded,
    format::{Codec, Format, Rate},
    gpu_conversion::Conversion,
};
use anyhow::{Context, Result, ensure};
use openuuyc_amf_sys as a;
use std::ptr::{self, NonNull};
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

fn check(result: a::AMF_RESULT, operation: &str) -> Result<()> {
    ensure!(
        result == a::AMF_RESULT_AMF_OK,
        "AMF {operation} 失败（{result}）"
    );
    Ok(())
}
fn wide(name: &[u8]) -> Vec<u16> {
    name.iter().map(|&c| u16::from(c)).collect()
}
fn integer(value: i64) -> a::AMFVariantStruct {
    a::AMFVariantStruct {
        type_: 2,
        __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 { int64Value: value },
    }
}
fn boolean(value: bool) -> a::AMFVariantStruct {
    a::AMFVariantStruct {
        type_: 1,
        __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 {
            boolValue: u8::from(value),
        },
    }
}
fn frame_rate(value: u32) -> a::AMFVariantStruct {
    a::AMFVariantStruct {
        type_: 7,
        __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 {
            rateValue: a::AMFRate { num: value, den: 1 },
        },
    }
}
trait Release {
    unsafe fn release(p: *mut Self);
}
struct Owned<T: Release>(NonNull<T>);
impl<T: Release> Owned<T> {
    unsafe fn take(p: *mut T) -> Result<Self> {
        Ok(Self(NonNull::new(p).context("AMF返回空接口")?))
    }
    fn raw(&self) -> *mut T {
        self.0.as_ptr()
    }
}
impl<T: Release> Drop for Owned<T> {
    fn drop(&mut self) {
        unsafe { T::release(self.raw()) }
    }
}
macro_rules! interface {
    ($t:ty,$vt:ty) => {
        impl Release for $t {
            unsafe fn release(p: *mut Self) {
                unsafe {
                    if let Some(release) = (*(*p).pVtbl).Release {
                        release(p);
                    }
                }
            }
        }
        impl Owned<$t> {
            fn vt(&self) -> &$vt {
                unsafe { &*self.0.as_ref().pVtbl }
            }
        }
    };
}
interface!(a::AMFContext, a::AMFContextVtbl);
interface!(a::AMFComponent, a::AMFComponentVtbl);
interface!(a::AMFSurface, a::AMFSurfaceVtbl);
interface!(a::AMFData, a::AMFDataVtbl);
interface!(a::AMFBuffer, a::AMFBufferVtbl);
interface!(a::AMFCaps, a::AMFCapsVtbl);
interface!(a::AMFIOCaps, a::AMFIOCapsVtbl);

pub(crate) struct Encoder {
    component: Option<Owned<a::AMFComponent>>,
    context: Option<Owned<a::AMFContext>>,
    pending: Option<(Owned<a::AMFSurface>, bool)>,
    conversion: Conversion,
    format: Format,
    rate: Rate,
    maximum: (u32, u32),
    frame_rate: super::encoder_rate::Controller,
    configured_fps: u32,
    _library: libloading::os::windows::Library,
}
impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        ensure!(format.valid(), "AMF输入格式无效");
        let usages: &[i64] = if format.codec == Codec::H265 {
            &[1, 2]
        } else {
            &[2]
        };
        let paths: &[bool] = if format.chroma == 1 {
            &[true, false]
        } else {
            &[false]
        };
        let mut failure = None;
        for &usage in usages {
            for &compute in paths {
                let candidate = (|| {
                    let conversion = if compute {
                        Conversion::compute(device, size, format, 1)?
                    } else {
                        Conversion::pixel(device, size, format, 1)?
                    };
                    Self::create(device, size, format, rate, conversion, usage)
                })();
                match candidate {
                    Ok(encoder) => return Ok(encoder),
                    Err(error) => {
                        tracing::debug!(%error, usage, compute, "AMF input/usage candidate rejected");
                        failure = Some(error);
                    }
                }
            }
        }
        Err(failure.context("AMF没有输入候选")?)
    }
    fn create(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
        conversion: Conversion,
        usage: i64,
    ) -> Result<Self> {
        unsafe {
            let library = libloading::os::windows::Library::load_with_flags("amfrt64.dll", 0x800)?;
            let init = library
                .get::<unsafe extern "C" fn(u64, *mut *mut a::AMFFactory) -> a::AMF_RESULT>(
                    b"AMFInit\0",
                )?;
            let mut factory = ptr::null_mut();
            check(init(0x1_0005_0002_0000, &mut factory), "Init")?;
            ensure!(
                !factory.is_null() && !(*factory).pVtbl.is_null(),
                "AMF工厂无效"
            );
            let factory_vt = &*(*factory).pVtbl;
            let mut context = ptr::null_mut();
            check(
                factory_vt
                    .CreateContext
                    .context("AMF CreateContext接口缺失")?(factory, &mut context),
                "CreateContext",
            )?;
            let context = Owned::<a::AMFContext>::take(context)?;
            check(
                context.vt().InitDX11.context("AMF InitDX11接口缺失")?(
                    context.raw(),
                    device.as_raw(),
                    110,
                ),
                "InitDX11",
            )?;
            let mut component = ptr::null_mut();
            let id = wide(if format.codec == Codec::H264 {
                b"AMFVideoEncoderVCE_AVC\0"
            } else {
                b"AMFVideoEncoder_HEVC\0"
            });
            check(
                factory_vt
                    .CreateComponent
                    .context("AMF CreateComponent接口缺失")?(
                    factory,
                    context.raw(),
                    id.as_ptr(),
                    &mut component,
                ),
                "CreateComponent",
            )?;
            let component = Owned::<a::AMFComponent>::take(component)?;
            let mut this = Self {
                component: Some(component),
                context: Some(context),
                pending: None,
                conversion,
                format,
                rate,
                maximum: size,
                frame_rate: super::encoder_rate::Controller::new(rate),
                configured_fps: rate.fps,
                _library: library,
            };
            let rate = this.initial_rate(rate);
            this.rate = rate;
            this.frame_rate = super::encoder_rate::Controller::new(rate);
            this.set_initial(b"Usage\0", b"HevcUsage\0", integer(usage))?;
            this.set_initial(
                b"FrameSize\0",
                b"HevcFrameSize\0",
                a::AMFVariantStruct {
                    type_: 5,
                    __bindgen_anon_1: a::AMFVariantStruct__bindgen_ty_1 {
                        sizeValue: a::AMFSize {
                            width: size.0 as i32,
                            height: size.1 as i32,
                        },
                    },
                },
            )?;
            this.set_initial(
                b"QualityPreset\0",
                b"HevcQualityPreset\0",
                integer(if format.codec == Codec::H265 { 10 } else { 1 }),
            )?;
            this.set_initial(
                b"RateControlMethod\0",
                b"HevcRateControlMethod\0",
                integer(2),
            )?;
            this.set_initial(b"FrameRate\0", b"HevcFrameRate\0", frame_rate(rate.fps))?;
            this.set_initial(
                b"TargetBitrate\0",
                b"HevcTargetBitrate\0",
                integer(rate.target.into()),
            )?;
            this.set_initial(
                b"PeakBitrate\0",
                b"HevcPeakBitrate\0",
                integer(rate.peak.max(rate.target).into()),
            )?;
            this.set_initial(
                b"VBVBufferSize\0",
                b"HevcVBVBufferSize\0",
                integer(vbv(rate)),
            )?;
            this.set_initial(b"EnableVBAQ\0", b"HevcEnableVBAQ\0", boolean(true))?;
            this.set_initial(b"EnforceHRD\0", b"HevcEnforceHRD\0", boolean(false))?;
            this.set_initial(
                b"HighMotionQualityBoostEnable\0",
                b"HevcHighMotionQualityBoostEnable\0",
                boolean(false),
            )?;
            this.set_initial(b"QueryTimeout\0", b"HevcQueryTimeout\0", integer(10))?;
            if format.codec == Codec::H264 {
                for (name, value) in [
                    (b"BPicturesPattern\0".as_slice(), integer(0)),
                    (b"IDRPeriod\0", integer(0)),
                    (b"SlicesPerFrame\0", integer(1)),
                    (b"CABACEnable\0", integer(1)),
                    (b"DeBlockingFilter\0", boolean(true)),
                    (b"EnableVBAQ\0", boolean(true)),
                ] {
                    this.set_initial(name, name, value)?;
                }
            } else {
                for (name, value) in [
                    (b"HevcColorBitDepth\0".as_slice(), i64::from(format.depth)),
                    (b"HevcHeaderInsertionMode\0", 2),
                    (b"HevcOutputMode\0", 0),
                    (b"HevcGOPSize\0", 1_000_000),
                    (b"HevcSlicesPerFrame\0", 1),
                ] {
                    this.set_initial(name, name, integer(value))?;
                }
            }
            let color = format.color(None);
            if format.codec == Codec::H265 {
                this.enable_multi_hw();
            }
            this.set_initial(
                b"OutColorPrimaries\0",
                b"HevcOutColorPrimaries\0",
                integer(color.primaries.into()),
            )?;
            this.set_initial(
                a::AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
                a::AMF_VIDEO_ENCODER_HEVC_OUTPUT_TRANSFER_CHARACTERISTIC,
                integer(color.transfer.into()),
            )?;
            this.set_initial(
                b"OutMatrixCoeff\0",
                b"HevcOutMatrixCoeff\0",
                integer(color.matrix.into()),
            )?;
            this.set_initial(
                b"FullRangeColor\0",
                b"HevcNominalRange\0",
                boolean(color.range == 2),
            )?;
            let native = match (format.chroma, format.depth) {
                (1, 8) => 1,
                (1, 10) => 10,
                (3, 8) => 15,
                (3, 10) => 16,
                _ => unreachable!(),
            };
            let comp = this.component.as_ref().unwrap();
            check(
                comp.vt().Init.context("AMF Init接口缺失")?(
                    comp.raw(),
                    native,
                    size.0 as i32,
                    size.1 as i32,
                ),
                "Encoder Init",
            )?;
            this.maximum = this.query_maximum(native).unwrap_or(size);
            ensure!(
                size.0 <= this.maximum.0 && size.1 <= this.maximum.1,
                "AMF尺寸超出实际能力"
            );
            Ok(this)
        }
    }
    fn initial_rate(&self, mut rate: Rate) -> Rate {
        let maximum = (|| -> Result<u32> {
            unsafe {
                let comp = self.component.as_ref().context("AMF编码器已关闭")?;
                let mut caps = ptr::null_mut();
                check(
                    comp.vt().GetCaps.context("AMF能力接口缺失")?(comp.raw(), &mut caps),
                    "GetCaps",
                )?;
                let caps = Owned::<a::AMFCaps>::take(caps)?;
                let mut value = a::AMFVariantStruct::default();
                let name = if self.format.codec == Codec::H265 {
                    b"HevcMaxBitrate\0".as_slice()
                } else {
                    b"MaxBitrate\0".as_slice()
                };
                check(
                    caps.vt().GetProperty.context("AMF能力属性接口缺失")?(
                        caps.raw(),
                        wide(name).as_ptr(),
                        &mut value,
                    ),
                    "MaxBitrate",
                )?;
                ensure!(
                    value.type_ == 2 && value.__bindgen_anon_1.int64Value > 0,
                    "AMF最大码率无效"
                );
                Ok(u32::try_from(value.__bindgen_anon_1.int64Value).unwrap_or(u32::MAX))
            }
        })();
        if let Ok(maximum) = maximum {
            if rate.peak.max(rate.target) > maximum {
                rate.target = rate.target.min(maximum);
                rate.peak = maximum;
                tracing::debug!(
                    maximum,
                    "AMF initial bitrate limited by hardware capability"
                );
            }
        }
        rate
    }
    fn set_initial(&self, avc: &[u8], hevc: &[u8], value: a::AMFVariantStruct) -> Result<()> {
        let result = self.set(avc, hevc, value);
        if self.format.codec == Codec::H265
            && matches!(hevc, b"HevcUsage\0" | b"HevcColorBitDepth\0")
        {
            return result;
        }
        if let Err(error) = result {
            // C52620/C53780: optional properties do not replace the actual Init
            // and output probe. Requested SPS/depth is still verified outside.
            tracing::debug!(%error, "AMF initial property rejected; continuing actual probe");
        }
        Ok(())
    }
    fn set(&self, avc: &[u8], hevc: &[u8], value: a::AMFVariantStruct) -> Result<()> {
        let name = wide(if self.format.codec == Codec::H264 {
            avc
        } else {
            hevc
        });
        let comp = self.component.as_ref().context("AMF编码器已关闭")?;
        unsafe {
            check(
                comp.vt().SetProperty.context("AMF SetProperty接口缺失")?(
                    comp.raw(),
                    name.as_ptr(),
                    value,
                ),
                &String::from_utf16_lossy(&name),
            )
        }
    }
    fn enable_multi_hw(&self) {
        let result = (|| -> Result<()> {
            unsafe {
                let comp = self.component.as_ref().context("AMF编码器已关闭")?;
                let mut caps = ptr::null_mut();
                check(
                    comp.vt().GetCaps.context("AMF GetCaps接口缺失")?(comp.raw(), &mut caps),
                    "GetCaps",
                )?;
                let caps = Owned::<a::AMFCaps>::take(caps)?;
                let mut count = a::AMFVariantStruct::default();
                check(
                    caps.vt().GetProperty.context("AMF GetProperty接口缺失")?(
                        caps.raw(),
                        wide(a::AMF_VIDEO_ENCODER_HEVC_CAP_NUM_OF_HW_INSTANCES).as_ptr(),
                        &mut count,
                    ),
                    "HevcNumOfHwInstances",
                )?;
                if count.type_ == 2 && count.__bindgen_anon_1.int64Value > 1 {
                    self.set(
                        a::AMF_VIDEO_ENCODER_HEVC_MULTI_HW_INSTANCE_ENCODE,
                        a::AMF_VIDEO_ENCODER_HEVC_MULTI_HW_INSTANCE_ENCODE,
                        boolean(true),
                    )?;
                }
                Ok(())
            }
        })();
        if let Err(error) = result {
            tracing::debug!(%error,"AMF optional multi-instance configuration unavailable");
        }
    }
    fn query_maximum(&self, native: i32) -> Result<(u32, u32)> {
        unsafe {
            let comp = self.component.as_ref().unwrap();
            let mut caps = ptr::null_mut();
            check(
                comp.vt().GetCaps.context("AMF GetCaps接口缺失")?(comp.raw(), &mut caps),
                "GetCaps",
            )?;
            let caps = Owned::<a::AMFCaps>::take(caps)?;
            let mut input = ptr::null_mut();
            check(
                caps.vt().GetInputCaps.context("AMF GetInputCaps接口缺失")?(
                    caps.raw(),
                    &mut input,
                ),
                "GetInputCaps",
            )?;
            let input = Owned::<a::AMFIOCaps>::take(input)?;
            let vt = input.vt();
            let count = vt.GetNumOfFormats.context("AMF GetNumOfFormats接口缺失")?(input.raw());
            ensure!((0..=128).contains(&count), "AMF格式能力无效");
            let mut supported = false;
            for index in 0..count {
                let (mut format, mut is_native) = (0, 0u8);
                check(
                    vt.GetFormatAt.context("AMF GetFormatAt接口缺失")?(
                        input.raw(),
                        index,
                        &mut format,
                        &mut is_native,
                    ),
                    "GetFormatAt",
                )?;
                supported |= format == native;
            }
            ensure!(supported, "AMF实际能力不支持请求格式");
            let (mut min_w, mut max_w, mut min_h, mut max_h) = (0, 0, 0, 0);
            vt.GetWidthRange.context("AMF GetWidthRange接口缺失")?(
                input.raw(),
                &mut min_w,
                &mut max_w,
            );
            vt.GetHeightRange.context("AMF GetHeightRange接口缺失")?(
                input.raw(),
                &mut min_h,
                &mut max_h,
            );
            ensure!(
                (1..=16384).contains(&max_w) && (1..=16384).contains(&max_h),
                "AMF最大尺寸无效"
            );
            Ok((max_w as u32, max_h as u32))
        }
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        self.maximum
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        let requested = rate;
        let rate = update.rate;
        let changed_quality = self.rate.quality != rate.quality;
        // T C3FF90: runtime property failures are logged individually; other
        // accepted properties remain in force. No fictitious atomic rollback.
        for result in [
            self.set(
                b"FrameRate\0",
                b"HevcFrameRate\0",
                frame_rate(update.rate.fps),
            ),
            self.set(
                b"TargetBitrate\0",
                b"HevcTargetBitrate\0",
                integer(rate.target.into()),
            ),
            self.set(
                b"VBVBufferSize\0",
                b"HevcVBVBufferSize\0",
                integer(
                    vbv(rate)
                        * if rate.peak.max(rate.target) < 120_000_000 {
                            5
                        } else {
                            1
                        }
                        / i64::from(update.buffer_fps.max(1)),
                ),
            ),
            self.set(
                b"PeakBitrate\0",
                b"HevcPeakBitrate\0",
                integer(rate.peak.max(rate.target).into()),
            ),
        ] {
            if let Err(error) = result {
                tracing::warn!(%error,"AMF runtime property rejected");
            }
        }
        self.rate = requested;
        self.configured_fps = update.rate.fps;
        self.frame_rate.commit(update);
        Ok(changed_quality)
    }
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        if self.pending.is_some() {
            return self.output();
        }
        self.frame_rate.input(timestamp);
        self.conversion.convert(texture)?;
        unsafe {
            let context = self.context.as_ref().unwrap();
            let mut surface = ptr::null_mut();
            check(
                context
                    .vt()
                    .CreateSurfaceFromDX11Native
                    .context("AMF DX11Surface接口缺失")?(
                    context.raw(),
                    self.conversion.output.as_raw(),
                    &mut surface,
                    ptr::null_mut(),
                ),
                "CreateSurfaceFromDX11Native",
            )?;
            let surface = Owned::<a::AMFSurface>::take(surface)?;
            let vt = surface.vt();
            vt.SetPts.context("AMF SetPts接口缺失")?(surface.raw(), timestamp);
            vt.SetDuration.context("AMF SetDuration接口缺失")?(
                surface.raw(),
                10_000_000 / i64::from(self.configured_fps.max(1)),
            );
            if keyframe {
                let settings: Vec<(&[u8], a::AMFVariantStruct)> =
                    if self.format.codec == Codec::H264 {
                        vec![
                            (b"ForcePictureType\0", integer(2)),
                            (b"InsertSPS\0", boolean(true)),
                            (b"InsertPPS\0", boolean(true)),
                        ]
                    } else {
                        vec![
                            (b"HevcForcePictureType\0", integer(2)),
                            (b"HevcInsertHeader\0", boolean(true)),
                        ]
                    };
                for (name, value) in settings {
                    check(
                        vt.SetProperty.context("AMF surface SetProperty接口缺失")?(
                            surface.raw(),
                            wide(name).as_ptr(),
                            value,
                        ),
                        "force IDR",
                    )?;
                }
            }
            let comp = self.component.as_ref().unwrap();
            let submit = comp.vt().SubmitInput.context("AMF SubmitInput接口缺失")?;
            let mut accepted = false;
            for attempt in 0..500 {
                let status = submit(comp.raw(), surface.raw().cast());
                if status == a::AMF_RESULT_AMF_OK {
                    accepted = true;
                    break;
                }
                if (status as u32) <= 44 && (0x100000002042u64 >> status as u32) & 1 != 0 {
                    check(status, "SubmitInput")?;
                }
                super::encoder::retry_pause(attempt);
            }
            if !accepted {
                return Ok(Vec::new());
            }
            self.pending = Some((surface, keyframe));
        }
        self.output()
    }
    fn output(&mut self) -> Result<Vec<Encoded>> {
        unsafe {
            let comp = self.component.as_ref().unwrap();
            let mut data = ptr::null_mut();
            for attempt in 0..500 {
                let status = comp.vt().QueryOutput.context("AMF QueryOutput接口缺失")?(
                    comp.raw(),
                    &mut data,
                );
                if status != a::AMF_RESULT_AMF_OK && status != a::AMF_RESULT_AMF_REPEAT {
                    // Keep the outstanding surface alive until the encoder is closed.
                    if !data.is_null() {
                        drop(Owned::<a::AMFData>::take(data)?);
                    }
                    check(status, "QueryOutput")?;
                }
                if !data.is_null() {
                    break;
                }
                super::encoder::retry_pause(attempt);
            }
            ensure!(!data.is_null(), "AMF拉取输出超过500次尝试");
            let data = Owned::<a::AMFData>::take(data)?;
            let iid = a::AMFGuid {
                data1: 0xb04b7248,
                data2: 0xb6f0,
                data3: 0x4321,
                data41: 0xb6,
                data42: 0x91,
                data43: 0xba,
                data44: 0xa4,
                data45: 0x74,
                data46: 0x0f,
                data47: 0x9f,
                data48: 0xcb,
            };
            let mut buffer = ptr::null_mut();
            check(
                data.vt()
                    .QueryInterface
                    .context("AMF QueryInterface接口缺失")?(
                    data.raw(), &iid, &mut buffer
                ),
                "Buffer interface",
            )?;
            let buffer = Owned::<a::AMFBuffer>::take(buffer.cast())?;
            let size = buffer.vt().GetSize.context("AMF GetSize接口缺失")?(buffer.raw());
            let bytes =
                buffer.vt().GetNative.context("AMF GetNative接口缺失")?(buffer.raw()).cast::<u8>();
            ensure!(
                !bytes.is_null() && size > 0 && size <= 64 * 1024 * 1024,
                "AMF输出码流无效"
            );
            let mut kind = a::AMFVariantStruct::default();
            let name = wide(if self.format.codec == Codec::H264 {
                b"OutputDataType\0"
            } else {
                b"HevcOutputDataType\0"
            });
            let status = buffer.vt().GetProperty.context("AMF GetProperty接口缺失")?(
                buffer.raw(),
                name.as_ptr(),
                &mut kind,
            );
            let keyframe = if status == a::AMF_RESULT_AMF_OK && kind.type_ == 2 {
                kind.__bindgen_anon_1.int64Value == 0
            } else {
                tracing::debug!(
                    status,
                    "AMF frame type unavailable; preserving requested type"
                );
                self.pending
                    .as_ref()
                    .is_some_and(|(_, requested)| *requested)
            };
            let frame = Encoded {
                data: std::slice::from_raw_parts(bytes, size).to_vec(),
                keyframe,
                timestamp_100ns: buffer.vt().GetPts.context("AMF GetPts接口缺失")?(
                    buffer.raw(),
                ),
                is_new: true,
                timing: None,
                format: self.format,
                color: self.format.color(None),
            };
            self.pending = None;
            Ok(vec![frame])
        }
    }
}
fn vbv(rate: Rate) -> i64 {
    i64::from(rate.peak.max(rate.target))
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(comp) = self.component.as_ref() {
                if let Some(terminate) = comp.vt().Terminate {
                    terminate(comp.raw());
                }
            }
            self.pending.take();
            self.component.take();
            if let Some(context) = self.context.as_ref() {
                if let Some(terminate) = context.vt().Terminate {
                    terminate(context.raw());
                }
            }
            self.context.take();
        }
    }
}
