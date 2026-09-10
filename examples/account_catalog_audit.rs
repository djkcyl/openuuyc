//! Read-only fresh-service contract check. Never prints credentials or account values.
use anyhow::Result;
use openuuyc::client::AuthenticatedClient;

#[tokio::main]
async fn main() -> Result<()> {
    let client = AuthenticatedClient::from_saved_session()?;
    let result = async {
        let info = client.account_info().await?;
        if let Some(fields) = info.as_object() {
            for (key, value) in fields {
                println!(
                    "account field: {key}, kind={}",
                    match value {
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Object(_) => "object",
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::Null => "null",
                        _ => "scalar",
                    }
                );
            }
        }
        let groups = client.device_groups().await?;
        let persisted = openuuyc::auth::KeyringIdentityStore::new()?.load_or_create()?;
        let current = groups
            .entries()
            .find(|(_, d)| d.device_id == groups.current_device_id)
            .map(|(_, d)| d);
        println!(
            "current identity unchanged={}, registered name matches alias={}",
            persisted.client_identity()?.device_id == groups.current_device_id,
            current.is_some_and(|d| persisted
                .device_init_request()
                .is_ok_and(|r| r.name == d.alias))
        );
        if let Some(device) = current {
            println!("current viewer alias={}", device.alias);
        }
        println!(
            "desktop={} mobile={} tv={}",
            groups.desktop_devices.len(),
            groups.mobile_devices.len(),
            groups.tv_devices.len()
        );
        if std::env::args().any(|a| a == "--summary") {
            return Ok(());
        }
        for (index, (group, device)) in groups.entries().enumerate() {
            let detail = client.device_detail(device.validated_device_id()?).await?;
            println!(
                "device#{index} group={group} platform={} self={} details={}",
                device.platform,
                device.device_id == groups.current_device_id,
                detail.details.len()
            );
            for (key, value) in detail.details {
                // Names and network/account identities are deliberately not printed.
                let safe = [
                    "处理器",
                    "CPU",
                    "显卡",
                    "内存",
                    "主板",
                    "操作系统",
                    "系统版本",
                    "设备型号",
                ]
                .iter()
                .any(|s| key.contains(s));
                println!(
                    "  {key}: {}",
                    if safe { value.as_str() } else { "<present>" }
                );
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    client.close().await;
    result
}
