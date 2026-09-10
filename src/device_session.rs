//! DeviceInitializer owner, independent of any one authenticated account.
//! Server 3D14E0/3D2BE0/3D08D0 and 3CA320; GUI owns it across QR/logouts.
use anyhow::{Context, Result, bail};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    api::{ApiFailure, DeviceInitResponse, NrdApi},
    auth::{KeyringIdentityStore, KeyringSessionStore, NativeIdentity, SessionStore},
};

#[derive(Clone)]
pub(crate) struct DeviceHandle {
    commands: mpsc::UnboundedSender<Command>,
    identity: watch::Receiver<NativeIdentity>,
}

pub(crate) struct DeviceRuntime {
    handle: DeviceHandle,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

enum Command {
    Ensure {
        force: bool,
        result: mpsc::UnboundedSender<InitEvent>,
    },
    WatchAccount(CancellationToken),
    StartupRetry(tokio::sync::oneshot::Sender<std::time::Duration>),
    Controllable(bool, tokio::sync::oneshot::Sender<Result<()>>),
    Name(String, String, tokio::sync::oneshot::Sender<Result<()>>),
}

enum InitEvent {
    IdentityReset,
    Complete(Result<Box<NativeIdentity>>),
}

type Completion = Option<mpsc::UnboundedSender<InitEvent>>;
type Pending = FuturesUnordered<BoxFuture<'static, (Completion, Result<DeviceInitResponse>)>>;

struct Initializer {
    identity: NativeIdentity,
    published: watch::Sender<NativeIdentity>,
    identities: KeyringIdentityStore,
    sessions: KeyringSessionStore,
    accounts: Vec<CancellationToken>,
    initialized: bool,
    date: Option<chrono::NaiveDate>,
    errors: u8,
    startup_retries: u32,
    pending: Pending,
}

impl DeviceRuntime {
    pub(crate) fn start() -> Result<Self> {
        let identities = KeyringIdentityStore::new()?;
        let identity = identities.load_or_create()?;
        let sessions = KeyringSessionStore::new()?;
        let (published, receiver) = watch::channel(identity.clone());
        let (commands, incoming) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let state = Initializer {
            identity,
            published,
            identities,
            sessions,
            accounts: Vec::new(),
            initialized: false,
            date: None,
            errors: 0,
            startup_retries: 0,
            pending: Pending::new(),
        };
        let task = tokio::spawn(state.run(incoming, cancel.clone()));
        Ok(Self {
            handle: DeviceHandle {
                commands,
                identity: receiver,
            },
            cancel,
            task: Some(task),
        })
    }

    pub(crate) fn handle(&self) -> DeviceHandle {
        self.handle.clone()
    }

    pub(crate) async fn close(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            tracing::warn!(%error, "device initializer task failed");
        }
    }
}

impl Drop for DeviceRuntime {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Error/cancellation exits may drop the owner before normal close.
        // No HTTP completion may outlive it and write a new identity afterward.
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl DeviceHandle {
    pub(crate) fn identity(&self) -> NativeIdentity {
        self.identity.borrow().clone()
    }

    pub(crate) fn watch_account(&self, ended: CancellationToken) -> Result<()> {
        self.commands
            .send(Command::WatchAccount(ended))
            .map_err(|_| anyhow::anyhow!("device initializer has stopped"))
    }

    pub(crate) async fn ensure(&self, force: bool) -> Result<NativeIdentity> {
        let (result, mut events) = mpsc::unbounded_channel();
        self.commands
            .send(Command::Ensure { force, result })
            .context("device initializer has stopped")?;
        while let Some(event) = events.recv().await {
            match event {
                InitEvent::IdentityReset => {
                    // 3B49E0/3B7660 ignore intermediate result=2. It is not
                    // success, final failure, or permission to restore old auth.
                    tracing::warn!(
                        "device initialization reset local client UUID; waiting for reinitialization"
                    );
                }
                InitEvent::Complete(result) => return result.map(|identity| *identity),
            }
        }
        bail!("device initializer stopped before completing the request")
    }

    pub(crate) async fn set_controllable(&self, value: bool) -> Result<()> {
        let (result, response) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Controllable(value, result))
            .context("device initializer has stopped")?;
        response
            .await
            .context("device initializer stopped before saving local permission")?
    }

    pub(crate) async fn set_name(&self, expected_id: String, value: String) -> Result<()> {
        let (result, response) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Name(expected_id, value, result))
            .context("device initializer has stopped")?;
        response
            .await
            .context("device initializer stopped before saving name")?
    }

    pub(crate) async fn startup_retry_delay(&self) -> Result<std::time::Duration> {
        let (result, response) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::StartupRetry(result))
            .context("device initializer has stopped")?;
        response
            .await
            .context("device initializer stopped before scheduling retry")
    }
}

