//! Own virtual-device wallpaper. One bounded job per identity/account generation.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::task::JoinHandle;

const IMAGE: &[u8] = include_bytes!("../../../assets/virtual-device-wallpaper.jpg");

#[derive(Default)]
pub(super) struct Sync {
    identity: Option<String>,
    task: Option<JoinHandle<()>>,
}

impl Drop for Sync {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    schema: u8,
    image_sha256: String,
    url_sha256: String,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl AuthenticatedClient {
    pub(super) fn schedule_wallpaper(&self, list: &DeviceList) {
        let device = &self.device;
        let Ok(identity) = device.identity().client_identity() else {
            return;
        };
        if identity.device_id.is_empty()
            || list.current_device.device_id != identity.device_id
            || !self.is_active()
        {
            return;
        }
        let key = digest(
            serde_json::to_string(&[
                self.session.user_id(),
                &identity.client_id,
                &identity.system_id,
                &identity.device_id,
            ])
            .expect("identity strings serialize")
            .as_bytes(),
        );
        let mut sync = self.wallpaper.lock().unwrap_or_else(|e| e.into_inner());
        if sync.identity.as_ref() == Some(&key) {
            return;
        }
        let Some(api) = self.api.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        if let Some(task) = sync.task.take() {
            task.abort();
        }
        sync.identity = Some(key.clone());
        let device = device.clone();
        let session = self.session.clone();
        let ended = self.ended.clone();
        let current_url = list.current_device.wallpaper_url.clone();
        sync.task = Some(tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = ended.cancelled() => {},
                result = synchronize(api, device, identity, session, key, current_url) => {
                    match result {
                        Ok(true) => tracing::info!("virtual device wallpaper uploaded and verified"),
                        Ok(false) => tracing::info!("virtual device wallpaper already current; upload skipped"),
                        Err(error) => tracing::warn!(%error, "virtual device wallpaper not confirmed; no automatic replay in this account generation"),
                    }
                }
            }
        }));
    }
}

async fn verify_owner(
    device: &DeviceHandle,
    expected: &crate::account::api::ClientIdentity,
    session: &LoginSession,
) -> Result<()> {
    let live = device.identity().client_identity()?;
    if live.device_id != expected.device_id
        || live.client_id != expected.client_id
        || live.system_id != expected.system_id
    {
        bail!("virtual wallpaper identity changed");
    }
    let expected = expected.clone();
    let session = session.clone();
    tokio::task::spawn_blocking(move || {
        let saved = KeyringIdentityStore::new()?
            .load_existing()?
            .context("virtual wallpaper identity no longer exists")?
            .client_identity()?;
        let account = KeyringSessionStore::new()?
            .load()?
            .context("wallpaper account ended")?;
        if saved.device_id != expected.device_id
            || saved.client_id != expected.client_id
            || saved.system_id != expected.system_id
            || account.user_id() != session.user_id()
            || account.token() != session.token()
        {
            bail!("virtual wallpaper owner changed");
        }
        Ok(())
    })
    .await
    .context("wallpaper identity check interrupted")?
}

async fn synchronize(
    api: NrdApi,
    device: DeviceHandle,
    identity: crate::account::api::ClientIdentity,
    session: LoginSession,
    key: String,
    current_url: String,
) -> Result<bool> {
    verify_owner(&device, &identity, &session).await?;
    let image_sha256 = digest(IMAGE);
    let entry = Arc::new(
        keyring::Entry::new("com.openuuyc.wallpaper", &key)
            .map_err(|_| anyhow::anyhow!("wallpaper receipt store unavailable"))?,
    );
    let reader = Arc::clone(&entry);
    let receipt = tokio::task::spawn_blocking(move || -> Result<Option<Receipt>> {
        match reader.get_secret() {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => bail!("wallpaper receipt could not be read"),
        }
    })
    .await
    .context("wallpaper receipt read interrupted")??;
    if !current_url.is_empty()
        && receipt.as_ref().is_some_and(|r| {
            r.schema == 1
                && r.image_sha256 == image_sha256
                && r.url_sha256 == digest(current_url.as_bytes())
        })
    {
        return Ok(false);
    }

    let detail = api
        .device_detail(&identity.device_id)
        .await
        .map_err(|_| anyhow::anyhow!("virtual wallpaper device check failed"))?
        .into_data()
        .map_err(|_| anyhow::anyhow!("virtual wallpaper device check rejected"))?;
    if !crate::account::virtual_hardware::matches(
        detail.details.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    ) {
        bail!("current device does not report OpenUUYC virtual hardware; wallpaper unchanged");
    }
    let grant = api
        .wallpaper_grant()
        .await
        .map_err(|_| anyhow::anyhow!("wallpaper upload grant request failed"))?;
    verify_owner(&device, &identity, &session).await?;
    let uploaded = crate::account::api::wallpaper::upload_wallpaper(grant, IMAGE).await?;
    verify_owner(&device, &identity, &session).await?;

    // This records the desired uploaded image, not a claim that binding succeeded.
    // On restart it counts as success only if the server returns the same URL.
    let record = serde_json::to_vec(&Receipt {
        schema: 1,
        image_sha256,
        url_sha256: digest(uploaded.url.as_bytes()),
    })?;
    tokio::task::spawn_blocking(move || {
        entry
            .set_secret(&record)
            .map_err(|_| anyhow::anyhow!("wallpaper upload receipt could not be saved"))
    })
    .await
    .context("wallpaper receipt write interrupted")??;
    verify_owner(&device, &identity, &session).await?;
    let binding = api.bind_wallpaper(&uploaded.url).await;
    verify_owner(&device, &identity, &session).await?;
    let list = api
        .list_devices()
        .await
        .map_err(|_| anyhow::anyhow!("wallpaper readback failed; binding not replayed"))?
        .into_data()
        .map_err(|_| anyhow::anyhow!("wallpaper readback rejected; binding not replayed"))?;
    if list.current_device.device_id == identity.device_id
        && list.current_device.wallpaper_url == uploaded.url
    {
        return Ok(true);
    }
    if binding.is_err() {
        bail!("wallpaper binding failed or unconfirmed; binding not replayed");
    }
    bail!("wallpaper binding accepted but readback differs; binding not replayed")
}
