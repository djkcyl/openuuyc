use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Result, ensure};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Statistics {
    pub output_samples: u64,
    pub concealed_samples: u64,
    pub inserted_samples: u64,
    pub removed_samples: u64,
    pub discarded_packets: u64,
    pub buffer_ms: u32,
    pub target_ms: u32,
}

unsafe extern "C" {
    fn ou_neteq_create() -> *mut c_void;
    fn ou_neteq_destroy(receiver: *mut c_void);
    fn ou_neteq_insert(
        receiver: *mut c_void,
        payload: *const u8,
        length: usize,
        timestamp: u32,
        sequence: u16,
    ) -> i32;
    fn ou_neteq_audio(
        receiver: *mut c_void,
        output: *mut f32,
        length: usize,
        stats: *mut Statistics,
    ) -> i32;
}

pub(super) struct Receiver(NonNull<c_void>);
// One owner on the audio callback thread; no pointers or sample slices escape.
unsafe impl Send for Receiver {}

impl Receiver {
    pub fn new() -> Result<Self> {
        NonNull::new(unsafe { ou_neteq_create() })
            .map(Self)
            .ok_or_else(|| anyhow::anyhow!("NetEq初始化失败"))
    }

    pub fn insert(&mut self, data: &[u8], timestamp: u32, sequence: u16) -> bool {
        unsafe {
            ou_neteq_insert(
                self.0.as_ptr(),
                data.as_ptr(),
                data.len(),
                timestamp,
                sequence,
            ) == 0
        }
    }

    pub fn block(
        &mut self,
        output: &mut [f32; super::BLOCK * 2],
        stats: &mut Statistics,
    ) -> Result<()> {
        let result =
            unsafe { ou_neteq_audio(self.0.as_ptr(), output.as_mut_ptr(), output.len(), stats) };
        ensure!(result >= 0, "NetEq输出失败：{result}");
        if result == 1 {
            tracing::debug!("NetEq returned a silent block after a recoverable decode error");
        }
        Ok(())
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        unsafe { ou_neteq_destroy(self.0.as_ptr()) };
    }
}
