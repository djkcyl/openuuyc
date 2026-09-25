//! Desktop media preparation within an already authorized controlled connection.
use super::{VideoConfig, capture, encoder, format};
use crate::features::stream_control::publisher;
use crate::protocol::capability::DeviceCapability;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

pub(crate) struct Capabilities {
    pub screen: capture::Screen,
    pub codecs: Vec<format::Capability>,
    pub adapters: Vec<capture::EncodingAdapter>,
}

#[derive(Default)]
pub(super) struct Cache(tokio::sync::Mutex<Option<Arc<Capabilities>>>);
impl Cache {
    pub fn snapshot(&self) -> Option<Arc<Capabilities>> {
        self.0.try_lock().ok()?.clone()
    }
    pub async fn prepare(
        &self,
        screen: capture::Screen,
        active: impl Fn() -> bool + Send + 'static,
    ) -> Result<Arc<Capabilities>> {
        let mut cached = self.0.lock().await;
        ensure!(active(), "媒体准备已取消");
        let adapters = capture::encoding_adapters()?;
        if let Some(capabilities) = cached.as_ref()
            && capabilities.screen == screen
            && capabilities.adapters == adapters
        {
            return Ok(capabilities.clone());
        }
        let capabilities = Arc::new(probe(screen, adapters, active).await?);
        *cached = Some(capabilities.clone());
        Ok(capabilities)
    }
    pub async fn invalidate(&self) {
        self.0.lock().await.take();
    }
}

async fn probe(
    screen: capture::Screen,
    adapters: Vec<capture::EncodingAdapter>,
    active: impl Fn() -> bool + Send + 'static,
) -> Result<Capabilities> {
    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        ensure!(active(), "媒体准备已取消");
        let _runtime = encoder::Runtime::new()?;
        let mut desktop = capture::Desktop::open_selected(&screen)?;
        let codecs = encoder::probe(&mut desktop, &active)?;
        ensure!(active(), "媒体准备已取消");
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            screen_id = screen.id,
            capabilities = codecs.len(),
            "host media capabilities prepared"
        );
        Ok(Capabilities {
            screen,
            codecs,
            adapters,
        })
    })
    .await
    .context("被控能力检查任务中断")?
}

pub(crate) struct Prepared {
    pub screen: capture::Screen,
    pub negotiated: Arc<format::Negotiated>,
    pub config: VideoConfig,
}
impl Prepared {
    pub fn new(
        options: &publisher::ConnectOptions,
        screen: capture::Screen,
        capabilities: &[format::Capability],
        remote: &DeviceCapability,
    ) -> Result<Self> {
        let negotiated = Arc::new(format::Negotiated::new(
            capabilities,
            remote,
            &options.decoders,
        )?);
        let mut config = publisher::config(options.params.as_ref());
        let chroma = options
            .params
            .as_ref()
            .map_or(1, |p| if p.chroma == 3 { 3 } else { 1 });
        let hdr = options.params.as_ref().is_some_and(|p| p.hdr);
        negotiated.apply(
            &mut config,
            None,
            chroma,
            hdr,
            (screen.width, screen.height),
        )?;
        Ok(Self {
            screen,
            negotiated,
            config,
        })
    }
}
