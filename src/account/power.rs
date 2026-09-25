//! Account-device power commands. See docs/official-device-power.md.
use crate::account::api::DeviceInfo;
use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowerAction {
    Wake,
    Shutdown,
    Reboot,
}

impl PowerAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wake => "开机",
            Self::Shutdown => "关机",
            Self::Reboot => "重启",
        }
    }

    pub fn check(
        self,
        device: &DeviceInfo,
        features: &crate::account::feature_ability::FeatureCatalog,
    ) -> Result<()> {
        device.validated_device_id()?;
        if !matches!(device.platform, 1 | 4) || !device.controlled_support {
            bail!("该设备不支持电脑电源操作");
        }
        if !device.controllable {
            bail!("该设备未开放远程控制");
        }
        match self {
            Self::Wake => {
                if device.status != "DISCONNECTED" {
                    bail!("只有离线设备可以发送开机请求");
                }
                if !device.support_wol {
                    bail!("该设备未提供远程开机能力");
                }
            }
            Self::Shutdown | Self::Reboot => {
                let feature = if self == Self::Shutdown {
                    crate::account::feature_ability::Feature::Shutdown
                } else {
                    crate::account::feature_ability::Feature::Reboot
                };
                if !features
                    .policy(device.platform, &device.version_name)
                    .supports(feature)
                {
                    bail!("官方当前能力配置不支持该设备的电源操作");
                }
                if !device.is_connected() {
                    bail!("只有在线设备可以关机或重启");
                }
            }
        }
        Ok(())
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub(crate) struct PowerReceipt {
    pub current_device_in_same_network: Option<bool>,
    pub assist_count: Option<u32>,
    pub router_wol: Option<bool>,
}

impl PowerReceipt {
    pub fn summary(&self, action: PowerAction) -> String {
        if action != PowerAction::Wake {
            return "请求已受理，等待设备状态变化".into();
        }
        let mut routes = Vec::new();
        if self.current_device_in_same_network == Some(true) {
            routes.push("当前设备同网".to_owned());
        }
        if let Some(count) = self.assist_count {
            routes.push(format!("{count} 台协助设备"));
        }
        if self.router_wol == Some(true) {
            routes.push("路由器唤醒".to_owned());
        }
        if routes.is_empty() {
            "开机请求已受理，等待上线".into()
        } else {
            format!("开机请求已受理，等待上线 · {}", routes.join(" · "))
        }
    }
}
