//! Linux clipboard adapter for X11 and Wayland.
//!
//! Differences from the Windows/OLE adapter this replaces:
//! * X11 and Wayland have no delayed rendering across processes, so an offer
//!   from the remote is fetched immediately instead of when the user pastes.
//! * There is no clipboard-change notification, so local changes are polled.
//! * Files copied here are offered to the remote: it pulls the descriptor list
//!   and then the contents by offset, so nothing has to be promised in advance.
//!   Files the remote offers are mounted as a FUSE filesystem, so a paste or a
//!   drop gets real paths whose contents are fetched as they are read.
use super::formats::{self, Format};
use super::*;
use anyhow::Context as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Instant;

pub(super) use super::formats::safe_name;

/// Anything larger is a transfer, not a clipboard paste.
const MAX_CLIP: usize = 32 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(400);

pub(super) enum Command {
    Activate(Weak<Inner>),
    DropFiles(
        Arc<super::drag::Submission>,
        PreparedFiles,
        ClipboardFormatListRequestKind,
    ),
    Remove(u64),
    Offer(
        Weak<Inner>,
        u64,
        Vec<ClipboardFormat>,
        Option<ClipboardFormatListRequestKind>,
    ),
    Request(Weak<Inner>, u64, i64, ClipboardRequestKind),
    Text(Weak<Inner>, u64, i64, String),
    PublishFiles(
        Weak<Inner>,
        u64,
        PreparedFiles,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    ),
}

/// Local files picked for a drag or a drop, walked before they are offered.
pub(super) struct PreparedFiles {
    files: LocalFiles,
    summary: FileSummary,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileSummary {
    pub count: u32,
    pub bytes: u64,
    pub first_name: String,
}
impl PreparedFiles {
    pub fn summary(&self) -> FileSummary {
        self.summary.clone()
    }
}
pub(super) fn prepare_files(
    paths: Vec<PathBuf>,
    allowed: impl Fn() -> bool,
) -> Result<PreparedFiles> {
    ensure!(
        !paths.is_empty() && paths.len() <= MAX_FILES,
        "拖放文件数量无效"
    );
    let mut items = Vec::new();
    for path in paths {
        ensure!(allowed(), "文件准备已取消");
        ensure!(
            !std::fs::symlink_metadata(&path)?.is_symlink(),
            "不传输符号链接"
        );
        let root = std::fs::canonicalize(&path)?;
        let name = path
            .file_name()
            .context("不能拖放根目录")?
            .to_string_lossy()
            .into_owned();
        collect_file(&root, &root, &name, &mut items)?;
    }
    ensure!(allowed(), "文件准备已取消");
    let summary = file_summary(items.iter().map(|item| &item.desc))?;
    Ok(PreparedFiles {
        files: LocalFiles { items },
        summary,
    })
}
fn file_summary<'a>(
    items: impl Iterator<Item = &'a ClipboardFileDescriptor>,
) -> Result<FileSummary> {
    let mut summary = FileSummary {
        count: 0,
        bytes: 0,
        first_name: String::new(),
    };
    for item in items {
        if summary.first_name.is_empty() {
            summary.first_name = item.file_name.clone();
        }
        ensure!(safe_name(&item.file_name), "文件名称不安全");
        summary.count += 1;
        summary.bytes = summary
            .bytes
            .checked_add(item.file_size)
            .context("文件总大小溢出")?;
    }
    ensure!(
        summary.count > 0 && summary.count as usize <= MAX_FILES,
        "文件清单为空或过大"
    );
    Ok(summary)
}

/// How long a drop target may go without reading before the transfer is
/// called off. A file manager asking whether to replace a file stops reading
/// until the user answers.
const DROP_IDLE: Duration = Duration::from_secs(300);
/// How long a mounted drop outlives its offer, for a target still creating
/// the empty files and folders it has already listed.
const MOUNT_GRACE: Duration = Duration::from_secs(30);

