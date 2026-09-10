//! Server LoginManager's saved-user restoration, not a token refresh API.
use std::time::Duration;

use crate::{
    api::{ApiFailure, NrdApi},
    device_session::DeviceHandle,
};
use anyhow::Result;

#[derive(Clone, Copy, Debug)]
pub(crate) enum RestoreTrigger {
    DeviceStartup,
    ExplicitLogin,
}

pub(crate) async fn initialize(
    device: &DeviceHandle,
    trigger: RestoreTrigger,
) -> Result<crate::auth::NativeIdentity> {
    let mut long_retries = 0u32;
    loop {
        let explicit = matches!(trigger, RestoreTrigger::ExplicitLogin);
        match device.ensure(explicit || long_retries != 0).await {
            Ok(identity) => return Ok(identity),
            Err(error) => {
                let Some(failure) = error.downcast_ref::<ApiFailure>() else {
                    return Err(error);
                };
                let code = failure.code;
                // 3B4E60 schedules the first failed startup; 3B0790 only
                // schedules subsequent rounds for the -1 outcome.
                if long_retries != 0 && code != -1 {
                    return Err(error);
                }
                let delay = if explicit {
                    Duration::from_secs(if long_retries < 30 { 10 } else { 30 })
                } else {
                    device.startup_retry_delay().await?
                };
                long_retries = long_retries.saturating_add(1);
                tracing::warn!(?trigger, code, long_retries, seconds = delay.as_secs(), %error, "device initialization retry scheduled");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

pub(crate) async fn restore_user(
    api: &NrdApi,
    trigger: RestoreTrigger,
) -> Result<serde_json::Value, ApiFailure> {
    let retries = match trigger {
        RestoreTrigger::DeviceStartup => 10, // 3E1EA0
        RestoreTrigger::ExplicitLogin => 5,  // 3E2780
    };
    for attempt in 0..=retries {
        let response = api.get_user_info().await;
        let failure = match response {
            Ok(response) if response.code == 0 => {
                tracing::info!(?trigger, attempt = attempt + 1, "saved account restored");
                return Ok(response.data.unwrap_or(serde_json::Value::Null));
            }
            Ok(response) => ApiFailure {
                code: response.code,
                message: response.msg,
            },
            Err(error) => ApiFailure {
                code: -1,
                message: format!("{error:#}"),
            },
        };
        if failure.invalid_saved_credentials() || attempt == retries {
            return Err(failure);
        }
        tracing::warn!(
            ?trigger,
            attempt = attempt + 1,
            code = failure.code,
            "saved account restoration failed; retrying in 1000 ms"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    unreachable!("all restoration attempts return success or the final error")
}
