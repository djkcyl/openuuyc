//! Screen capture for the host role, with backends tried in order of
//! preference the way Sunshine picks its capture method.
//!
//! - NVIDIA NvFBC into CUDA (`nvfbc`): Xorg on NVIDIA, only while NVENC works,
//!   so frames go from the display to the encoder without leaving the GPU.
//! - X11 MIT-SHM (`x11`): Xorg sessions; no setup, no consent prompt.
//! - XDG ScreenCast portal + PipeWire (`portal`): Wayland sessions, and any
//!   desktop whose portal offers it; asks the local user once.
//!
//! Sunshine puts KMS first; on this driver stack it finds no scanout buffer
//! under Xorg (the NVIDIA X driver bypasses KMS), so it is not in the list
//! yet. On Wayland only the portal can see the desktop. `OPENUUYC_CAPTURE`
//! (for example `portal` or `nvfbc,x11`) overrides the order.
//!
//! Frames are BGRA, in system memory or, from NvFBC, in CUDA device memory.
mod nvfbc;
mod pipewire;
mod portal;
mod x11;

use anyhow::{Result, bail, ensure};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) use crate::media::capture::{Screen, SourceGone};

/// An X11 capture has no GPU device. The one value stands for the CPU path
/// that capture and the software encoder share, so the loop's "same device,
/// no transfer" test always holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device;

/// Nothing on the CPU path can be removed from under the capture.
pub(crate) fn device_lost(_device: &Device) -> bool {
    false
}

/// Linux captures on the CPU, so there is no adapter for a second device to be
/// opened on. The host only asks for one when a hardware encoder sits on a
/// different adapter than the capture, which cannot happen here.
pub(crate) fn create_device(_adapter: u64) -> Result<(Device, ())> {
    Ok((Device, ()))
}

/// GPU adapters a hardware encoder could run on. None are offered: encoding
/// on Linux is the Rust H.264 core, which needs no adapter.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EncodingAdapter {
    pub luid: u64,
    pub vendor: u32,
    pub name: String,
}

pub(crate) fn encoding_adapters() -> Result<Vec<EncodingAdapter>> {
    Ok(Vec::new())
}

/// Whether the desktop session is locked. X11 has no portable answer: a
/// screen locker is just another client drawing over the root window, and the
/// capture sees exactly what it draws.
pub(crate) fn session_locked() -> Option<bool> {
    None
}

/// A captured picture's BGRA pixels, wherever the capture left them.
#[derive(Clone)]
pub(crate) enum Image {
    /// Tightly packed, `width * height * 4` bytes.
    Cpu {
        pixels: Arc<Vec<u8>>,
        width: u32,
        height: u32,
    },
    /// A pitched CUDA allocation on the GPU driving the display.
    Cuda(Arc<super::cuda::Buffer>),
}
impl Image {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Cpu { width, height, .. } => (*width, *height),
            Self::Cuda(buffer) => (buffer.width, buffer.height),
        }
    }
    /// The pixels in system memory, read back from the GPU if need be.
    pub fn pixels(&self) -> Result<std::borrow::Cow<'_, [u8]>> {
        Ok(match self {
            Self::Cpu { pixels, .. } => std::borrow::Cow::Borrowed(pixels.as_slice()),
            Self::Cuda(buffer) => std::borrow::Cow::Owned(buffer.download()?),
        })
    }
}

/// One captured picture, already scaled to the size the encoder was asked for.
#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub image: Image,
    pub captured: Instant,
    pub is_new: bool,
    pub hdr_metadata: Option<crate::media::video_color::HdrMetadata>,
}