/// What a drop target has read of each offered file.
struct Progress {
    sizes: Vec<u64>,
    reached: Mutex<Vec<u64>>,
    bytes: AtomicU64,
    last: Mutex<Instant>,
}
impl Progress {
    fn record(&self, index: u32, offset: u64, length: usize) {
        self.bytes.fetch_add(length as u64, Ordering::AcqRel);
        *lock(&self.last) = Instant::now();
        if let Some(end) = lock(&self.reached).get_mut(index as usize) {
            *end = (*end).max(offset + length as u64);
        }
    }
    fn complete(&self) -> bool {
        lock(&self.reached)
            .iter()
            .zip(&self.sizes)
            .all(|(reached, size)| reached >= size)
    }
}

/// Files a peer offers for a drop or a drag, read on demand through a mount.
pub(super) struct RemoteOffer {
    pub(super) session: Arc<Inner>,
    pub(super) epoch: u64,
    alive: AtomicBool,
    task: AtomicU32,
    descriptors: Mutex<Option<Vec<ClipboardFileDescriptor>>>,
    reading: Mutex<()>,
    dragging: AtomicBool,
    mount: Mutex<Option<(Arc<super::fuse::Mount>, Arc<Progress>)>>,
}
#[derive(Clone)]
pub(crate) struct FileOffer(pub(super) Arc<RemoteOffer>);
pub(crate) struct FileOfferStatus {
    pub bytes_read: u64,
    pub completed: Option<Result<(), String>>,
}
impl FileOffer {
    pub fn prepare(&self) -> Result<FileSummary> {
        file_summary(self.0.list()?.iter())
    }
    /// The offered files as local paths for a drag source, mounted on first use.
    pub fn object(&self) -> Result<crate::platform::drag_drop::Files> {
        self.0.valid()?;
        let mut slot = lock(&self.0.mount);
        if slot.is_none() {
            let descriptors = self.0.list()?;
            let progress = Arc::new(Progress {
                sizes: descriptors
                    .iter()
                    .map(|d| {
                        if d.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                            0
                        } else {
                            d.file_size
                        }
                    })
                    .collect(),
                reached: Mutex::new(vec![0; descriptors.len()]),
                bytes: AtomicU64::new(0),
                last: Mutex::new(Instant::now()),
            });
            let tree = super::fuse::Tree::build(&descriptors)?;
            let reader = {
                let offer = Arc::downgrade(&self.0);
                let progress = progress.clone();
                Arc::new(move |index: u32, offset: u64, length: usize| {
                    let offer = offer.upgrade().context("拖放文件已失效")?;
                    offer.valid()?;
                    let data = offer.session.read_file(
                        offer.epoch,
                        offer.task(),
                        index,
                        offset,
                        length.min(FILE_BLOCK),
                        2,
                    )?;
                    progress.record(index, offset, data.len());
                    Ok(data)
                })
            };
            let mount = super::fuse::Mount::new(
                &super::fuse::mount_parent()?,
                super::fuse::generation(),
                tree,
                reader,
            )?;
            *slot = Some((Arc::new(mount), progress));
        }
        let (mount, _) = slot.as_ref().unwrap();
        Ok(crate::platform::drag_drop::Files {
            paths: mount.paths(),
            _hold: mount.clone(),
        })
    }
    pub fn dragging(&self, value: bool) {
        self.0.dragging.store(value, Ordering::Release);
        if !value && let Some((_, progress)) = lock(&self.0.mount).as_ref() {
            // The idle clock starts at the drop, not at the first hover.
            *lock(&progress.last) = Instant::now();
        }
    }
    pub fn cancel(&self) {
        self.0.cancel();
        self.0.session.cancel_pending();
    }
    pub(super) fn valid(&self) -> bool {
        self.0.valid().is_ok()
    }
    pub(super) fn manifest(&self) -> Result<Vec<ClipboardFileDescriptor>> {
        self.0.list()
    }
    pub(super) fn read(&self, index: usize, offset: u64, length: usize) -> Result<Vec<u8>> {
        self.0.valid()?;
        let _busy = self.0.enter_read()?;
        self.0.session.read_file(
            self.0.epoch,
            self.0.task(),
            index.try_into()?,
            offset,
            length,
            2,
        )
    }
    /// A drop is complete once the target has read every file to its end.
    pub fn status(&self) -> FileOfferStatus {
        let mount = lock(&self.0.mount);
        let Some((_, progress)) = mount.as_ref() else {
            return FileOfferStatus {
                bytes_read: 0,
                completed: None,
            };
        };
        let completed = if self.0.dragging.load(Ordering::Acquire) {
            None
        } else if progress.complete() {
            Some(Ok(()))
        } else if lock(&progress.last).elapsed() > DROP_IDLE {
            Some(Err("目标长时间未读取文件，拖放已停止".to_owned()))
        } else {
            None
        };
        FileOfferStatus {
            bytes_read: progress.bytes.load(Ordering::Acquire),
            completed,
        }
    }
}
impl RemoteOffer {
    fn new(session: Arc<Inner>, epoch: u64) -> Self {
        Self {
            session,
            epoch,
            alive: AtomicBool::new(true),
            task: AtomicU32::new(0),
            descriptors: Mutex::new(None),
            reading: Mutex::new(()),
            dragging: AtomicBool::new(false),
            mount: Mutex::new(None),
        }
    }
    fn cancel(&self) {
        self.alive.store(false, Ordering::Release);
        let task = self.task.swap(0, Ordering::AcqRel);
        if task != 0 {
            let _ = self.session.enqueue(Outbound::Cleanup(request(
                self.session.next(),
                ClipboardRequestKind::CancelRequest(ClipboardFileCancelRequest { task_id: task }),
            )));
        }
    }
    fn valid(&self) -> Result<()> {
        ensure!(
            self.alive.load(Ordering::Acquire) && self.session.valid(self.epoch),
            "剪贴板来源已失效"
        );
        Ok(())
    }
    fn task(&self) -> u32 {
        let existing = self.task.load(Ordering::Acquire);
        if existing != 0 {
            return existing;
        }
        let task = self.session.next() as u32;
        let _ = self
            .task
            .compare_exchange(0, task, Ordering::AcqRel, Ordering::Acquire);
        self.task.load(Ordering::Acquire)
    }
    fn list(&self) -> Result<Vec<ClipboardFileDescriptor>> {
        self.valid()?;
        let existing = lock(&self.descriptors).clone();
        if let Some(list) = existing {
            return Ok(list);
        }
        let _busy = self.enter_read()?;
        let list = self.session.descriptors(self.epoch, self.task())?;
        self.valid()?;
        *lock(&self.descriptors) = Some(list.clone());
        Ok(list)
    }
    fn enter_read(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        let guard = lock(&self.reading);
        self.valid()?;
        Ok(guard)
    }
}
impl Drop for RemoteOffer {
    fn drop(&mut self) {
        self.cancel();
        if let Some((mount, _)) = lock(&self.mount).take() {
            std::thread::spawn(move || {
                std::thread::sleep(MOUNT_GRACE);
                drop(mount);
            });
        }
    }
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
    /// The remote copy currently published locally, mounted for as long as the
    /// clipboard points at it.
    mounted: Option<super::fuse::Mount>,
    /// Holds the clipboard selection while those paths are the copy on it.
    offer: Option<super::x11_offer::FileOffer>,
    /// Files offered to one session for a drag or a drop rather than through
    /// the clipboard: a drop's list is handed out once, to the descriptor
    /// request that answers it; a drag's stays for every read of its session.
    drag_files: HashMap<u64, (Arc<LocalFiles>, Option<Arc<super::drag::Submission>>)>,
    /// Lists handed out from `drag_files`, which local copies do not retire.
    drag_tasks: HashMap<(u64, u32), Arc<LocalFiles>>,
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
        mounted: None,
        offer: None,
        drag_files: HashMap::new(),
        drag_tasks: HashMap::new(),
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
        // A drag session exchanges only the files it was given.
        .filter(|session| session.file_offers.is_none())
        .collect()
}

