//! Owned SpeexDSP resampler. Packet timing and concealment belong to NetEq.
use std::ffi::{c_int, c_void};
use std::ptr::NonNull;

use anyhow::{Result, bail, ensure};

unsafe extern "C" {
    fn speex_resampler_init(
        channels: u32,
        input: u32,
        output: u32,
        quality: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn speex_resampler_destroy(state: *mut c_void);
    fn speex_resampler_process_interleaved_float(
        state: *mut c_void,
        input: *const f32,
        input_len: *mut u32,
        output: *mut f32,
        output_len: *mut u32,
    ) -> c_int;
}

pub(super) struct Resampler(NonNull<c_void>);
unsafe impl Send for Resampler {}

impl Resampler {
    pub fn new(output_rate: u32) -> Result<Self> {
        let mut error = 0;
        let state = unsafe { speex_resampler_init(2, 48_000, output_rate, 5, &mut error) };
        let Some(state) = NonNull::new(state) else {
            bail!("音频重采样初始化失败：{error}");
        };
        let owned = Self(state);
        ensure!(error == 0, "音频重采样初始化失败：{error}");
        Ok(owned)
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(usize, usize)> {
        let mut input_len = (input.len() / 2) as u32;
        let mut output_len = (output.len() / 2) as u32;
        let error = unsafe {
            speex_resampler_process_interleaved_float(
                self.0.as_ptr(),
                input.as_ptr(),
                &mut input_len,
                output.as_mut_ptr(),
                &mut output_len,
            )
        };
        ensure!(error == 0, "音频重采样失败：{error}");
        Ok((input_len as usize * 2, output_len as usize * 2))
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        unsafe { speex_resampler_destroy(self.0.as_ptr()) };
    }
}
