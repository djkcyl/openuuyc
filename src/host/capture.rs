//! Desktop Duplication ownership and top-down BGRA frames.
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::{
                DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
                EnumDisplayDevicesW, EnumDisplaySettingsExW,
            },
        },
    },
    core::{Interface, PCWSTR},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Screen {
    pub id: i32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub primary: bool,
    pub fps: u32,
    pub dpi_scale: Option<u32>,
    pub hdr: bool,
    pub adapter: u64,
    pub identity: Option<String>,
}
#[derive(Debug, thiserror::Error)]
#[error("所选屏幕已断开；请重新选择共享屏幕")]
pub(crate) struct SourceGone;

fn outputs() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput1, Screen)>> {
    let displays = display_info::DisplayInfo::all().unwrap_or_default();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut result = Vec::new();
        let mut adapter_index = 0;
        loop {
            let adapter = match factory.EnumAdapters1(adapter_index) {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            adapter_index += 1;
            let mut output_index = 0;
            loop {
                let output = match adapter.EnumOutputs(output_index) {
                    Ok(output) => output,
                    Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(error) => return Err(error.into()),
                };
                output_index += 1;
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let bounds = desc.DesktopCoordinates;
                let width = u32::try_from(bounds.right - bounds.left)?;
                let height = u32::try_from(bounds.bottom - bounds.top)?;
                ensure!(
                    width > 0 && height > 0 && width <= 16384 && height <= 16384,
                    "无效屏幕尺寸"
                );
                let length = desc
                    .DeviceName
                    .iter()
                    .position(|v| *v == 0)
                    .unwrap_or(desc.DeviceName.len());
                let name = String::from_utf16_lossy(&desc.DeviceName[..length]);
                let dpi_scale = displays
                    .iter()
                    .find(|d| d.name == name)
                    .map(|d| d.scale_factor)
                    .filter(|scale| scale.is_finite() && (0.5..=10.0).contains(scale))
                    .map(|scale| (scale * 100.0).round() as u32);
                let mut mode = DEVMODEW {
                    dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                };
                let fps = if EnumDisplaySettingsExW(
                    PCWSTR(desc.DeviceName.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
                .as_bool()
                    && mode.dmDisplayFrequency > 1
                {
                    mode.dmDisplayFrequency
                } else {
                    60
                };
                result.push((
                    adapter.clone(),
                    output.cast()?,
                    Screen {
                        id: result.len() as i32,
                        name: name.clone(),
                        width,
                        height,
                        left: bounds.left,
                        top: bounds.top,
                        primary: bounds.left == 0 && bounds.top == 0,
                        fps,
                        dpi_scale,
                        hdr: output
                            .cast::<IDXGIOutput6>()
                            .ok()
                            .and_then(|o| o.GetDesc1().ok())
                            .is_some_and(|d| {
                                d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                            }),
                        adapter: {
                            let luid = adapter.GetDesc1()?.AdapterLuid;
                            (u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart)
                        },
                        identity: display_identity(&name),
                    },
                ));
            }
        }
        Ok(result)
    }
}

pub(crate) fn screens() -> Result<Vec<Screen>> {
    Ok(outputs()?
        .into_iter()
        .map(|(_, _, screen)| screen)
        .collect())
}

pub(super) fn display_identity(name: &str) -> Option<String> {
    const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 1;
    let name: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
    let mut device = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    if !unsafe {
        EnumDisplayDevicesW(
            PCWSTR(name.as_ptr()),
            0,
            &mut device,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    }
    .as_bool()
    {
        return None;
    }
    let end = device
        .DeviceID
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(device.DeviceID.len());
    (end > 0).then(|| String::from_utf16_lossy(&device.DeviceID[..end]))
}
pub(crate) fn refresh(selected: &Screen) -> Result<Screen> {
    let mut found = screens()?
        .into_iter()
        .find(|s| match &selected.identity {
            Some(identity) => s.identity.as_ref() == Some(identity),
            None => s.adapter == selected.adapter && s.name == selected.name,
        })
        .ok_or(SourceGone)?;
    found.id = selected.id;
    Ok(found)
}
pub(crate) fn create_device(luid: u64) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut selected = None;
        for index in 0..64 {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(a) => a,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            let id = adapter.GetDesc1()?.AdapterLuid;
            if ((u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart)) == luid {
                selected = Some(adapter);
                break;
            }
        }
        let adapter = selected.context("采集适配器已移除")?;
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        Ok((
            device.context("创建图形设备失败")?,
            context.context("创建图形上下文失败")?,
        ))
    }
}
pub(crate) fn adapters() -> Result<Vec<(u64, u32)>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut result = Vec::new();
        for index in 0..64 {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(a) => a,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            let desc = adapter.GetDesc1()?;
            if matches!(desc.VendorId, 0x10de | 0x1002 | 0x8086) {
                result.push((
                    (u64::from(desc.AdapterLuid.HighPart as u32) << 32)
                        | u64::from(desc.AdapterLuid.LowPart),
                    desc.VendorId,
                ));
            }
        }
        Ok(result)
    }
}