fn process(state: &mut State, command: Command) {
    for (files, ticket) in state.drag_files.values_mut() {
        if let Some(ticket) = ticket
            && !ticket.valid()
            && !files.items.is_empty()
        {
            ticket.fail("拖放等待超时或权限已撤销".into());
            // Keep the rejected publication until its descriptor request
            // arrives; never substitute ordinary copied files.
            *files = Arc::new(LocalFiles { items: Vec::new() });
        }
    }
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
            state.drag_files.remove(&id);
            state.drag_tasks.retain(|(session, _), _| *session != id);
        }
        Command::Offer(weak, epoch, formats, drop) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch))
                && let Err(error) = if drop.is_some() || session.file_offers.is_some() {
                    // A dropped, auto-saved or dragged file list is not a
                    // copy; leave the local clipboard alone.
                    offer_files(&session, epoch, formats, drop)
                } else {
                    accept_offer(state, &session, epoch, formats)
                }
            {
                // The notice shows the outermost line; the cause is only in
                // the chain, and it is the part worth reading.
                tracing::debug!(error = format!("{error:#}"), "接受远端剪贴板失败");
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
        Command::DropFiles(ticket, files, action) => {
            let result = (|| -> Result<()> {
                ensure!(ticket.valid(), "文件拖放已过期");
                let session = ticket.session.upgrade().context("连接已结束")?;
                let links = formats::outgoing(
                    &file_format_ids(),
                    session.platform.load(Ordering::Acquire),
                    true,
                );
                state
                    .drag_files
                    .insert(session.id, (Arc::new(files.files), Some(ticket.clone())));
                session.emit(
                    ticket.epoch,
                    request(
                        session.next(),
                        ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                            formats: links.into_iter().map(|format| format.wire).collect(),
                            has_action: 1,
                            drag_drop_action: Some(action),
                        }),
                    ),
                )?;
                ticket.published.store(true, Ordering::Release);
                Ok(())
            })();
            if let Err(error) = result {
                ticket.fail(error.to_string());
            }
        }
        Command::PublishFiles(weak, epoch, files, reply) => {
            let result = (|| -> Result<()> {
                let session = weak
                    .upgrade()
                    .filter(|session| session.valid(epoch) && session.file_allowed())
                    .context("文件发布已取消")?;
                let links = formats::outgoing(
                    &file_format_ids(),
                    session.platform.load(Ordering::Acquire),
                    true,
                );
                state
                    .drag_files
                    .insert(session.id, (Arc::new(files.files), None));
                session.emit(
                    epoch,
                    request(
                        session.next(),
                        ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                            formats: links.into_iter().map(|format| format.wire).collect(),
                            has_action: 0,
                            drag_drop_action: None,
                        }),
                    ),
                )
            })();
            let _ = reply.send(result.map_err(|error| error.to_string()));
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
    let files = session.file_allowed();
    let links: Vec<Format> = offered
        .into_iter()
        .filter_map(|format| formats::incoming(format, platform, files))
        .collect();
    // Files win when offered: a peer that copied files usually also offers
    // their names as text, and pasting the names is not what was asked for.
    if files && links.iter().any(|link| link.local == descriptor_format()) {
        return accept_files(state, session, epoch);
    }
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

