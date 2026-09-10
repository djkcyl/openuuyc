//! One device connection, independent video-track decoders and local windows.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::watch;

use super::{NativeViewerSession, ViewerDisplayHandle, ViewerLaunchConfig};
use crate::media::{ConnectionMediaProfile, VideoCodec};
use crate::rtc::NativePeer;
use crate::stream_control::{RemoteScreen, StreamControlHandle};

#[derive(Clone)]
pub(crate) struct ScreenPlayback {
    peer: Weak<NativePeer>,
    control: StreamControlHandle,
    profile: ConnectionMediaProfile,
    alias: String,
    visible: watch::Sender<Vec<i32>>,
    runtime: tokio::runtime::Handle,
    decoders: Arc<std::sync::Mutex<HashMap<i32, Arc<NativeViewerSession>>>>,
    opening: Arc<tokio::sync::Mutex<()>>,
    capture_lock: Arc<tokio::sync::Mutex<()>>,
    visibility_update: Arc<AtomicBool>,
}

impl ScreenPlayback {
    pub(crate) fn new(
        peer: &Arc<NativePeer>,
        profile: ConnectionMediaProfile,
        alias: &str,
        initial: i32,
    ) -> Self {
        let control = peer.stream_control_handle();
        let (visible, mut receiver) = watch::channel(vec![initial]);
        let activity_control = control.clone();
        let capture_lock = Arc::new(tokio::sync::Mutex::new(()));
        let activity_lock = Arc::clone(&capture_lock);
        peer.spawn_viewing_task(async move {
            let mut inactive = HashMap::<i32, Instant>::new();
            let mut stopped = HashSet::new();
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    changed = receiver.changed() => if changed.is_err() { break; },
                    _ = tick.tick() => {},
                }
                let visible = receiver.borrow_and_update().clone();
                let screens = activity_control.snapshot().screens;
                inactive.retain(|id, _| screens.iter().any(|screen| screen.id == *id));
                for screen in screens {
                    if screen.id < 0 { continue; }
                    if visible.contains(&screen.id) {
                        inactive.remove(&screen.id);
                        stopped.remove(&screen.id);
                    } else if screen.video_track_index >= 0 && !stopped.contains(&screen.id) {
                        let since = inactive.entry(screen.id).or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_secs(5) {
                            let _operation = activity_lock.lock().await;
                            if receiver.borrow().contains(&screen.id) { continue; }
                            // No -1/all-screen operation: other windows keep playing.
                            if let Err(error) = activity_control.set_screen_capture(screen.id, false).await {
                                tracing::warn!(screen_id = screen.id, %error, "stop inactive screen capture failed");
                            }
                            stopped.insert(screen.id);
                        }
                    }
                }
            }
        });
        Self {
            peer: Arc::downgrade(peer),
            control,
            profile,
            alias: alias.to_owned(),
            visible,
            runtime: tokio::runtime::Handle::current(),
            decoders: Arc::new(std::sync::Mutex::new(HashMap::new())),
            opening: Arc::new(tokio::sync::Mutex::new(())),
            capture_lock,
            visibility_update: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn screens(&self) -> Vec<RemoteScreen> {
        self.control
            .snapshot()
            .screens
            .into_iter()
            .filter(|screen| screen.id >= 0)
            .collect()
    }

    pub(crate) fn set_visible(&self, mut screens: Vec<i32>) {
        screens.sort_unstable();
        screens.dedup();
        let changed = self.visible.send_if_modified(|current| {
            if *current == screens {
                false
            } else {
                *current = screens;
                true
            }
        });
        let wanted = self.visible.borrow().clone();
        let needs_update = super::mutex_lock(&self.decoders).values().any(|session| {
            session.is_software() && session.decode_paused() == wanted.contains(&session.screen_id)
        });
        if !(changed || needs_update) || self.visibility_update.swap(true, Ordering::AcqRel) {
            return;
        }
        let factory = self.clone();
        self.runtime.spawn(async move {
            let _opening = factory.opening.lock().await;
            let visible = factory.visible.borrow().clone();
            let sessions = super::mutex_lock(&factory.decoders)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            // Release hidden CPU decoders before resuming the selected one.
            // Late RTP for a paused decoder is retired without decoding.
            for session in &sessions {
                if session.is_software()
                    && !visible.contains(&session.screen_id)
                    && let Err(error) = session.pause_software().await
                {
                    tracing::debug!(%error, "hidden software decoder already stopped");
                }
            }
            for session in sessions {
                if session.is_software()
                    && factory.visible.borrow().contains(&session.screen_id)
                    && session.resume_decode()
                {
                    let _capture = factory.capture_lock.lock().await;
                    if let Err(error) = factory
                        .control
                        .set_screen_capture(session.screen_id, true)
                        .await
                    {
                        tracing::warn!(%error, "resume software screen capture failed");
                    }
                    if let Some(peer) = factory.peer.upgrade()
                        && let Some(track) = peer.video_tracks().get(session.track_index)
                    {
                        let _ = peer.request_keyframe(track.metadata.ssrc).await;
                    }
                }
            }
            factory.visibility_update.store(false, Ordering::Release);
        });
    }

    pub(crate) fn focus(&self, screen_id: i32) {
        if let Some(peer) = self.peer.upgrade()
            && let Some(screen) = self
                .screens()
                .into_iter()
                .find(|screen| screen.id == screen_id)
            && let Some(track) = peer.video_tracks().get(screen.video_track_index)
        {
            peer.select_viewed_video_track(track.index);
        }
    }

    pub(crate) fn open(
        &self,
        screen_id: i32,
        display: ViewerDisplayHandle,
        replace: Option<i32>,
        cancelled_pending: Option<i32>,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<Result<Arc<NativeViewerSession>>>,
    ) {
        let replace_software = replace.is_some_and(|id| {
            super::mutex_lock(&self.decoders)
                .values()
                .any(|session| session.screen_id == id && session.is_software())
        });
        self.visible.send_modify(|visible| {
            visible.retain(|id| {
                Some(*id) != cancelled_pending && !(replace_software && Some(*id) == replace)
            });
            if !visible.contains(&screen_id) {
                visible.push(screen_id);
            }
        });
        let (send, receive) = tokio::sync::oneshot::channel();
        let factory = self.clone();
        let task = self.runtime.spawn(async move {
            let result = factory.open_screen(screen_id, display, replace).await;
            let _ = send.send(result);
        });
        (task, receive)
    }

    pub(crate) fn register(&self, session: Arc<NativeViewerSession>) {
        super::mutex_lock(&self.decoders).insert(session.track_index, session);
    }

    pub(crate) fn close_decoders(&self) {
        for session in super::mutex_lock(&self.decoders).values() {
            session.close_handle().close();
        }
        super::mutex_lock(&self.decoders).clear();
    }

    async fn open_screen(
        &self,
        screen_id: i32,
        display: ViewerDisplayHandle,
        replace: Option<i32>,
    ) -> Result<Arc<NativeViewerSession>> {
        let _opening = self.opening.lock().await;
        let peer = self.peer.upgrade().context("观看连接已关闭")?;
        let existing_sessions = super::mutex_lock(&self.decoders)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in existing_sessions {
            if session.is_software()
                && session.screen_id != screen_id
                && (Some(session.screen_id) == replace
                    || !self.visible.borrow().contains(&session.screen_id))
            {
                self.visible
                    .send_modify(|visible| visible.retain(|id| *id != session.screen_id));
                session.pause_software().await?;
            }
        }
        let screen = self
            .screens()
            .into_iter()
            .find(|screen| screen.id == screen_id)
            .context("显示器已断开")?;
        let generation;
        {
            let _operation = self.capture_lock.lock().await;
            generation = self.control.snapshot().screens_generation;
            self.control.set_screen_capture(screen_id, true).await?;
        }
        let track = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let screen = self
                    .screens()
                    .into_iter()
                    .find(|screen| screen.id == screen_id)
                    .context("显示器已断开")?;
                if self.control.snapshot().screens_generation > generation
                    && screen.video_track_index >= 0
                    && let Some(track) = peer.video_tracks().get(screen.video_track_index)
                {
                    return Ok::<_, anyhow::Error>(track);
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .map_err(|_| anyhow!("显示器未返回视频，请重试"))??;
        let existing = { super::mutex_lock(&self.decoders).get(&track.index).cloned() };
        if let Some(session) = existing {
            session.resume_decode();
            peer.request_keyframe(track.metadata.ssrc).await?;
            return Ok(session);
        }
        let mut session = NativeViewerSession::launch(ViewerLaunchConfig {
            codec: codec(&track.metadata.codec)?,
            hardware_decode: self.profile.hardware_decode,
            title: format!("{}{alias}", crate::VIEWER_TITLE_PREFIX, alias = self.alias),
            initial_width: screen.width,
            initial_height: screen.height,
            frame_rate: self.profile.stream_fps,
            receiver_feedback: track.feedback.clone(),
            performance: track.performance.clone(),
            stream_control: self.control.clone(),
            display,
        })
        .await?;
        session.track_index = track.index;
        session.screen_id = screen_id;
        track.add_sink(session.video_sink()).await;
        track.start();
        peer.request_keyframe(track.metadata.ssrc).await?;
        tokio::time::timeout(Duration::from_secs(12), session.startup())
            .await
            .context("等待屏幕解码器启动超时")??;
        let session = Arc::new(session);
        self.register(Arc::clone(&session));
        Ok(session)
    }
}

fn codec(value: &str) -> Result<VideoCodec> {
    match value.to_ascii_lowercase().as_str() {
        "video/h264" => Ok(VideoCodec::H264),
        "video/h265" | "video/hevc" => Ok(VideoCodec::H265),
        _ => bail!("不支持的视频编码：{value}"),
    }
}