/// The active RandR outputs, in the protocol's terms. The identity is the
/// one the display topology uses, so screens and targets pair up exactly.
pub(crate) fn screens() -> Result<Vec<Screen>> {
    // Monitors come from RandR, which XWayland also provides on Wayland.
    ensure!(
        std::env::var_os("DISPLAY").is_some(),
        "没有 X11 显示（DISPLAY 未设置），无法枚举显示器"
    );
    let monitors = super::display::topology::Topology::query(true)?.monitors();
    ensure!(!monitors.is_empty(), "没有检测到显示器");
    monitors
        .into_iter()
        .map(|monitor| {
            Ok(Screen {
                id: super::display::source_id(&monitor.identity)?,
                device_name: monitor.name.clone(),
                display_name: monitor.name,
                width: monitor.width,
                height: monitor.height,
                left: monitor.left,
                top: monitor.top,
                primary: monitor.primary,
                fps: monitor.hz.filter(|hz| *hz > 1).unwrap_or(60),
                // X11 has no per-output scale to report.
                dpi_scale: None,
                hdr: false,
                adapter: 0,
                render_adapter: None,
                identity: Some(monitor.identity),
            })
        })
        .collect()
}

/// Whether this is a Wayland session, where X11 sees only XWayland clients.
pub(super) fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland")
}

/// The same physical output as `selected`, as it is now.
pub(crate) fn refresh(selected: &Screen) -> Result<Screen> {
    let mut found = screens()?
        .into_iter()
        .find(|screen| screen.identity == selected.identity)
        .ok_or(SourceGone)?;
    found.id = selected.id;
    Ok(found)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    NvFbc,
    X11,
    Portal,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::NvFbc => "NvFBC",
            Self::X11 => "X11",
            Self::Portal => "XDG 门户",
        }
    }
}

