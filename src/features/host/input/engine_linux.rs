//! The Linux input engine: the same ownership contract as the Windows engine
//! (every down it sends is released on loss, interruption or disconnect) on
//! top of the X Test injector.
use super::geometry::Geometry;
use super::wire::{Event, KeyOrButton};
use crate::platform::linux::input::{self as native, Injector, Lock};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

/// The UU wire button mask values, in the order X numbers them.
const BUTTONS: [(u8, u8, u8); 5] = [
    // (wire value, held bit, X button)
    (1, 1, 1),
    (2, 2, 3),
    (16, 4, 2),
    (32, 8, 8),
    (64, 16, 9),
];

pub(crate) struct Engine {
    injector: Injector,
    keys: BTreeSet<u16>,
    /// Keys released by an interruption, owed nothing further but still
    /// counted as the controller's until it lifts them.
    parked: BTreeSet<u16>,
    buttons: u8,
    touch: Option<u32>,
    wheel: [i32; 2],
    geometry: Geometry,
    last_heartbeat: Instant,
    last_input: Instant,
    last_key: Instant,
    interruptible: bool,
    interrupted: bool,
    backspace: bool,
    threshold: Duration,
    mobile: bool,
    configuration: super::config::Configuration,
    foreground: Option<Option<u32>>,
    mouse_policy: super::config::MousePolicy,
}

