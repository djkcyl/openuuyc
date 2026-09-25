//! Persistent access policy, isolated by account and this registered device.
use anyhow::{Result, bail, ensure};
use keyring::{Entry, Error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Store(Arc<Entry>);
#[derive(Serialize, Deserialize)]
struct Record {
    schema: u8,
    allow_control: bool,
    #[serde(default)]
    encoding: super::EncodingSettings,
}

impl Store {
    pub fn new(account: &str, device: &str) -> Result<Self> {
        ensure!(!account.is_empty(), "无法确定被控设置所属账号");
        crate::account::api::validate_device_id(device)?;
        let service = format!("com.openuuyc.host.{:x}", Sha256::digest(account.as_bytes()));
        Ok(Self(Arc::new(
            Entry::new(&service, device).map_err(|_| anyhow::anyhow!("被控设置存储不可用"))?,
        )))
    }
    pub fn load(&self) -> Result<(bool, super::EncodingSettings)> {
        let bytes = match self.0.get_secret() {
            Ok(bytes) => bytes,
            Err(Error::NoEntry) => return Ok((false, Default::default())),
            Err(_) => bail!("无法读取被控设置"),
        };
        let record: Record =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("已保存的被控设置无效"))?;
        ensure!(record.schema == 1, "不支持的被控设置格式");
        record.encoding.validate()?;
        Ok((record.allow_control, record.encoding))
    }
    pub fn save(&self, allowed: bool, encoding: super::EncodingSettings) -> Result<()> {
        encoding.validate()?;
        self.0
            .set_secret(&serde_json::to_vec(&Record {
                schema: 1,
                allow_control: allowed,
                encoding,
            })?)
            .map_err(|_| anyhow::anyhow!("无法保存被控设置"))
    }
}
