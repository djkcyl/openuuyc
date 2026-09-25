//! Per-account, per-publisher viewing choices; never remote physical modes.
use anyhow::{Context, Result, bail};
use keyring::{Entry, Error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::features::stream_control::{
    LoadedStreamControl, MAX_CUSTOM_BITRATE_MBPS, SavedStreamControl, StreamControlHandle,
    StreamControlSettings, ViewingPreferenceUpdate,
};

#[derive(Clone)]
pub(crate) struct ViewingSettingsStore {
    viewing: Arc<Entry>,
    audio: Arc<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u8,
    settings: Option<StreamControlSettings>,
    #[serde(default = "crate::features::stream_control::default_auto_quality")]
    auto_frame_quality: i32,
}

#[derive(Serialize, Deserialize)]
struct AudioRecord {
    schema: u8,
    settings: crate::media::audio::AudioSettings,
}

impl ViewingSettingsStore {
    pub(crate) fn new(user_id: &str, publisher_id: &str) -> Result<Self> {
        crate::account::api::validate_device_id(publisher_id)?;
        if user_id.is_empty() {
            bail!("无法确定画面设置所属账号");
        }
        let service = format!(
            "com.openuuyc.viewing.{:x}",
            Sha256::digest(user_id.as_bytes())
        );
        let audio_service = format!(
            "com.openuuyc.audio.{:x}",
            Sha256::digest(user_id.as_bytes())
        );
        Ok(Self {
            viewing: Arc::new(
                Entry::new(&service, publisher_id)
                    .map_err(|_| anyhow::anyhow!("画面设置存储不可用"))?,
            ),
            audio: Arc::new(
                Entry::new(&audio_service, publisher_id)
                    .map_err(|_| anyhow::anyhow!("音量设置存储不可用"))?,
            ),
        })
    }

    pub(crate) async fn load(&self) -> Result<Option<LoadedStreamControl>> {
        let entry = Arc::clone(&self.viewing);
        tokio::task::spawn_blocking(move || {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(None),
                Err(_) => bail!("无法读取此设备的画面设置"),
            };
            let record: Record = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("已保存的画面设置格式无效"))?;
            if record.schema != 1
                || record.settings.is_some_and(|settings| {
                    !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&settings.custom_bitrate_mbps)
                })
                || !(1..=6).contains(&record.auto_frame_quality)
            {
                bail!("已保存的画面设置无效");
            }
            Ok(Some(LoadedStreamControl {
                settings: record.settings,
                auto_frame_quality: record.auto_frame_quality,
            }))
        })
        .await
        .context("画面设置读取任务中断")?
    }

    async fn save(&self, saved: SavedStreamControl) -> Result<()> {
        let entry = Arc::clone(&self.viewing);
        tokio::task::spawn_blocking(move || {
            let bytes = serde_json::to_vec(&Record {
                schema: 1,
                settings: Some(saved.settings),
                auto_frame_quality: saved.auto_frame_quality,
            })?;
            entry
                .set_secret(&bytes)
                .map_err(|_| anyhow::anyhow!("无法保存此设备的画面设置"))
        })
        .await
        .context("画面设置保存任务中断")?
    }

    pub(crate) async fn load_audio(&self) -> Result<Option<crate::media::audio::AudioSettings>> {
        let entry = Arc::clone(&self.audio);
        tokio::task::spawn_blocking(move || {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(None),
                Err(_) => bail!("无法读取此设备的音量设置"),
            };
            let record: AudioRecord = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("已保存的音量设置格式无效"))?;
            if record.schema != 1 || record.settings.volume > crate::media::audio::MAX_VOLUME {
                bail!("已保存的音量设置无效");
            }
            Ok(Some(record.settings))
        })
        .await
        .context("音量设置读取任务中断")?
    }

    async fn save_audio(&self, settings: crate::media::audio::AudioSettings) -> Result<()> {
        let entry = Arc::clone(&self.audio);
        tokio::task::spawn_blocking(move || {
            let bytes = serde_json::to_vec(&AudioRecord {
                schema: 1,
                settings,
            })?;
            entry
                .set_secret(&bytes)
                .map_err(|_| anyhow::anyhow!("无法保存此设备的音量设置"))
        })
        .await
        .context("音量设置保存任务中断")?
    }

    pub(crate) fn bind_audio(self, handle: StreamControlHandle) -> PreferenceWriter {
        // Subscribe after restoring startup settings, so --mute and defaults
        // do not silently overwrite saved user choices.
        let mut updates = handle.audio().preference_updates();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let changed = tokio::select! {
                    _ = cancelled.cancelled() => false,
                    changed = updates.changed() => changed.is_ok(),
                };
                if changed {
                    loop {
                        tokio::select! {
                            _ = cancelled.cancelled() => break,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => break,
                            update = updates.changed() => if update.is_err() { break; },
                        }
                    }
                }
                if changed || updates.has_changed().unwrap_or(false) {
                    let settings = *updates.borrow_and_update();
                    if let Some(settings) = settings {
                        let error = self.save_audio(settings).await.err().map(|e| e.to_string());
                        if error.is_none() {
                            tracing::debug!(?settings, "saved audio settings for this device");
                        }
                        handle.set_audio_persistence_error(error);
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
                        let result = match settings {
                            ViewingPreferenceUpdate::Settings(saved) => self.save(saved).await,
                            ViewingPreferenceUpdate::AutoQuality(quality) => {
                                self.save_auto_quality(quality).await
                            }
                        };
                        let error = result.err().map(|e| e.to_string());
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

impl ViewingSettingsStore {
    async fn save_auto_quality(&self, quality: i32) -> Result<()> {
        let entry = Arc::clone(&self.viewing);
        tokio::task::spawn_blocking(move || {
            let mut record = match entry.get_secret() {
                Ok(bytes) => {
                    serde_json::from_slice::<Record>(&bytes).context("读取画面设置失败")?
                }
                Err(Error::NoEntry) => Record {
                    schema: 1,
                    settings: None,
                    auto_frame_quality: quality,
                },
                Err(_) => bail!("无法读取此设备的画面设置"),
            };
            anyhow::ensure!(
                record.schema == 1 && (1..=6).contains(&quality),
                "自动画质状态无效"
            );
            record.auto_frame_quality = quality;
            entry
                .set_secret(&serde_json::to_vec(&record)?)
                .map_err(|_| anyhow::anyhow!("无法保存自动画质"))
        })
        .await
        .context("自动画质写入任务中断")?
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
