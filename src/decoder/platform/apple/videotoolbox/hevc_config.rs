//! HEVC Annex-B to/from the `hvcC` pieces required by VideoToolbox.
//!
//! This is the upstream `mediaway` implementation kept beside the vendored decoder because
//! crates.io `iso-bmff 0.1.1` predates its HEVC module.

use mediaway_common::Bytes;

const VPS_NUT: u8 = 32;
const SPS_NUT: u8 = 33;
const PPS_NUT: u8 = 34;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HvccOut {
    pub(super) payload: Bytes,
    pub(super) hvcc: Option<Bytes>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HevcDecoderConfig {
    pub(super) nal_length_size: u8,
    pub(super) vps: Vec<Bytes>,
    pub(super) sps: Vec<Bytes>,
    pub(super) pps: Vec<Bytes>,
}

pub(super) fn to_hvcc(data: &[u8]) -> HvccOut {
    if !matches!(find_start_code(data), Some((0, _))) {
        return HvccOut {
            payload: Bytes::copy_from_slice(data),
            hvcc: None,
        };
    }

    let mut output = Vec::with_capacity(data.len());
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;
    for nal in NalIter::new(data) {
        if nal.len() < 2 {
            continue;
        }
        match (nal[0] >> 1) & 0x3f {
            VPS_NUT => vps = Some(nal),
            SPS_NUT => sps = Some(nal),
            PPS_NUT => pps = Some(nal),
            _ => {}
        }
        let length = u32::try_from(nal.len()).unwrap_or(u32::MAX);
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(nal);
    }

    HvccOut {
        payload: Bytes::from(output),
        hvcc: match (vps, sps, pps) {
            (Some(vps), Some(sps), Some(pps)) => Some(build_hvcc(vps, sps, pps)),
            _ => None,
        },
    }
}

pub(super) fn parse_hevc_decoder_config(record: &[u8]) -> Option<HevcDecoderConfig> {
    if record.len() < 23 || record[0] != 1 {
        return None;
    }
    let nal_length_size = (record[21] & 0x03) + 1;
    let mut position = 23usize;
    let mut vps = Vec::new();
    let mut sps = Vec::new();
    let mut pps = Vec::new();

    for _ in 0..record[22] {
        let nal_type = *record.get(position)? & 0x3f;
        position += 1;
        let count = record.get(position..position + 2)?;
        let count = u16::from_be_bytes([count[0], count[1]]);
        position += 2;
        for _ in 0..count {
            let length = record.get(position..position + 2)?;
            let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
            position += 2;
            let nal = record.get(position..position + length)?;
            position += length;
            match nal_type {
                VPS_NUT => vps.push(Bytes::copy_from_slice(nal)),
                SPS_NUT => sps.push(Bytes::copy_from_slice(nal)),
                PPS_NUT => pps.push(Bytes::copy_from_slice(nal)),
                _ => {}
            }
        }
    }

    Some(HevcDecoderConfig {
        nal_length_size,
        vps,
        sps,
        pps,
    })
}

fn build_hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Bytes {
    let mut output = Vec::with_capacity(38 + vps.len() + sps.len() + pps.len());
    output.push(1);
    if sps.len() >= 15 {
        output.push(sps[3]);
        output.extend_from_slice(&sps[4..8]);
        output.extend_from_slice(&sps[8..14]);
        output.push(sps[14]);
    } else {
        output.extend_from_slice(&[0; 12]);
    }
    output.extend_from_slice(&[0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0b]);
    output.push(3);
    for (nal_type, nal) in [(VPS_NUT, vps), (SPS_NUT, sps), (PPS_NUT, pps)] {
        output.push(0x80 | nal_type);
        output.extend_from_slice(&1u16.to_be_bytes());
        output.extend_from_slice(&(u16::try_from(nal.len()).unwrap_or(u16::MAX)).to_be_bytes());
        output.extend_from_slice(nal);
    }
    Bytes::from(output)
}

fn find_start_code(data: &[u8]) -> Option<(usize, usize)> {
    let mut position = 0usize;
    while position + 3 <= data.len() {
        if data[position..].starts_with(&[0, 0, 0, 1]) {
            return Some((position, 4));
        }
        if data[position..].starts_with(&[0, 0, 1]) {
            return Some((position, 3));
        }
        position += 1;
    }
    None
}

struct NalIter<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> NalIter<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
}

impl<'a> Iterator for NalIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.position >= self.data.len() {
                return None;
            }
            let (start_code, start_code_length) = find_start_code(&self.data[self.position..])?;
            let start = self.position + start_code + start_code_length;
            let end = find_start_code(&self.data[start..])
                .map_or(self.data.len(), |(offset, _)| start + offset);
            self.position = end;
            if start < end {
                return Some(&self.data[start..end]);
            }
        }
    }
}
