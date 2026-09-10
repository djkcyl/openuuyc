use super::*;
use crate::login::sms::{self, PhoneNumber};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum LoginMethod {
    #[default]
    Qr,
    Phone,
}

pub(super) struct PhoneForm {
    pub generation: u64,
    pub submitting: bool,
    pub status: String,
    pub error: Option<String>,
    pub country: String,
    pub mobile: String,
    pub code: String,
    pub agreed: bool,
    pub sending: bool,
    pub resend_at: Option<Instant>,
    pub requested: Option<PhoneNumber>,
}
impl Default for PhoneForm {
    fn default() -> Self {
        Self {
            generation: 0,
            submitting: false,
            status: String::new(),
            error: None,
            country: "+86".into(),
            mobile: String::new(),
            code: String::new(),
            agreed: false,
            sending: false,
            resend_at: None,
            requested: None,
        }
    }
}
impl PhoneForm {
    pub fn remaining(&self) -> u64 {
        self.resend_at
            .map(|at| {
                at.saturating_duration_since(Instant::now())
                    .as_secs_f64()
                    .ceil() as u64
            })
            .unwrap_or(0)
    }
    pub fn contact(&self) -> Result<PhoneNumber> {
        PhoneNumber::parse(&self.country, &self.mobile)
    }
    pub fn can_submit(&self) -> bool {
        self.agreed
            && sms::validate_code(&self.code).is_ok()
            && self
                .contact()
                .is_ok_and(|phone| self.requested.as_ref() == Some(&phone))
    }
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.submitting = false;
        self.sending = false;
        self.code.clear();
        self.requested = None;
        self.status.clear();
        self.error = None;
    }
    pub fn clear_private(&mut self) {
        self.cancel();
        self.mobile.clear();
        self.agreed = false;
    }
}

impl DeviceCenterApp {
    pub(super) fn cancel_phone_login(&mut self) {
        self.phone.cancel();
        self.sync_login_running();
        let _ = self
            .worker
            .commands
            .send(GuiCommand::CancelSms(self.login_generation));
    }

    pub(super) fn request_sms_code(&mut self) {
        if self.phone.sending
            || self.phone.submitting
            || self.login_restoring
            || self.logout_pending
            || self.active_session.is_some()
            || self.phone.remaining() > 0
        {
            return;
        }
        let phone = match self.phone.contact() {
            Ok(phone) => phone,
            Err(error) => {
                self.phone.status = error.to_string();
                return;
            }
        };
        if !self.phone.agreed {
            self.phone.status = "请先同意用户协议和隐私政策".into();
            return;
        }
        self.phone.cancel();
        self.phone.sending = true;
        self.sync_login_running();
        self.phone.resend_at = Some(Instant::now() + sms::RESEND_INTERVAL);
        self.phone.status = "正在发送验证码…".into();
        if self
            .worker
            .commands
            .send(GuiCommand::RequestSms {
                generation: self.login_generation,
                attempt: self.phone.generation,
                phone,
                agreed: self.phone.agreed,
            })
            .is_err()
        {
            self.phone.sending = false;
            self.sync_login_running();
            self.phone.resend_at = None;
            self.phone.status = "登录服务不可用".into();
        }
    }

    pub(super) fn submit_sms_login(&mut self) {
        if self.phone.sending
            || self.phone.submitting
            || self.login_restoring
            || self.logout_pending
            || self.active_session.is_some()
            || !self.phone.can_submit()
        {
            return;
        }
        let Ok(phone) = self.phone.contact() else {
            return;
        };
        let code = std::mem::take(&mut self.phone.code);
        self.phone.generation = self.phone.generation.wrapping_add(1);
        self.phone.submitting = true;
        self.sync_login_running();
        self.phone.error = None;
        self.phone.status = "正在登录…".into();
        if self
            .worker
            .commands
            .send(GuiCommand::LoginSms {
                generation: self.login_generation,
                attempt: self.phone.generation,
                phone,
                code,
                agreed: self.phone.agreed,
            })
            .is_err()
        {
            self.phone.submitting = false;
            self.sync_login_running();
            self.phone.status = "登录服务不可用".into();
        }
    }
}
