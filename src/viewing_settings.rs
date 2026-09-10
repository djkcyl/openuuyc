//! Per-account, per-publisher viewing choices; never remote physical modes.
use anyhow::{Context, Result, bail};
use keyring::{Entry, Error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::stream_control::{MAX_CUSTOM_BITRATE_MBPS, StreamControlHandle, StreamControlSettings};

#[derive(Clone)]
pub(crate) struct ViewingSettingsStore(Arc<Entry>);

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u8,
    settings: StreamControlSettings,
}

impl ViewingSettingsStore {
    pub(crate) fn new(user_id: &str, publisher_id: &str) -> Result<Self> {
        crate::api::validate_device_id(publisher_id)?;
        if user_id.is_empty() {
            bail!("无法确定画面设置所属账号");
        }
        let service = format!(
            "com.openuuyc.viewing.{:x}",
            Sha256::digest(user_id.as_bytes())
        );
        Ok(Self(Arc::new(
            Entry::new(&service, publisher_id)
                .map_err(|_| anyhow::anyhow!("画面设置存储不可用"))?,
        )))
    }

    pub(crate) async fn load(&self) -> Result<Option<StreamControlSettings>> {
        let entry = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(None),
                Err(_) => bail!("无法读取此设备的画面设置"),
            };
            let record: Record = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("已保存的画面设置格式无效"))?;
            if record.schema != 1
                || !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&record.settings.custom_bitrate_mbps)
                || !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&record.settings.adaptive_ceiling_mbps)
            {
                bail!("已保存的画面设置无效");
            }
            Ok(Some(record.settings))
        })
        .await
        .context("画面设置读取任务中断")?
    }

    async fn save(&self, settings: StreamControlSettings) -> Result<()> {
        let entry = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            let bytes = serde_json::to_vec(&Record {
                schema: 1,
                settings,
            })?;
            entry
                .set_secret(&bytes)
                .map_err(|_| anyhow::anyhow!("无法保存此设备的画面设置"))
        })
        .await
        .context("画面设置保存任务中断")?
    }

    pub(crate) fn bind(self, handle: StreamControlHandle) -> PreferenceWriter {
        let mut updates = handle.preference_updates();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let changed = tokio::select! {
                    _ = cancelled.cancelled() => false,
                    changed = updates.changed() => changed.is_ok(),
                };
                if changed || updates.has_changed().unwrap_or(false) {
                    let settings = *updates.borrow_and_update();
                    if let Some(settings) = settings {
                        let error = self.save(settings).await.err().map(|e| e.to_string());
                        if error.is_none() {
                            tracing::info!(?settings, "saved viewing settings for this device");
                        }
                        handle.set_persistence_error(error);
                    }
                }
                if cancelled.is_cancelled() || !changed {
                    break;
                }
            }
        });
        PreferenceWriter {
            stop,
            task: Some(task),
        }
    }
}

pub(crate) struct PreferenceWriter {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl PreferenceWriter {
    pub(crate) async fn finish(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for PreferenceWriter {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