/// The capture methods to try, best first.
fn candidates() -> Result<Vec<Kind>> {
    if let Ok(order) = std::env::var("OPENUUYC_CAPTURE") {
        let kinds = order
            .split(',')
            .map(|name| match name.trim() {
                "nvfbc" => Ok(Kind::NvFbc),
                "x11" => Ok(Kind::X11),
                "portal" => Ok(Kind::Portal),
                other => bail!("OPENUUYC_CAPTURE 中有未知的采集方式：{other}"),
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(!kinds.is_empty(), "OPENUUYC_CAPTURE 为空");
        return Ok(kinds);
    }
    Ok(if wayland_session() {
        vec![Kind::Portal]
    } else {
        vec![Kind::NvFbc, Kind::X11, Kind::Portal]
    })
}

enum Backend {
    NvFbc(nvfbc::Grab),
    X11(x11::Grab),
    Portal(portal::Grab),
}
impl Backend {
    /// `size` is the frame size for backends that scale while capturing.
    fn open(kind: Kind, screen: &Screen, cursor: bool, size: (u32, u32)) -> Result<Self> {
        Ok(match kind {
            Kind::NvFbc => Self::NvFbc(nvfbc::Grab::open(screen, cursor, size)?),
            Kind::X11 => Self::X11(x11::Grab::open(screen)?),
            Kind::Portal => Self::Portal(portal::Grab::open(screen, cursor)?),
        })
    }
    fn kind(&self) -> Kind {
        match self {
            Self::NvFbc(_) => Kind::NvFbc,
            Self::X11(_) => Kind::X11,
            Self::Portal(_) => Kind::Portal,
        }
    }
    /// Open the first capture method that works for `screen`.
    fn select(screen: &Screen, cursor: bool, size: (u32, u32)) -> Result<Self> {
        let mut failures = Vec::new();
        for kind in candidates()? {
            match Self::open(kind, screen, cursor, size) {
                Ok(backend) => {
                    tracing::info!(
                        backend = backend.name(),
                        screen = screen.id,
                        "host capture backend selected"
                    );
                    return Ok(backend);
                }
                Err(error) => {
                    tracing::info!(backend = kind.label(), error = %format!("{error:#}"), "host capture backend unavailable");
                    failures.push(format!("{}：{error:#}", kind.label()));
                }
            }
        }
        bail!("没有可用的屏幕采集方式（{}）", failures.join("；"))
    }
    fn name(&self) -> &'static str {
        match self {
            Self::NvFbc(grab) => grab.name(),
            Self::X11(grab) => grab.name(),
            Self::Portal(grab) => grab.name(),
        }
    }
}

/// A capture of one monitor, kept open between frames.
pub(crate) struct Desktop {
    pub device: Device,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    /// The pointer as of the latest `next`, for the cursor channel.
    pub cursor: Option<super::cursor_shape::Snapshot>,
    sampler: super::cursor_shape::Sampler,
    backend: Backend,
    /// The last system-memory picture, which X11 compares against.
    last: Option<Arc<Vec<u8>>>,
    last_size: (u32, u32),
    refreshed: Instant,
}

impl Desktop {
    pub fn open_selected(selected: &Screen) -> Result<Self> {
        let screen = refresh(selected)?;
        let size = (screen.width & !1, screen.height & !1);
        let backend = Backend::select(&screen, false, size)?;
        Ok(Self {
            device: Device,
            screen,
            generation: 0,
            available: true,
            cursor: None,
            sampler: Default::default(),
            backend,
            last: None,
            last_size: (0, 0),
            refreshed: Instant::now(),
        })
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Neither backend captures HDR.
    pub fn hdr_available(&self) -> bool {
        false
    }

    /// The next picture, or `None` when nothing changed before `timeout` ms.
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        _hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        if self.refreshed.elapsed() >= Duration::from_secs(1) {
            let current = refresh(&self.screen)?;
            self.refreshed = Instant::now();
            self.screen.display_name.clone_from(&current.display_name);
            if current != self.screen {
                let size = output(current.width, current.height, quality, maximum);
                let backend = Backend::open(self.backend.kind(), &current, cursor, size)
                    .or_else(|_| Backend::select(&current, cursor, size))?;
                self.backend = backend;
                self.screen = current;
                self.generation = self.generation.wrapping_add(1);
                self.last = None;
            }
        }
        // The portal draws the cursor or leaves it out for the whole session;
        // a change of request moves to a session with the other mode.
        if let Backend::Portal(grab) = &self.backend
            && grab.cursor() != cursor
        {
            self.backend = Backend::Portal(portal::Grab::open(&self.screen, cursor)?);
            self.last = None;
        }
        // NvFBC scales and draws the cursor as it captures; both are fixed
        // per session, so a different request opens a new one.
        let wanted = output(self.screen.width, self.screen.height, quality, maximum);
        if let Backend::NvFbc(grab) = &self.backend
            && !grab.matches(cursor, wanted)
        {
            self.backend = Backend::NvFbc(nvfbc::Grab::open(&self.screen, cursor, wanted)?);
        }
        self.cursor = match self.sampler.sample() {
            Ok(pointer) => Some(pointer),
            Err(error) => {
                tracing::debug!(%error, "host cursor sampling failed");
                None
            }
        };
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout));
        let result = match &mut self.backend {
            // The driver waits for a new frame and delivers it on the GPU.
            Backend::NvFbc(grab) => {
                return match grab.next(timeout) {
                    Ok(frame) => {
                        self.available = true;
                        Ok(frame.map(|buffer| Frame {
                            width: buffer.width,
                            height: buffer.height,
                            image: Image::Cuda(buffer),
                            captured: Instant::now(),
                            is_new: true,
                            hdr_metadata: None,
                        }))
                    }
                    Err(error) => {
                        self.available = false;
                        Err(error)
                    }
                };
            }
            Backend::X11(grab) => loop {
                if let Err(error) = grab.grab(&self.screen, cursor) {
                    break Err(error);
                }
                let size = output(self.screen.width, self.screen.height, quality, maximum);
                let image = scale(&grab.source, (self.screen.width, self.screen.height), size);
                // X11 cannot say whether anything changed; compare.
                if self.last_size != size
                    || self
                        .last
                        .as_deref()
                        .is_none_or(|last| last.as_slice() != image.as_slice())
                {
                    break Ok(Some((size, image)));
                }
                let now = Instant::now();
                if now >= deadline {
                    break Ok(None);
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(8)));
            },
            // PipeWire only delivers a picture when the monitor changed.
            Backend::Portal(grab) => grab
                .next(deadline.saturating_duration_since(Instant::now()))
                .map(|picture| {
                    picture.map(|picture| {
                        let size = output(picture.width, picture.height, quality, maximum);
                        let image = scale(&picture.pixels, (picture.width, picture.height), size);
                        (size, image)
                    })
                }),
        };
        match result {
            Ok(Some((size, image))) => {
                self.available = true;
                let image = Arc::new(image);
                self.last = Some(image.clone());
                self.last_size = size;
                Ok(Some(Frame {
                    width: size.0,
                    height: size.1,
                    image: Image::Cpu {
                        pixels: image,
                        width: size.0,
                        height: size.1,
                    },
                    captured: Instant::now(),
                    is_new: true,
                    hdr_metadata: None,
                }))
            }
            Ok(None) => {
                self.available = true;
                Ok(None)
            }
            Err(error) => {
                self.available = false;
                Err(error)
            }
        }
    }
}

