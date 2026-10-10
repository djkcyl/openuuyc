//! Ordinary-user overlay execution; privileged residents only forward bounded commands.
//!
//! Linux has no overlay renderer yet: opening a board is refused with a
//! reason, everything else answers as an idle Windows host would.
#[cfg(windows)]
use super::model::Model;
use super::{Action, Context};
#[cfg(windows)]
use crate::platform::windows::{
    annotation::Overlay,
    host_service::{pipe::Pipe, process, vault},
};
use anyhow::Result;
#[cfg(windows)]
use anyhow::ensure;
#[cfg(windows)]
use serde::{Deserialize, Serialize};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[cfg(not(windows))]
#[derive(Default)]
pub(super) struct Backend;
#[cfg(not(windows))]
impl Backend {
    pub fn interval(&self) -> Duration {
        Duration::from_millis(50)
    }
    pub fn exchange(
        &mut self,
        context: Context,
        command: Option<Action>,
        cancel: &CancellationToken,
    ) -> Result<i32> {
        if !context.allowed || cancel.is_cancelled() {
            return Ok(4);
        }
        match command.as_ref().map(Action::toggle) {
            None | Some(Some(false)) => Ok(0),
            Some(Some(true)) => anyhow::bail!("Linux 暂不支持被控端批注（远程画笔）"),
            Some(None) => Ok(4),
        }
    }
}

#[cfg(windows)]
const PREFIX: &str = r"\\.\pipe\OpenUUYC.Annotation.";
#[cfg(windows)]
#[derive(Serialize, Deserialize)]
struct Request {
    context: Context,
    command: Option<Action>,
}

#[cfg(windows)]
pub(super) struct Backend {
    local: Option<Local>,
    remote: Option<Remote>,
    system: Option<bool>,
}
#[cfg(windows)]
impl Default for Backend {
    fn default() -> Self {
        Self {
            local: None,
            remote: None,
            system: None,
        }
    }
}
#[cfg(windows)]
impl Backend {
    pub fn interval(&self) -> Duration {
        if self.local.as_ref().is_some_and(|l| l.model.animated()) {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(50)
        }
    }
    pub fn exchange(
        &mut self,
        context: Context,
        command: Option<Action>,
        cancel: &CancellationToken,
    ) -> Result<i32> {
        if !context.allowed || cancel.is_cancelled() {
            self.local = None;
            self.remote = None;
            return Ok(4);
        }
        let active = process::active_session();
        if self
            .remote
            .as_ref()
            .is_some_and(|r| r.session != active || !r.agent.alive())
        {
            self.remote = None;
        }
        let opening = command.as_ref().and_then(Action::toggle) == Some(true);
        let closing = command.as_ref().and_then(Action::toggle) == Some(false);
        if self.local.is_none() && self.remote.is_none() && !opening {
            return Ok(if closing || command.is_none() { 0 } else { 4 });
        }
        let system = match self.system {
            Some(v) => v,
            None => {
                let v = vault::sid(std::process::id())? == "S-1-5-18";
                self.system = Some(v);
                v
            }
        };
        let code = if system {
            if self.remote.is_none() {
                if !logged_on(active) {
                    return Ok(2);
                }
                self.remote = Some(Remote::new(active, cancel)?);
            }
            let r = self.remote.as_ref().unwrap();
            let permitted = || {
                !cancel.is_cancelled() && r.agent.alive() && process::active_session() == r.session
            };
            r.pipe.send(&Request { context, command }, permitted)?;
            r.pipe
                .receive_timeout::<i32>(Duration::from_secs(2), permitted)?
        } else {
            if self.local.is_none() {
                self.local = Some(Local::new()?);
            }
            self.local
                .as_mut()
                .unwrap()
                .exchange(context, command.as_ref())?
        };
        if closing {
            self.local = None;
            self.remote = None;
        }
        Ok(code)
    }
}

