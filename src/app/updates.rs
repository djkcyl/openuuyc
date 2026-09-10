//! Public release checks are independent of UU login and device sessions.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

const LATEST_RELEASE: &str = "https://api.github.com/repos/djkcyl/openuuyc/releases/latest";
const RELEASES: &str = "https://github.com/djkcyl/openuuyc/releases";
const RETRY_INTERVAL: Duration = Duration::from_secs(10);

pub(super) enum State {
    Checking,
    Current,
    Ahead,
    Available { version: String, url: String },
    NoRelease,
    Failed(String),
}

pub(super) struct UpdateCheck {
    pub state: State,
    pending: Option<Receiver<Result<State>>>,
    cancel: Option<oneshot::Sender<()>>,
    retry_at: Instant,
}

impl UpdateCheck {
    pub fn start(ctx: &egui::Context) -> Self {
        let mut check = Self {
            state: State::Checking,
            pending: None,
            cancel: None,
            retry_at: Instant::now(),
        };
        check.request(ctx);
        check
    }

    pub fn request(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() || Instant::now() < self.retry_at {
            return;
        }
        self.retry_at = Instant::now() + RETRY_INTERVAL;
        let (tx, rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let ctx = ctx.clone();
        let worker = std::thread::Builder::new()
            .name("release-check".into())
            .spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        let result = runtime.block_on(async {
                            tokio::select! {
                                biased;
                                _ = cancel_rx => None,
                                result = latest() => Some(result),
                            }
                        });
                        runtime.shutdown_background();
                        result
                    }
                    Err(error) => Some(Err(error).context("无法启动更新检查")),
                };
                if let Some(result) = result {
                    let _ = tx.send(result);
                    ctx.request_repaint();
                }
            });
        match worker {
            Ok(_) => {
                self.pending = Some(rx);
                self.cancel = Some(cancel_tx);
                self.state = State::Checking;
            }
            Err(error) => self.state = State::Failed(format!("无法启动更新检查：{error}")),
        }
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.pending else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("更新检查未完成")),
        };
        self.pending = None;
        self.cancel = None;
        self.state = result.unwrap_or_else(|error| State::Failed(format!("{error:#}")));
    }

    pub fn retry_wait(&self) -> u64 {
        self.retry_at
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            .ceil() as u64
    }
}

impl Drop for UpdateCheck {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

async fn latest() -> Result<State> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .user_agent(concat!("OpenUUYC/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("无法创建更新检查连接")?;
    let mut response = client
        .get(LATEST_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .send()
        .await
        .context("无法连接 GitHub，请检查网络后重试")?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => return Ok(State::NoRelease),
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            bail!("GitHub 暂时限制请求，请稍后重试")
        }
        _ => response
            .error_for_status_ref()
            .context("GitHub 更新检查失败")?,
    };
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("读取版本信息失败")? {
        ensure!(
            body.len() + chunk.len() <= 512 * 1024,
            "GitHub 版本信息过大"
        );
        body.extend_from_slice(&chunk);
    }
    let release: Release = serde_json::from_slice(&body).context("无法解析 GitHub 版本信息")?;
    ensure!(
        !release.draft && !release.prerelease,
        "GitHub 未返回正式版本"
    );
    let version = semver::Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .context("GitHub 正式版本号格式不正确")?;
    ensure!(version.pre.is_empty(), "GitHub 最新版本不是正式版本");
    let current =
        semver::Version::parse(env!("CARGO_PKG_VERSION")).context("当前程序版本号格式不正确")?;
    match version.cmp_precedence(&current) {
        std::cmp::Ordering::Equal => Ok(State::Current),
        std::cmp::Ordering::Less => Ok(State::Ahead),
        std::cmp::Ordering::Greater => {
            // Construct the destination on our fixed repository, ignoring remote URLs.
            let mut url = url::Url::parse(RELEASES)?;
            url.path_segments_mut()
                .expect("GitHub base URL")
                .push("tag")
                .push(&release.tag_name);
            Ok(State::Available {
                version: version.to_string(),
                url: url.to_string(),
            })
        }
    }
}
