//! Temporary click rings, separate from the following pointer and drawing history.
use super::*;

const LIFETIME: Duration = Duration::from_millis(450);
const MAX_CLICKS: usize = 4;

pub(super) struct ClickPulse {
    owner: u64,
    stroke: Stroke,
    center: Point,
    radii: Point,
    style: Style,
    started: Instant,
    step: u8,
    shown: bool,
    cancelled: bool,
}
pub(super) fn stop(a: &mut Annotation, owner: u64) {
    for pulse in &mut a.clicks {
        if pulse.owner == owner {
            pulse.cancelled = true;
        }
    }
}
pub(super) fn retire(a: &mut Annotation) {
    a.clicks.retain(|pulse| {
        !(pulse.cancelled
            && !pulse.shown
            && !a
                .pending
                .values()
                .any(|p| matches!(p, Pending::Click(id, _) if *id == pulse.stroke.id)))
    });
}
fn update(pulse: &mut ClickPulse, now: Instant) -> bool {
    let age = now.saturating_duration_since(pulse.started);
    if pulse.cancelled || age >= LIFETIME {
        pulse.cancelled = true;
        return false;
    }
    let step = ((age.as_secs_f32() / LIFETIME.as_secs_f32()) * 8.).floor() as u8;
    if pulse.step == step {
        return false;
    }
    pulse.step = step;
    let t = step as f32 / 8.;
    let radius = 0.35 + 0.65 * t;
    pulse.stroke.points = (0..=40)
        .map(|index| {
            let angle = index as f32 / 40. * std::f32::consts::TAU;
            Point {
                x: (pulse.center.x + angle.cos() * pulse.radii.x * radius).clamp(0., 1.),
                y: (pulse.center.y + angle.sin() * pulse.radii.y * radius).clamp(0., 1.),
            }
        })
        .collect();
    let alpha = (((pulse.style.argb >> 24) as f32 * (1. - t)).round() as u32).max(1);
    pulse.stroke.style = Style {
        argb: (pulse.style.argb & 0x00ff_ffff) | (alpha << 24),
        ..pulse.style
    };
    true
}
impl StreamControlHandle {
    pub(crate) fn annotation_pointer_click(
        &self,
        owner: u64,
        screen: i32,
        center: Point,
        radii: Point,
        style: Style,
    ) -> Result<()> {
        if !center.valid()
            || !radii.valid()
            || radii.x <= 0.
            || radii.y <= 0.
            || !style.width.is_finite()
            || !(1.0..=64.0).contains(&style.width)
        {
            bail!("点击指示参数无效");
        }
        let mut s = lock(&self.shared);
        ensure_ready(&s)?;
        if !s.annotation.enabled
            || s.annotation.uncertain
            || s.annotation.toggling()
            || !s.annotation.live_shape.as_ref().is_some_and(|p| {
                p.pointer && p.owner == owner && p.stroke.screen == screen && p.finish.is_none()
            })
        {
            return Ok(());
        }
        retire(&mut s.annotation);
        if s.annotation.clicks.len() >= MAX_CLICKS {
            return Ok(());
        }
        let id = s.annotation.next_id;
        s.annotation.next_id = id.checked_add(1).ok_or_else(|| anyhow!("请重新开启批注"))?;
        s.annotation.clicks.push(ClickPulse {
            owner,
            stroke: Stroke {
                id,
                screen,
                points: Vec::new(),
                style,
            },
            center,
            radii,
            style,
            started: Instant::now(),
            step: u8::MAX,
            shown: false,
            cancelled: false,
        });
        self.drive_clicks(&mut s, Instant::now());
        Ok(())
    }
    pub(super) fn drive_clicks(&self, s: &mut StreamControlState, now: Instant) {
        for index in 0..s.annotation.clicks.len() {
            let pulse = &s.annotation.clicks[index];
            let id = pulse.stroke.id;
            if s.annotation
                .pending
                .values()
                .any(|p| matches!(p,Pending::Click(pending,_) if *pending==id))
            {
                continue;
            }
            let valid_screen = s
                .screens
                .iter()
                .any(|screen| screen.id == pulse.stroke.screen);
            let pulse = &mut s.annotation.clicks[index];
            if !valid_screen {
                pulse.cancelled = true;
            }
            let changed = update(pulse, now);
            if !changed && !pulse.cancelled {
                continue;
            }
            let clear = pulse
                .shown
                .then(|| clear_request(2, id, Some(pulse.stroke.screen)));
            let draw =
                (!pulse.cancelled).then(|| stroke_request(&pulse.stroke, &pulse.stroke.points));
            let shown = !pulse.cancelled;
            for (request, kind) in clear
                .into_iter()
                .map(|r| (r, 2))
                .chain(draw.into_iter().map(|r| (r, 1)))
            {
                if let Err(e) = self.send_draw(s, request, Pending::Click(id, kind)) {
                    s.annotation.uncertain(e.to_string());
                    return;
                }
            }
            s.annotation.clicks[index].shown = shown;
        }
        retire(&mut s.annotation);
    }
}
