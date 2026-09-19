//! Linux clipboard adapter for X11 and Wayland.
//!
//! Differences from the Windows/OLE adapter this replaces:
//! * X11 and Wayland have no delayed rendering across processes, so an offer
//!   from the remote is fetched immediately instead of when the user pastes.
//! * There is no clipboard-change notification, so local changes are polled.
//! * Files copied here are offered to the remote: it pulls the descriptor list
//!   and then the contents by offset, so nothing has to be promised in advance.
//!   The reverse direction still cannot be served, because a local paste needs
//!   real paths on this machine before the pasting application asks for them.
use super::formats::{self, Format};
use super::*;
use anyhow::Context as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

pub(super) use super::formats::safe_name;

/// Anything larger is a transfer, not a clipboard paste.
const MAX_CLIP: usize = 32 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(400);

pub(super) enum Command {
    Activate(Weak<Inner>),
    Remove(u64),
    Offer(Weak<Inner>, u64, Vec<ClipboardFormat>),
    Request(Weak<Inner>, u64, i64, ClipboardRequestKind),
    Text(Weak<Inner>, u64, i64, String),
}

struct Worker {
    sender: SyncSender<Command>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

static WORKER: OnceLock<std::result::Result<Worker, String>> = OnceLock::new();

/// One file the local clipboard offers, described the way a Windows peer reads
/// it: a backslash path relative to the copied root, Windows attribute bits and
/// a FILETIME.
struct LocalFile {
    desc: ClipboardFileDescriptor,
    /// `None` for a directory, which has no contents to serve.
    path: Option<PathBuf>,
    /// The copied item this entry was reached through; contents are refused if
    /// the path stops resolving inside it.
    root: PathBuf,
}

/// A flattened snapshot of everything one local copy put on the clipboard.
struct LocalFiles {
    items: Vec<LocalFile>,
}

/// What this client currently owns locally, as the remote would see it.
#[derive(Default)]
struct LocalSnapshot {
    text: Option<String>,
    image: Option<Vec<u8>>,
    /// The paths a local copy put on the clipboard. They are only walked when
    /// the remote asks for the list, so copying a large tree locally costs
    /// nothing until someone pastes it there.
    sources: Vec<PathBuf>,
}

impl LocalSnapshot {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.image.is_none() && self.sources.is_empty()
    }

    /// CF_UNICODETEXT and CF_DIB are what a Windows peer understands; files
    /// travel as the descriptor and contents pair, as they do in OLE.
    fn format_ids(&self) -> Vec<(u32, String)> {
        let mut ids = Vec::new();
        if !self.sources.is_empty() {
            ids.push((
                formats::register("FileGroupDescriptorW"),
                "FileGroupDescriptorW".into(),
            ));
            ids.push((formats::register("FileContents"), "FileContents".into()));
            return ids;
        }
        if self.text.is_some() {
            ids.push((13, String::new()));
            ids.push((1, String::new()));
        }
        if self.image.is_some() {
            ids.push((8, String::new()));
        }
        ids
    }

    fn data(&self, format: &Format) -> Result<Vec<u8>> {
        match format.local {
            13 => self
                .text
                .as_deref()
                .map(formats::unicode)
                .context("本地剪贴板没有文本"),
            1 => self
                .text
                .as_deref()
                .map(|text| {
                    let mut bytes = text.replace('\n', "\r\n").into_bytes();
                    bytes.push(0);
                    bytes
                })
                .context("本地剪贴板没有文本"),
            8 => self.image.clone().context("本地剪贴板没有图片"),
            _ => bail!("不支持的本地剪贴板格式"),
        }
    }
}

/// Windows file attributes this client synthesises from a Unix mode.
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// Seconds between the Windows epoch (1601-01-01) and the Unix one.
const FILETIME_EPOCH_OFFSET: u64 = 11_644_473_600;

/// A Unix mtime as the FILETIME a Windows peer stamps the pasted file with.
fn filetime(meta: &std::fs::Metadata) -> u64 {
    let seconds = meta.mtime();
    if seconds < -(FILETIME_EPOCH_OFFSET as i64) {
        return 0;
    }
    let ticks = (seconds + FILETIME_EPOCH_OFFSET as i64) as u64;
    ticks
        .saturating_mul(10_000_000)
        .saturating_add(meta.mtime_nsec().max(0) as u64 / 100)
}

