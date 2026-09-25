use anyhow::{Context, Result, bail};
use display_info::DisplayInfo;

const FPS_CHOICES: [FrameRateChoice; 5] = [
    FrameRateChoice::Auto,
    FrameRateChoice::Fps144,
    FrameRateChoice::Fps90,
    FrameRateChoice::Fps60,
    FrameRateChoice::Fps30,
];
const FPS_LEVELS_ASCENDING: [u32; 4] = [30, 60, 90, 144];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LocalDisplayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl LocalDisplayInfo {
    pub const FALLBACK: Self = Self {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameRateChoice {
    Auto,
    Fps144,
    Fps90,
    Fps60,
    Fps30,
}

impl FrameRateChoice {
    pub fn value(self, display: LocalDisplayInfo) -> u32 {
        match self {
            Self::Auto => max_frame_rate_level(display.refresh_hz),
            Self::Fps144 => 144,
            Self::Fps90 => 90,
            Self::Fps60 => 60,
            Self::Fps30 => 30,
        }
    }

    pub fn available(_display: LocalDisplayInfo) -> Vec<Self> {
        // The ordinary desktop menu exposes all four explicit levels. The
        // receiver refresh rate limits fps_count, not the user's level list.
        FPS_CHOICES.to_vec()
    }

    pub fn label(self, display: LocalDisplayInfo) -> String {
        let value = self.value(display);
        if self == Self::Auto {
            format!("自动 {value} FPS")
        } else {
            format!("{value} FPS")
        }
    }
}

impl std::str::FromStr for FrameRateChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "144" | "144fps" => Ok(Self::Fps144),
            "90" | "90fps" => Ok(Self::Fps90),
            "60" | "60fps" => Ok(Self::Fps60),
            "30" | "30fps" => Ok(Self::Fps30),
            _ => bail!("unsupported frame-rate choice: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CodecPreference {
    Auto,
    H264,
    H265,
}

impl CodecPreference {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动 H.265/H.264",
            Self::H264 => "H.264",
            Self::H265 => "H.265",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::H265,
            Self::H265 => Self::H264,
            Self::H264 => Self::Auto,
        }
    }
}

impl std::str::FromStr for CodecPreference {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "h264" | "avc" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            _ => bail!("unsupported codec preference: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TransportChoice {
    Auto,
    P2p,
    Relay,
}

impl TransportChoice {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（LAN/P2P/relay）",
            Self::P2p => "仅 LAN/P2P",
            Self::Relay => "仅 relay",
        }
    }
}

impl std::str::FromStr for TransportChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "p2p" | "direct" | "lan" => Ok(Self::P2p),
            "relay" | "turn" => Ok(Self::Relay),
            _ => bail!("unsupported transport choice: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ConnectionMediaOptions {
    pub muted: bool,
    pub frame_rate: FrameRateChoice,
    pub codec: CodecPreference,
    pub hardware_decode: bool,
    pub transport: TransportChoice,
    /// Take keyboard and mouse control as soon as the control channel is ready,
    /// instead of waiting for the player's 键鼠控制 button.
    pub auto_mouse_control: bool,
    /// Offer and accept files through the clipboard. Off by default: a copy
    /// reaches for whatever it names, so it carries more than a paste of text
    /// or a picture does, and the player's 文件复制 switch turns it on per
    /// session.
    pub clipboard_files: bool,
}

impl Default for ConnectionMediaOptions {
    fn default() -> Self {
        Self {
            muted: false,
            frame_rate: FrameRateChoice::Auto,
            codec: CodecPreference::Auto,
            hardware_decode: true,
            transport: TransportChoice::Auto,
            auto_mouse_control: true,
            clipboard_files: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct ConnectionMediaProfile {
    pub muted: bool,
    pub local_display: LocalDisplayInfo,
    pub stream_fps: u32,
    pub decoder_fps_cap: u32,
    pub codec: CodecPreference,
    pub hardware_decode: bool,
    pub auto_mouse_control: bool,
    pub clipboard_files: bool,
}

impl ConnectionMediaOptions {
    pub(crate) fn resolve(self, display: LocalDisplayInfo) -> Result<ConnectionMediaProfile> {
        let stream_fps = self.frame_rate.value(display);
        Ok(ConnectionMediaProfile {
            muted: self.muted,
            local_display: display,
            stream_fps,
            decoder_fps_cap: display.refresh_hz.max(stream_fps),
            codec: self.codec,
            hardware_decode: self.hardware_decode,
            auto_mouse_control: self.auto_mouse_control,
            clipboard_files: self.clipboard_files,
        })
    }
}

pub fn detect_local_display() -> Result<LocalDisplayInfo> {
    let displays = DisplayInfo::all().context("failed to enumerate local displays")?;
    let display = displays
        .iter()
        .find(|display| display.is_primary)
        .or_else(|| {
            displays
                .iter()
                .max_by_key(|display| u64::from(display.width) * u64::from(display.height))
        })
        .context("no local display was detected")?;
    if display.width == 0 || display.height == 0 {
        bail!("local display reported an invalid resolution");
    }
    // Use the maximum refresh of active displays with a 30 Hz floor.
    // The primary display still supplies geometry; using
    // only its refresh would incorrectly limit viewing on a faster monitor.
    let refresh_hz = displays
        .iter()
        .filter(|display| display.frequency.is_finite() && display.frequency > 0.0)
        .map(|display| display.frequency.round() as u32)
        .max()
        .unwrap_or(30)
        .max(30);
    Ok(LocalDisplayInfo {
        width: display.width,
        height: display.height,
        refresh_hz,
    })
}

/// Current active display dimensions offered by the UU controller. This is
/// neither a list of supported physical modes nor a request to change one.
pub(crate) fn local_display_dimensions() -> Vec<(u32, u32)> {
    let mut modes = DisplayInfo::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.width != 0 && d.height != 0)
        .map(|d| (d.width, d.height))
        .collect::<Vec<_>>();
    modes.sort_unstable();
    modes.dedup();
    if modes.is_empty() {
        modes.push((1920, 1080));
    }
    modes
}

fn max_frame_rate_level(refresh_hz: u32) -> u32 {
    FPS_LEVELS_ASCENDING
        .into_iter()
        .find(|level| refresh_hz <= *level)
        .unwrap_or(144)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VideoCodec {
    H264,
    H265,
}

impl std::str::FromStr for VideoCodec {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            _ => bail!("unsupported video codec: {value}"),
        }
    }
}
