// SPDX-License-Identifier: LGPL-2.1-or-later
use crate::{Error, Result};

/// A bounded MSB reader. Wide loads never require external padding and EOF
/// leaves the cursor unchanged, including Exp-Golomb overflow/failure.
#[derive(Clone)]
pub struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}
impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
    pub fn at(data: &'a [u8], position: usize) -> Result<Self> {
        if position > data.len().saturating_mul(8) {
            return Err(Error::Truncated);
        }
        Ok(Self { data, position })
    }
    pub fn position(&self) -> usize {
        self.position
    }
    pub fn remaining(&self) -> usize {
        self.data.len() * 8 - self.position
    }
    #[inline]
    pub fn peek_padded(&self, n: u32) -> u32 {
        let n = n.min(32);
        let take = self.remaining().min(n as usize) as u32;
        if take == 0 {
            return 0;
        }
        let mut copy = self.clone();
        copy.read(take).unwrap_or(0) << (n - take)
    }
    #[inline]
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.remaining() < n {
            return Err(Error::Truncated);
        }
        self.position += n;
        Ok(())
    }
    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::Invalid(crate::Fault::BitWidth));
        }
        if n == 0 {
            return Ok(0);
        }
        if self.remaining() < n as usize {
            return Err(Error::Truncated);
        }
        let at = self.position / 8;
        let skip = self.position % 8;
        let value = if let Some(bytes) = self.data.get(at..at + 8) {
            let cache = u64::from_be_bytes(bytes.try_into().unwrap());
            ((cache << skip) >> (64 - n)) as u32
        } else {
            let count = (skip + n as usize).div_ceil(8);
            let mut cache = 0u64;
            for &b in &self.data[at..at + count] {
                cache = (cache << 8) | b as u64;
            }
            ((cache >> (count * 8 - skip - n as usize)) & ((1u64 << n) - 1)) as u32
        };
        self.position += n as usize;
        Ok(value)
    }
    pub fn ue(&mut self) -> Result<u32> {
        // FFmpeg-style wide prefix lookup. No one-bit cursor update for each
        // leading zero; validate the complete code before changing the cursor.
        let zeros = self.peek_padded(32).leading_zeros();
        if zeros == 32 {
            return Err(if self.remaining() < 32 {
                Error::Truncated
            } else {
                Error::Invalid(crate::Fault::ExpGolombOverflow)
            });
        }
        if self.remaining() < (2 * zeros + 1) as usize {
            return Err(Error::Truncated);
        }
        self.position += zeros as usize + 1;
        Ok(((1u64 << zeros) - 1 + self.read(zeros)? as u64) as u32)
    }
    pub fn se(&mut self) -> Result<i32> {
        let before = self.position;
        let n = self.ue()? as i64;
        let signed = if n & 1 != 0 { (n + 1) / 2 } else { -n / 2 };
        i32::try_from(signed).map_err(|_| {
            self.position = before;
            Error::Invalid(crate::Fault::SignedExpGolombOverflow)
        })
    }
    pub fn align_ones(&mut self) -> Result<()> {
        while self.position & 7 != 0 {
            if self.read(1)? != 1 {
                return Err(Error::Invalid(crate::Fault::CabacAlignment));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wide_golomb_handles_unaligned_codes_and_keeps_failed_cursor() {
        for offset in 0..8 {
            for value in [0u32, 1, 2, 3, 7, 16, 255, 65535, 0x7fffffff, 0xfffffffe] {
                let code = value as u64 + 1;
                let count = 64 - code.leading_zeros() as usize;
                let mut bits = vec![true; offset];
                bits.extend(std::iter::repeat_n(false, count - 1));
                bits.extend((0..count).rev().map(|i| (code >> i) & 1 != 0));
                let mut data = vec![0; bits.len().div_ceil(8)];
                for (index, &b) in bits.iter().enumerate() {
                    data[index / 8] |= u8::from(b) << (7 - index % 8);
                }
                let mut reader = Bits::at(&data, offset).unwrap();
                assert_eq!(reader.ue().unwrap(), value);
                assert_eq!(reader.position(), bits.len());
                for bytes in 0..data.len() {
                    if bytes * 8 < offset {
                        continue;
                    }
                    let mut short = Bits::at(&data[..bytes], offset).unwrap();
                    assert!(short.ue().is_err());
                    assert_eq!(short.position(), offset);
                }
            }
        }
        let mut empty = Bits::new(&[]);
        assert_eq!(empty.peek_padded(32), 0);
        assert_eq!(empty.ue(), Err(Error::Truncated));
        let mut overflow = Bits::new(&[0, 0, 0, 0, 1]);
        assert_eq!(
            overflow.ue(),
            Err(Error::Invalid(crate::Fault::ExpGolombOverflow))
        );
        assert_eq!(overflow.position(), 0);
    }
}
