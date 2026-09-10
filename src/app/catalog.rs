//! Account-device metadata. No room creation or remote control is involved.
use crate::{
    api::{DeviceDetail, DeviceGroups},
    client::AuthenticatedClient,
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct CachedDetail {
    pub value: std::result::Result<DeviceDetail, String>,
    alias: String,
    version: String,
    loaded_at: Instant,
}

#[derive(Clone)]
pub(super) struct Catalog {
    pub groups: DeviceGroups,
    pub details: HashMap<String, CachedDetail>,
    pub suggested_name: String,
}

impl Catalog {
    pub async fn load(
        client: Arc<AuthenticatedClient>,
        previous: Option<Self>,
        force: bool,
        progress: impl Fn(&Self),
    ) -> anyhow::Result<Self> {
        let groups = client.device_groups().await?;
        crate::api::validate_device_id(&groups.current_device_id)?;
        let mut details = previous.map(|c| c.details).unwrap_or_default();
        details.retain(|id, _| groups.entries().any(|(_, d)| &d.device_id == id));
        let mut suggested_name = client.suggested_device_name();
        if groups
            .entries()
            .any(|(_, d)| d.device_id != groups.current_device_id && d.alias == suggested_name)
        {
            suggested_name.push('-');
            suggested_name.push_str(&groups.current_device_id[12..]);
        }
        progress(&Self {
            groups: groups.clone(),
            details: details.clone(),
            suggested_name: suggested_name.clone(),
        });
        for (_, device) in groups.entries() {
            let fresh = details.get(&device.device_id).is_some_and(|d| {
                !force
                    && d.alias == device.alias
                    && d.version == device.version_name
                    && d.loaded_at.elapsed()
                        < if d.value.is_ok() {
                            Duration::from_secs(300)
                        } else {
                            Duration::from_secs(30)
                        }
            });
            if fresh {
                continue;
            }
            let value = client
                .device_detail(device.validated_device_id()?)
                .await
                .map_err(|e| format!("{e:#}"));
            if !client.is_active() {
                anyhow::bail!("account session has ended");
            }
            details.insert(
                device.device_id.clone(),
                CachedDetail {
                    value,
                    alias: device.alias.clone(),
                    version: device.version_name.clone(),
                    loaded_at: Instant::now(),
                },
            );
            progress(&Self {
                groups: groups.clone(),
                details: details.clone(),
                suggested_name: suggested_name.clone(),
            });
        }
        Ok(Self {
            groups,
            details,
            suggested_name,
        })
    }

    pub fn virtual_status(&self, id: &str) -> Option<bool> {
        let detail = self.details.get(id)?.value.as_ref().ok()?;
        (!detail.details.is_empty()).then(|| {
            crate::virtual_hardware::matches(
                detail
                    .details
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
        })
    }

    pub fn is_virtual(&self, id: &str) -> bool {
        self.virtual_status(id) == Some(true)
    }
}