impl Initializer {
    async fn run(
        mut self,
        mut incoming: mpsc::UnboundedReceiver<Command>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                command = incoming.recv() => match command {
                    None => break,
                    Some(Command::WatchAccount(ended)) => {
                        self.accounts.retain(|account| !account.is_cancelled());
                        self.accounts.push(ended);
                    }
                    Some(Command::StartupRetry(reply)) => {
                        // 3CA2F0's +44 counter belongs to the device session;
                        // setting initialized only resets +40, not this counter.
                        let seconds = if self.startup_retries < 60 { 3 } else { 30 };
                        self.startup_retries = self.startup_retries.saturating_add(1);
                        let _ = reply.send(std::time::Duration::from_secs(seconds));
                    }
                    Some(Command::Controllable(value, reply)) => {
                        let result = self.reload().and_then(|()| {
                            let mut next = self.identity.clone(); next.set_controllable(value); self.commit(next)
                        });
                        let _ = reply.send(result);
                    }
                    Some(Command::Name(expected_id, value, reply)) => {
                        let result = self.reload().and_then(|()| {
                            if self.identity.client_identity()?.device_id != expected_id { bail!("虚拟设备身份已改变，未覆盖新身份"); }
                            let mut next = self.identity.clone(); next.set_device_name(value); self.commit(next)
                        });
                        let _ = reply.send(result);
                    }
                    Some(Command::Ensure { force, result }) => {
                        if let Err(error) = self.reload() { Self::finish(Some(result), Err(error)); continue; }
                        let today = chrono::Local::now().date_naive();
                        if !force && self.initialized && self.date == Some(today) {
                            Self::finish(Some(result), Ok(self.identity.clone()));
                        } else {
                            // ensureInitialized resets immediate failures, but
                            // recursive initDevice calls below do not.
                            self.errors = 0;
                            self.start_request(Some(result));
                        }
                    }
                },
                Some((completion, result)) = self.pending.next(), if !self.pending.is_empty() => {
                    self.received(completion, result);
                }
            }
        }
        // The HTTP futures live in this owner, not detached retry tasks.
        self.pending.clear();
        tracing::debug!("device initializer HTTP requests and callbacks released");
    }

    fn reload(&mut self) -> Result<()> {
        let identity = self.identities.load_or_create()?;
        if identity != self.identity {
            self.identity = identity;
            self.published.send_replace(self.identity.clone());
            self.initialized = false;
        }
        Ok(())
    }

    fn commit(&mut self, next: NativeIdentity) -> Result<()> {
        // UU has one service-owned identity; this client also has CLI/GUI
        // processes. An older process must not overwrite another's UUID reset.
        if !self.identities.replace_if_matches(&self.identity, &next)? {
            bail!(
                "virtual device identity changed in another process; this initializer is no longer its owner"
            );
        }
        self.identity = next;
        self.published.send_replace(self.identity.clone());
        Ok(())
    }

    fn start_request(&mut self, completion: Completion) {
        self.date = Some(chrono::Local::now().date_naive()); // 3D2BE0, on request submission.
        let request = (|| -> Result<_> {
            // B16B00 uses the unauthenticated device-registration builder;
            // unlike AE7290 it never calls B85C10 to attach account headers.
            let api = NrdApi::new(self.identity.client_identity()?)?;
            Ok((api, self.identity.device_init_request()?))
        })();
        match request {
            Ok((api, body)) => {
                tracing::debug!(
                    attempt = self.errors + 1,
                    notifying = completion.is_some(),
                    "device initialization request submitted"
                );
                self.pending.push(
                    async move {
                        (
                            completion,
                            api.init_windows_device(&body)
                                .await
                                .and_then(|response| response.into_data()),
                        )
                    }
                    .boxed(),
                );
            }
            Err(error) => Self::finish(completion, Err(error)),
        }
    }

    fn received(&mut self, completion: Completion, response: Result<DeviceInitResponse>) {
        match response {
            Ok(response) => {
                let result = (|| -> Result<_> {
                    let mut next = self.identity.clone();
                    next.complete_registration(response.validated_device_id()?)?;
                    self.commit(next)?;
                    self.initialized = true;
                    self.errors = 0; // 3CA3D0/3CA3C0.
                    Ok(self.identity.clone())
                })();
                Self::finish(completion, result);
            }
            Err(error) => {
                let error = match error.downcast::<ApiFailure>() {
                    Ok(error) => error,
                    Err(error) => ApiFailure {
                        code: -1,
                        message: format!("{error:#}"),
                    },
                };
                let code = error.code;
                tracing::warn!(code, attempt = self.errors + 1, %error, "device initialization failed");
                if self.errors >= 2 {
                    self.initialized = false;
                    self.errors = 0;
                    Self::finish(completion, Err(error.into()));
                    return;
                }
                self.errors += 1;
                if code == 1006 {
                    let reset = (|| -> Result<()> {
                        let mut next = self.identity.clone();
                        next.reset_client_uuid();
                        self.commit(next)?;
                        if let Some(callback) = &completion {
                            let _ = callback.send(InitEvent::IdentityReset);
                        }
                        if let Some(session) = self.sessions.load()? {
                            self.sessions.clear_if_matches(&session)?;
                        }
                        for account in self.accounts.drain(..) {
                            account.cancel();
                        }
                        Ok(())
                    })();
                    if let Err(error) = reset {
                        Self::finish(completion, Err(error));
                        return;
                    }
                    // Both native calls submit real HTTP operations. The first
                    // has a no-op completion, not a no-op response handler.
                    self.start_request(None);
                }
                self.start_request(completion);
            }
        }
    }

    fn finish(completion: Completion, result: Result<NativeIdentity>) {
        if let Some(completion) = completion {
            let _ = completion.send(InitEvent::Complete(result.map(Box::new)));
        } else if let Err(error) = result {
            tracing::warn!(%error, "device background reinitialization failed");
        }
    }
}