fn descriptor_format() -> u32 {
    formats::register("FileGroupDescriptorW")
}

/// The descriptor and contents pair a file list travels as.
fn file_format_ids() -> Vec<(u32, String)> {
    vec![
        (descriptor_format(), "FileGroupDescriptorW".into()),
        (formats::register("FileContents"), "FileContents".into()),
    ]
}

/// A file list offered for a drop or a drag rather than as a copy: the files
/// stay remote until the receiving side reads them.
fn offer_files(
    session: &Arc<Inner>,
    epoch: u64,
    offered: Vec<ClipboardFormat>,
    action: Option<ClipboardFormatListRequestKind>,
) -> Result<()> {
    ensure!(!offered.is_empty(), "独立文件提议为空");
    let platform = session.platform.load(Ordering::Acquire);
    let links: Vec<Format> = offered
        .into_iter()
        .filter_map(|format| formats::incoming(format, platform, session.file_allowed()))
        .collect();
    let contents = formats::register("FileContents");
    ensure!(
        session.file_allowed()
            && links.iter().any(|link| link.local == descriptor_format())
            && links
                .iter()
                .all(|link| link.local == descriptor_format() || link.local == contents),
        "独立会话只接受文件格式"
    );
    let offer = FileOffer(Arc::new(RemoteOffer::new(session.clone(), epoch)));
    if let Some(action) = action {
        return super::drag::receive(offer, action);
    }
    session
        .file_offers
        .as_ref()
        .context("文件提议没有接收方")?
        .try_send(offer)
        .map_err(|_| anyhow!("文件提议接收方已关闭或繁忙"))
}

