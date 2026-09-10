//! Opt-in OpenUUYC budget policy, not a replacement for the host's GCC/encoder.
//! All observations are low-frequency; no media queue or playout timing changes.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_secs(5);
const ACK_TIMEOUT: Duration = Duration::from_secs(15);
const RECOVERY_SECONDS: f64 = 30.0;

#[derive(Clone, Copy, Debug)]
pub(crate) struct BudgetSample {
    pub at: Instant,
    pub received_bytes: u64,
    pub encoded_bytes: u64,
    pub primary_packets: u64,
    pub repaired_packets: u64,
    pub frames: u64,
    pub pending_nacks: u64,
    pub rtt_ms: Option<f64>,
    pub local_delay_ms: f64,
    pub decoder_delay_ms: f64,
    pub geometry: (u64, u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetPhase {
    Waiting,
    Settling,
    Observing,
    Stable,
    Congested,
    Suspended,
}

#[derive(Clone, Debug)]
pub struct AdaptiveBitrateSnapshot {
    pub phase: BudgetPhase,
    pub applied_mbps: Option<u32>,
    pub pending_mbps: Option<u32>,
    pub suggested_mbps: Option<u32>,
    pub recovery_ceiling_mbps: u32,
    /// Received video RTP (including repair), not total network wire traffic.
    pub received_video_rtp_mbps: Option<f64>,
    /// Pre-pressure delivered video goodput, excluding repair and RTP overhead.
    pub reference_video_mbps: Option<f64>,
    pub reference_rtp_mbps: Option<f64>,
    pub message: &'static str,
}

struct Pending {
    sequence: i64,
    cap: u32,
    at: Instant,
}

struct RateSample {
    at: Instant,
    seconds: f64,
    video_mbps: f64,
    rtp_mbps: f64,
}

#[derive(Clone, Copy)]
struct RateReference {
    video_mbps: f64,
    rtp_mbps: f64,
}

pub(crate) struct BudgetPolicy {
    limit: u32,
    automatic: bool,
    applied: Option<u32>,
    pending: Option<Pending>,
    context: Option<String>,
    previous: Option<BudgetSample>,
    settle_until: Instant,
    pressure: VecDeque<bool>,
    recent_rates: VecDeque<RateSample>,
    pressure_reference: Option<RateReference>,
    healthy_seconds: f64,
    stable_cap: Option<u32>,
    recovery_ceiling: u32,
    base_rtt: Option<f64>,
    geometry: Option<(u64, u64)>,
    view: AdaptiveBitrateSnapshot,
}

impl BudgetPolicy {
    pub(crate) fn new(limit: u32, automatic: bool, now: Instant) -> Self {
        Self {
            limit,
            automatic,
            applied: None,
            pending: None,
            context: None,
            previous: None,
            settle_until: now + SETTLE,
            pressure: VecDeque::with_capacity(5),
            recent_rates: VecDeque::with_capacity(3),
            pressure_reference: None,
            healthy_seconds: 0.0,
            stable_cap: None,
            recovery_ceiling: limit,
            base_rtt: None,
            geometry: None,
            view: AdaptiveBitrateSnapshot {
                phase: BudgetPhase::Waiting,
                applied_mbps: None,
                pending_mbps: None,
                suggested_mbps: None,
                recovery_ceiling_mbps: limit,
                received_video_rtp_mbps: None,
                reference_video_mbps: None,
                reference_rtp_mbps: None,
                message: "等待远端确认视频预算",
            },
        }
    }

    pub(crate) fn snapshot(&self) -> AdaptiveBitrateSnapshot {
        let mut view = self.view.clone();
        view.applied_mbps = self.applied;
        view.pending_mbps = self.pending.as_ref().map(|p| p.cap);
        view.recovery_ceiling_mbps = self.recovery_ceiling;
        view
    }

    pub(crate) fn submitted(&mut self, sequence: i64, cap: u32, now: Instant) {
        self.pending = Some(Pending {
            sequence,
            cap,
            at: now,
        });
        self.view.phase = BudgetPhase::Waiting;
        self.view.message = "正在请求远端更新视频预算";
        self.clear_window(now);
    }

    pub(crate) fn set_automatic(&mut self, automatic: bool) {
        self.automatic = automatic;
    }

    pub(crate) fn acknowledge(&mut self, sequence: i64, now: Instant) {
        if self.pending.as_ref().is_none_or(|p| p.sequence != sequence) {
            return;
        }
        self.applied = self.pending.take().map(|p| p.cap);
        self.view.suggested_mbps = None;
        self.view.phase = BudgetPhase::Settling;
        self.view.message = "预算已确认，等待换流稳定";
        self.clear_window(now);
    }

    pub(crate) fn suspend(&mut self) {
        self.pending = None;
        self.view.phase = BudgetPhase::Suspended;
        self.view.message = "设置未确认或通道中断；已暂停自动调整，请重新应用";
        self.view.suggested_mbps = None;
        self.previous = None;
    }

    fn clear_window(&mut self, now: Instant) {
        self.previous = None;
        self.pressure.clear();
        self.recent_rates.clear();
        self.pressure_reference = None;
        self.view.suggested_mbps = None;
        self.healthy_seconds = 0.0;
        self.settle_until = now + SETTLE;
    }

    /// Returns a proposal only. The owner reuses the real, acknowledged control
    /// channel; a proposal never changes the applied budget by itself.
    pub(crate) fn observe(
        &mut self,
        sample: BudgetSample,
        context: &str,
        ready: bool,
    ) -> Option<u32> {
        if self.view.phase == BudgetPhase::Suspended {
            return None;
        }
        if let Some(pending) = &self.pending {
            if sample.at.saturating_duration_since(pending.at) >= ACK_TIMEOUT {
                self.suspend();
            }
            return None;
        }
        if !ready || self.applied.is_none() {
            self.clear_window(sample.at);
            self.view.phase = BudgetPhase::Waiting;
            self.view.message = "等待连接和串流设置就绪";
            return None;
        }
        if self.context.as_deref() != Some(context) {
            self.context = Some(context.to_owned());
            self.base_rtt = None;
            self.stable_cap = None;
            self.recovery_ceiling = self.limit;
            self.view.suggested_mbps = None;
            self.view.reference_video_mbps = None;
            self.view.reference_rtp_mbps = None;
            self.clear_window(sample.at);
        }
        if self.geometry != Some(sample.geometry) {
            self.geometry = Some(sample.geometry);
            self.clear_window(sample.at);
        }
        let previous = self.previous.replace(sample);
        if sample.at < self.settle_until {
            self.view.phase = BudgetPhase::Settling;
            self.view.message = "启动、换流或切路后暂缓判断";
            return None;
        }
        let previous = previous?;
        let seconds = sample
            .at
            .saturating_duration_since(previous.at)
            .as_secs_f64();
        if !(0.5..=2.5).contains(&seconds)
            || sample.received_bytes < previous.received_bytes
            || sample.primary_packets < previous.primary_packets
            || sample.frames < previous.frames
            || sample.encoded_bytes < previous.encoded_bytes
            || sample.repaired_packets < previous.repaired_packets
        {
            self.clear_window(sample.at);
            return None;
        }
        let packets = sample.primary_packets - previous.primary_packets;
        let repairs = sample
            .repaired_packets
            .saturating_sub(previous.repaired_packets);
        let frames = sample.frames - previous.frames;
        let mbps = (sample.received_bytes - previous.received_bytes) as f64 * 8.0 / seconds / 1e6;
        let encoded_mbps = sample.encoded_bytes.saturating_sub(previous.encoded_bytes) as f64 * 8.0
            / seconds
            / 1e6;
        self.view.received_video_rtp_mbps = Some(mbps);
        let active = packets >= 64 && mbps >= 0.5 && (frames >= 4 || sample.pending_nacks >= 8);
        if !active {
            self.pressure.clear();
            self.recent_rates.clear();
            self.pressure_reference = None;
            self.view.suggested_mbps = None;
            self.healthy_seconds = 0.0;
            self.view.phase = BudgetPhase::Observing;
            self.view.message = "画面活动不足，暂不估算；码率低于上限不代表带宽不足";
            return None;
        }
        let rtt = sample.rtt_ms.filter(|rtt| rtt.is_finite() && *rtt >= 0.0);
        if let Some(rtt) = rtt {
            self.base_rtt = Some(self.base_rtt.map_or(rtt, |old| old.min(rtt)));
        }
        let delayed = rtt
            .zip(self.base_rtt)
            .is_some_and(|(rtt, base)| rtt > (base + 15.0).max(base * 1.5));
        let local_wait = sample.local_delay_ms > 25.0 && sample.decoder_delay_ms < 10.0;
        let repair_share = repairs as f64 / (packets + repairs).max(1) as f64;
        // Repair pressure is a reason to try a lower budget, not proof of a
        // particular ISP capacity. Neither FPS shortfall nor a lone spike is used.
        let pressure = repairs >= 8
            && (repair_share >= 0.05 || (repair_share >= 0.02 && (delayed || local_wait)));
        while self.recent_rates.front().is_some_and(|rate| {
            sample.at.saturating_duration_since(rate.at) > Duration::from_secs(8)
        }) {
            self.recent_rates.pop_front();
        }
        // Freeze the pre-pressure rate at the first pressure sample. Once loss
        // reduces delivered throughput, that collapse must not become a new
        // capacity estimate, nor should the user's unused ceiling be the basis.
        if pressure && !self.pressure.iter().any(|p| *p) && self.view.suggested_mbps.is_none() {
            self.pressure_reference = if self.recent_rates.len() >= 2 {
                let seconds: f64 = self.recent_rates.iter().map(|rate| rate.seconds).sum();
                let rtp = self
                    .recent_rates
                    .iter()
                    .map(|rate| rate.rtp_mbps * rate.seconds)
                    .sum::<f64>()
                    / seconds;
                let video = self
                    .recent_rates
                    .iter()
                    .map(|rate| rate.video_mbps * rate.seconds)
                    .sum::<f64>()
                    / seconds;
                Some(RateReference {
                    video_mbps: video.min(rtp),
                    rtp_mbps: rtp,
                })
            } else {
                None
            };
        }
        self.pressure.push_back(pressure);
        if self.pressure.len() > 5 {
            self.pressure.pop_front();
        }
        let cap = self.applied.expect("ready budget");
        if self.pressure.iter().filter(|&&p| p).count() >= 3 {
            self.healthy_seconds = 0.0;
            self.view.phase = BudgetPhase::Congested;
            if cap == 1 {
                self.view.suggested_mbps = None;
                self.view.message = "最低视频预算下仍有持续修复，需检查线路；不继续重复降码率";
                return None;
            }
            let Some(reference) = self.pressure_reference else {
                self.view.suggested_mbps = None;
                self.view.message = "持续承压，但缺少过载前速率样本；请先手动降低上限";
                return None;
            };
            let trial = (reference.video_mbps.min(f64::from(cap)) * 0.9)
                .floor()
                .max(1.0) as u32;
            let trial = trial.min(cap - 1);
            // Do not climb back toward a configured 100M when pressure actually
            // began near 50M. Keep the same measured headroom on this route;
            // a new route or explicit reassessment can establish a new bound.
            self.recovery_ceiling = self.recovery_ceiling.min(trial);
            let suggestion = self
                .stable_cap
                .filter(|stable| *stable < cap)
                .map_or(trial, |stable| stable.min(trial));
            self.view.suggested_mbps = Some(suggestion);
            self.view.reference_video_mbps = Some(reference.video_mbps);
            self.view.reference_rtp_mbps = Some(reference.rtp_mbps);
            self.view.message = "持续承压：按过载前有效视频速率留出约10%余量试调";
            return self.automatic.then_some(suggestion);
        }
        if !pressure && repair_share < 0.01 && !delayed {
            self.healthy_seconds += seconds;
            if !self.pressure.iter().any(|p| *p) && sample.pending_nacks < 8 && !local_wait {
                self.recent_rates.push_back(RateSample {
                    at: sample.at,
                    seconds,
                    video_mbps: encoded_mbps,
                    rtp_mbps: mbps,
                });
                if self.recent_rates.len() > 3 {
                    self.recent_rates.pop_front();
                }
            }
        } else {
            self.healthy_seconds = 0.0;
        }
        if self.healthy_seconds >= 15.0 {
            if encoded_mbps >= f64::from(cap) * 0.7 {
                self.stable_cap = Some(cap);
            }
            self.view.phase = BudgetPhase::Stable;
            self.view.suggested_mbps = None;
            self.view.message = if cap < self.limit {
                if self.automatic {
                    if cap < self.recovery_ceiling {
                        "当前预算稳定；仅在本线路参考上限内缓慢恢复"
                    } else {
                        "当前预算稳定；保留承压余量，手动重评可重新试上限"
                    }
                } else {
                    "当前预算稳定；仅建议模式不会自动调整"
                }
            } else {
                "当前内容下稳定；上限是预算，不要求实际码率跑满"
            };
        } else if self.view.suggested_mbps.is_none() {
            self.view.phase = BudgetPhase::Observing;
            self.view.message = "观察实际链路，不依据单次尖峰调整";
        }
        if self.automatic
            && self.healthy_seconds >= RECOVERY_SECONDS
            && encoded_mbps >= f64::from(cap) * 0.7
        {
            let next = (cap + (cap / 20).max(1))
                .min(self.limit)
                .min(self.recovery_ceiling);
            if next > cap {
                return Some(next);
            }
        }
        None
    }
}
