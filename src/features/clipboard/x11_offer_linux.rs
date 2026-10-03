//! Owning the X11 clipboard for a file copy.
//!
//! A file copy is not one payload but several descriptions of the same thing,
//! and which one a paste reads depends on the application: GTK's file managers
//! take `x-special/gnome-copied-files`, because it also says whether the copy
//! was a cut, while most everything else reads `text/uri-list` or falls back to
//! the paths as plain text. Only the owner of a selection can answer for more
//! than one target, and the clipboard library used for text and images offers
//! `text/uri-list` alone, so this takes the selection itself.
//!
//! Ownership passes back the moment anything else claims the clipboard,
//! including this process putting text on it: the owner thread is told, and
//! stops.

use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

/// How long the owner thread waits for an event before looking at whether it
/// has been asked to stop.
const POLL: Duration = Duration::from_millis(50);

/// The targets this offers, most specific first, which is the order `TARGETS`
/// should list them in.
const TARGETS: [&str; 6] = [
    "x-special/gnome-copied-files",
    "text/uri-list",
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "STRING",
    "TEXT",
];

/// Percent-encode a path for a `file:` URI.
///
/// Everything outside the unreserved set is escaped, which is stricter than
/// what file managers write -- they leave brackets and parentheses alone -- but
/// anything reading a URI has to decode the escapes anyway.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The bytes each target carries for `paths`.
fn payload(target: &str, paths: &[std::path::PathBuf]) -> Vec<u8> {
    let uris: Vec<String> = paths.iter().map(|path| encode(path)).collect();
    match target {
        // The first line is the operation; a paste that reads this one is
        // deciding between copying and moving.
        "x-special/gnome-copied-files" => format!("copy\n{}", uris.join("\n")).into_bytes(),
        // RFC 2483 delimits this with CRLF, including after the last entry.
        "text/uri-list" => uris
            .iter()
            .map(|uri| format!("{uri}\r\n"))
            .collect::<String>()
            .into_bytes(),
        // Plain text gets the paths themselves: a URI pasted into a terminal
        // or a text box is not what anyone wanted.
        _ => paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes(),
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    /// One per entry in [`TARGETS`], in the same order.
    data: Vec<Atom>,
}

fn intern(connection: &impl Connection, name: &str) -> Result<Atom> {
    Ok(connection
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("intern X11 atom {name}"))?
        .atom)
}

/// A file copy published on the clipboard, owned until this is dropped or
/// something else takes the selection.
pub(super) struct FileOffer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FileOffer {
    /// Take the clipboard and answer for `paths` until told otherwise.
    pub(super) fn publish(paths: Vec<std::path::PathBuf>) -> Result<Self> {
        if paths.is_empty() {
            bail!("没有要发布的文件");
        }
        let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().context("分配 X11 窗口失败")?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()
            .context("创建 X11 剪贴板窗口失败")?;

        let atoms = Atoms {
            clipboard: intern(&connection, "CLIPBOARD")?,
            targets: intern(&connection, "TARGETS")?,
            timestamp: intern(&connection, "TIMESTAMP")?,
            data: TARGETS
                .iter()
                .map(|name| intern(&connection, name))
                .collect::<Result<_>>()?,
        };

        // ICCCM asks for a real timestamp rather than CurrentTime, and the way
        // to get one is to provoke an event that carries it.
        let marker = intern(&connection, "OPENUUYC_TIMESTAMP")?;
        connection.change_property8(PropMode::APPEND, window, marker, AtomEnum::STRING, &[])?;
        connection.flush()?;
        let mut time = x11rb::CURRENT_TIME;
        for _ in 0..100 {
            match connection.poll_for_event()? {
                Some(Event::PropertyNotify(event)) if event.window == window => {
                    time = event.time;
                    break;
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }

        connection
            .set_selection_owner(window, atoms.clipboard, time)?
            .check()
            .context("取得剪贴板所有权失败")?;
        connection.flush()?;
        if connection
            .get_selection_owner(atoms.clipboard)?
            .reply()?
            .owner
            != window
        {
            bail!("剪贴板所有权被其他程序抢占");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("UU clipboard files".into())
            .spawn(move || serve(&connection, window, &atoms, &paths, &worker_stop))
            .context("启动剪贴板所有权线程失败")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for FileOffer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answer selection requests until ownership is lost or the offer is retired.
fn serve(
    connection: &impl Connection,
    window: x11rb::protocol::xproto::Window,
    atoms: &Atoms,
    paths: &[std::path::PathBuf],
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) {
        let event = match connection.poll_for_event() {
            Ok(Some(event)) => event,
            Ok(None) => {
                std::thread::sleep(POLL);
                continue;
            }
            Err(error) => {
                tracing::debug!(%error, "剪贴板所有权连接中断");
                return;
            }
        };
        match event {
            // Something else took the clipboard, which retires this offer.
            Event::SelectionClear(event) if event.owner == window => return,
            Event::SelectionRequest(request) => {
                if let Err(error) = answer(connection, atoms, paths, &request) {
                    tracing::debug!(%error, "回应剪贴板请求失败");
                }
            }
            _ => {}
        }
    }
    // Releasing on the way out lets the next owner take over cleanly.
    let _ = connection.set_selection_owner(x11rb::NONE, atoms.clipboard, x11rb::CURRENT_TIME);
    let _ = connection.flush();
}

fn answer(
    connection: &impl Connection,
    atoms: &Atoms,
    paths: &[std::path::PathBuf],
    request: &SelectionRequestEvent,
) -> Result<()> {
    // A requestor that sent no property is using the obsolete convention; the
    // target doubles as the property in that case.
    let property = if request.property == x11rb::NONE {
        request.target
    } else {
        request.property
    };
    let mut accepted = true;
    if request.target == atoms.targets {
        let mut list = vec![atoms.targets, atoms.timestamp];
        list.extend_from_slice(&atoms.data);
        connection.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::ATOM,
            &list,
        )?;
    } else if request.target == atoms.timestamp {
        connection.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::INTEGER,
            &[request.time],
        )?;
    } else if let Some(index) = atoms.data.iter().position(|atom| *atom == request.target) {
        let data = payload(TARGETS[index], paths);
        connection.change_property8(
            PropMode::REPLACE,
            request.requestor,
            property,
            request.target,
            &data,
        )?;
    } else {
        // Refusing is reported by naming no property, which is what tells the
        // requestor to try a different target.
        accepted = false;
    }
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property: if accepted { property } else { x11rb::NONE },
    };
    connection.send_event(false, request.requestor, EventMask::NO_EVENT, notify)?;
    connection.flush()?;
    Ok(())
}