#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub texture: ID3D11Texture2D,
    pub captured: std::time::Instant,
    pub is_new: bool,
    pub hdr_metadata: Option<crate::video_color::HdrMetadata>,
}

enum Backend {
    Dxgi(Duplication),
    Gdi(super::gdi::Capture),
}
pub(crate) struct Desktop {
    pub device: ID3D11Device,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    backend: Backend,
    retry_at: std::time::Instant,
    retries: u8,
    refreshed: std::time::Instant,
}
impl Desktop {
    pub fn open_selected(selected: &Screen) -> Result<Self> {
        let screen = refresh(selected)?;
        let backend = match Duplication::open(&screen.name) {
            Ok(capture) => {
                ensure!(
                    capture.screen.identity == screen.identity
                        && capture.screen.adapter == screen.adapter,
                    "采集准备期间所选屏幕变化"
                );
                Backend::Dxgi(capture)
            }
            Err(error) => {
                tracing::debug!(%error,"desktop duplication unavailable; selecting GDI");
                Backend::Gdi(super::gdi::Capture::new(&screen)?)
            }
        };
        let device = match &backend {
            Backend::Dxgi(d) => d.device.clone(),
            Backend::Gdi(g) => g.device.clone(),
        };
        Ok(Self {
            device,
            screen,
            generation: 0,
            available: true,
            backend,
            retry_at: std::time::Instant::now() + std::time::Duration::from_secs(10),
            retries: 0,
            refreshed: std::time::Instant::now(),
        })
    }
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Dxgi(_) => "DXGI",
            Backend::Gdi(_) => "GDI",
        }
    }
    pub fn hdr_available(&self) -> bool {
        match &self.backend {
            Backend::Dxgi(d) => d.source_hdr.is_some(),
            Backend::Gdi(_) => false,
        }
    }
    fn replace(&mut self, backend: Backend) {
        self.device = match &backend {
            Backend::Dxgi(d) => d.device.clone(),
            Backend::Gdi(g) => g.device.clone(),
        };
        self.backend = backend;
        self.generation = self.generation.wrapping_add(1);
    }
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        let request = super::preprocess::Request {
            quality,
            maximum,
            hdr,
        };
        if self.refreshed.elapsed() >= std::time::Duration::from_secs(1) {
            let current = refresh(&self.screen)?;
            self.refreshed = std::time::Instant::now();
            if current != self.screen {
                let mut replacement = Self::open_selected(&current)?;
                replacement.generation = self.generation.wrapping_add(1);
                *self = replacement;
            }
        }
        if matches!(self.backend, Backend::Gdi(_))
            && self.retries < 5
            && std::time::Instant::now() >= self.retry_at
        {
            self.retries += 1;
            self.retry_at = std::time::Instant::now() + std::time::Duration::from_secs(10);
            if let Ok(mut preferred) = Duplication::open(&self.screen.name) {
                ensure!(
                    preferred.screen.identity == self.screen.identity
                        && preferred.screen.adapter == self.screen.adapter,
                    "恢复期间所选屏幕变化"
                );
                if let Ok(Some(frame)) = preferred.next(timeout, quality, cursor, hdr, maximum) {
                    self.replace(Backend::Dxgi(preferred));
                    self.retries = 0;
                    self.available = true;
                    return Ok(Some(frame));
                }
            }
        }
        let frame = match &mut self.backend {
            Backend::Dxgi(d) => d.next(timeout, quality, cursor, hdr, maximum),
            Backend::Gdi(g) => g.next(request, cursor).map(Some),
        };
        match frame {
            Ok(frame) => {
                self.available = true;
                Ok(frame)
            }
            Err(error) => {
                self.available = false;
                if error
                    .downcast_ref::<windows::core::Error>()
                    .is_some_and(|e| {
                        e.code() == windows::Win32::Foundation::E_ACCESSDENIED
                            || e.code() == DXGI_ERROR_SESSION_DISCONNECTED
                    })
                {
                    return Ok(None);
                }
                if self.screen.identity.is_none()
                    && error
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|e| e.code() == DXGI_ERROR_ACCESS_LOST)
                {
                    return Err(SourceGone.into());
                }
                // Re-establish only the same physical output. Never reselect by
                // a reused enumeration index after unplug/replug or desktop loss.
                self.screen = refresh(&self.screen)?;
                // T BC3830 maps a non-timeout acquisition failure to 4;
                // BAA520/BC80A0 disable that candidate and select GDI. Creating
                // another duplication successfully does not prove it can acquire
                // frames. The existing delayed probe requires a real frame before
                // switching back, avoiding an endless ACCESS_LOST rebuild loop.
                let replacement = Backend::Gdi(super::gdi::Capture::new(&self.screen)?);
                tracing::debug!(error = %format!("{error:#}"), "selected capture failed; falling back to GDI");
                self.replace(replacement);
                self.retry_at = std::time::Instant::now() + std::time::Duration::from_secs(10);
                Ok(None)
            }
        }
    }
}

