//! Decode-return categories consumed atomically by the receive-stream owner.
//! UU: CF8240 / CF87B0 -> 4F6CDE -> 1E7170 / 1E82FE.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VideoDecodeResult {
    Decoded,
    RequestKeyframe,
    Error,
    InvalidInput,
    Uninitialized,
    Fallback,
}

impl VideoDecodeResult {
    pub(crate) const fn accepted(self) -> bool {
        matches!(self, Self::Decoded | Self::RequestKeyframe)
    }

    pub(crate) const fn counts_towards_fallback(self, keyframe: bool) -> bool {
        !matches!(self, Self::Decoded | Self::Fallback)
            && (keyframe || matches!(self, Self::Error | Self::Uninitialized))
    }

    pub(crate) fn from_error(error: &anyhow::Error) -> Self {
        use crate::decoder::platform::DecodeError;
        match error.downcast_ref::<DecodeError>() {
            Some(DecodeError::Unsupported | DecodeError::HardwareFailure) => Self::Fallback,
            Some(DecodeError::NeedKeyframe) => Self::RequestKeyframe,
            Some(DecodeError::Closed | DecodeError::NoBackend) => Self::Uninitialized,
            Some(DecodeError::InvalidInput) => Self::InvalidInput,
            _ => Self::Error,
        }
    }
}
