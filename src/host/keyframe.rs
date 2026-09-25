//! Authenticated RTCP requests: target filtering, FIR deduplication, then the
//! sender's request interval. Local frame controls bypass this network gate.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use webrtc::rtcp::{
    goodbye::Goodbye,
    packet::Packet,
    payload_feedbacks::{
        full_intra_request::FullIntraRequest, picture_loss_indication::PictureLossIndication,
    },
};

#[derive(Default)]
pub(super) struct Feedback {
    primary: Option<u32>,
    fir: HashMap<u32, (u8, Instant)>,
    requested: Option<Instant>,
}
impl Feedback {
    pub fn accept(&mut self, packet: &dyn Packet, primary: u32, now: Instant) -> bool {
        if self.primary != Some(primary) {
            self.primary = Some(primary);
            self.fir.clear();
            self.requested = None;
        }
        if let Some(bye) = packet.as_any().downcast_ref::<Goodbye>() {
            // T5239DA removes the departing feedback sender's FIR sequence.
            for sender in &bye.sources {
                self.fir.remove(sender);
            }
            return false;
        }
        let valid = if let Some(pli) = packet.as_any().downcast_ref::<PictureLossIndication>() {
            pli.media_ssrc == primary
        } else if let Some(fir) = packet.as_any().downcast_ref::<FullIntraRequest>() {
            let mut valid = false;
            // T52430A updates FIR history before T34CF72's request gate, even
            // when that later gate rejects this request as too early.
            for entry in &fir.fir {
                if entry.ssrc != primary {
                    continue;
                }
                if self
                    .fir
                    .get(&fir.sender_ssrc)
                    .is_some_and(|(sequence, at)| {
                        *sequence == entry.sequence_number
                            || now.saturating_duration_since(*at) < Duration::from_millis(17)
                    })
                {
                    continue;
                }
                self.fir
                    .insert(fir.sender_ssrc, (entry.sequence_number, now));
                valid = true;
            }
            valid
        } else {
            false
        };
        if !valid
            || self
                .requested
                .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_millis(600))
        {
            return false;
        }
        self.requested = Some(now);
        true
    }
}
