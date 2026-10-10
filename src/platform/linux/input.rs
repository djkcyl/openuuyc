//! Controlled-session input injection on Linux.
//!
//! Events are injected through the X Test extension, which needs no device
//! permissions inside the user's own Xorg session. Keys travel by physical
//! position: a Windows virtual-key code is mapped to its evdev scan position
//! and from there to the X keycode (evdev + 8 under the evdev and libinput
//! drivers), so the local layout decides the character, as it would for a
//! keyboard plugged into this machine.
//!
//! Sunshine injects through uinput instead, which also reaches Wayland
//! sessions and the login screen but needs write access to `/dev/uinput`
//! (a udev rule). That backend is the planned second injector behind the same
//! calls; this one covers Xorg sessions without any setup.
use anyhow::{Context, Result, bail, ensure};
use std::time::Duration;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Keycode, Keysym, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;
const NO_SYMBOL: Keysym = 0;
/// The pause after each typed character.
const TEXT_PACE: Duration = Duration::from_millis(3);

/// Lock keys whose state a controller can ask to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
    Caps,
    Num,
    Scroll,
}
impl Lock {
    pub fn from_virtual_key(key: u16) -> Option<Self> {
        match key {
            0x14 => Some(Self::Caps),
            0x90 => Some(Self::Num),
            0x91 => Some(Self::Scroll),
            _ => None,
        }
    }
}

pub(crate) struct Injector {
    connection: RustConnection,
    root: Window,
    min_keycode: Keycode,
    max_keycode: Keycode,
}

