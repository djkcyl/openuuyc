//! Shape notifications share the session's reports; video cursor composition
//! remains controlled by each stream's CaptureSetting.
use super::{ReportRoutes, screens::Reports};
use crate::{
    features::{
        host::lock,
        remote_cursor::{self, CursorImage},
    },
    platform::windows::cursor_shape,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    reports: Arc<Reports>,
    mut route: watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    lease: crate::features::host::Lease,
) {
    let mut timer = tokio::time::interval(Duration::from_millis(33));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sent = None;
    let mut revision = None;
    let mut cached = None::<(usize, Option<u32>, CursorImage)>;
    loop {
        tokio::select! {_=cancel.cancelled()=>return,_=timer.tick()=>{},r=route.changed()=>{if r.is_err(){return;}}}
        if !lease.requested() {
            return;
        }
        let routes = route.borrow_and_update().clone();
        let Some(channel) = routes.text.as_ref().and_then(std::sync::Weak::upgrade) else {
            continue;
        };
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            sent = None;
        }
        let media = reports.media();
        let pointer = cursor_shape::pointer().ok();
        let screen = pointer.and_then(|p| {
            lock(&reports.catalog)
                .iter()
                .find(|info| {
                    let s = &info.screen;
                    media
                        .iter()
                        .any(|(_, m)| m.capturing && m.visible && m.screen.id == s.id)
                        && i64::from(p.x) >= i64::from(s.left)
                        && i64::from(p.x) < i64::from(s.left) + i64::from(s.width)
                        && i64::from(p.y) >= i64::from(s.top)
                        && i64::from(p.y) < i64::from(s.top) + i64::from(s.height)
                })
                .map(|info| info.screen.clone())
        });
        let visible = pointer.is_some_and(|p| p.showing) && screen.is_some();
        let key = (
            visible,
            if visible { pointer.unwrap().handle } else { 0 },
            screen.as_ref().map_or(-1, |s| s.id),
            screen
                .as_ref()
                .map(|s| (s.dpi_scale, s.width, s.height, s.left, s.top)),
        );
        if sent == Some(key) {
            continue;
        }
        let dpi = screen.as_ref().and_then(|s| s.dpi_scale);
        if visible
            && cached
                .as_ref()
                .is_none_or(|(handle, scale, _)| *handle != key.1 || *scale != dpi)
        {
            let handle = key.1;
            let image = tokio::task::spawn_blocking(move || -> anyhow::Result<CursorImage> {
                use image::ImageEncoder;
                let shape = cursor_shape::shape(handle)?;
                let mut png = Vec::new();
                image::codecs::png::PngEncoder::new(&mut png).write_image(
                    &shape.rgba,
                    shape.width,
                    shape.height,
                    image::ExtendedColorType::Rgba8,
                )?;
                anyhow::ensure!(png.len() <= 4 * 1024 * 1024, "光标图片过大");
                Ok(CursorImage {
                    png,
                    width: shape.width,
                    height: shape.height,
                    hotspot: shape.hotspot,
                    system_type: shape.kind,
                })
            })
            .await;
            match image {
                Ok(Ok(image)) => cached = Some((handle, dpi, image)),
                _ => {
                    cached = None;
                    continue;
                }
            }
        }
        if !lease.requested() {
            return;
        }
        let position = match (pointer, screen.as_ref()) {
            (Some(p), Some(s)) => [
                (f64::from(p.x) - f64::from(s.left)) / f64::from(s.width),
                (f64::from(p.y) - f64::from(s.top)) / f64::from(s.height),
            ],
            _ => [0.0, 0.0],
        };
        let payload = remote_cursor::encode_shape(
            if visible {
                cached.as_ref().map(|(_, _, image)| image)
            } else {
                None
            },
            key.2,
            position,
        );
        let bytes = crate::features::stream_control::publisher::cursor_report(
            payload,
            reports.sequence.fetch_add(1, Ordering::Relaxed),
        );
        // Cancellation here closes this whole connection, never one reliable
        // message in a still-active SCTP association.
        let bytes = bytes::Bytes::from(bytes);
        let result =
            tokio::select! {_=cancel.cancelled()=>return,r=channel.send_text_bytes(&bytes)=>r};
        if result.is_ok() {
            sent = Some(key);
        }
    }
}
