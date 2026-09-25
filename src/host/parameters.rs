//! T D7BFB0/D7C110/B402E0/B40910/B361A0, not the UI's bitrate labels.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

// The current ordinary Windows sender's numerical budget table, in Mbps:
// FPS tier -> quality -> source pixel-area tier. Cloud platform 51 is excluded.
const BUDGET: [[[f64; 4]; 4]; 5] = [
    [
        [1., 1., 1., 1.],
        [3.5, 5.5, 5.5, 5.5],
        [8., 10., 12., 12.],
        [10., 20., 30., 40.],
    ],
    [
        [1.5, 1.5, 1.5, 1.5],
        [5.5, 7.5, 7.5, 7.5],
        [10., 14., 16., 16.],
        [20., 30., 40., 50.],
    ],
    [
        [1.5, 1.5, 1.5, 1.5],
        [6., 10., 10., 10.],
        [20., 30., 40., 40.],
        [30., 40., 50., 60.],
    ],
    [
        [1.5, 1.5, 1.5, 1.5],
        [8., 12., 12., 12.],
        [25., 35., 45., 45.],
        [35., 45., 55., 65.],
    ],
    [
        [1.5, 1.5, 1.5, 1.5],
        [10., 15., 15., 15.],
        [30., 40., 50., 50.],
        [40., 50., 60., 70.],
    ],
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub minimum: u32,
    pub maximum: u32,
    pub initial: u32,
    pub probe: u32,
    pub adaptive: bool,
}
impl Bounds {
    pub fn network_maximum(self) -> u32 {
        self.maximum.max(self.probe)
    }
    pub fn allocation(self, network: u32, extended: bool) -> u32 {
        if network == 0 {
            0
        } else {
            network.max(self.minimum).min(if extended {
                self.network_maximum()
            } else {
                self.maximum
            })
        }
    }
}

pub(crate) fn dimensions(quality: i32) -> (u32, u32) {
    match quality {
        1 => (1280, 720),
        3 => (2560, 1440),
        4 | 6 => (3840, 2160),
        _ => (1920, 1080),
    }
}

fn standard(quality: i32, size: (u32, u32), fps: u32) -> Bounds {
    let f = match fps {
        0..=30 => 0,
        31..=60 => 1,
        61..=90 => 2,
        91..=120 => 3,
        _ => 4,
    };
    let q = match quality {
        1 => 0,
        3 => 2,
        4 => 3,
        _ => 1,
    };
    let area = u64::from(size.0) * u64::from(size.1);
    let a = if area < 1920 * 1080 {
        0
    } else if area < 2560 * 1440 {
        1
    } else if area < 3840 * 2160 {
        2
    } else {
        3
    };
    let rate = BUDGET[f][q][a] * 1_000_000.;
    Bounds {
        minimum: if rate * 0.4 > 15_000_000. {
            (15_000_000. + rate / 1_000_000.) as u32
        } else {
            (rate * 0.4) as u32
        },
        maximum: (rate * 0.95) as u32,
        initial: (rate * 0.2) as u32,
        probe: 0,
        adaptive: false,
    }
}

pub(crate) fn fixed(quality: i32, custom: u32, size: (u32, u32), fps: u32) -> Bounds {
    if quality != 6 {
        return standard(quality, size, fps);
    }
    let rate = f64::from(custom.clamp(1_000_000, 500_000_000));
    Bounds {
        minimum: (rate * 0.02 + rate / 1_000_000.).max(500_000. + rate / 1_000_000.) as u32,
        maximum: rate as u32,
        initial: (rate * 0.05 + rate / 1_000_000.) as u32,
        probe: 0,
        adaptive: true,
    }
}

pub(crate) fn automatic(quality: i32, fps: u32, initial: bool) -> Bounds {
    let size = dimensions(quality);
    let mut result = standard(quality, size, fps);
    result.probe = if quality >= 4 {
        result.maximum
    } else {
        let next = standard(quality + 1, size, fps);
        if initial {
            next.maximum
        } else {
            ((f64::from(next.minimum) + f64::from(next.maximum)) * 0.8) as u32
        }
    };
    result
}

pub(crate) struct AutoQuality {
    current: i32,
    maximum: i32,
    startup: i32,
    probes: VecDeque<u32>,
    lower: VecDeque<u32>,
    at: Option<Instant>,
}
impl AutoQuality {
    pub fn new(startup: i32, maximum: i32) -> Self {
        let maximum = maximum.clamp(1, 4);
        let startup = startup.clamp(1, maximum);
        Self {
            current: startup,
            maximum,
            startup,
            probes: VecDeque::with_capacity(15),
            lower: VecDeque::with_capacity(15),
            at: None,
        }
    }
    pub fn quality(&self) -> i32 {
        self.current
    }
    pub fn limit(&mut self, maximum: i32) {
        self.maximum = maximum.clamp(1, 4);
        self.current = self.current.min(self.maximum);
    }
    pub fn observe(
        &mut self,
        now: Instant,
        fps: u32,
        has_frames: bool,
        probe: u32,
        lower: u32,
        loss: f64,
    ) -> Option<i32> {
        if self
            .at
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1))
        {
            return None;
        }
        self.at = Some(now);
        if !has_frames {
            return None;
        }
        for (window, value) in [(&mut self.probes, probe), (&mut self.lower, lower)] {
            if window.len() == 15 {
                window.pop_front();
            }
            window.push_back(value);
        }
        if self.startup > 2 && self.probes.len() < 5 {
            return None;
        }
        let mean = |window: &VecDeque<u32>| -> u32 {
            (window.iter().map(|v| u64::from(*v)).sum::<u64>() / window.len() as u64) as u32
        };
        let probe = mean(&self.probes);
        let lower = mean(&self.lower);
        let bounds = standard(self.current, dimensions(self.current), fps);
        let mut selected = self.current;
        if probe > bounds.maximum && loss <= f64::EPSILON && self.current < self.maximum {
            let next = standard(self.current + 1, dimensions(self.current), fps);
            if f64::from(probe) > (f64::from(next.minimum) + f64::from(next.maximum)) * 0.5 {
                selected += 1;
            }
        }
        // The lower-limit decision takes precedence over a simultaneous probe.
        if lower != 0 && lower <= bounds.minimum {
            selected = (self.current - 1).max(2);
        }
        selected = selected.min(self.maximum);
        if selected == self.current {
            None
        } else {
            self.current = selected;
            Some(selected)
        }
    }
}

/// The explicitly enabled congestion-window admission path is separate from
/// the SDK's disabled generic frame dropper (T34ACBA -> T34772E).
#[derive(Default)]
pub(crate) struct WindowAdmission {
    counter: u64,
}
impl WindowAdmission {
    pub fn next(&mut self, target: u32, minimum: u32, ratio: f64) -> (u32, bool) {
        if !ratio.is_finite() || ratio <= 0.01 || target <= minimum {
            return (target, false);
        }
        let reduce = (target - minimum).min((f64::from(target) * ratio) as u32);
        if reduce == 0 {
            return (target, false);
        }
        let period = u64::from((target / reduce).max(2));
        let drop = self.counter % period == 0;
        self.counter = self.counter.wrapping_add(1);
        (target - target / period as u32, drop)
    }
}