fn attributes(meta: &std::fs::Metadata) -> u32 {
    let mut bits = if meta.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    // Owner write is the closest thing this filesystem has to the read-only bit.
    if meta.mode() & 0o200 == 0 {
        bits |= FILE_ATTRIBUTE_READONLY;
    }
    bits
}

/// Walk one copied path into the flat descriptor list the protocol carries.
/// Names are relative to the copied item and use the separator Windows expects.
fn collect_file(path: &Path, root: &Path, name: &str, items: &mut Vec<LocalFile>) -> Result<()> {
    ensure!(
        items.len() < MAX_FILES && safe_name(name),
        "文件数量或名称不支持"
    );
    let meta = std::fs::symlink_metadata(path)?;
    // A symlink is not copied: the peer would receive its target under a name
    // that promises otherwise, and the target may sit outside the copy.
    ensure!(!meta.is_symlink(), "不传输符号链接");
    ensure!(meta.is_dir() || meta.is_file(), "只支持普通文件与目录");
    let canonical = std::fs::canonicalize(path)?;
    ensure!(canonical.starts_with(root), "文件超出复制范围");
    items.push(LocalFile {
        desc: ClipboardFileDescriptor {
            file_name: name.replace('/', "\\"),
            file_attributes: attributes(&meta),
            last_write_time: filetime(&meta),
            file_size: if meta.is_dir() { 0 } else { meta.len() },
        },
        path: (!meta.is_dir()).then(|| canonical.clone()),
        root: root.to_path_buf(),
    });
    if meta.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&canonical)?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|entry| entry.file_name())
            .collect();
        entries.sort();
        for entry in entries {
            collect_file(
                &canonical.join(&entry),
                root,
                &format!("{name}\\{}", entry.to_string_lossy()),
                items,
            )?;
        }
    }
    Ok(())
}

/// Flatten the paths the local clipboard holds. One unreadable entry fails the
/// whole offer rather than handing the peer a list it cannot complete.
fn collect_files(paths: &[PathBuf]) -> Result<LocalFiles> {
    let mut items = Vec::new();
    for path in paths {
        let root = std::fs::canonicalize(path)?;
        let name = root
            .file_name()
            .context("无法确定文件名")?
            .to_string_lossy()
            .into_owned();
        collect_file(&root, &root, &name, &mut items)?;
    }
    ensure!(!items.is_empty(), "文件列表为空");
    Ok(LocalFiles { items })
}

