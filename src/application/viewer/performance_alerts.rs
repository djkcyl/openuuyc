//! Performance alerts: nothing on screen while the stream is healthy; when a
//! metric turns abnormal, only that metric appears, like a game's network
//! problem indicator. Each alert lingers briefly after it clears so a
//! flickering value stays readable.
//!
//! The thresholds are stricter than the compact HUD's colours on purpose: the
//! HUD colours a value that is always on screen, while an alert has to stay
//! quiet on an ordinary internet connection.
use super::hud::{bad_color, compact_hud_line, compact_performance_frame, warning_color};
use crate::diagnostics::performance::PerformanceSnapshot;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long an alert stays after its metric has recovered.
const HOLD: Duration = Duration::from_secs(3);
/// The span counters are compared across, for rates kept only as totals.
const WINDOW: Duration = Duration::from_secs(1);
/// A receive rate below this means a static or paused picture, where long
/// gaps between frames are expected rather than stutter.
const ACTIVE_FPS: f64 = 15.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Warning,
    Bad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Metric {
    Loss,
    Rtt,
    Jitter,
    FrameDelay,
    Pace,
    Jank,
    Dropped,
    LocalDelay,
    DecoderQueue,
}

#[derive(Clone, Debug, PartialEq)]
struct Alert {
    metric: Metric,
    level: Level,
    text: String,
}

/// Totals sampled over time, for metrics the monitor keeps as counters.
#[derive(Clone, Copy)]
struct Counters {
    at: Instant,
    dropped: u64,
    rendered: u64,
    jank: u64,
    big_jank: u64,
    receive_fps: f64,
}
impl Counters {
    fn of(s: &PerformanceSnapshot, at: Instant) -> Self {
        Self {
            at,
            dropped: s.dropped_present_frames,
            rendered: s.total_rendered_frames,
            jank: s.jank_count,
            big_jank: s.big_jank_count,
            receive_fps: s.receive_fps,
        }
    }
}

/// Changes over the last window, when one has elapsed.
#[derive(Clone, Copy, Default)]
struct Window {
    dropped: u64,
    rendered: u64,
    jank: u64,
    big_jank: u64,
    /// The receive rate at the window's start and now.
    active: bool,
}

fn level_above(value: f64, warning: f64, bad: f64) -> Option<Level> {
    if value > bad {
        Some(Level::Bad)
    } else if value > warning {
        Some(Level::Warning)
    } else {
        None
    }
}

fn evaluate(s: &PerformanceSnapshot, window: Option<Window>) -> Vec<Alert> {
    let mut alerts = Vec::new();
    let mut push = |metric, level: Option<Level>, text: String| {
        if let Some(level) = level {
            alerts.push(Alert {
                metric,
                level,
                text,
            });
        }
    };
    push(
        Metric::Loss,
        level_above(s.packet_loss_percent, 0.1, 1.0),
        format!("丢包 {:.1}%", s.packet_loss_percent),
    );
    if let Some(rtt) = s.current_delay_ms {
        push(
            Metric::Rtt,
            level_above(rtt, 80.0, 150.0),
            format!("网络往返 {rtt:.0} ms"),
        );
    }
    push(
        Metric::Jitter,
        level_above(s.rtp_jitter_ms, 10.0, 30.0),
        format!("网络抖动 {:.1} ms", s.rtp_jitter_ms),
    );
    if let Some(delay) = s.frame_delay_ms {
        push(
            Metric::FrameDelay,
            level_above(delay as f64, 60.0, 100.0),
            format!("帧延迟 {delay} ms"),
        );
    }
    // Frames arrive but are not all shown: this side cannot keep up.
    if s.receive_fps >= ACTIVE_FPS {
        let shortfall = 1.0 - s.render_fps / s.receive_fps;
        push(
            Metric::Pace,
            level_above(shortfall, 0.05, 0.10),
            format!("呈现 {:.0} / 接收 {:.0} fps", s.render_fps, s.receive_fps),
        );
    }
    push(
        Metric::LocalDelay,
        level_above(s.local_frame_delay_ms, 20.0, 40.0),
        format!("本地处理 {:.1} ms", s.local_frame_delay_ms),
    );
    push(
        Metric::DecoderQueue,
        level_above(s.decoder_queue_frames as f64, 2.0, 6.0),
        format!("解码积压 {} 帧", s.decoder_queue_frames),
    );
    if let Some(w) = window {
        let total = w.dropped + w.rendered;
        if total >= 10 {
            let percent = w.dropped as f64 * 100.0 / total as f64;
            push(
                Metric::Dropped,
                level_above(percent, 1.0, 5.0),
                format!("呈现丢帧 {percent:.1}%"),
            );
        }
        if w.active && w.jank > 0 {
            push(
                Metric::Jank,
                Some(if w.big_jank > 0 {
                    Level::Bad
                } else {
                    Level::Warning
                }),
                format!("画面卡顿 {} 次", w.jank),
            );
        }
    }
    alerts
}

