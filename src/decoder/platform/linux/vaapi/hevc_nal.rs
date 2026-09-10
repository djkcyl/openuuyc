//! HEVC NAL unit header parsing + emulation-prevention removal — VA-API decode's crate-local
//! copy of `vulkan::hevc_params::HevcNalUnit::parse`/`remove_emulation_prevention` (cited, not
//! imported — this session's own no-cross-module-import convention, see
//! `adr/linux/0003-vaapi-hevc-p-slice-dpb.md` § Alternatives Considered). Codec-agnostic bit
//! layout (2-byte HEVC NAL header, `00 00 03` emulation prevention — ITU-T H.265 § 7.3.1.1 /
//! § 7.4.2), so this crate writes its own local copy rather than reusing
//! `mediaway_sw::h264::NalUnit` (H.264's own 1-byte header layout).

#![forbid(unsafe_code)]

use crate::decoder::platform::DecodeError;

/// HEVC NAL unit type (`nal_unit_type`, ITU-T H.265 Table 7-1) — only the values this crate's
/// decode path checks get named variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HevcNalUnitType {
    /// Coded slice segment, trailing picture, not a reference (`TRAIL_N`, 0) or a reference
    /// (`TRAIL_R`, 1) — the common non-IRAP inter-picture types this workspace's own sibling
    /// encoder (`mediaway-encoder` ADR-0003) emits for every P picture.
    Trail,
    /// IDR picture, no leading pictures (`IDR_W_RADL` 19 / `IDR_N_LP` 20) — the only intra
    /// picture type this crate's own decode path accepts.
    Idr,
    /// CRA picture (`CRA_NUT`, 21) — an intra random-access point that is not an IDR. Named
    /// (not folded into `Other`) so this crate's own dispatch can explicitly reject it as
    /// `Unsupported` rather than silently skip real coded picture data, matching this ADR's own
    /// permanent scope cut (`docs/roadmap.md`).
    Cra,
    /// Video parameter set (32).
    Vps,
    /// Sequence parameter set (33).
    Sps,
    /// Picture parameter set (34).
    Pps,
    /// Any other type value (SEI, AUD, other slice types, extensions) — ignored wherever this
    /// crate's decode path dispatches on NAL type, mirroring this crate's H.264 sibling's
    /// identical `_ => {}` disposition for non-picture-bearing NAL types.
    Other(u8),
}

impl HevcNalUnitType {
    #[must_use]
    const fn from_u8(value: u8) -> Self {
        match value {
            0..=9 => Self::Trail,
            19 | 20 => Self::Idr,
            21 => Self::Cra,
            32 => Self::Vps,
            33 => Self::Sps,
            34 => Self::Pps,
            other => Self::Other(other),
        }
    }
}

/// One parsed HEVC NAL unit: decoded type, whether it is a reference picture, plus RBSP payload
/// (2-byte header already stripped, emulation-prevention bytes already removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HevcNalUnit {
    pub(super) unit_type: HevcNalUnitType,
    /// Whether this NAL unit's picture is usable as a future reference — ITU-T H.265's own
    /// `_N`/`_R` VCL NAL-type-suffix convention for `Trail`/`Tsa`/`Stsa`/`Radl`/`Rasl` types
    /// (`raw type % 2 == 1` within `0..=9`, mirrors this crate's own sibling encoder's
    /// `nal_unit_type = if is_idr { 19 } else { 1 }` choice — `1` is always `TRAIL_R`, always a
    /// reference), or always `true` for any IRAP type (`16..=21`, covers `Idr`/`Cra`) — the same
    /// `nal_ref_idc != 0` role H.264's own `NalUnit::ref_idc` plays for this crate's H.264
    /// sibling decode path.
    pub(super) is_reference: bool,
    pub(super) rbsp: Vec<u8>,
}

impl HevcNalUnit {
    /// Parse one NAL unit's 2-byte header + de-emulated RBSP from `data`, which must start at
    /// the first header byte (no start code / length prefix), e.g. one element of
    /// [`mediaway_sw::h264::split_annex_b`].
    ///
    /// # Errors
    ///
    /// [`DecodeError::InvalidInput`] if `data` is shorter than 2 bytes, or
    /// [`DecodeError::Unsupported`] if `nuh_layer_id != 0` (multi-layer/scalable HEVC is out of
    /// scope).
    pub(super) fn parse(data: &[u8]) -> Result<Self, DecodeError> {
        let first = *data.first().ok_or(DecodeError::InvalidInput)?;
        let second = *data.get(1).ok_or(DecodeError::InvalidInput)?;
        // forbidden_zero_bit (1 bit) + nal_unit_type (6 bits) + nuh_layer_id high bit (1 bit),
        // all in the first byte; nuh_layer_id low 5 bits + nuh_temporal_id_plus1 (3 bits) in the
        // second.
        let nal_unit_type = (first >> 1) & 0x3F;
        let nuh_layer_id = ((first & 0x1) << 5) | (second >> 3);
        if nuh_layer_id != 0 {
            return Err(DecodeError::Unsupported);
        }
        let rbsp = remove_emulation_prevention(data.get(2..).ok_or(DecodeError::InvalidInput)?);
        Ok(Self {
            unit_type: HevcNalUnitType::from_u8(nal_unit_type),
            is_reference: is_reference_nal_unit_type(nal_unit_type),
            rbsp,
        })
    }
}

/// ITU-T H.265 Table 7-1's `_N`/`_R` VCL-type-suffix convention (`0..=9`: even = non-reference,
/// odd = reference) plus the IRAP range (`16..=21`: `BLA_W_LP`..`CRA_NUT`, always reference) —
/// see [`HevcNalUnit::is_reference`]'s own doc.
const fn is_reference_nal_unit_type(nal_unit_type: u8) -> bool {
    match nal_unit_type {
        0..=9 => nal_unit_type % 2 == 1,
        16..=21 => true,
        _ => false,
    }
}

/// Remove `emulation_prevention_three_byte` — identical `00 00 03` rule to H.264 (ITU-T H.265
/// § 7.3.1.1).
fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zero_run = 0u32;
    for &byte in data {
        if zero_run >= 2 && byte == 0x03 {
            zero_run = 0;
            continue;
        }
        out.push(byte);
        zero_run = if byte == 0 { zero_run + 1 } else { 0 };
    }
    out
}