impl LocalFiles {
    /// Serve one `FileContentsRequest`. `flags` is 1 for the size and 2 for a
    /// range of the contents, as the official client sends them.
    fn read(&self, ask: &ClipboardFileContentsRequest) -> Result<Vec<u8>> {
        let file = self
            .items
            .get(ask.list_index as usize)
            .context("无效的文件索引")?;
        ensure!(ask.requested_len as usize <= FILE_BLOCK, "文件读取请求过大");
        if ask.flags == 1 {
            return Ok(file.desc.file_size.to_le_bytes().to_vec());
        }
        ensure!(
            ask.flags == 2 && file.desc.file_attributes & FILE_ATTRIBUTE_DIRECTORY == 0,
            "无效的文件读取类型"
        );
        let path = file.path.as_ref().context("该条目没有内容")?;
        // O_NOFOLLOW closes the window where the final component is swapped for
        // a link between the walk and this read.
        let mut handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        ensure!(
            std::fs::canonicalize(path)?.starts_with(&file.root),
            "文件超出原始复制范围"
        );
        let length =
            (u64::from(ask.requested_len)).min(file.desc.file_size.saturating_sub(ask.pos_offset));
        if length == 0 {
            return Ok(Vec::new());
        }
        use std::io::{Read as _, Seek as _};
        handle.seek(std::io::SeekFrom::Start(ask.pos_offset))?;
        let mut data = vec![0u8; length as usize];
        let mut filled = 0;
        while filled < data.len() {
            match handle.read(&mut data[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        data.truncate(filled);
        Ok(data)
    }
}

struct State {
    receiver: Receiver<Command>,
    clipboard: Option<arboard::Clipboard>,
    sessions: HashMap<u64, Weak<Inner>>,
    published: HashMap<u64, Vec<Format>>,
    local: LocalSnapshot,
    /// File lists handed out per session and task, kept until the clipboard
    /// changes so a slow remote can keep reading the copy it started on.
    tasks: HashMap<(u64, u32), Arc<LocalFiles>>,
    /// Set while this adapter writes, so the poll does not report its own write.
    writing: bool,
}

pub(super) fn start() -> Result<()> {
    WORKER
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(64);
            let thread = std::thread::Builder::new()
                .name("UU clipboard".into())
                .spawn(move || run(receiver))
                .map_err(|error| format!("启动剪贴板线程失败：{error}"))?;
            Ok(Worker {
                sender,
                thread: Mutex::new(Some(thread)),
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow!(error.clone()))
}

pub(super) fn post(command: Command) -> Result<()> {
    start()?;
    let worker = WORKER
        .get()
        .and_then(|worker| worker.as_ref().ok())
        .context("剪贴板线程不可用")?;
    worker
        .sender
        .try_send(command)
        .map_err(|_| anyhow!("剪贴板队列已满或已关闭"))
}

/// The Windows adapter pumps its STA here. This worker is a plain thread, so
/// callers waiting on a response simply keep waiting on their condvar.
pub(super) fn pump() {}

pub(super) fn shutdown() {
    let Some(Ok(worker)) = WORKER.get() else {
        return;
    };
    // Dropping every sender ends the receive loop; the clone here is the last one.
    let thread = lock(&worker.thread).take();
    if let Some(thread) = thread {
        let _ = worker.sender.try_send(Command::Remove(u64::MAX));
        drop(thread);
    }
}

fn run(receiver: Receiver<Command>) {
    let clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            tracing::warn!(%error, "系统剪贴板不可用，剪贴板同步将保持关闭");
            None
        }
    };
    let mut state = State {
        receiver,
        clipboard,
        sessions: HashMap::new(),
        published: HashMap::new(),
        local: LocalSnapshot::default(),
        tasks: HashMap::new(),
        writing: false,
    };
    loop {
        match state.receiver.recv_timeout(POLL) {
            Ok(command) => {
                if matches!(command, Command::Remove(u64::MAX)) {
                    break;
                }
                process(&mut state, command);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = poll_local(&mut state) {
                    tracing::debug!(%error, "读取本地剪贴板失败");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn sessions(state: &State) -> Vec<Arc<Inner>> {
    state
        .sessions
        .values()
        .filter_map(std::sync::Weak::upgrade)
        .collect()
}

fn process(state: &mut State, command: Command) {
    match command {
        Command::Activate(weak) => {
            if let Some(session) = weak.upgrade() {
                state.sessions.insert(session.id, Arc::downgrade(&session));
                tracing::debug!(
                    id = session.id,
                    sessions = state.sessions.len(),
                    files = session.file_allowed(),
                    "剪贴板会话已注册"
                );
            }
        }
        Command::Remove(id) => {
            state.sessions.remove(&id);
            state.published.remove(&id);
        }
        Command::Offer(weak, epoch, formats) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch))
                && let Err(error) = accept_offer(state, &session, epoch, formats)
            {
                session.fail(error.to_string());
            }
        }
        Command::Text(weak, epoch, id, text) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch)) {
                let result = if text.is_empty() {
                    Err(anyhow!("空文本"))
                } else {
                    write_text(state, &text)
                };
                if let Err(error) = &result {
                    tracing::debug!(%error, "写入本地剪贴板文本失败");
                }
                let _ = session.emit(
                    epoch,
                    Envelope {
                        request: None,
                        response: Some(Response {
                            header: Some(Header { id }),
                            clip: None,
                            text: Some(ClipboardTextChangeResponse {
                                err: if result.is_ok() { 1 } else { 2 },
                            }),
                        }),
                    },
                );
            }
        }
        Command::Request(weak, epoch, id, kind) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch))
                && let Err(error) = serve(state, &session, epoch, id, kind)
            {
                session.fail(error.to_string());
            }
        }
    }
}

/// Fetch the best offered format now, because the local clipboard cannot
/// promise data it does not yet hold.
fn accept_offer(
    state: &mut State,
    session: &Arc<Inner>,
    epoch: u64,
    offered: Vec<ClipboardFormat>,
) -> Result<()> {
    if offered.is_empty() {
        return Ok(());
    }
    let platform = session.platform.load(Ordering::Acquire);
    // Files need delayed rendering, so they are never accepted here.
    let links: Vec<Format> = offered
        .into_iter()
        .filter_map(|format| formats::incoming(format, platform, false))
        .collect();
    let Some(format) = [13u32, 1, 8, 17]
        .into_iter()
        .find_map(|id| links.iter().find(|format| format.local == id))
        .cloned()
    else {
        return Ok(());
    };
    let data = session.data(epoch, &format.wire)?;
    ensure!(data.len() <= MAX_CLIP, "剪贴板内容过大");
    let data = formats::convert(data, &format, platform, false)?;
    match format.local {
        13 => {
            let text = utf16_text(&data)?;
            write_text(state, &text)?;
        }
        1 => {
            let text = String::from_utf8_lossy(&data)
                .trim_end_matches('\0')
                .replace("\r\n", "\n");
            write_text(state, &text)?;
        }
        8 | 17 => write_image(state, &data)?,
        _ => return Ok(()),
    }
    *lock(&session.error) = None;
    Ok(())
}

