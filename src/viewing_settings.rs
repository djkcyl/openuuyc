//! Per-account, per-publisher viewing choices; never remote physical modes.
use anyhow::{Context, Result, bail};
use keyring::{Entry, Error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::stream_control::{MAX_CUSTOM_BITRATE_MBPS, StreamControlHandle, StreamControlSettings};

#[derive(Clone)]
pub(crate) struct ViewingSettingsStore {
    viewing: Arc<Entry>,
    audio: Arc<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u8,
    settings: StreamControlSettings,
}

#[derive(Serialize, Deserialize)]
struct AudioRecord {
    schema: u8,
    settings: crate::audio::AudioSettings,
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

    pub(crate) async fn load(&self) -> Result<Option<StreamControlSettings>> {
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
        let entry = Arc::clone(&self.viewing);
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

    pub(crate) async fn load_audio(&self) -> Result<Option<crate::audio::AudioSettings>> {
        let entry = Arc::clone(&self.audio);
        tokio::task::spawn_blocking(move || {
            let bytes = match entry.get_secret() {
                Ok(bytes) => bytes,
                Err(Error::NoEntry) => return Ok(None),
                Err(_) => bail!("无法读取此设备的音量设置"),
            };
            let record: AudioRecord = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("已保存的音量设置格式无效"))?;
            if record.schema != 1 || record.settings.volume > 100 {
                bail!("已保存的音量设置无效");
            }
            Ok(Some(record.settings))
        })
        .await
        .context("音量设置读取任务中断")?
    }

    async fn save_audio(&self, settings: crate::audio::AudioSettings) -> Result<()> {
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

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    struct TestEntries(Vec<Arc<Entry>>);
    impl Drop for TestEntries {
        fn drop(&mut self) {
            for entry in &self.0 {
                let _ = entry.delete_credential();
            }
        }
    }

    #[tokio::test]
    async fn windows_audio_credentials_isolate_and_flush_on_close() {
        let account = format!(
            "audio-native-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let store = ViewingSettingsStore::new(&account, "aaaaaaaaaaaaaaaa").unwrap();
        let _cleanup = TestEntries(vec![store.viewing.clone(), store.audio.clone()]);
        let second_device = ViewingSettingsStore::new(&account, "bbbbbbbbbbbbbbbb").unwrap();
        let second_account =
            ViewingSettingsStore::new(&format!("{account}-other"), "aaaaaaaaaaaaaaaa").unwrap();
        assert!(store.load_audio().await.unwrap().is_none());
        let saved = crate::audio::AudioSettings {
            volume: 37,
            muted: false,
        };
        store.save_audio(saved).await.unwrap();
        assert!(second_device.load_audio().await.unwrap().is_none());
        assert!(second_account.load_audio().await.unwrap().is_none());
        let profile = crate::media::ConnectionMediaOptions::default()
            .resolve(crate::media::LocalDisplayInfo {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .unwrap();
        let (handle, _outgoing, _echo) = StreamControlHandle::new(
            profile,
            crate::performance::PerformanceMonitor::new("native credential check"),
        );
        let audio = handle.audio();
        // Startup restore and a temporary --mute must not persist themselves.
        audio.set_settings(crate::audio::AudioSettings {
            muted: true,
            ..saved
        });
        let mut writer = store.clone().bind_audio(handle.clone());
        writer.finish().await;
        let restored = store.load_audio().await.unwrap().unwrap();
        assert_eq!(restored.volume, 37);
        assert!(!restored.muted);
        // Close during a drag's debounce window: flush the last real edit.
        let mut writer = store.clone().bind_audio(handle.clone());
        audio.set_settings(crate::audio::AudioSettings {
            volume: 23,
            muted: true,
        });
        audio.set_settings(crate::audio::AudioSettings {
            volume: 41,
            muted: false,
        });
        writer.finish().await;
        let reloaded_store = ViewingSettingsStore::new(&account, "aaaaaaaaaaaaaaaa").unwrap();
        let restored = reloaded_store.load_audio().await.unwrap().unwrap();
        assert_eq!(restored.volume, 41);
        assert!(!restored.muted);
        // Independent video writes cannot replace the audio record.
        let video = handle.snapshot().settings;
        store.save(video).await.unwrap();
        let restored = reloaded_store.load_audio().await.unwrap().unwrap();
        assert_eq!(restored.volume, 41);
        assert!(!restored.muted);
    }
}