impl Injector {
    pub fn open() -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
        ensure!(
            connection
                .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
                .is_some(),
            "X 服务器没有 XTEST 扩展，无法注入输入"
        );
        connection.xtest_get_version(2, 2)?.reply()?;
        let setup = connection.setup();
        let root = setup.roots[screen].root;
        let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);
        Ok(Self {
            connection,
            root,
            min_keycode,
            max_keycode,
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?
            .check()
            .context("注入输入事件失败")?;
        Ok(())
    }

    /// A key by Windows virtual-key code.
    pub fn key(&self, key: u16, down: bool) -> Result<()> {
        let code = evdev(key).with_context(|| format!("不支持的按键 {key:#04x}"))?;
        let keycode = u8::try_from(code + 8).context("按键超出 X11 键码范围")?;
        ensure!(
            (self.min_keycode..=self.max_keycode).contains(&keycode),
            "按键超出 X 服务器键码范围"
        );
        self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
    }

    /// A button by X button number (1 left, 2 middle, 3 right, 8/9 side).
    pub fn button(&self, button: u8, down: bool) -> Result<()> {
        self.fake(
            if down { BUTTON_PRESS } else { BUTTON_RELEASE },
            button,
            0,
            0,
        )
    }

    /// Move to a root-window position.
    pub fn move_to(&self, x: i32, y: i32) -> Result<()> {
        self.fake(
            MOTION_NOTIFY,
            0,
            i16::try_from(x).context("指针坐标超出 X11 范围")?,
            i16::try_from(y).context("指针坐标超出 X11 范围")?,
        )
    }

    /// Move by a pixel delta from wherever the pointer is.
    pub fn move_by(&self, mut dx: i32, mut dy: i32) -> Result<()> {
        while dx != 0 || dy != 0 {
            let x = dx.clamp(i16::MIN.into(), i16::MAX.into());
            let y = dy.clamp(i16::MIN.into(), i16::MAX.into());
            self.fake(MOTION_NOTIFY, 1, x as i16, y as i16)?;
            dx -= x;
            dy -= y;
        }
        Ok(())
    }

    /// One wheel notch: X delivers wheels as buttons 4-7.
    pub fn wheel_notch(&self, horizontal: bool, positive: bool) -> Result<()> {
        // Windows: positive vertical is away from the user (up), positive
        // horizontal is to the right.
        let button = match (horizontal, positive) {
            (false, true) => 4,
            (false, false) => 5,
            (true, false) => 6,
            (true, true) => 7,
        };
        self.button(button, true)?;
        self.button(button, false)
    }

    pub fn toggled(&self, lock: Lock) -> Result<bool> {
        let leds = self.connection.get_keyboard_control()?.reply()?.led_mask;
        // Core LEDs 1-3 are Caps, Num and Scroll Lock on every X keyboard map.
        Ok(leds
            & match lock {
                Lock::Caps => 1,
                Lock::Num => 2,
                Lock::Scroll => 4,
            }
            != 0)
    }

    pub fn pointer(&self) -> Result<(i32, i32)> {
        let reply = self.connection.query_pointer(self.root)?.reply()?;
        Ok((reply.root_x.into(), reply.root_y.into()))
    }

    /// Type text independently of the key layout, the way Windows' Unicode
    /// keyboard input does: each character's keysym is found in the current
    /// map, or bound for the moment to a spare keycode.
    ///
    /// Unicode input on Windows ignores the keys being held, so modifiers the
    /// controller still holds (the Ctrl of the Ctrl+V that asked for a paste)
    /// are released while typing and pressed again afterwards, and Caps Lock
    /// does not change the case of a letter. A Windows line break (CR LF)
    /// is one Enter.
    pub fn text(&self, text: &str, permitted: impl Fn() -> bool) -> Result<()> {
        let count = self.max_keycode - self.min_keycode + 1;
        let map = self
            .connection
            .get_keyboard_mapping(self.min_keycode, count)?
            .reply()?;
        let per = usize::from(map.keysyms_per_keycode);
        ensure!(per > 0, "键盘映射无效");
        let find = |sym: Keysym| -> Option<(Keycode, bool)> {
            map.keysyms.chunks(per).enumerate().find_map(|(i, syms)| {
                let code = self.min_keycode + i as u8;
                if syms.first() == Some(&sym) {
                    Some((code, false))
                } else if syms.get(1) == Some(&sym) {
                    Some((code, true))
                } else {
                    None
                }
            })
        };
        let spare = map
            .keysyms
            .chunks(per)
            .rposition(|syms| syms.iter().all(|s| *s == NO_SYMBOL))
            .map(|i| self.min_keycode + i as u8);
        let shift = evdev(0xa0)
            .map(|code| (code + 8) as u8)
            .context("缺少 Shift 键")?;
        let caps = self.toggled(Lock::Caps).unwrap_or(false);
        let held = self.held_modifiers()?;
        for code in &held {
            self.fake(KEY_RELEASE, *code, 0, 0)?;
        }
        // Caps Lock changes the case of letters, including those bound to the
        // spare keycode, so it is off while typing.
        if caps {
            self.tap(0x14)?;
        }
        let mut bound = false;
        let result = (|| -> Result<()> {
            let mut previous = None;
            for c in text.chars() {
                ensure!(permitted(), "文字输入已取消");
                let after_cr = previous == Some('\r');
                previous = Some(c);
                if c == '\n' && after_cr {
                    continue;
                }
                let sym = keysym(c);
                let (code, shifted) = match find(sym) {
                    Some(found) => found,
                    None => {
                        let spare = spare.context("没有空闲键码可用于输入该字符")?;
                        let syms = vec![sym; per];
                        self.connection
                            .change_keyboard_mapping(1, spare, per as u8, &syms)?
                            .check()?;
                        bound = true;
                        // Clients refresh their keymap on MappingNotify; give
                        // the focused one a moment before the key arrives.
                        self.connection.get_input_focus()?.reply()?;
                        std::thread::sleep(Duration::from_millis(12));
                        (spare, false)
                    }
                };
                if shifted {
                    self.fake(KEY_PRESS, shift, 0, 0)?;
                }
                let typed = self
                    .fake(KEY_PRESS, code, 0, 0)
                    .and_then(|()| self.fake(KEY_RELEASE, code, 0, 0));
                if shifted {
                    self.fake(KEY_RELEASE, shift, 0, 0)?;
                }
                typed?;
                // Input methods (IBus) pass each key through another process
                // and drop keys that arrive faster than they are handled.
                std::thread::sleep(if bound {
                    Duration::from_millis(12)
                } else {
                    TEXT_PACE
                });
            }
            Ok(())
        })();
        if bound && let Some(spare) = spare {
            let empty = vec![NO_SYMBOL; per];
            let _ = self
                .connection
                .change_keyboard_mapping(1, spare, per as u8, &empty);
            let _ = self.connection.flush();
        }
        if caps {
            let _ = self.tap(0x14);
        }
        for code in &held {
            let _ = self.fake(KEY_PRESS, *code, 0, 0);
        }
        result
    }

    /// Press and release a key by Windows virtual-key code.
    fn tap(&self, key: u16) -> Result<()> {
        self.key(key, true)?;
        self.key(key, false)
    }

    /// Modifier keys other than the lock keys that are down right now.
    fn held_modifiers(&self) -> Result<Vec<Keycode>> {
        let mapping = self.connection.get_modifier_mapping()?.reply()?;
        let keys = self.connection.query_keymap()?.reply()?.keys;
        let per = usize::from(mapping.keycodes_per_modifier()).max(1);
        let mut held = Vec::new();
        for (index, codes) in mapping.keycodes.chunks(per).enumerate() {
            // Lock (index 1) is a toggle: pressing it again would flip it.
            if index == 1 {
                continue;
            }
            for &code in codes {
                if code != 0
                    && keys[usize::from(code / 8)] & (1 << (code % 8)) != 0
                    && !held.contains(&code)
                {
                    held.push(code);
                }
            }
        }
        Ok(held)
    }

    /// The process that owns the active window, as the window manager reports.
    pub fn foreground(&self) -> Option<u32> {
        let atom = |name: &str| {
            self.connection
                .intern_atom(true, name.as_bytes())
                .ok()?
                .reply()
                .ok()
                .map(|r| r.atom)
                .filter(|atom| *atom != 0)
        };
        let active = atom("_NET_ACTIVE_WINDOW")?;
        let pid = atom("_NET_WM_PID")?;
        let window = self
            .connection
            .get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()?;
        self.connection
            .get_property(false, window, pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
    }

    pub fn flush(&self) -> Result<()> {
        self.connection.flush()?;
        Ok(())
    }
}

/// Lock the session through logind, as Win+L does on Windows.
pub(crate) fn lock_session() -> Result<()> {
    let status = std::process::Command::new("loginctl")
        .arg("lock-session")
        .status()
        .context("无法调用 loginctl 锁定会话")?;
    ensure!(status.success(), "锁定会话失败：{status}");
    Ok(())
}

/// Open the desktop's system monitor, the counterpart of Task Manager.
pub(crate) fn system_monitor() -> Result<()> {
    for program in [
        "gnome-system-monitor",
        "plasma-systemmonitor",
        "ksysguard",
        "mate-system-monitor",
        "xfce4-taskmanager",
    ] {
        match std::process::Command::new(program)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("没有找到系统监视器程序")
}

fn keysym(c: char) -> Keysym {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        // Latin-1 keysyms equal their code points; everything else uses the
        // Unicode keysym range.
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 | c as u32,
    }
}

/// Windows virtual-key code to evdev key code, by physical position (US
/// names). The same table as the viewer's winit mapping, read backwards.
pub(crate) fn evdev(key: u16) -> Option<u16> {
    Some(match key {
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0d => 28,  // Enter
        0x10 => 42,  // Shift
        0x11 => 29,  // Control
        0x12 => 56,  // Alt
        0x13 => 119, // Pause
        0x14 => 58,  // Caps Lock
        0x15 => 122, // Kana / Hangul
        0x19 => 123, // Hanja
        0x1b => 1,   // Escape
        0x1c => 92,  // Convert (Henkan)
        0x1d => 94,  // NonConvert (Muhenkan)
        0x20 => 57,  // Space
        0x21 => 104, // Page Up
        0x22 => 109, // Page Down
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2c => 99,  // Print Screen
        0x2d => 110, // Insert
        0x2e => 111, // Delete
        0x2f => 138, // Help
        0x30 => 11,
        0x31..=0x39 => key - 0x31 + 2,
        0x41 => 30,
        0x42 => 48,
        0x43 => 46,
        0x44 => 32,
        0x45 => 18,
        0x46 => 33,
        0x47 => 34,
        0x48 => 35,
        0x49 => 23,
        0x4a => 36,
        0x4b => 37,
        0x4c => 38,
        0x4d => 50,
        0x4e => 49,
        0x4f => 24,
        0x50 => 25,
        0x51 => 16,
        0x52 => 19,
        0x53 => 31,
        0x54 => 20,
        0x55 => 22,
        0x56 => 47,
        0x57 => 17,
        0x58 => 45,
        0x59 => 21,
        0x5a => 44,
        0x5b => 125, // Left Windows / Super
        0x5c => 126, // Right Windows / Super
        0x5d => 127, // Menu
        0x5f => 142, // Sleep
        0x60 => 82,  // Numpad 0
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73,                      // Numpad 9
        0x6a => 55,                      // Numpad *
        0x6b => 78,                      // Numpad +
        0x6c => 121,                     // Numpad separator
        0x6d => 74,                      // Numpad -
        0x6e => 83,                      // Numpad .
        0x6f => 98,                      // Numpad /
        0x70..=0x79 => key - 0x70 + 59,  // F1-F10
        0x7a => 87,                      // F11
        0x7b => 88,                      // F12
        0x7c..=0x87 => key - 0x7c + 183, // F13-F24
        0x90 => 69,                      // Num Lock
        0x91 => 70,                      // Scroll Lock
        0xa0 => 42,                      // Left Shift
        0xa1 => 54,                      // Right Shift
        0xa2 => 29,                      // Left Control
        0xa3 => 97,                      // Right Control
        0xa4 => 56,                      // Left Alt
        0xa5 => 100,                     // Right Alt
        0xa6 => 158,                     // Browser Back
        0xa7 => 159,                     // Browser Forward
        0xa8 => 173,                     // Browser Refresh
        0xa9 => 128,                     // Browser Stop
        0xaa => 217,                     // Browser Search
        0xab => 156,                     // Browser Favorites
        0xac => 172,                     // Browser Home
        0xad => 113,                     // Volume Mute
        0xae => 114,                     // Volume Down
        0xaf => 115,                     // Volume Up
        0xb0 => 163,                     // Next Track
        0xb1 => 165,                     // Previous Track
        0xb2 => 166,                     // Stop
        0xb3 => 164,                     // Play/Pause
        0xb4 => 155,                     // Mail
        0xb5 => 226,                     // Media Select
        0xb6 => 157,                     // Launch App 1 (Computer)
        0xb7 => 140,                     // Launch App 2 (Calculator)
        0xba => 39,                      // ;
        0xbb => 13,                      // =
        0xbc => 51,                      // ,
        0xbd => 12,                      // -
        0xbe => 52,                      // .
        0xbf => 53,                      // /
        0xc0 => 41,                      // `
        0xc1 => 89,                      // Ro
        0xc2 => 121,                     // ABNT C2 / Numpad ,
        0xdb => 26,                      // [
        0xdc => 43,                      // \
        0xdd => 27,                      // ]
        0xde => 40,                      // '
        0xe2 => 86,                      // 102nd key
        0xff => 124,                     // Yen
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::evdev;

    #[test]
    fn letters_and_digits_follow_the_us_rows() {
        assert_eq!(evdev(b'Q'.into()), Some(16));
        assert_eq!(evdev(b'A'.into()), Some(30));
        assert_eq!(evdev(b'Z'.into()), Some(44));
        assert_eq!(evdev(b'1'.into()), Some(2));
        assert_eq!(evdev(b'9'.into()), Some(10));
        assert_eq!(evdev(0x70), Some(59));
        assert_eq!(evdev(0x79), Some(68));
        assert_eq!(evdev(0x7c), Some(183));
    }
}

#[cfg(test)]
mod session_tests {
    /// Needs a running Xorg session: `DISPLAY=:0 cargo test -- --ignored`.
    /// Moves the pointer and puts it back.
    #[test]
    #[ignore]
    fn pointer_moves_where_asked() {
        let injector = super::Injector::open().unwrap();
        let before = injector.pointer().unwrap();
        injector.move_to(123, 234).unwrap();
        injector.flush().unwrap();
        assert_eq!(injector.pointer().unwrap(), (123, 234));
        injector.move_by(10, -4).unwrap();
        assert_eq!(injector.pointer().unwrap(), (133, 230));
        println!(
            "caps lock on: {}",
            injector.toggled(super::Lock::Caps).unwrap()
        );
        println!("foreground pid: {:?}", injector.foreground());
        injector.move_to(before.0, before.1).unwrap();
        injector.flush().unwrap();
    }

    /// Types OPENUUYC_TYPE_TEXT into whatever has focus on DISPLAY.
    #[test]
    #[ignore = "types into the focused window of an X display"]
    fn types_text() {
        let text = std::env::var("OPENUUYC_TYPE_TEXT").unwrap();
        super::Injector::open()
            .unwrap()
            .text(&text, || true)
            .unwrap();
    }
}