fn utf16_text(bytes: &[u8]) -> Result<String> {
    ensure!(bytes.len().is_multiple_of(2), "无效的Unicode剪贴板");
    let mut words: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    while words.last() == Some(&0) {
        words.pop();
    }
    if words.first() == Some(&0xfeff) {
        words.remove(0);
    }
    Ok(String::from_utf16(&words)?.replace("\r\n", "\n"))
}

fn write_text(state: &mut State, text: &str) -> Result<()> {
    let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
    state.writing = true;
    let result = clipboard.set_text(text.to_owned());
    state.writing = false;
    result.context("写入系统剪贴板失败")?;
    state.local = LocalSnapshot {
        text: Some(text.to_owned()),
        ..Default::default()
    };
    state.tasks.clear();
    Ok(())
}

fn write_image(state: &mut State, dib: &[u8]) -> Result<()> {
    let image = dib_to_rgba(dib)?;
    let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
    state.writing = true;
    let result = clipboard.set_image(arboard::ImageData {
        width: image.0 as usize,
        height: image.1 as usize,
        bytes: std::borrow::Cow::Owned(image.2),
    });
    state.writing = false;
    result.context("写入系统剪贴板失败")?;
    state.local = LocalSnapshot {
        image: Some(dib.to_vec()),
        ..Default::default()
    };
    state.tasks.clear();
    Ok(())
}

/// Strip the line ending a `text/uri-list` entry carries into the path.
///
/// RFC 2483 delimits that format with CRLF, which is what GNOME and every other
/// file manager here writes, but the parser these paths come from splits on the
/// line feed alone and leaves the carriage return on the end of the name. The
/// path then resolves to nothing and the copy looks empty.
fn trim_uri_path(path: PathBuf) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    let bytes = path.as_os_str().as_bytes();
    let trimmed = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
        .map_or(0, |last| last + 1);
    if trimmed == bytes.len() {
        return path;
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes[..trimmed].to_vec()))
}

/// Read the local clipboard and, when it changed, announce the new formats.
fn poll_local(state: &mut State) -> Result<()> {
    if state.writing || state.sessions.is_empty() {
        return Ok(());
    }
    let Some(clipboard) = state.clipboard.as_mut() else {
        return Ok(());
    };
    // The whole outbound path starts here, so when files never reach the remote
    // this says whether they were on the local clipboard at all.
    if tracing::enabled!(target: "openuuyc::clipboard", tracing::Level::DEBUG) {
        let probe = clipboard.get().file_list();
        tracing::debug!(target: "openuuyc::clipboard",
            sessions = state.sessions.len(),
            files = ?probe.as_ref().map(Vec::len).map_err(ToString::to_string),
            first = ?probe.as_ref().ok().and_then(|paths| paths.first().cloned()),
            held = state.local.sources.len(),
            "剪贴板轮询");
    }
    // A file manager puts the paths on the clipboard as `text/uri-list` and a
    // plain-text copy of the same names, so files are looked for first.
    let sources = clipboard
        .get()
        .file_list()
        .unwrap_or_default()
        .into_iter()
        .map(trim_uri_path)
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>();
    let snapshot = if !sources.is_empty() {
        LocalSnapshot {
            sources,
            ..Default::default()
        }
    } else if let Some(text) = clipboard.get_text().ok().filter(|text| !text.is_empty()) {
        LocalSnapshot {
            text: Some(text),
            ..Default::default()
        }
    } else {
        match clipboard.get_image() {
            Ok(image) => LocalSnapshot {
                image: Some(rgba_to_dib(
                    image.width as u32,
                    image.height as u32,
                    &image.bytes,
                )?),
                ..Default::default()
            },
            Err(_) => LocalSnapshot::default(),
        }
    };
    if snapshot.text == state.local.text
        && snapshot.image == state.local.image
        && snapshot.sources == state.local.sources
    {
        return Ok(());
    }
    if !state.local.sources.is_empty() || !snapshot.sources.is_empty() {
        tracing::debug!(
            files = snapshot.sources.len(),
            text = snapshot.text.is_some(),
            image = snapshot.image.is_some(),
            "本地剪贴板已变化"
        );
    }
    state.local = snapshot;
    // The previous copy is gone; a read still in flight against it now fails.
    state.tasks.clear();
    if state.local.is_empty() {
        state.published.clear();
        return Ok(());
    }
    let ids = state.local.format_ids();
    for session in sessions(state) {
        let links = formats::outgoing(
            &ids,
            session.platform.load(Ordering::Acquire),
            session.file_allowed(),
        );
        if links.is_empty() {
            state.published.remove(&session.id);
            continue;
        }
        state.published.insert(session.id, links.clone());
        session.emit(
            session.epoch.load(Ordering::Acquire),
            request(
                session.next(),
                ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                    formats: links.into_iter().map(|format| format.wire).collect(),
                    has_action: 0,
                    drag_drop_action: None,
                }),
            ),
        )?;
        *lock(&session.error) = None;
    }
    Ok(())
}

