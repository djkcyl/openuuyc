//! Shared repair reservations, congestion recovery and FEC suppression.
//! Evidence: docs/official-4412-host-sender.md; all rates below are bits/second.
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Media,
    Rtx,
    Fec,
    Probe,
}
#[derive(Default, Clone, Copy)]
struct Rates {
    media: f64,
    rtx: f64,
    fec: f64,
}
struct Reservation {
    bytes: usize,
    limit: usize,
    emergency: bool,
}
struct Bucket {
    at: Instant,
    pressure: Duration,
    count: u32,
    target: f64,
    payload: f64,
    loss: f64,
}
pub(crate) struct Budget {
    network: bool,
    state: u8,
    bwe: f64,
    demand: f64,
    rates: Rates,
    totals: [u64; 3],
    baseline: Option<(Instant, [u64; 3])>,
    sampled: Option<Instant>,
    queue_enter: Option<Instant>,
    nack: Option<Instant>,
    evidence: bool,
    recovery_cap: f64,
    recovery_tick: Instant,
    recovery_safe: Option<Instant>,
    suppressed: bool,
    overuse: VecDeque<(Duration, f64, f64, f64)>,
    safe: Option<Instant>,
    window: Instant,
    used: usize,
    debt: usize,
    reserved: usize,
    next: u64,
    reservations: HashMap<u64, Reservation>,
    buckets: VecDeque<Bucket>,
    last_tick: Instant,
    last_pressure: bool,
    strategy: bool,
    entered: Option<Instant>,
    exited: Option<Instant>,
    hold: Duration,
}
impl Default for Budget {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            network: false,
            state: 0,
            bwe: 0.0,
            demand: 0.0,
            rates: Rates::default(),
            totals: [0; 3],
            baseline: None,
            sampled: None,
            queue_enter: None,
            nack: None,
            evidence: false,
            recovery_cap: 0.0,
            recovery_tick: now,
            recovery_safe: None,
            suppressed: false,
            overuse: VecDeque::new(),
            safe: None,
            window: now,
            used: 0,
            debt: 0,
            reserved: 0,
            next: 0,
            reservations: HashMap::new(),
            buckets: VecDeque::new(),
            last_tick: now,
            last_pressure: false,
            strategy: false,
            entered: None,
            exited: None,
            hold: Duration::from_secs(30),
        }
    }
}
impl Budget {
    pub fn reset(&mut self, network: bool) {
        let next = self.next;
        *self = Self::default();
        self.next = next;
        self.network = network;
    }
    pub fn network(&mut self, available: bool) {
        if self.network != available {
            self.reset(available);
        }
    }
    pub fn nack(&mut self) {
        self.nack = Some(Instant::now());
        if self.state != 0 {
            self.evidence = true;
        }
    }
    pub fn record(&mut self, kind: Kind, bytes: usize) {
        let index = match kind {
            Kind::Media => 0,
            Kind::Rtx => 1,
            Kind::Fec => 2,
            Kind::Probe => return,
        };
        self.totals[index] = self.totals[index].saturating_add(bytes as u64);
    }
    fn roll(&mut self, now: Instant) {
        if now.duration_since(self.window) >= Duration::from_millis(500) {
            self.window = now;
            self.used = self.debt;
            self.debt = 0;
        }
    }
    pub fn update(&mut self, bwe: u32, loss: f64, delay: bool, pressure: bool, queue: Duration) {
        let now = Instant::now();
        self.roll(now);
        self.bwe = f64::from(bwe);
        let elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;
        if let Some((at, counts)) = self.baseline {
            let span = now.duration_since(at);
            let delta = [
                self.totals[0].saturating_sub(counts[0]),
                self.totals[1].saturating_sub(counts[1]),
                self.totals[2].saturating_sub(counts[2]),
            ];
            if span >= Duration::from_millis(200) && delta.iter().sum::<u64>() >= 12000 {
                let scale = 8.0 / span.as_secs_f64();
                self.rates = Rates {
                    media: delta[0] as f64 * scale,
                    rtx: delta[1] as f64 * scale,
                    fec: delta[2] as f64 * scale,
                };
                self.sampled = Some(now);
                self.baseline = Some((now, self.totals));
                self.overuse.push_back((
                    span,
                    self.rates.media,
                    self.rates.media + self.rates.rtx + self.rates.fec,
                    self.bwe,
                ));
                while self.overuse.len() > 1
                    && self.overuse.iter().skip(1).map(|s| s.0).sum::<Duration>()
                        >= Duration::from_secs(2)
                {
                    self.overuse.pop_front();
                }
            } else if span >= Duration::from_millis(300) {
                self.sampled = None;
                self.baseline = Some((now, self.totals));
            }
        } else {
            self.baseline = Some((now, self.totals));
        }
        if self
            .sampled
            .is_some_and(|at| now.duration_since(at) > Duration::from_millis(300))
        {
            self.sampled = None;
        }
        if queue >= Duration::from_millis(50) {
            self.queue_enter.get_or_insert(now);
        } else {
            self.queue_enter = None;
        }
        let queue_pressure = queue >= Duration::from_millis(100)
            || self
                .queue_enter
                .is_some_and(|at| now.duration_since(at) >= Duration::from_millis(50));
        let congested = delay || pressure || queue_pressure;
        let quiet = !delay && !pressure && queue <= Duration::from_millis(10);
        if congested && self.state != 1 {
            self.state = 1;
            self.recovery_safe = None;
            self.recovery_cap = 0.0;
            self.evidence = loss > 0.0
                || self
                    .nack
                    .is_some_and(|at| now.duration_since(at) <= Duration::from_secs(1));
        } else if self.state == 1 && quiet {
            self.state = 2;
            self.recovery_cap = self.floor();
            self.recovery_tick = now;
            self.recovery_safe = None;
        }
        if self.state != 0 && loss > 0.0 {
            self.evidence = true;
        }
        if self.state == 2 {
            if !quiet {
                self.recovery_safe = None;
            } else if now.duration_since(self.recovery_tick) >= Duration::from_millis(500) {
                self.recovery_tick = now;
                let demand = self.demand.max(self.rates.rtx + self.rates.fec);
                if self.rates.media + demand <= self.bwe {
                    self.recovery_cap =
                        (self.recovery_cap + (self.bwe * 0.1).min(500_000.0)).min(self.bwe);
                    if demand <= self.recovery_cap {
                        let at = *self.recovery_safe.get_or_insert(now);
                        if now.duration_since(at) >= Duration::from_secs(5) {
                            self.state = 0;
                            self.evidence = false;
                        }
                    }
                } else {
                    self.recovery_safe = None;
                }
            }
        }
        if !self.network || self.bwe == 0.0 {
            self.overuse.clear();
            self.safe = None;
        } else if self.suppressed {
            let safe = self.state == 0
                && quiet
                && self.sampled.is_some()
                && self.rates.media + self.demand.max(self.rates.rtx) <= self.bwe * 0.9;
            if safe {
                let at = *self.safe.get_or_insert(now);
                if now.duration_since(at) >= Duration::from_secs(5) {
                    self.suppressed = false;
                    self.safe = None;
                    self.overuse.clear();
                }
            } else {
                self.safe = None;
            }
        } else if self.state != 0 && self.evidence && self.sampled.is_some() {
            let duration = self.overuse.iter().map(|s| s.0).sum::<Duration>();
            let baseline = self
                .overuse
                .iter()
                .map(|s| s.3 * s.0.as_secs_f64())
                .sum::<f64>();
            if duration >= Duration::from_secs(2)
                && baseline > 0.0
                && self
                    .overuse
                    .iter()
                    .map(|s| s.1 * s.0.as_secs_f64())
                    .sum::<f64>()
                    >= baseline * 0.95
                && self
                    .overuse
                    .iter()
                    .map(|s| s.2 * s.0.as_secs_f64())
                    .sum::<f64>()
                    >= baseline * 1.05
            {
                self.suppressed = true;
                self.overuse.clear();
            }
        } else {
            self.overuse.clear();
        }
        // CongestionStrategy uses detector pressure, not a lone jitter sample.
        if self
            .buckets
            .back()
            .is_none_or(|b| now.duration_since(b.at) >= Duration::from_secs(1))
        {
            self.buckets.push_back(Bucket {
                at: now,
                pressure: Duration::ZERO,
                count: 0,
                target: 0.0,
                payload: 0.0,
                loss: 0.0,
            });
            while self.buckets.len() > 90 {
                self.buckets.pop_front();
            }
        }
        if let Some(bucket) = self.buckets.back_mut() {
            if self.last_pressure {
                bucket.pressure += elapsed;
            }
            bucket.count += 1;
            bucket.target += self.bwe;
            bucket.payload += self.rates.media;
            bucket.loss += loss;
        }
        self.last_pressure = pressure;
        let sample: Vec<_> = self.buckets.iter().rev().skip(1).take(5).collect();
        if sample.len() == 5 {
            let target = sample
                .iter()
                .map(|b| b.target / f64::from(b.count.max(1)))
                .sum::<f64>()
                / 5.0;
            let payload = sample
                .iter()
                .map(|b| b.payload / f64::from(b.count.max(1)))
                .sum::<f64>()
                / 5.0;
            let loss = sample
                .iter()
                .map(|b| b.loss / f64::from(b.count.max(1)))
                .sum::<f64>()
                / 5.0;
            let all = sample
                .iter()
                .all(|b| b.pressure >= Duration::from_millis(200));
            let any = sample
                .iter()
                .any(|b| b.pressure >= Duration::from_millis(200));
            if !self.strategy && all && target <= 330_000.0 && loss >= 0.12 {
                self.hold = if self
                    .exited
                    .is_some_and(|at| now.duration_since(at) < Duration::from_secs(12))
                {
                    (self.hold * 2).min(Duration::from_secs(300))
                } else {
                    Duration::from_secs(30)
                };
                self.strategy = true;
                self.entered = Some(now);
            } else if self.strategy
                && (payload > 450_000.0
                    || (!any
                        && self
                            .entered
                            .is_some_and(|at| now.duration_since(at) >= self.hold)))
            {
                self.strategy = false;
                self.entered = None;
                self.exited = Some(now);
            }
        }
    }
    fn floor(&self) -> f64 {
        if self.evidence {
            self.demand.max(self.bwe * 0.2).min(self.bwe)
        } else if self.sampled.is_some() {
            (self.bwe - self.rates.media).max(0.0) + self.bwe * 0.2
        } else {
            self.bwe * 0.2
        }
    }
    fn allowance(&self) -> usize {
        let rate = if self.suppressed {
            self.bwe * 0.2
        } else if self.state == 1 {
            self.floor()
        } else if self.state == 2 {
            self.recovery_cap.max(self.floor()).min(self.bwe)
        } else if self.sampled.is_some() && self.bwe > self.rates.media {
            self.bwe - self.rates.media
        } else {
            self.bwe * 0.5
        };
        (rate.max(0.0) / 16.0) as usize
    }
    fn reserve(&mut self, bytes: usize, limit: usize, emergency: bool) -> u64 {
        self.next = self.next.wrapping_add(1).max(1);
        self.reserved = self.reserved.saturating_add(bytes);
        self.reservations.insert(
            self.next,
            Reservation {
                bytes,
                limit,
                emergency,
            },
        );
        self.next
    }
    pub fn reserve_fec(&mut self, count: usize, size: usize) -> (usize, u64) {
        self.roll(Instant::now());
        if !self.network || self.suppressed || self.strategy || size == 0 {
            return (0, 0);
        }
        let limit = self.allowance();
        let count = if self.state == 0 {
            count
        } else {
            count.min(limit.saturating_sub(self.used + self.reserved) / size)
        };
        if count == 0 {
            return (0, 0);
        }
        (count, self.reserve(count * size, limit, false))
    }
    pub fn reserve_rtx(&mut self, bytes: usize, previous: u32) -> Option<u64> {
        self.roll(Instant::now());
        if !self.network {
            return None;
        }
        let limit = self.allowance();
        let enough = bytes <= limit.saturating_sub(self.used + self.reserved);
        if self.state == 0 && !self.suppressed || enough {
            return Some(self.reserve(bytes, limit, false));
        }
        if previous < 2 {
            Some(self.reserve(bytes, limit, true))
        } else {
            None
        }
    }
    pub fn finish(&mut self, id: u64, actual: usize) {
        let Some(reservation) = self.reservations.remove(&id) else {
            return;
        };
        self.reserved = self.reserved.saturating_sub(reservation.bytes);
        let before = self.used;
        self.used = self.used.saturating_add(actual);
        let debt = if reservation.emergency {
            self.used
                .saturating_sub(reservation.limit)
                .saturating_sub(before.saturating_sub(reservation.limit))
        } else {
            actual.saturating_sub(reservation.bytes)
        };
        self.debt = self.debt.saturating_add(debt);
    }
    pub fn media_rate(&self, total: u32) -> u32 {
        let all = self.rates.media + self.rates.rtx + self.rates.fec;
        let overhead = if self.sampled.is_some() && all > 0.0 {
            (self.rates.rtx + self.rates.fec) / all
        } else {
            0.0
        };
        (f64::from(total) * (1.0 - overhead.min(0.5))).max(30_000.0) as u32
    }
    pub fn fec_demand(&mut self, rate: f64) {
        self.demand = rate.max(0.0).min(self.bwe);
    }
    pub fn fec_enabled(&self) -> bool {
        self.network && !self.suppressed && !self.strategy
    }
}