struct Duplication {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    rotation: DXGI_MODE_ROTATION,
    pub screen: Screen,
    converter: super::preprocess::Converter,
    sdr_white: f32,
    source_hdr: Option<crate::video_color::HdrMetadata>,
    quality: Option<super::preprocess::Request>,
    cursor: super::cursor::Cursor,
    cursor_capture: bool,
}

impl Duplication {
    pub(crate) fn open(name: &str) -> Result<Self> {
        let (adapter, output, screen) = outputs()?
            .into_iter()
            .find(|(_, _, screen)| screen.name == name)
            .context("所选屏幕已断开")?;
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("未创建采集设备")?;
            let advanced = output
                .cast::<IDXGIOutput6>()
                .ok()
                .and_then(|o| o.GetDesc1().ok());
            let use_float = advanced.as_ref().is_some_and(|d| {
                matches!(
                    d.ColorSpace,
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                        | DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709
                )
            });
            let duplication = if let Ok(output5) = output.cast::<IDXGIOutput5>() {
                let formats = if use_float {
                    vec![DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT]
                } else {
                    vec![DXGI_FORMAT_B8G8R8A8_UNORM]
                };
                output5.DuplicateOutput1(&device, 0, &formats).or_else(|_| {
                    output5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                })
            } else {
                output.DuplicateOutput(&device)
            }
            .context("无法采集当前桌面；请检查屏幕、锁屏或其他采集程序")?;
            let source_hdr = advanced
                .filter(|d| {
                    d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                        && duplication.GetDesc().ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT
                })
                .map(|d| crate::video_color::HdrMetadata {
                    max_luminance: d.MaxLuminance.clamp(0., 20000.) as u16,
                    min_luminance: (d.MinLuminance * 10000.).clamp(0., 50000.) as u16,
                    chromaticity: [35400, 14600, 8500, 39850, 6550, 2300, 15635, 16450],
                    max_content_light_level: d.MaxLuminance.clamp(0., 20000.) as u16,
                    max_frame_average_light_level: d.MaxFullFrameLuminance.clamp(0., 20000.) as u16,
                });
            let rotation = duplication.GetDesc().Rotation;
            let converter = super::preprocess::Converter::new(&device)?;
            let sdr_white = crate::display_hdr::sdr_white_scale_or(
                &screen.name,
                if source_hdr.is_some() { 3.75 } else { 1. },
            );
            Ok(Self {
                device,
                context: context.context("未创建采集上下文")?,
                duplication,
                rotation,
                screen,
                converter,
                sdr_white,
                source_hdr,
                quality: None,
                cursor: Default::default(),
                cursor_capture: false,
            })
        }
    }

    /// A timeout means there is no new desktop image, not a lost capture source.
    pub(crate) fn next(
        &mut self,
        timeout_ms: u32,
        quality: i32,
        cursor_capture: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        let request = super::preprocess::Request {
            quality,
            maximum,
            hdr,
        };
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match self
                .duplication
                .AcquireNextFrame(timeout_ms.min(50), &mut info, &mut resource)
            {
                Ok(()) => {}
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    // A quality change must also work on a completely static desktop.
                    if self.quality != Some(request) || self.cursor_capture != cursor_capture {
                        if let Some((texture, desc)) = self.converter.last_input() {
                            let texture = self.converter.convert(
                                &self.device,
                                &self.context,
                                &texture,
                                desc,
                                self.rotation,
                                self.sdr_white,
                                self.source_hdr,
                                request,
                                if cursor_capture {
                                    self.cursor.render(self.screen.width, self.screen.height)
                                } else {
                                    None
                                },
                            )?;
                            let mut output = D3D11_TEXTURE2D_DESC::default();
                            texture.GetDesc(&mut output);
                            self.quality = Some(request);
                            self.cursor_capture = cursor_capture;
                            return Ok(Some(Frame {
                                width: output.Width,
                                height: output.Height,
                                texture,
                                captured: std::time::Instant::now(),
                                is_new: false,
                                hdr_metadata: if hdr { self.source_hdr } else { None },
                            }));
                        }
                    }
                    return Ok(None);
                }
                Err(error) => return Err(error).context("桌面采集失效，需要重建所选屏幕采集"),
            }
            // Always release the acquired frame, including conversion and allocation errors.
            let captured = std::time::Instant::now();
            let result = (|| -> Result<Option<Frame>> {
                self.cursor.update(&self.device, &self.duplication, &info)?;
                let texture: ID3D11Texture2D = resource.context("采集未返回纹理")?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                ensure!(
                    matches!(
                        desc.Format,
                        DXGI_FORMAT_B8G8R8A8_UNORM
                            | DXGI_FORMAT_R8G8B8A8_UNORM
                            | DXGI_FORMAT_R16G16B16A16_FLOAT
                    ),
                    "不支持的采集像素格式：{}",
                    desc.Format.0
                );
                ensure!(
                    desc.Width > 0
                        && desc.Height > 0
                        && desc.Width <= 16384
                        && desc.Height <= 16384,
                    "采集纹理尺寸无效"
                );
                let texture = self.converter.convert(
                    &self.device,
                    &self.context,
                    &texture,
                    desc,
                    self.rotation,
                    self.sdr_white,
                    self.source_hdr,
                    request,
                    if cursor_capture {
                        self.cursor.render(self.screen.width, self.screen.height)
                    } else {
                        None
                    },
                )?;
                texture.GetDesc(&mut desc);
                self.quality = Some(request);
                self.cursor_capture = cursor_capture;
                Ok(Some(Frame {
                    width: desc.Width,
                    height: desc.Height,
                    texture,
                    captured,
                    is_new: info.LastPresentTime != 0
                        || (cursor_capture && info.LastMouseUpdateTime != 0),
                    hdr_metadata: if hdr { self.source_hdr } else { None },
                }))
            })();
            let released = self.duplication.ReleaseFrame();
            let result = result?;
            released.context("释放采集帧失败")?;
            Ok(result)
        }
    }
}