/// Mount the remote's copy and point the local clipboard at it.
///
/// Only the list is fetched here. The contents follow one read at a time,
/// through the filesystem, if and when something actually opens them.
fn accept_files(state: &mut State, session: &Arc<Inner>, epoch: u64) -> Result<()> {
    // The task id the official Windows client uses for an inbound offer.
    const TASK: u32 = 0;
    let descriptors = session.descriptors(epoch, TASK)?;
    ensure!(!descriptors.is_empty(), "远端剪贴板文件列表为空");
    let tree = super::fuse::Tree::build(&descriptors)?;
    let parent = super::fuse::mount_parent()?;
    let reader = {
        let session = Arc::downgrade(session);
        Arc::new(move |index: u32, offset: u64, length: usize| {
            let session = session.upgrade().context("剪贴板会话已结束")?;
            session.read_file(epoch, TASK, index, offset, length.min(FILE_BLOCK), 2)
        })
    };
    let mount = super::fuse::Mount::new(&parent, super::fuse::generation(), tree, reader)?;
    let paths = mount.paths();
    tracing::debug!(
        files = descriptors.len(),
        root = %mount.root().display(),
        "已挂载远端剪贴板文件"
    );
    write_files(state, &paths)?;
    // Held until the next copy replaces it: the clipboard still points here,
    // and the paste may be minutes away.
    state.mounted = Some(mount);
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
    state.mounted = None;
    state.offer = None;
    Ok(())
}