fn serve(
    state: &mut State,
    session: &Arc<Inner>,
    epoch: u64,
    id: i64,
    kind: ClipboardRequestKind,
) -> Result<()> {
    match kind {
        ClipboardRequestKind::FormatDataAsk(ask) => {
            let result = (|| -> Result<Vec<u8>> {
                let formats = state.published.get(&session.id).context("原剪贴板已失效")?;
                let format = formats
                    .iter()
                    .find(|format| {
                        format.wire.id == ask.format_id
                            && (ask.format_name.is_empty() || ask.format_name == format.wire.name)
                    })
                    .context("未发布该剪贴板格式")?;
                let data = state.local.data(format)?;
                formats::convert(data, format, session.platform.load(Ordering::Acquire), true)
            })();
            match result {
                Ok(data) if !data.is_empty() => {
                    session.enqueue(Outbound::Blocks(epoch, id, ask.block_key, data))
                }
                _ => session.emit(
                    epoch,
                    response(
                        id,
                        ClipboardResponseKind::FormatDataConfirm(ClipboardFormatDataConfirm {
                            err: 2,
                            block_key: ask.block_key,
                            block_count: 0,
                        }),
                    ),
                ),
            }
        }
        ClipboardRequestKind::FileDescListRequest(ask) => {
            let result = (|| -> Result<Arc<LocalFiles>> {
                ensure!(session.file_allowed(), "文件剪贴板已关闭");
                ensure!(
                    state.published.contains_key(&session.id),
                    "原文件剪贴板已失效"
                );
                ensure!(!state.local.sources.is_empty(), "本地剪贴板没有文件");
                ensure!(state.tasks.len() < 32, "文件任务过多");
                Ok(Arc::new(collect_files(&state.local.sources)?))
            })();
            let Ok(files) = result else {
                return session.emit(
                    epoch,
                    response(
                        id,
                        ClipboardResponseKind::FileDescListResponse(
                            ClipboardFileDescriptorListResponse {
                                task_id: ask.task_id,
                                segment_count: 0,
                                err: 2,
                            },
                        ),
                    ),
                );
            };
            state.tasks.insert((session.id, ask.task_id), files.clone());
            // Stay below the SDK's 512 KiB message ceiling even with long names.
            let mut segments = Vec::<Vec<ClipboardFileDescriptor>>::new();
            let mut current = Vec::new();
            let mut bytes = 0;
            for item in &files.items {
                let size = item.desc.encoded_len() + 8;
                if current.len() == 1500 || bytes + size > 450_000 {
                    segments.push(std::mem::take(&mut current));
                    bytes = 0;
                }
                current.push(item.desc.clone());
                bytes += size;
            }
            if !current.is_empty() {
                segments.push(current);
            }
            let mut messages = std::collections::VecDeque::new();
            messages.push_back(response(
                id,
                ClipboardResponseKind::FileDescListResponse(ClipboardFileDescriptorListResponse {
                    task_id: ask.task_id,
                    segment_count: segments.len() as u32,
                    err: 1,
                }),
            ));
            for (index, items) in segments.into_iter().enumerate() {
                messages.push_back(request(
                    session.next(),
                    ClipboardRequestKind::DescSegment(ClipboardFileDescriptorSegment {
                        task_id: ask.task_id,
                        segment_id: index as u32 + 1,
                        file_descs: items,
                    }),
                ));
            }
            session.enqueue(Outbound::Packets(epoch, messages))
        }
        ClipboardRequestKind::FileContentsRequest(ask) => {
            let result = if session.file_allowed() {
                state
                    .tasks
                    .get(&(session.id, ask.task_id))
                    .context("文件任务已失效")
                    .and_then(|files| files.read(&ask))
            } else {
                Err(anyhow!("文件剪贴板已关闭"))
            };
            let (err, data) = match result {
                Ok(data) => (1, data),
                Err(error) => {
                    tracing::debug!(%error, index = ask.list_index, "读取本地文件失败");
                    (2, Vec::new())
                }
            };
            session.emit(
                epoch,
                response(
                    id,
                    ClipboardResponseKind::FileContentsResponse(ClipboardFileContentsResponse {
                        task_id: ask.task_id,
                        data,
                        err,
                        pos_offset: ask.pos_offset,
                        list_index: ask.list_index,
                    }),
                ),
            )
        }
        ClipboardRequestKind::CancelRequest(ask) => {
            state.tasks.remove(&(session.id, ask.task_id));
            session.emit(
                epoch,
                response(
                    id,
                    ClipboardResponseKind::CancelResponse(ClipboardFileCancelResponse {
                        task_id: ask.task_id,
                        err: 1,
                    }),
                ),
            )
        }
        _ => Ok(()),
    }
}