#[cfg(windows)]
struct Local {
    model: Model,
    context: Context,
    overlay: Overlay,
    shown: bool,
    revision: u64,
    desktop: Option<(Instant, i32)>,
    last_animation: Instant,
}
#[cfg(windows)]
impl Local {
    fn new() -> Result<Self> {
        Ok(Self {
            model: Model::default(),
            context: Context::default(),
            overlay: Overlay::new()?,
            shown: false,
            revision: u64::MAX,
            desktop: None,
            last_animation: Instant::now(),
        })
    }
    fn exchange(&mut self, context: Context, command: Option<&Action>) -> Result<i32> {
        self.overlay.pump();
        self.model.reconcile(&self.context, &context);
        self.context = context;
        let desktop = match self
            .desktop
            .filter(|(at, _)| command.is_none() && at.elapsed() < Duration::from_millis(50))
        {
            Some((_, code)) => code,
            None => {
                let code = if !logged_on(process::active_session()) {
                    2
                } else if !crate::features::host::clipboard::agent::desktop_available() {
                    3
                } else {
                    0
                };
                self.desktop = Some((Instant::now(), code));
                code
            }
        };
        if !self.context.allowed {
            self.model.clear();
        }
        let code = command.map_or(0, |c| self.model.apply_action(&self.context, c, desktop));
        if desktop != 0 {
            self.model.suspend();
        }
        if desktop == 0 && self.last_animation.elapsed() >= Duration::from_millis(16) {
            let now = Instant::now();
            self.model.tick(now);
            self.last_animation = now;
        }
        let visible = self.model.enabled && self.context.allowed && desktop == 0;
        if !visible {
            self.overlay.hide();
        } else if !self.shown || self.revision != self.model.revision {
            if !self.shown {
                self.model
                    .dirty
                    .extend(self.context.screens.iter().map(|s| s.id));
            }
            self.overlay.render(
                &self.context.screens,
                &self.model.strokes,
                &self.model.boards,
                &self.model.dirty,
                Model::screen(&self.context, None),
            )?;
            self.model.dirty.clear();
        }
        self.shown = visible;
        self.revision = self.model.revision;
        Ok(code)
    }
}

#[cfg(windows)]
fn logged_on(session: u32) -> bool {
    use windows::{Win32::System::RemoteDesktop::*, core::PWSTR};
    if session == u32::MAX {
        return false;
    }
    unsafe {
        let mut text = PWSTR::null();
        let mut bytes = 0;
        if WTSQuerySessionInformationW(None, session, WTSUserName, &mut text, &mut bytes).is_err() {
            return false;
        }
        let available = !text.is_null() && bytes > 2 && *text.0 != 0;
        if !text.is_null() {
            WTSFreeMemory(text.0.cast());
        }
        available
    }
}
#[cfg(windows)]
struct Remote {
    pipe: Pipe,
    agent: crate::platform::windows::host_service::user_backend::Lease,
    session: u32,
}
#[cfg(windows)]
impl Remote {
    fn new(session: u32, cancel: &CancellationToken) -> Result<Self> {
        let name = format!("{PREFIX}{}", uuid::Uuid::new_v4().simple());
        let pipe = Pipe::server(&name, true)?;
        let agent = crate::platform::windows::host_service::user_backend::Lease::connect(
            crate::platform::windows::host_service::user_backend::Role::Annotation,
            &name, session, || !cancel.is_cancelled())?;
        let until = Instant::now() + Duration::from_secs(2);
        pipe.accept(|| !cancel.is_cancelled() && agent.alive() && Instant::now() < until)?;
        ensure!(
            pipe.peer_pid(true)? == agent.pid && pipe.peer_session(true)? == session,
            "批注用户进程身份不匹配"
        );
        Ok(Self {
            pipe,
            agent,
            session,
        })
    }
}

#[cfg(windows)]
pub(crate) fn run(name: &str, parent: u32) -> Result<()> {
    ensure!(name.starts_with(PREFIX) && name.len() < 150, "批注管道无效");
    ensure!(
        vault::sid(std::process::id())? != "S-1-5-18",
        "批注不能使用系统权限"
    );
    let pipe = Pipe::client(name)?.ok_or_else(|| anyhow::anyhow!("批注会话已结束"))?;
    let session = process::session(std::process::id())?;
    ensure!(
        pipe.peer_pid(false)? == parent && pipe.peer_session(false)? == session,
        "批注父进程身份不匹配"
    );
    let permitted = || crate::platform::windows::host_service::user_backend::permitted()
        && process::active_session() == session && pipe.queued_bytes().is_ok();
    let mut local = Local::new()?;
    let mut last_request = Instant::now();
    let mut last_idle = Instant::now();
    while permitted() {
        local.overlay.pump();
        if !pipe.available()? {
            ensure!(
                last_request.elapsed() < Duration::from_secs(3),
                "批注会话心跳已停止"
            );
            // Keep HWNDs responsive and hide promptly on secure desktop changes.
            if last_idle.elapsed() >= Duration::from_millis(16) {
                local.exchange(local.context.clone(), None)?;
                last_idle = Instant::now();
            }
            continue;
        }
        let request: Request = pipe.receive_timeout(Duration::from_secs(2), permitted)?;
        ensure!(request.context.screens.len() <= 32, "批注显示器数量超限");
        let code = local.exchange(request.context, request.command.as_ref())?;
        pipe.send(&code, permitted)?;
        last_request = Instant::now();
    }
    Ok(())
}
