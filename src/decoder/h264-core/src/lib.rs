// SPDX-License-Identifier: LGPL-2.1-or-later
//! Platform-independent, byte-plane H.264 decoding implementation.
//! The Windows client uses this core for all software video decoding.
#![deny(unsafe_code)]

pub mod bits;
pub mod cabac;
mod cavlc;
mod dpb;
pub mod dsp;
mod entropy;
mod fault;
mod headers;
pub use fault::Fault;
mod order;
pub mod picture;
#[cfg(feature = "profile")]
pub mod profile;
pub mod reconstruct;
mod residual;
mod scan;
pub mod stream;
mod tables;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Invalid(Fault),
    Unsupported(Fault),
    Allocation,
    NeedKeyframe,
    Cancelled,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(fault) => write!(f, "invalid input: {}", fault.message()),
            Self::Unsupported(fault) => write!(f, "unsupported: {}", fault.message()),
            _ => write!(f, "{self:?}"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
