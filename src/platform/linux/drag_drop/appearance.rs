//! Drag-image metadata and the local identity of a drag. XDND has no shared
//! drag image the way the Windows shell does, so none is drawn or produced,
//! and the identity is only needed for the drag return, which Linux does not
//! offer (`super::RETURN_CAPABLE`).
pub(crate) use crate::protocol::drag_drop::DragImage;
use anyhow::{Result, bail};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub token: u64,
    pub drag: u64,
}

pub fn identify(_data: &super::Files, _identity: Identity) -> Result<()> {
    bail!("Linux 版暂不支持拖回原拖动")
}

pub fn decorate(_data: &super::Files, _image: &DragImage) -> Result<()> {
    bail!("X11 拖放没有拖动预览图")
}

pub fn selection(_paths: &[PathBuf]) -> Result<DragImage> {
    bail!("X11 拖放没有拖动预览图")
}
