//! Sender-initiated route switching: primary RR statistics, 15-sample minima.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use webrtc::ice_transport::{
    ice_candidate_pair::RTCIceCandidatePair, ice_candidate_type::RTCIceCandidateType,
};
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Policy {
    enabled: bool,
    loss: u32,
    latency: u32,
}

impl Policy {
    pub fn from_signal(value: &serde_json::Value) -> Self {
        let integer = |name: &str| {
            value
                .get(name)
                .and_then(|v| v.as_i64())
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(0)
        };
        let loss = integer("force_auto_switch_pkt_loss");
        let latency = integer("force_auto_switch_latency");
        let possible_loss = integer("possible_auto_switch_pkt_loss");
        let possible_latency = integer("possible_auto_switch_latency");
        let minimum_latency = integer("possible_auto_switch_min_latency");
        // T AA6DF0 -> B00C70 -> C8EAE0 validates the whole group before
        // C917B0 consumes the force thresholds. Even valid force values must
        // fall back when the other relationships are invalid or missing.
        let valid = loss >= 10
            && latency >= 200
            && minimum_latency >= 200
            && possible_loss >= 5
            && possible_loss <= loss
            && possible_latency >= minimum_latency
            && possible_latency <= latency;
        let policy = Self {
            enabled: value
                .get("auto_switch_network")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            loss: if valid { loss as u32 } else { 30 },
            latency: if valid { latency as u32 } else { 600 },
        };
        tracing::info!(
            ?policy,
            defaulted = !valid,
            "host effective automatic route policy"
        );
        policy
    }
}
pub(super) struct AutoSwitch {
    policy: Policy,
    latest: Option<(u8, Duration)>,
    window: VecDeque<(u8, Duration)>,
    at: Option<Instant>,
    route: Option<(bool, bool)>,
    attempt: u8,
    quality: Option<(bool, i32)>,
}
impl AutoSwitch {
    pub fn attempt(&self) -> u8 {
        self.attempt
    }
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            latest: None,
            window: VecDeque::with_capacity(15),
            at: None,
            route: None,
            attempt: 0,
            quality: None,
        }
    }
    pub fn quality(&mut self, automatic: bool, quality: i32, settings_changed: bool) {
        if settings_changed || self.quality != Some((automatic, quality)) {
            // T C8FF40 (new settings) and C90330 (automatic tier change)
            // reset the loss/RTT windows before evaluating the next tier.
            self.window.clear();
        }
        self.quality = Some((automatic, quality));
    }
    pub fn route(&mut self, pair: &RTCIceCandidatePair) {
        self.route = Some((
            pair.local.typ == RTCIceCandidateType::Relay
                || pair.remote.typ == RTCIceCandidateType::Relay,
            pair.local.relay_protocol.eq_ignore_ascii_case("tls")
                && pair.remote.relay_protocol.eq_ignore_ascii_case("tls"),
        ));
        self.window.clear();
        self.latest = None;
    }
    pub fn report(&mut self, loss: u8, rtt: Duration) {
        self.latest = Some((loss, rtt));
    }
    pub fn tick(&mut self, connected: bool) -> Option<u8> {
        if !connected {
            self.window.clear();
            self.latest = None;
            return None;
        }
        if !self.policy.enabled
            || self.policy.loss < 10
            || self.policy.latency < 200
            || self
                .quality
                .is_some_and(|(automatic, quality)| automatic && quality != 2)
        {
            return None;
        }
        let (relay, tls) = self.route?;
        let latest = self.latest?;
        let now = Instant::now();
        if self
            .at
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
        {
            return None;
        }
        self.at = Some(now);
        if self.window.len() == 15 {
            self.window.pop_front();
        }
        self.window.push_back(latest);
        if self.window.len() < 15 {
            return None;
        }
        let loss = self
            .window
            .iter()
            .map(|v| u32::from(v.0) * 100 / 256)
            .min()
            .unwrap_or(0);
        let latency = self.window.iter().map(|v| v.1).min().unwrap_or_default();
        if loss <= self.policy.loss && latency <= Duration::from_millis(self.policy.latency.into())
        {
            return None;
        }
        if tls {
            return None;
        }
        self.attempt = if self.attempt == 2 || relay { 2 } else { 1 };
        self.window.clear();
        Some(self.attempt)
    }
}
