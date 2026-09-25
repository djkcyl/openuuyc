//! Configured FPS and VBV FPS have separate roles (T C48020..C48EC0).
use super::Rate;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// The driver decision and the last committed decision are separate. A failed
/// reconfigure must not consume a change (T C48760 / C480B0).
#[derive(Clone, Copy)]
pub(crate) struct Update {
    pub rate: Rate,
    pub buffer_fps: u32,
    pub control_changed: bool,
    maximum: u32,
}

#[derive(Clone)]
pub(crate) struct Controller {
    frames: FrameRate,
    applied: Update,
    feedback: Option<(u32, u32, u32)>,
}
impl Controller {
    pub fn new(rate: Rate) -> Self {
        Self {
            frames: FrameRate::new(rate.fps),
            feedback: None,
            applied: Update {
                rate,
                buffer_fps: rate.fps,
                control_changed: false,
                maximum: rate.fps,
            },
        }
    }
    pub fn input(&mut self, timestamp: i64) {
        self.frames.input(timestamp);
    }
    pub fn decide(&mut self, rate: Rate) -> Option<Update> {
        self.frames.set_maximum(rate.fps);
        let old = self.applied;
        let control_changed = old.maximum != rate.fps || old.rate.quality != rate.quality;
        if control_changed {
            // C48020 is a frame-control update using the previously accepted
            // feedback. It does not fabricate a fresh SetRates sample.
            return Some(Update {
                rate: Rate {
                    fps: self.frames.configured,
                    quality: rate.quality,
                    ..old.rate
                },
                buffer_fps: old.buffer_fps.min(rate.fps).max(1),
                control_changed: true,
                maximum: rate.fps,
            });
        }
        let observed = self.frames.observed.min(rate.fps).max(1);
        let feedback = (rate.target, rate.peak, observed);
        if self.feedback == Some(feedback) {
            return None;
        }
        self.feedback = Some(feedback);
        let (configured, observed) = self.frames.settings(rate.fps);
        if !control_changed
            && !bitrate_changed(old.rate.target, rate.target)
            && !bitrate_changed(old.rate.peak, rate.peak)
            && old.rate.fps.abs_diff(configured) <= 5
            && old.buffer_fps.abs_diff(observed) <= 5
        {
            return None;
        }
        Some(Update {
            rate: Rate {
                fps: configured,
                ..rate
            },
            buffer_fps: observed,
            control_changed,
            maximum: rate.fps,
        })
    }
    pub fn commit(&mut self, update: Update) {
        self.applied = update;
    }
}
fn bitrate_changed(previous: u32, wanted: u32) -> bool {
    if previous == wanted {
        return false;
    }
    let (percent, absolute) = if wanted > previous {
        (10, 10_000_000)
    } else {
        (5, 5_000_000)
    };
    let difference = previous.abs_diff(wanted);
    previous == 0
        || u64::from(difference) * 100 >= u64::from(previous) * percent
        || difference >= absolute
}

#[derive(Clone)]
struct FrameRate {
    maximum: u32,
    configured: u32,
    observed: u32,
    previous: u32,
    inputs: VecDeque<i64>,
    first: Option<i64>,
    lower: VecDeque<u32>,
    lowering_since: Option<Instant>,
}
impl FrameRate {
    pub fn new(maximum: u32) -> Self {
        Self {
            maximum,
            configured: maximum,
            observed: maximum,
            previous: 0,
            inputs: VecDeque::new(),
            first: None,
            lower: VecDeque::new(),
            lowering_since: None,
        }
    }
    pub fn input(&mut self, time_100ns: i64) {
        if self.inputs.back().is_some_and(|&last| time_100ns <= last) {
            self.inputs.clear();
            self.first = None;
        }
        self.inputs.push_back(time_100ns);
        let first = *self.first.get_or_insert(time_100ns);
        while self.inputs.len() > 2
            && self
                .inputs
                .front()
                .is_some_and(|&t| time_100ns - t > 10_000_000)
        {
            self.inputs.pop_front();
        }
        if time_100ns - first >= 10_000_000 {
            let span = time_100ns - self.inputs[0];
            if span > 0 {
                self.observed = (((self.inputs.len() - 1) as f64 * 10_000_000.0 / span as f64)
                    .round() as u32)
                    .clamp(1, self.maximum);
            }
        }
    }
    fn set_maximum(&mut self, maximum: u32) {
        let maximum = maximum.max(1);
        if self.maximum != maximum {
            // C48EC0 resets the configured tier, not the input-rate producer.
            // Raising the limit does not imply that input already reached it.
            self.maximum = maximum;
            self.configured = maximum;
            self.observed = self.observed.min(maximum);
            self.previous = 0;
            self.lower.clear();
            self.lowering_since = None;
        }
    }
    pub fn settings(&mut self, maximum: u32) -> (u32, u32) {
        self.set_maximum(maximum);
        let maximum = self.maximum;
        let observed = self.observed.min(maximum).max(1);
        let minimum = 30.min(maximum);
        let wanted = (observed * 110).div_ceil(100).clamp(minimum, maximum);
        let tier = (minimum + (wanted - minimum).div_ceil(5) * 5).min(maximum);
        if self.previous > 0 && observed * 100 >= self.previous * 125 && self.configured < maximum {
            self.configured = maximum;
            self.lower.clear();
            self.lowering_since = None;
        } else if tier >= self.configured {
            self.configured = tier;
            self.lower.clear();
            self.lowering_since = None;
        } else {
            let since = *self.lowering_since.get_or_insert_with(Instant::now);
            self.lower.push_back(tier);
            if self.lower.len() > 5 {
                self.lower.pop_front();
            }
            if self.lower.len() >= 5 && since.elapsed() >= Duration::from_millis(4000) {
                let settling = self
                    .lower
                    .iter()
                    .zip(self.lower.iter().skip(1))
                    .all(|(a, b)| a >= b);
                self.configured = if settling {
                    *self.lower.back().unwrap()
                } else {
                    *self.lower.iter().max().unwrap()
                };
                self.lower.clear();
                self.lowering_since = None;
            }
        }
        self.previous = observed;
        (self.configured, observed)
    }
}