impl Engine {
    /// `hardware`, `origin` and `privileged` describe a Windows session; the X
    /// session injector is the same for every caller.
    pub fn new(
        _hardware: bool,
        policy: super::wire::Policy,
        _origin: u32,
        _privileged: bool,
    ) -> Result<Self> {
        let now = Instant::now();
        Ok(Self {
            injector: Injector::open()?,
            keys: BTreeSet::new(),
            parked: BTreeSet::new(),
            buttons: 0,
            touch: None,
            wheel: [0; 2],
            geometry: Default::default(),
            last_heartbeat: now,
            last_input: now,
            last_key: now,
            interruptible: false,
            interrupted: false,
            backspace: false,
            threshold: Duration::from_millis(if matches!(policy, super::wire::Policy::Windows) {
                150
            } else {
                300
            }),
            mobile: matches!(policy, super::wire::Policy::Mobile),
            configuration: Default::default(),
            foreground: None,
            mouse_policy: Default::default(),
        })
    }
    pub fn backend(&self) -> &'static str {
        "X11 XTest · 当前用户"
    }
    /// Ctrl+Alt+Del is an ordinary key chord to an X session, so nothing is
    /// ever withheld for a secure-attention path.
    pub fn take_sas(&mut self) -> bool {
        false
    }
    pub fn configure(&mut self, configuration: super::config::Configuration) -> Result<()> {
        configuration.validate()?;
        self.configuration = configuration;
        self.foreground = None;
        if !self.configuration.keyboard_interrupts {
            self.interruptible = false;
        }
        self.observe_foreground();
        Ok(())
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        self.mouse_policy.clone()
    }
    /// Apply the server's per-application mouse rule to the focused program,
    /// matched by executable name as on Windows.
    fn observe_foreground(&mut self) {
        let pid = self.injector.foreground();
        if self.foreground == Some(pid) {
            return;
        }
        self.foreground = Some(pid);
        let name = pid
            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok())
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
            });
        let mode = name
            .as_ref()
            .and_then(|name| self.configuration.games.get(name))
            .map_or(2, |rule| rule.mode);
        if self.mouse_policy.mode.unwrap_or(2) == mode {
            return;
        }
        let mut restore = None;
        if mode == 2
            && let Ok(screen) = self.geometry.screen(None)
            && let Ok(rect) = screen.rect()
            && let Ok((x, y)) = self.injector.pointer()
            && (rect.left..rect.right).contains(&x)
            && (rect.top..rect.bottom).contains(&y)
        {
            restore = Some([
                f64::from(x - rect.left) / f64::from(screen.width),
                f64::from(y - rect.top) / f64::from(screen.height),
            ]);
        }
        self.mouse_policy = super::config::MousePolicy {
            mode: Some(mode),
            restore,
        };
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        if geometry != self.geometry {
            self.release()?;
            self.geometry = geometry;
        }
        self.observe_foreground();
        Ok(())
    }
    fn key(&mut self, key: u16, down: bool) -> Result<()> {
        if down {
            self.injector.key(key, true)?;
            self.parked.remove(&key);
            self.keys.insert(key);
        } else if self.keys.remove(&key) {
            if let Err(error) = self.injector.key(key, false) {
                self.keys.insert(key);
                return Err(error);
            }
        } else {
            self.parked.remove(&key);
        }
        Ok(())
    }
    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let (_, bit, native) = BUTTONS
            .into_iter()
            .find(|(wire, _, _)| *wire == button)
            .ok_or_else(|| anyhow::anyhow!("无效鼠标按键"))?;
        if !down && self.buttons & bit == 0 {
            return Ok(());
        }
        self.injector.button(native, down)?;
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        Ok(())
    }
    fn absolute(&mut self, screen: Option<i32>, x: f64, y: f64) -> Result<()> {
        let r = self.geometry.screen(screen)?.rect()?;
        let px = (f64::from(r.left) + f64::from(r.right - r.left) * x)
            .round()
            .clamp(f64::from(r.left), f64::from(r.right - 1));
        let py = (f64::from(r.top) + f64::from(r.bottom - r.top) * y)
            .round()
            .clamp(f64::from(r.top), f64::from(r.bottom - 1));
        self.injector.move_to(px as i32, py as i32)
    }
    fn wheel(&mut self, horizontal: bool, delta: i32) -> Result<()> {
        // 120 units per notch as on Windows. X wheels only click, so partial
        // (touchpad) increments accumulate until they make a whole notch.
        let slot = &mut self.wheel[usize::from(horizontal)];
        if (*slot > 0) != (delta > 0) {
            *slot = 0;
        }
        *slot = slot.saturating_add(delta);
        while slot.unsigned_abs() >= 120 {
            let positive = *slot > 0;
            self.injector.wheel_notch(horizontal, positive)?;
            *slot -= if positive { 120 } else { -120 };
        }
        Ok(())
    }
    /// Emulated touch: X Test has no touch devices, so the primary contact
    /// drives the pointer with the left button, as a touchscreen's pointer
    /// emulation would. Further contacts are not delivered.
    fn touch(&mut self, kind: u8, points: &[super::wire::Point]) -> Result<()> {
        if kind == 1 || kind >= 3 {
            if self.touch.take().is_some() {
                self.button(1, false)?;
            }
            if kind >= 3 {
                return Ok(());
            }
        }
        let primary = match self.touch {
            Some(id) => points.iter().find(|p| p.id == id),
            None => points.first(),
        };
        let Some(point) = primary else {
            if self.touch.take().is_some() {
                self.button(1, false)?;
            }
            return Ok(());
        };
        self.absolute(None, point.x, point.y)?;
        if self.touch.is_none() {
            self.button(1, true)?;
            self.touch = Some(point.id);
        }
        Ok(())
    }
    fn chord(&mut self, keys: &[u16], permitted: &impl Fn() -> bool) -> Result<()> {
        for &key in keys {
            ensure!(permitted(), "快捷输入已撤销");
            self.key(key, true)?;
        }
        for &key in keys.iter().rev() {
            self.key(key, false)?;
        }
        Ok(())
    }
    pub fn apply(
        &mut self,
        event: Event,
        geometry: Geometry,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        let result = self.apply_inner(event, geometry, &permitted);
        let flushed = self.injector.flush();
        result.and(flushed)
    }
    fn apply_inner(
        &mut self,
        event: Event,
        geometry: Geometry,
        permitted: &impl Fn() -> bool,
    ) -> Result<()> {
        ensure!(permitted(), "被控输入许可已撤销");
        self.synchronize(geometry)?;
        ensure!(permitted(), "被控输入许可已撤销");
        if matches!(event, Event::Heartbeat) {
            self.last_heartbeat = Instant::now();
            return Ok(());
        }
        self.last_input = Instant::now();
        match event {
            Event::Command(command) => {
                use super::wire::Command;
                self.release()?;
                ensure!(permitted(), "被控输入许可已撤销");
                match command {
                    // Show the desktop / the window overview with the Super
                    // shortcuts GNOME and KDE bind by default.
                    Command::Desktop => self.chord(&[0x5b, 0x44], permitted)?,
                    Command::TaskView => self.chord(&[0x5b], permitted)?,
                    Command::TaskManager => native::system_monitor()?,
                    Command::Lock => native::lock_session()?,
                }
            }
            Event::Heartbeat => {}
            Event::Key {
                key,
                down,
                interruptible,
                toggle,
            } => {
                if down && key == 0x4c && [0x5b, 0x5c].iter().any(|k| self.keys.contains(k)) {
                    // Win+L locks the session on Windows; do the same here.
                    self.release()?;
                    ensure!(permitted(), "被控输入许可已撤销");
                    return native::lock_session();
                }
                self.last_key = self.last_input;
                self.interruptible = interruptible && self.configuration.keyboard_interrupts;
                self.interrupted = false;
                self.backspace = key == 8 && down;
                match (toggle, Lock::from_virtual_key(key)) {
                    (Some(wanted), Some(lock)) => {
                        if !down && self.injector.toggled(lock)? != wanted {
                            self.key(key, true)?;
                            self.key(key, false)?;
                        }
                    }
                    _ => self.key(key, down)?,
                }
            }
            Event::Button { button, down } => self.button(button, down)?,
            Event::Click { button } => {
                self.button(button, true)?;
                self.button(button, false)?;
            }
            Event::Relative { x, y } => {
                ensure!(
                    x.unsigned_abs() <= 1_000_000 && y.unsigned_abs() <= 1_000_000,
                    "相对位移过大"
                );
                self.injector.move_by(x, y)?;
            }
            Event::RelativeScaled { screen, x, y } => {
                let s = self.geometry.screen(screen)?;
                self.injector.move_by(
                    (f64::from(s.width) * x).round() as i32,
                    (f64::from(s.height) * y).round() as i32,
                )?;
            }
            Event::Absolute { screen, x, y } => self.absolute(screen, x, y)?,
            Event::Wheel { x, y } => {
                let (horizontal, delta) = if x.unsigned_abs() > y.unsigned_abs() {
                    (true, x)
                } else {
                    (false, y)
                };
                if delta != 0 {
                    self.wheel(horizontal, delta)?;
                }
            }
            Event::Text(text) => self.injector.text(&text, permitted)?,
            Event::Combination { down, keys } => {
                for item in keys {
                    ensure!(permitted(), "组合输入已取消");
                    match item {
                        KeyOrButton::Key(k) => self.key(k, down)?,
                        KeyOrButton::Button(b) => self.button(b, down)?,
                    }
                }
            }
            Event::Touch { kind, points } => self.touch(kind, &points)?,
        }
        Ok(())
    }
    pub fn tick(&mut self) -> Result<()> {
        self.synchronize(self.geometry.clone())?;
        if self.interruptible && !self.interrupted && !self.keys.is_empty() {
            let heartbeat = self.last_heartbeat.elapsed();
            let timeout = heartbeat > self.threshold && self.last_input <= self.last_heartbeat;
            if timeout
                || (self.backspace
                    && heartbeat > Duration::from_millis(100)
                    && self.last_key.elapsed() > Duration::from_millis(100))
            {
                self.neutral_keys(self.backspace || self.mobile)?;
                self.interrupted = true;
            }
        }
        self.injector.flush()
    }
    pub fn release(&mut self) -> Result<()> {
        let mut failure = self.neutral_keys(true).err();
        for (button, bit, _) in BUTTONS {
            if self.buttons & bit != 0
                && let Err(e) = self.button(button, false)
            {
                failure = Some(e);
            }
        }
        self.touch = None;
        self.wheel = [0; 2];
        self.interruptible = false;
        self.backspace = false;
        let flushed = self.injector.flush();
        match failure {
            Some(e) => Err(e),
            None => flushed,
        }
    }
    /// Lift every held key. Unless `clear`, the keys stay parked as the
    /// controller's, so its eventual release is absorbed instead of repeated.
    fn neutral_keys(&mut self, clear: bool) -> Result<()> {
        let mut failure = None;
        for key in self.keys.clone() {
            match self.key(key, false) {
                Ok(()) if !clear => {
                    self.parked.insert(key);
                }
                Ok(()) => {}
                Err(e) => failure = Some(e),
            }
        }
        if clear {
            self.parked.clear();
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
