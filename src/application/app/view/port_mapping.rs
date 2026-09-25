use super::*;
impl DeviceCenterApp {
    pub(in crate::application::app) fn open_file_transfer(&mut self, id: String) {
        if self.logout_pending {
            return;
        }
        let Some(device) = self
            .devices
            .as_ref()
            .and_then(|list| list.my_binded_devices.iter().find(|d| d.device_id == id))
            .cloned()
        else {
            return;
        };
        if self
            .worker
            .commands
            .send(GuiCommand::Files {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
    pub(in crate::application::app) fn open_port_mapping(&mut self, id: String) {
        if self.logout_pending {
            return;
        }
        let Some(device) = self
            .devices
            .as_ref()
            .and_then(|list| list.my_binded_devices.iter().find(|d| d.device_id == id))
            .cloned()
        else {
            return;
        };
        if self
            .worker
            .commands
            .send(GuiCommand::Ports {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
}
