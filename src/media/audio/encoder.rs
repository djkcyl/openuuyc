//! Shared Opus sending configuration and encoder for microphone and future desktop capture.
use anyhow::Result;
use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate};
const RATE: u32 = 48_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub stereo: bool,
    pub fec: bool,
    pub dtx: bool,
    pub cbr: bool,
    pub bitrate: u32,
    pub packet_ms: u32,
    pub max_playback_rate: u32,
}

impl Config {
    pub(crate) fn negotiated(fmtp: &str, ptime: Option<u32>) -> Result<Self> {
        let params: std::collections::HashMap<_, _> = fmtp
            .split(';')
            .filter_map(|v| v.trim().split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let number = |key| params.get(key).and_then(|s| s.parse::<u32>().ok());
        let flag = |key| params.get(key).is_some_and(|s| *s == "1");
        let requested = ptime.or_else(|| number("ptime")).unwrap_or(20);
        // T121408 rounds up to the next supported packet duration, capped at 120.
        let packet_ms = [10, 20, 40, 60, 120]
            .into_iter()
            .find(|v| *v >= requested)
            .unwrap_or(120);
        Ok(Self {
            stereo: flag("stereo"),
            fec: flag("useinbandfec"),
            dtx: flag("usedtx"),
            cbr: flag("cbr"),
            bitrate: number("maxaveragebitrate")
                .unwrap_or(100_000)
                .clamp(6_000, 100_000),
            packet_ms,
            max_playback_rate: number("maxplaybackrate").unwrap_or(RATE).clamp(8_000, RATE),
        })
    }
}

pub(crate) fn create_encoder(config: Config) -> std::result::Result<Encoder, opusic_c::ErrorCode> {
    let mut e = Encoder::new(
        if config.stereo {
            Channels::Stereo
        } else {
            Channels::Mono
        },
        SampleRate::Hz48000,
        if config.stereo {
            Application::Audio
        } else {
            Application::Voip
        },
    )?;
    e.set_bitrate(Bitrate::Value(config.bitrate))?;
    e.set_complexity(9)?;
    e.set_vbr(!config.cbr)?;
    e.set_dtx(config.dtx)?;
    e.set_inband_fec(if config.fec {
        InbandFec::Mode1
    } else {
        InbandFec::Off
    })?;
    e.set_max_bandwidth(match config.max_playback_rate {
        0..=8000 => opusic_c::Bandwidth::Narrow,
        8001..=12000 => opusic_c::Bandwidth::Medium,
        12001..=16000 => opusic_c::Bandwidth::Wide,
        16001..=24000 => opusic_c::Bandwidth::Superwide,
        _ => opusic_c::Bandwidth::Full,
    })?;
    Ok(e)
}
