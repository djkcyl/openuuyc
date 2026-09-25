//! Ordinary desktop observers share one connection estimate. Their configured
//! minimums are enforced; remaining rate is shared equally up to each ceiling.
use super::parameters::Bounds;
use std::collections::BTreeMap;

pub(super) fn total(streams: &BTreeMap<usize, Bounds>) -> Bounds {
    if streams.len() == 1 {
        return *streams.values().next().unwrap();
    }
    let mut sum = Bounds {
        minimum: 0,
        maximum: 0,
        initial: 0,
        probe: 0,
        adaptive: false,
    };
    for b in streams.values() {
        sum.minimum = sum.minimum.saturating_add(b.minimum);
        sum.maximum = sum.maximum.saturating_add(b.maximum);
        sum.initial = sum.initial.saturating_add(b.initial);
        sum.probe = sum.probe.saturating_add(b.network_maximum());
        sum.adaptive |= b.adaptive;
    }
    sum
}

pub(super) fn share(
    streams: &BTreeMap<usize, Bounds>,
    index: usize,
    target: u32,
    extended: bool,
) -> u32 {
    if target == 0 || !streams.contains_key(&index) {
        return 0;
    }
    let mut rates: Vec<_> = streams
        .iter()
        .map(|(&id, b)| {
            (
                id,
                b.minimum,
                if extended {
                    b.network_maximum()
                } else {
                    b.maximum
                },
            )
        })
        .collect();
    let minimum: u64 = rates.iter().map(|(_, min, _)| u64::from(*min)).sum();
    let mut remaining = u64::from(target).saturating_sub(minimum);
    // Saturated small streams return their unused share to the remaining ones.
    while remaining > 0 {
        let count = rates.iter().filter(|(_, rate, max)| rate < max).count() as u64;
        if count == 0 {
            break;
        }
        let step = remaining / count;
        if step == 0 {
            break;
        }
        for (_, rate, max) in &mut rates {
            let added = step.min(u64::from(max.saturating_sub(*rate)));
            *rate += added as u32;
            remaining -= added;
        }
    }
    rates
        .into_iter()
        .find(|(id, _, _)| *id == index)
        .map_or(0, |(_, rate, _)| rate)
}
