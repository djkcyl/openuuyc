//! Account device deltas from the authenticated presence connection.
//! G E26D20/E30DC0/BDD590: these pushes carry device data, not just an ID.
use crate::account::api::{DeviceGroups, DeviceInfo, DeviceList};
use anyhow::{Result, bail};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Bound,
    Changed,
    Removed,
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
struct Fields {
    alias: Option<String>,
    status: Option<String>,
    platform: Option<i32>,
    controllable: Option<bool>,
    controlled_support: Option<bool>,
    support_wol: Option<bool>,
    version_name: Option<String>,
    wallpaper_url: Option<String>,
    update_started_at: Option<i64>,
    participants_info: Option<Vec<serde_json::Value>>,
}

#[derive(Clone)]
pub(crate) struct DeviceChange {
    pub id: String,
    pub kind: ChangeKind,
    fields: Fields,
}

impl DeviceChange {
    pub fn parse(push: &serde_json::Value) -> Result<Option<Self>> {
        let kind = match push.get("type").and_then(|v| v.as_str()) {
            Some("device_binded") => ChangeKind::Bound,
            Some("device_info_changed") => ChangeKind::Changed,
            Some("device_unbind") => ChangeKind::Removed,
            _ => return Ok(None),
        };
        let data = push
            .get("data")
            .filter(|v| v.is_object())
            .ok_or_else(|| anyhow::anyhow!("device push data is not an object"))?;
        let id = data
            .get("device_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("device push has no ID"))?;
        crate::account::api::validate_device_id(id)?;
        // Unbind only requires the target ID; unrelated optional data cannot
        // prevent retirement of this account's current virtual identity.
        let fields = if kind == ChangeKind::Removed {
            Fields::default()
        } else {
            serde_json::from_value::<Fields>(data.clone())?
        };
        if fields
            .participants_info
            .as_ref()
            .is_some_and(|v| v.iter().any(|p| !p.is_object()))
        {
            bail!("device push participant is not an object");
        }
        Ok(Some(Self {
            id: id.into(),
            kind,
            fields,
        }))
    }

    pub fn renamed(id: String, alias: String) -> Self {
        Self {
            id,
            kind: ChangeKind::Changed,
            fields: Fields {
                alias: Some(alias),
                ..Default::default()
            },
        }
    }

    pub fn removed(id: String) -> Self {
        Self {
            id,
            kind: ChangeKind::Removed,
            fields: Fields::default(),
        }
    }

    pub fn is_cloud(&self) -> bool {
        self.fields.platform == Some(51)
    }

    pub fn bound_device(&self) -> Option<DeviceInfo> {
        if self.kind != ChangeKind::Bound || !matches!(self.fields.platform?, 1..=5) {
            return None;
        }
        Some(DeviceInfo {
            device_id: self.id.clone(),
            alias: self.fields.alias.clone()?,
            status: self.fields.status.clone()?,
            platform: self.fields.platform?,
            controllable: self.fields.controllable?,
            controlled_support: self.fields.controlled_support?,
            support_wol: self.fields.support_wol?,
            version_name: self.fields.version_name.clone()?,
            wallpaper_url: self.fields.wallpaper_url.clone().unwrap_or_default(),
            update_started_at: self.fields.update_started_at.unwrap_or_default(),
            participants_info: self.fields.participants_info.clone().unwrap_or_default(),
        })
    }

    pub fn merge(&mut self, newer: &Self) {
        if self.kind == ChangeKind::Removed && newer.kind == ChangeKind::Changed {
            return;
        }
        if newer.kind != ChangeKind::Changed || self.kind == ChangeKind::Removed {
            *self = newer.clone();
            return;
        }
        macro_rules! merge { ($($field:ident),*) => { $(if newer.fields.$field.is_some() { self.fields.$field = newer.fields.$field.clone(); })* }; }
        merge!(
            alias,
            status,
            platform,
            controllable,
            controlled_support,
            support_wol,
            version_name,
            wallpaper_url,
            update_started_at,
            participants_info
        );
    }

    fn apply_device(&self, device: &mut DeviceInfo) {
        if self.kind == ChangeKind::Bound {
            device.status.clear();
            device.controllable = false;
            device.controlled_support = false;
            device.support_wol = false;
        }
        // Missing fields preserve the last authoritative value. They must not
        // manufacture online state or enable a capability.
        macro_rules! apply { ($($field:ident),*) => { $(if let Some(value) = &self.fields.$field { device.$field = value.clone(); })* }; }
        apply!(
            alias,
            status,
            platform,
            controllable,
            controlled_support,
            support_wol,
            version_name,
            wallpaper_url,
            update_started_at,
            participants_info
        );
    }

    pub fn apply_list(&self, list: &mut DeviceList) {
        if self.kind == ChangeKind::Removed || self.is_cloud() {
            list.my_binded_devices.retain(|d| d.device_id != self.id);
        } else {
            let known = list.current_device.device_id == self.id
                || list
                    .my_binded_devices
                    .iter()
                    .any(|d| d.device_id == self.id);
            for device in std::iter::once(&mut list.current_device)
                .chain(list.my_binded_devices.iter_mut())
                .filter(|d| d.device_id == self.id)
            {
                self.apply_device(device);
            }
            if !known
                && let Some(device) = self.bound_device()
                && matches!(device.platform, 1 | 4)
            {
                list.my_binded_devices.push(device);
            }
        }
        // Membership of an unknown device is resolved from the canonical
        // list/groups once, rather than inferred from controllability alone.
    }

    pub fn apply_groups(&self, groups: &mut DeviceGroups) {
        let known = groups.entries().any(|(_, d)| d.device_id == self.id);
        let mut found = None;
        for devices in [
            &mut groups.desktop_devices,
            &mut groups.mobile_devices,
            &mut groups.tv_devices,
        ] {
            if let Some(index) = devices.iter().position(|d| d.device_id == self.id) {
                if self.kind == ChangeKind::Removed || self.is_cloud() {
                    devices.remove(index);
                } else {
                    self.apply_device(&mut devices[index]);
                    let platform = devices[index].platform;
                    if self.fields.platform.is_some() && matches!(platform, 1..=5) {
                        found = Some(devices.remove(index));
                    }
                }
            }
        }
        if !known && found.is_none() {
            found = self.bound_device();
        }
        if let Some(device) = found {
            match device.platform {
                1 | 4 => groups.desktop_devices.push(device),
                2 | 3 => groups.mobile_devices.push(device),
                5 => groups.tv_devices.push(device),
                _ => unreachable!(),
            }
        }
    }
}