/// A packed device-independent bitmap, as CF_DIB carries it.
fn dib_to_rgba(dib: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    ensure!(dib.len() >= 40, "位图数据过短");
    let header = u32::from_le_bytes(dib[0..4].try_into()?) as usize;
    ensure!(
        (40..=124).contains(&header) && dib.len() > header,
        "不支持的位图头"
    );
    let width = i32::from_le_bytes(dib[4..8].try_into()?);
    let height = i32::from_le_bytes(dib[8..12].try_into()?);
    let depth = u16::from_le_bytes(dib[14..16].try_into()?);
    let compression = u32::from_le_bytes(dib[16..20].try_into()?);
    ensure!(depth == 32 || depth == 24, "仅支持 24/32 位位图");
    // BI_RGB and BI_BITFIELDS with the usual masks share this pixel layout.
    ensure!(compression == 0 || compression == 3, "不支持压缩位图");
    let bottom_up = height > 0;
    let width = u32::try_from(width.abs()).context("位图宽度无效")?;
    let height = u32::try_from(height.abs()).context("位图高度无效")?;
    ensure!(
        width > 0 && height > 0 && width <= 32768 && height <= 32768,
        "位图尺寸无效"
    );
    let bytes = usize::from(depth / 8);
    let stride = ((width as usize * bytes) + 3) & !3;
    let masks = if compression == 3 { 12 } else { 0 };
    let start = header + masks;
    ensure!(
        dib.len() >= start + stride * height as usize,
        "位图数据不完整"
    );
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for row in 0..height as usize {
        let source = if bottom_up {
            height as usize - 1 - row
        } else {
            row
        };
        let line = &dib[start + source * stride..][..stride];
        for column in 0..width as usize {
            let pixel = &line[column * bytes..][..bytes];
            let target = (row * width as usize + column) * 4;
            rgba[target] = pixel[2];
            rgba[target + 1] = pixel[1];
            rgba[target + 2] = pixel[0];
            rgba[target + 3] = if bytes == 4 { pixel[3] } else { 255 };
        }
    }
    // A 32-bit DIB with an all-zero alpha channel is opaque in practice.
    if bytes == 4 && rgba.iter().skip(3).step_by(4).all(|alpha| *alpha == 0) {
        for alpha in rgba.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
    }
    Ok((width, height, rgba))
}

fn rgba_to_dib(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    ensure!(width > 0 && height > 0, "图片尺寸无效");
    let pixels = width as usize * height as usize;
    ensure!(rgba.len() >= pixels * 4, "图片数据不完整");
    ensure!(pixels * 4 <= MAX_CLIP, "图片过大");
    let mut dib = Vec::with_capacity(40 + pixels * 4);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(width as i32).to_le_bytes());
    // Positive height keeps the bottom-up order Windows applications expect.
    dib.extend_from_slice(&(height as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&((pixels * 4) as u32).to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    for row in (0..height as usize).rev() {
        for column in 0..width as usize {
            let pixel = &rgba[(row * width as usize + column) * 4..][..4];
            dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Ok(dib)
}