/// The encoded size for a source of `width`x`height` at `quality`.
fn output(width: u32, height: u32, quality: i32, maximum: (u32, u32)) -> (u32, u32) {
    let size = crate::media::geometry::output_size(width, height, quality);
    crate::media::geometry::fit_size(size.0, size.1, maximum)
}

/// Bilinear BGRA scaling, or a crop to even dimensions when the size already
/// fits. The encoder takes exactly the size it was opened with.
fn scale(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (source_width, source_height) = (from.0 as usize, from.1 as usize);
    let (width, height) = (to.0 as usize, to.1 as usize);
    let mut out = vec![0u8; width * height * 4];
    if width <= source_width
        && height <= source_height
        && (source_width - width) <= 1
        && (source_height - height) <= 1
    {
        for y in 0..height {
            let start = y * source_width * 4;
            out[y * width * 4..(y + 1) * width * 4]
                .copy_from_slice(&source[start..start + width * 4]);
        }
        return out;
    }
    // 16.16 fixed point, sampling pixel centres.
    let step_x = ((source_width as u64) << 16) / width as u64;
    let step_y = ((source_height as u64) << 16) / height as u64;
    let max_x = source_width - 1;
    let max_y = source_height - 1;
    for y in 0..height {
        let sy = ((y as u64 * step_y) + (step_y >> 1)).saturating_sub(1 << 15);
        let y0 = ((sy >> 16) as usize).min(max_y);
        let y1 = (y0 + 1).min(max_y);
        let fy = (sy & 0xffff) as u32;
        for x in 0..width {
            let sx = ((x as u64 * step_x) + (step_x >> 1)).saturating_sub(1 << 15);
            let x0 = ((sx >> 16) as usize).min(max_x);
            let x1 = (x0 + 1).min(max_x);
            let fx = (sx & 0xffff) as u32;
            let at = |px: usize, py: usize| (py * source_width + px) * 4;
            let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
            let o = (y * width + x) * 4;
            for channel in 0..4 {
                let top = u32::from(source[a + channel]) * (0x10000 - fx)
                    + u32::from(source[b + channel]) * fx;
                let bottom = u32::from(source[c + channel]) * (0x10000 - fx)
                    + u32::from(source[d + channel]) * fx;
                let value = ((u64::from(top >> 8) * u64::from(0x10000 - fy)
                    + u64::from(bottom >> 8) * u64::from(fy))
                    >> 24) as u32;
                out[o + channel] = value.min(255) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::scale;

    #[test]
    fn crop_to_even_keeps_pixels() {
        let source: Vec<u8> = (0..3 * 3 * 4).map(|v| v as u8).collect();
        let out = scale(&source, (3, 3), (2, 2));
        assert_eq!(&out[..8], &source[..8]);
        assert_eq!(&out[8..16], &source[12..20]);
    }

    #[test]
    fn downscale_of_flat_colour_is_flat() {
        let source = [10u8, 20, 30, 255].repeat(64 * 48);
        let out = scale(&source, (64, 48), (32, 24));
        assert!(out.chunks_exact(4).all(|p| p == [10, 20, 30, 255]));
    }
}

#[cfg(test)]
mod session_tests {
    /// Needs a desktop session: `OPENUUYC_CAPTURE=portal cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn captures_frames_for_a_few_seconds() {
        let screens = super::screens().unwrap();
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend: {}", desktop.backend_name());
        let started = std::time::Instant::now();
        let (mut frames, mut size) = (0, None);
        while started.elapsed() < std::time::Duration::from_secs(4) {
            if let Some(frame) = desktop.next(50, 4, false, false, (3840, 2160)).unwrap() {
                frames += 1;
                size = Some((frame.width, frame.height));
            }
        }
        println!("frames: {frames} size: {size:?}");
        assert!(frames > 0);
    }
}