/// Publish paths as a file copy on the local clipboard.
///
/// A file copy has to be offered under several names at once, and only the
/// owner of the selection can answer for more than one, so this takes the
/// clipboard itself rather than going through the library used for text and
/// images. On a desktop where that fails there is still `text/uri-list`, which
/// is enough for anything that does not insist on the GTK file-manager name.
fn write_files(state: &mut State, paths: &[PathBuf]) -> Result<()> {
    state.writing = true;
    let offer = super::x11_offer::FileOffer::publish(paths.to_vec());
    let result = match offer {
        Ok(offer) => {
            state.offer = Some(offer);
            Ok(())
        }
        Err(error) => {
            tracing::debug!(error = format!("{error:#}"), "接管剪贴板失败，改用单一格式");
            state.offer = None;
            let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
            clipboard
                .set()
                .file_list(paths)
                .map_err(anyhow::Error::from)
        }
    };
    state.writing = false;
    result.context("写入系统剪贴板失败")?;
    state.local = LocalSnapshot {
        sources: paths.to_vec(),
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
    state.mounted = None;
    state.offer = None;
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
    let files = clipboard
        .get()
        .file_list()
        .unwrap_or_default()
        .into_iter()
        .map(trim_uri_path)
        .filter(|path| path.is_absolute())
        .collect::<Vec<_>>();
    // A copy the remote sent is published as paths into this process's own
    // filesystem, and under plain-text targets too. While it is still what the
    // clipboard holds, nothing changed here: offering the paths back would
    // ask the remote for the files it just gave us, and offering their text
    // would replace the remote's copy with a path that only exists here.
    if own_copy(&files) {
        return Ok(());
    }
    let sources = files;
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
            let explicit = drag_list(state, session, ask.task_id);
            let dragged = explicit.is_some() || session.file_offers.is_some();
            let result = (|| -> Result<Arc<LocalFiles>> {
                ensure!(session.file_allowed(), "文件剪贴板已关闭");
                if let Some(files) = explicit {
                    let files = files?;
                    ensure!(!files.items.is_empty(), "文件列表为空");
                    return Ok(files);
                }
                ensure!(session.file_offers.is_none(), "拖放文件提议已失效");
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
            if dragged {
                ensure!(state.drag_tasks.len() < 32, "文件任务过多");
                state
                    .drag_tasks
                    .insert((session.id, ask.task_id), files.clone());
            } else {
                state.tasks.insert((session.id, ask.task_id), files.clone());
            }
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
                    .or_else(|| state.drag_tasks.get(&(session.id, ask.task_id)))
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
            state.drag_tasks.remove(&(session.id, ask.task_id));
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

/// Whether every path is one this process mounted for a remote copy.
fn own_copy(paths: &[PathBuf]) -> bool {
    let Some(parent) = super::fuse::mount_root() else {
        return false;
    };
    own_paths(paths, &parent)
}
fn own_paths(paths: &[PathBuf], parent: &Path) -> bool {
    !paths.is_empty() && paths.iter().all(|path| path.starts_with(parent))
}

/// The file list a drag or drop handed this session, if it has one. A drop's
/// list answers one descriptor request, which hands the submission off.
fn drag_list(
    state: &mut State,
    session: &Arc<Inner>,
    task: u32,
) -> Option<Result<Arc<LocalFiles>>> {
    if let Some(files) = state.drag_tasks.get(&(session.id, task)) {
        return Some(Ok(files.clone()));
    }
    let (files, ticket) = state.drag_files.get(&session.id)?.clone();
    let Some(ticket) = ticket else {
        return Some(Ok(files));
    };
    state.drag_files.remove(&session.id);
    if !ticket.valid() {
        return Some(Err(anyhow!("拖放文件提议已失效")));
    }
    ticket.handed_off();
    Some(Ok(files))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read_tree(path: &Path, out: &mut Vec<(String, Vec<u8>)>, base: &Path) {
        if path.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                read_tree(&entry, out, base);
            }
        } else {
            let name = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push((name, std::fs::read(path).unwrap()));
        }
    }

    /// A drag between two isolated sessions: one publishes a folder, the
    /// other mounts the offer and reads it back through the filesystem.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "mounts FUSE under XDG_RUNTIME_DIR"]
    async fn drag_offer_reads_through_mount() {
        let (source, _) = Clipboard::isolated_files(Arc::new(|| true));
        let (target, mut offers) = Clipboard::isolated_files(Arc::new(|| true));
        for clipboard in [&source, &target] {
            clipboard.set_enabled(true).unwrap();
            clipboard.policy(true, true);
        }
        let (to_target, to_source) = (target.clone(), source.clone());
        tokio::spawn(source.sender(move |_, data| {
            let target = to_target.clone();
            async move { target.receive(&data).map(|_| ()) }
        }));
        tokio::spawn(target.sender(move |_, data| {
            let source = to_source.clone();
            async move { source.receive(&data).map(|_| ()) }
        }));
        let root = std::env::temp_dir().join(format!("openuuyc-drag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("folder/sub")).unwrap();
        std::fs::write(root.join("folder/a.txt"), b"alpha").unwrap();
        let big: Vec<u8> = (0..1_300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(root.join("folder/sub/big.bin"), &big).unwrap();
        std::fs::write(root.join("folder/empty"), b"").unwrap();
        let summary = source
            .publish_files(vec![root.join("folder")])
            .await
            .unwrap();
        assert_eq!(summary.bytes, 5 + big.len() as u64);
        let offer = tokio::time::timeout(Duration::from_secs(5), offers.recv())
            .await
            .unwrap()
            .unwrap();
        let expected = {
            let mut out = Vec::new();
            read_tree(&root.join("folder"), &mut out, &root);
            out
        };
        tokio::task::spawn_blocking(move || {
            let summary = offer.prepare().unwrap();
            assert_eq!(summary.count, 5);
            offer.dragging(true);
            let files = offer.object().unwrap();
            assert_eq!(files.paths.len(), 1);
            offer.dragging(false);
            assert!(offer.status().completed.is_none());
            let base = files.paths[0].parent().unwrap().to_path_buf();
            let mut read = Vec::new();
            read_tree(&files.paths[0], &mut read, &base);
            assert_eq!(read, expected);
            let status = offer.status();
            assert_eq!(status.bytes_read, 5 + 1_300_000);
            assert_eq!(status.completed, Some(Ok(())));
        })
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The official send to the controller's receive folder: the host
    /// session offers files with AutoSave, the controller saves them under
    /// Downloads and reports back.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "writes into $HOME/Downloads"]
    async fn auto_save_lands_in_downloads() {
        let host = Clipboard::guarded(None);
        host.host_role();
        let controller = Clipboard::guarded(None);
        for clipboard in [&host, &controller] {
            clipboard.set_enabled(true).unwrap();
            clipboard.set_files(true);
            clipboard.policy(true, true);
        }
        let (to_controller, to_host) = (controller.clone(), host.clone());
        tokio::spawn(host.sender(move |_, data| {
            let controller = to_controller.clone();
            async move { controller.receive(&data).map(|_| ()) }
        }));
        tokio::spawn(controller.sender(move |_, data| {
            let host = to_host.clone();
            async move { host.receive(&data).map(|_| ()) }
        }));
        let root = std::env::temp_dir().join(format!("openuuyc-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let name = format!("openuuyc-autosave-{}.txt", std::process::id());
        std::fs::write(root.join(&name), b"saved").unwrap();
        let ticket = host.send_files(vec![root.join(&name)]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match ticket.state() {
                super::super::drag::SubmissionState::Complete { total, saved } => {
                    assert_eq!((total, saved), (1, 1));
                    break;
                }
                super::super::drag::SubmissionState::Failed(error) => panic!("{error}"),
                _ => {}
            }
            assert!(Instant::now() < deadline, "auto-save did not complete");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let saved = crate::platform::file_locations::user_dir("XDG_DOWNLOAD_DIR", "Downloads")
            .unwrap()
            .join(&name);
        assert_eq!(std::fs::read(&saved).unwrap(), b"saved");
        let _ = std::fs::remove_file(saved);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The official drop: the controller offers files with a drop point and
    /// the host drags them, through the mount, into whatever lies there.
    /// OPENUUYC_DND_DROP is the normalized point (`x,y`) on the first screen.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs an X display with a drop target"]
    async fn official_drop_reaches_target() {
        let at = std::env::var("OPENUUYC_DND_DROP").unwrap();
        let (x, y) = at.split_once(',').unwrap();
        let screen = crate::platform::capture::screens().unwrap()[0].id;
        let controller = Clipboard::guarded(None);
        let host = Clipboard::guarded(None);
        host.host_role();
        for clipboard in [&host, &controller] {
            clipboard.set_enabled(true).unwrap();
            clipboard.set_files(true);
            clipboard.policy(true, true);
        }
        let (to_controller, to_host) = (controller.clone(), host.clone());
        tokio::spawn(host.sender(move |_, data| {
            let controller = to_controller.clone();
            async move { controller.receive(&data).map(|_| ()) }
        }));
        tokio::spawn(controller.sender(move |_, data| {
            let host = to_host.clone();
            async move { host.receive(&data).map(|_| ()) }
        }));
        let path = std::env::temp_dir().join("openuuyc-official-drop.txt");
        std::fs::write(&path, b"dropped by the controller").unwrap();
        let ticket = controller
            .drop_files(
                vec![path],
                crate::protocol::drag_drop::Point {
                    screen,
                    x: x.parse().unwrap(),
                    y: y.parse().unwrap(),
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(
            ticket.state(),
            super::super::drag::SubmissionState::HandedOff
        ) {
            if let super::super::drag::SubmissionState::Failed(error) = ticket.state() {
                panic!("{error}");
            }
            assert!(Instant::now() < deadline, "drop was not taken");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Give the host's drag and the target's read time to finish.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            host.snapshot().error.is_none(),
            "{:?}",
            host.snapshot().error
        );
    }

    #[test]
    fn remote_copies_are_not_offered_back() {
        let parent = Path::new("/run/user/1000/openuuyc/clipboard");
        assert!(own_paths(&[parent.join("4/a.png")], parent));
        assert!(own_paths(
            &[parent.join("4/a.png"), parent.join("4/b")],
            parent
        ));
        assert!(!own_paths(
            &[parent.join("4/a.png"), PathBuf::from("/home/a/b")],
            parent
        ));
        assert!(!own_paths(&[], parent));
        assert!(!own_paths(
            &[PathBuf::from("/run/user/1000/openuuyc/clipboard-other/x")],
            parent
        ));
    }
}
