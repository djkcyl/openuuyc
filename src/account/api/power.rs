use super::*;
use crate::account::power::{PowerAction, PowerReceipt};

const WAKE: Contract = Contract {
    method: Method::Post,
    path: "/api/v1/device/{id}/wake_on_lan/req",
    json_content_type: true,
    ..DEVICE_LIST
};
const REBOOT: Contract = Contract {
    path: "/api/v1/device/{id}/reboot",
    ..WAKE
};
const SHUTDOWN: Contract = Contract {
    path: "/api/v1/transport/{id}/rpc/request",
    ..WAKE
};

pub(super) fn is_write(contract: Contract) -> bool {
    matches!(contract, WAKE | REBOOT | SHUTDOWN)
}

impl NrdApi {
    pub(crate) async fn device_power(
        &self,
        id: &str,
        action: PowerAction,
    ) -> Result<ApiEnvelope<PowerReceipt>> {
        validate_device_id(id)?;
        let contract = match action {
            PowerAction::Wake => WAKE,
            PowerAction::Reboot => REBOOT,
            PowerAction::Shutdown => SHUTDOWN,
        };
        let path = contract.path.replace("{id}", id);
        let body = if action == PowerAction::Shutdown {
            serde_json::to_vec(&serde_json::json!({"p":"shutdown","need_response":false}))?
        } else {
            Vec::new()
        };
        let response: ApiEnvelope<serde_json::Value> = self.send(contract, &path, body).await?;
        // CommonResponse can omit data. Optional WoL diagnostics must not turn
        // a successfully accepted power command into a retryable parse error.
        let receipt = response
            .data
            .and_then(|data| serde_json::from_value(data).ok())
            .unwrap_or_default();
        Ok(ApiEnvelope {
            code: response.code,
            msg: response.msg,
            data: Some(receipt),
        })
    }
}
