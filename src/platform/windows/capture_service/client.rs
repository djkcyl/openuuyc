use super::{
    Layout, NAME, Reply, Request,
    texture::{self, Surface},
};
use crate::platform::windows::{
    capture::{self, Frame, Screen},
    host_service::{install, pipe::Pipe},
};
use anyhow::{Result, ensure};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::Graphics::Direct3D11::*;
pub(crate) struct Client {
    pipe: Pipe,
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    pub hdr: bool,
    dxgi: bool,
    pub cursor: Option<crate::platform::windows::cursor_shape::Snapshot>,
    surface: Option<Surface>,
    offer_surface: bool,
    pool: Vec<(ID3D11Texture2D, Arc<()>)>,
    pool_layout: Option<Layout>,
}
impl Client {
    pub fn connect(selected: &Screen) -> Result<Option<Self>> {
        let Some(pipe) = Pipe::client(NAME)? else {
            return Ok(None);
        };
        if let Err(error) = install::verify_running(pipe.peer_pid(false)?) {
            if error.is::<install::NeedsUpdate>() {
                return Ok(None);
            }
            return Err(error);
        }
        pipe.send(
            &Request::Open {
                screen: selected.clone(),
                client: std::process::id(),
            },
            &|| true,
        )?;
        let reply: Reply = pipe.receive_timeout(Duration::from_secs(10), &|| true)?;
        if let Some(error) = reply.error {
            anyhow::bail!(error)
        }
        ensure!(
            reply.screen.identity == selected.identity && reply.screen.adapter == selected.adapter,
            "服务采集源身份不匹配"
        );
        let (device, context) = capture::create_device(reply.screen.adapter)?;
        Ok(Some(Self {
            pipe,
            device,
            context,
            screen: reply.screen,
            generation: reply.generation,
            available: reply.available,
            hdr: reply.hdr,
            dxgi: reply.dxgi,
            cursor: reply.pointer,
            surface: None,
            offer_surface: false,
            pool: Vec::new(),
            pool_layout: None,
        }))
    }
    pub fn backend_name(&self) -> &'static str {
        if self.dxgi { "DXGI" } else { "GDI" }
    }
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        for _ in 0..2 {
            let offered = self
                .surface
                .as_ref()
                .filter(|_| self.offer_surface)
                .map(|s| s.handle.0.0 as usize as u64);
            self.pipe.send(
                &Request::Next {
                    timeout,
                    quality,
                    cursor,
                    hdr,
                    maximum,
                    surface: offered,
                },
                &|| true,
            )?;
            let reply: Reply = self.pipe.receive(&|| true)?;
            self.available = reply.available;
            self.hdr = reply.hdr;
            self.dxgi = reply.dxgi;
            self.cursor = reply.pointer;
            if reply.gone {
                return Err(capture::SourceGone.into());
            }
            if let Some(error) = reply.error {
                anyhow::bail!(error)
            }
            if reply.generation != self.generation || self.screen.adapter != reply.screen.adapter {
                self.pool.clear();
                self.pool_layout = None;
            }
            if self.screen.adapter != reply.screen.adapter {
                (self.device, self.context) = capture::create_device(reply.screen.adapter)?;
                self.surface = None;
                self.offer_surface = false;
            }
            self.screen = reply.screen;
            self.generation = reply.generation;
            if reply.needs_surface {
                let layout = reply
                    .layout
                    .ok_or_else(|| anyhow::anyhow!("服务没有提供纹理配置"))?;
                ensure!(
                    layout.adapter == self.screen.adapter,
                    "采集纹理适配器不匹配"
                );
                self.surface = Some(Surface::new(&self.device, layout)?);
                self.offer_surface = true;
                continue;
            }
            if !reply.frame {
                return Ok(None);
            }
            self.offer_surface = false;
            let source = self
                .surface
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("服务未准备共享纹理"))?;
            ensure!(
                reply.layout == Some(source.layout),
                "服务返回了过期采集纹理"
            );
            if self.pool_layout != Some(source.layout) {
                self.pool.clear();
                self.pool_layout = Some(source.layout);
            }
            let index = match self
                .pool
                .iter()
                .position(|(_, lease)| Arc::strong_count(lease) == 1)
            {
                Some(index) => index,
                None => {
                    ensure!(self.pool.len() < 3, "采集纹理仍被消费者持有");
                    self.pool.push((
                        texture::texture(&self.device, &texture::descriptor(source.layout)?)?,
                        Arc::new(()),
                    ));
                    self.pool.len() - 1
                }
            };
            let (texture, storage) = &self.pool[index];
            let guard = source.acquire(1)?;
            if guard.is_none() {
                let layout = source.layout;
                drop(guard);
                self.surface = Some(Surface::new(&self.device, layout)?);
                self.offer_surface = true;
                // Discard this unconsumed frame. The next request explicitly
                // replaces the producer's surface; do not extend GPU waits or
                // reopen the capture source/encoder for a transient stall.
                return Ok(None);
            }
            let guard = guard.unwrap();
            unsafe {
                self.context.CopyResource(texture, &source.texture);
                self.context.Flush();
            }
            guard.release(0)?;
            let (now, frequency) = super::counter()?;
            let age = Duration::from_secs_f64(
                (now.saturating_sub(reply.captured_qpc).max(0) as f64 / frequency as f64).min(10.0),
            );
            return Ok(Some(Frame {
                width: source.layout.width,
                height: source.layout.height,
                image: texture.clone(),
                captured: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
                is_new: reply.is_new,
                hdr_metadata: reply.metadata,
                _storage: Some(storage.clone()),
            }));
        }
        // Keep the new handle pending across calls if the source changed again
        // during negotiation. The capture worker already paces subsequent calls.
        Ok(None)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.pipe.send(&Request::Close, &|| true);
    }
}