#[derive(Clone, Default)]
struct State {
    identity: usize,
    history: VecDeque<Counters>,
    /// Alerts on screen, each with the time it may disappear.
    shown: Vec<(Alert, Instant)>,
}
impl State {
    fn update(&mut self, identity: usize, s: &PerformanceSnapshot, now: Instant) {
        if self.identity != identity {
            *self = Self {
                identity,
                ..Self::default()
            };
        }
        let current = Counters::of(s, now);
        // Counters only grow within one session; anything else restarts.
        if self.history.back().is_some_and(|last| {
            current.rendered < last.rendered
                || current.dropped < last.dropped
                || current.jank < last.jank
        }) {
            self.history.clear();
        }
        self.history.push_back(current);
        while self
            .history
            .get(1)
            .is_some_and(|next| now.duration_since(next.at) >= WINDOW)
        {
            self.history.pop_front();
        }
        let window = self
            .history
            .front()
            .filter(|start| now.duration_since(start.at) >= WINDOW)
            .map(|start| Window {
                dropped: current.dropped - start.dropped,
                rendered: current.rendered - start.rendered,
                jank: current.jank - start.jank,
                big_jank: current.big_jank - start.big_jank,
                active: start.receive_fps >= ACTIVE_FPS && current.receive_fps >= ACTIVE_FPS,
            });
        for alert in evaluate(s, window) {
            match self
                .shown
                .iter_mut()
                .find(|(a, _)| a.metric == alert.metric)
            {
                Some(entry) => *entry = (alert, now + HOLD),
                None => self.shown.push((alert, now + HOLD)),
            }
        }
        self.shown.retain(|(_, until)| *until > now);
        self.shown
            .sort_by(|(a, _), (b, _)| b.level.cmp(&a.level).then(a.metric.cmp(&b.metric)));
    }
}

/// Shows the abnormal metrics only, in the compact HUD's place and style.
pub(super) fn show(
    ctx: &egui::Context,
    identity: usize,
    stats: &PerformanceSnapshot,
    id: &'static str,
) {
    let key = egui::Id::new((ctx.viewport_id(), id, "performance-alerts"));
    let mut state = ctx.data_mut(|d| d.get_temp::<State>(key).unwrap_or_default());
    state.update(identity, stats, Instant::now());
    // Keep evaluating on a still picture, where nothing else repaints.
    ctx.request_repaint_after(Duration::from_millis(250));
    if !state.shown.is_empty() {
        egui::Window::new("性能异常")
            .id(egui::Id::new((id, "performance-alerts")))
            // No controls: it must not take input from the video below.
            .interactable(false)
            .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -44.0])
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .frame(compact_performance_frame())
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for (alert, _) in &state.shown {
                    let color = match alert.level {
                        Level::Warning => warning_color(),
                        Level::Bad => bad_color(),
                    };
                    compact_hud_line(ui, &alert.text, color);
                }
            });
    }
    ctx.data_mut(|d| d.insert_temp(key, state));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> PerformanceSnapshot {
        let base = crate::diagnostics::performance::PerformanceMonitor::new("测试").snapshot();
        PerformanceSnapshot {
            receive_fps: 60.0,
            render_fps: 60.0,
            current_delay_ms: Some(30.0),
            frame_delay_ms: Some(25),
            packet_loss_percent: 0.0,
            rtp_jitter_ms: 2.0,
            local_frame_delay_ms: 6.0,
            decoder_queue_frames: 0,
            ..(*base).clone()
        }
    }

    #[test]
    fn a_healthy_stream_shows_nothing() {
        assert!(evaluate(&healthy(), Some(Window::default())).is_empty());
    }

    #[test]
    fn only_the_abnormal_metrics_appear() {
        let s = PerformanceSnapshot {
            packet_loss_percent: 2.5,
            current_delay_ms: Some(95.0),
            ..healthy()
        };
        let alerts = evaluate(&s, None);
        let metrics: Vec<_> = alerts.iter().map(|a| (a.metric, a.level)).collect();
        assert_eq!(
            metrics,
            [(Metric::Loss, Level::Bad), (Metric::Rtt, Level::Warning)]
        );
    }

    #[test]
    fn stutter_counts_only_while_frames_stream() {
        let window = Window {
            jank: 1,
            big_jank: 1,
            rendered: 60,
            ..Window::default()
        };
        let idle = Window {
            active: false,
            ..window
        };
        assert!(evaluate(&healthy(), Some(idle)).is_empty());
        let active = Window {
            active: true,
            ..window
        };
        let alerts = evaluate(&healthy(), Some(active));
        assert_eq!(alerts.len(), 1);
        assert_eq!(
            (alerts[0].metric, alerts[0].level),
            (Metric::Jank, Level::Bad)
        );
    }

    #[test]
    fn an_alert_lingers_after_it_clears() {
        let mut state = State::default();
        let start = Instant::now();
        let lossy = PerformanceSnapshot {
            packet_loss_percent: 3.0,
            ..healthy()
        };
        state.update(1, &lossy, start);
        assert_eq!(state.shown.len(), 1);
        state.update(1, &healthy(), start + Duration::from_secs(1));
        assert_eq!(state.shown.len(), 1, "held after recovery");
        state.update(1, &healthy(), start + Duration::from_secs(4));
        assert!(state.shown.is_empty(), "gone once the hold expires");
    }
}
