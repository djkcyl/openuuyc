//! Minimal Annex-B and bit-reading helpers used by the retained desktop backends.

use mediaway_common::Bytes;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub(crate) enum BitstreamError {
    #[error("truncated video bitstream")]
    UnexpectedEof,
    #[error("no Annex-B start code found")]
    NoStartCode,
    #[error("exponential-Golomb value out of range")]
    ExpGolombOverflow,
}

#[derive(Debug)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) const fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    pub(crate) const fn bits_read(&self) -> usize {
        self.bit_pos
    }

    pub(crate) fn read_bit(&mut self) -> Result<u32, BitstreamError> {
        let byte_index = self.bit_pos / 8;
        let byte = *self
            .data
            .get(byte_index)
            .ok_or(BitstreamError::UnexpectedEof)?;
        let shift = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Ok(u32::from((byte >> shift) & 1))
    }

    pub(crate) fn read_bits(&mut self, count: u32) -> Result<u32, BitstreamError> {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | self.read_bit()?;
        }
        Ok(value)
    }

    pub(crate) fn read_ue(&mut self) -> Result<u32, BitstreamError> {
        let mut leading_zero_bits = 0u32;
        while self.read_bit()? == 0 {
            leading_zero_bits += 1;
            if leading_zero_bits >= u32::BITS {
                return Err(BitstreamError::ExpGolombOverflow);
            }
        }
        if leading_zero_bits == 0 {
            return Ok(0);
        }
        let suffix = self.read_bits(leading_zero_bits)?;
        1u32.checked_shl(leading_zero_bits)
            .and_then(|value| value.checked_sub(1))
            .and_then(|value| value.checked_add(suffix))
            .ok_or(BitstreamError::ExpGolombOverflow)
    }

    pub(crate) fn read_se(&mut self) -> Result<i32, BitstreamError> {
        let code = self.read_ue()?;
        let magnitude =
            i32::try_from(code.div_ceil(2)).map_err(|_| BitstreamError::ExpGolombOverflow)?;
        Ok(if code % 2 == 0 { -magnitude } else { magnitude })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NalUnitType {
    NonIdrSlice,
    SliceDataPartition,
    IdrSlice,
    Sei,
    Sps,
    Pps,
    AccessUnitDelimiter,
    EndOfSequence,
    EndOfStream,
    FillerData,
    Other(u8),
}

impl NalUnitType {
    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::NonIdrSlice,
            2..=4 => Self::SliceDataPartition,
            5 => Self::IdrSlice,
            6 => Self::Sei,
            7 => Self::Sps,
            8 => Self::Pps,
            9 => Self::AccessUnitDelimiter,
            10 => Self::EndOfSequence,
            11 => Self::EndOfStream,
            12 => Self::FillerData,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NalUnit {
    pub(crate) ref_idc: u8,
    pub(crate) unit_type: NalUnitType,
    pub(crate) rbsp: Bytes,
}

impl NalUnit {
    pub(crate) fn parse(data: &[u8]) -> Result<Self, BitstreamError> {
        let header = *data.first().ok_or(BitstreamError::UnexpectedEof)?;
        Ok(Self {
            ref_idc: (header >> 5) & 0b11,
            unit_type: NalUnitType::from_u8(header & 0b1_1111),
            rbsp: Bytes::from(remove_emulation_prevention(&data[1..])),
        })
    }
}

fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut zero_run = 0u32;
    for &byte in data {
        if zero_run >= 2 && byte == 0x03 {
            zero_run = 0;
            continue;
        }
        output.push(byte);
        zero_run = if byte == 0 { zero_run + 1 } else { 0 };
    }
    output
}

pub(crate) fn split_annex_b(data: &[u8]) -> Result<Vec<&[u8]>, BitstreamError> {
    let mut marks = Vec::new();
    let mut index = 0usize;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            marks.push(index);
            index += 3;
        } else {
            index += 1;
        }
    }
    if marks.is_empty() {
        return Err(BitstreamError::NoStartCode);
    }
    let mut units = Vec::with_capacity(marks.len());
    for pair in marks.windows(2) {
        units.push(trim_trailing_zeros(&data[pair[0] + 3..pair[1]]));
    }
    if let Some(&last) = marks.last() {
        units.push(trim_trailing_zeros(&data[last + 3..]));
    }
    Ok(units)
}

fn trim_trailing_zeros(data: &[u8]) -> &[u8] {
    let end = data
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    &data[..end]
}
