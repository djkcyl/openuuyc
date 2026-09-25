//! Source OLE objects stay on one STA; remote providers are thread-safe COM objects.
//! COM callbacks may pump the calling thread's messages;
//! no RefCell borrow or application mutex is held across a call into OLE/network.
use super::formats::{self, Format};
use super::*;
use std::{
    cell::{Cell, RefCell},
    io::{Read, Seek, SeekFrom},
    mem::ManuallyDrop,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*, Ole::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{BOOL, HRESULT, Interface, Ref, implement},
};
#[link(name = "ole32")]
unsafe extern "system" {
    #[link_name = "OleIsCurrentClipboard"]
    fn ole_is_current(object: *mut std::ffi::c_void) -> HRESULT;
}
fn is_current(object: &IDataObject) -> bool {
    unsafe { ole_is_current(object.as_raw()) == S_OK }
}
fn clipboard_retry<T>(
    mut action: impl FnMut() -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    for attempt in 0..10 {
        match action() {
            Err(error) if error.code() == HRESULT(0x800401D0u32 as i32) && attempt < 9 => {
                std::thread::sleep(Duration::from_millis(10))
            }
            result => return result,
        }
    }
    unreachable!()
}

const WAKE: u32 = WM_APP + 73;
pub(super) enum Command {
    Activate(Weak<Inner>),
    Remove(u64),
    Offer(Weak<Inner>, u64, Vec<ClipboardFormat>),
    Request(Weak<Inner>, u64, i64, ClipboardRequestKind),
    Text(Weak<Inner>, u64, i64, String),
}
struct Worker {
    sender: SyncSender<Command>,
    hwnd: isize,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}
static WORKER: OnceLock<std::result::Result<Worker, String>> = OnceLock::new();
thread_local! {static STATE:RefCell<Option<State>>=const{RefCell::new(None)};static WRITING:Cell<bool>=const{Cell::new(false)};}
struct Writing(bool);
impl Writing {
    fn new() -> Self {
        Self(WRITING.with(|w| w.replace(true)))
    }
}
impl Drop for Writing {
    fn drop(&mut self) {
        WRITING.with(|w| w.set(self.0));
    }
}
struct State {
    receiver: Receiver<Command>,
    sessions: HashMap<u64, Weak<Inner>>,
    published: HashMap<u64, (Arc<LocalSource>, Vec<Format>)>,
    tasks: HashMap<(u64, u32), Arc<LocalFiles>>,
    owner: Option<(IDataObject, Arc<RemoteOffer>)>,
    sequence: u32,
}
pub(super) fn start() -> Result<()> {
    WORKER
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(64);
            let (ready, wait) = sync_channel(1);
            let thread = std::thread::Builder::new()
                .name("UU clipboard STA".into())
                .spawn(move || unsafe {
                    let result = (|| -> windows::core::Result<HWND> {
                        OleInitialize(None)?;
                        let module = GetModuleHandleW(None)?;
                        let class = windows::core::w!("OpenUUYC.Clipboard.STA");
                        let wc = WNDCLASSW {
                            lpfnWndProc: Some(window_proc),
                            hInstance: module.into(),
                            lpszClassName: class,
                            ..Default::default()
                        };
                        if RegisterClassW(&wc) == 0 {
                            return Err(windows::core::Error::from_thread());
                        }
                        STATE.with(|s| {
                            *s.borrow_mut() = Some(State {
                                receiver,
                                sessions: HashMap::new(),
                                published: HashMap::new(),
                                tasks: HashMap::new(),
                                owner: None,
                                sequence: 0,
                            })
                        });
                        let hwnd = CreateWindowExW(
                            WINDOW_EX_STYLE(0),
                            class,
                            windows::core::w!(""),
                            WINDOW_STYLE(0),
                            0,
                            0,
                            0,
                            0,
                            Some(HWND_MESSAGE),
                            None,
                            Some(module.into()),
                            None,
                        )?;
                        AddClipboardFormatListener(hwnd)?;
                        Ok(hwnd)
                    })();
                    match result {
                        Ok(hwnd) => {
                            let _ = ready.send(Ok(hwnd.0 as isize));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                            let _ = RemoveClipboardFormatListener(hwnd);
                            let _ = DestroyWindow(hwnd);
                        }
                        Err(e) => {
                            let _ = ready.send(Err(e.to_string()));
                        }
                    }
                    STATE.with(|s| {
                        s.borrow_mut().take();
                    });
                    OleUninitialize();
                })
                .map_err(|e| e.to_string())?;
            let hwnd = wait.recv().map_err(|e| e.to_string())??;
            Ok(Worker {
                sender,
                hwnd,
                thread: Mutex::new(Some(thread)),
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow!(e.clone()))
}
pub(super) fn post(command: Command) -> Result<()> {
    let Some(worker) = WORKER.get() else {
        return if matches!(command, Command::Remove(_)) {
            Ok(())
        } else {
            Err(anyhow!("剪贴板服务尚未启动"))
        };
    };
    let worker = worker.as_ref().map_err(|e| anyhow!(e.clone()))?;
    worker
        .sender
        .try_send(command)
        .map_err(|_| anyhow!("剪贴板工作队列繁忙"))?;
    unsafe {
        PostMessageW(Some(HWND(worker.hwnd as _)), WAKE, WPARAM(0), LPARAM(0))?;
    }
    Ok(())
}
pub(super) fn pump() {
    unsafe {
        let mut msg = MSG::default();
        for _ in 0..64 {
            if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                break;
            }
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
pub(super) fn shutdown() {
    if let Some(Ok(worker)) = WORKER.get() {
        let _ =
            unsafe { PostMessageW(Some(HWND(worker.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = lock(&worker.thread).take() {
            let _ = thread.join();
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        WM_CLOSE => {
            for s in sessions() {
                s.active.store(false, Ordering::Release);
                s.cancel_pending();
            }
            retire_owner(true);
            unsafe { PostQuitMessage(0) };
            true
        }
        WM_CLIPBOARDUPDATE => {
            if let Err(e) = local_change() {
                set_error_all(e.to_string());
            }
            true
        }
        WAKE => {
            for _ in 0..64 {
                let command =
                    STATE.with(|s| s.borrow().as_ref().and_then(|s| s.receiver.try_recv().ok()));
                let Some(command) = command else {
                    break;
                };
                process(command);
            }
            true
        }
        _ => false,
    }));
    if matches!(result, Ok(true)) {
        LRESULT(0)
    } else {
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }
}
fn sessions() -> Vec<Arc<Inner>> {
    STATE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| {
                s.sessions
                    .values()
                    .filter_map(Weak::upgrade)
                    .filter(|s| s.active.load(Ordering::Acquire))
                    .collect()
            })
            .unwrap_or_default()
    })
}
fn set_error_all(error: String) {
    for s in sessions() {
        s.fail(error.clone());
    }
}
fn retire_owner(clear: bool) {
    let owner = STATE.with(|s| s.borrow_mut().as_mut().and_then(|s| s.owner.take()));
    if let Some((object, offer)) = owner {
        offer.cancel();
        offer.session.cancel_pending();
        if clear && is_current(&object) {
            let _writing = Writing::new();
            if let Err(error) = retain_rendered(&offer) {
                offer.session.fail(error.to_string());
            }
        }
    }
}
fn retain_rendered(offer: &RemoteOffer) -> Result<()> {
    let hwnd = WORKER
        .get()
        .and_then(|w| w.as_ref().ok())
        .map(|w| HWND(w.hwnd as _))
        .ok_or_else(|| anyhow!("剪贴板窗口已关闭"))?;
    let cache = lock(&offer.cached).clone();
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(opened, "剪贴板正在被其他程序占用");
    let result = (|| -> Result<()> {
        unsafe {
            EmptyClipboard()?;
        }
        for (id, bytes) in cache {
            if matches!(id, 14 | 0x8e) {
                unsafe {
                    let h = SetEnhMetaFileBits(&bytes);
                    if !h.is_invalid() && SetClipboardData(id, Some(HANDLE(h.0))).is_err() {
                        let _ = DeleteEnhMetaFile(Some(h));
                    }
                }
            } else {
                let mut medium = memory_medium(&bytes)?;
                if unsafe { SetClipboardData(id, Some(HANDLE(medium.u.hGlobal.0))) }.is_err() {
                    unsafe {
                        ReleaseStgMedium(&mut medium);
                    }
                }
            }
        }
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result
}
fn process(command: Command) {
    match command {
        Command::Activate(weak) => {
            if let Some(s) = weak.upgrade() {
                STATE.with(|v| {
                    if let Some(v) = v.borrow_mut().as_mut() {
                        v.sessions.insert(s.id, Arc::downgrade(&s));
                    }
                });
            }
        }
        Command::Remove(id) => {
            let clear = STATE.with(|v| {
                let mut v = v.borrow_mut();
                let Some(v) = v.as_mut() else {
                    return false;
                };
                v.sessions.remove(&id);
                v.published.remove(&id);
                v.tasks.retain(|(s, _), _| *s != id);
                v.owner.as_ref().is_some_and(|(_, o)| o.session.id == id)
            });
            if clear {
                retire_owner(true);
            }
        }
        Command::Offer(weak, epoch, formats) => {
            if let Some(s) = weak.upgrade().filter(|s| s.valid(epoch)) {
                if let Err(e) = set_offer(s.clone(), epoch, formats, None) {
                    s.fail(e.to_string());
                }
            }
        }
        Command::Text(weak, epoch, id, text) => {
            if let Some(s) = weak.upgrade().filter(|s| s.valid(epoch)) {
                let result = if text.is_empty() {
                    Err(anyhow!("空文本"))
                } else {
                    set_offer(
                        s.clone(),
                        epoch,
                        vec![ClipboardFormat {
                            id: 13,
                            name: String::new(),
                        }],
                        Some(formats::unicode(&text)),
                    )
                };
                let _ = s.emit(
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
            if let Some(s) = weak.upgrade().filter(|s| s.valid(epoch)) {
                if let Err(e) = serve(&s, epoch, id, kind) {
                    s.fail(e.to_string());
                }
            }
        }
    }
}
fn set_offer(
    s: Arc<Inner>,
    epoch: u64,
    formats: Vec<ClipboardFormat>,
    text: Option<Vec<u8>>,
) -> Result<()> {
    if formats.is_empty() {
        retire_owner(false);
        let _writing = Writing::new();
        clipboard_retry(|| unsafe { OleSetClipboard(None::<&IDataObject>) })?;
        *lock(&s.error) = None;
        STATE.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.sequence = unsafe { GetClipboardSequenceNumber() };
            }
        });
        return Ok(());
    }
    let platform = s.platform.load(Ordering::Acquire);
    let mut links: Vec<Format> = formats
        .into_iter()
        .filter_map(|f| formats::incoming(f, platform, s.file_allowed()))
        .collect();
    let descriptor = formats::register("FileGroupDescriptorW");
    let is_files = links.iter().any(|f| f.local == descriptor);
    if is_files && s.file_allowed() {
        let contents = formats::register("FileContents");
        if !links.iter().any(|f| f.local == contents) {
            links.push(Format {
                wire: ClipboardFormat {
                    id: contents,
                    name: "FileContents".into(),
                },
                local: contents,
            });
        }
    }
    if links.is_empty() {
        return Ok(());
    }
    retire_owner(false);
    let offer = Arc::new(RemoteOffer {
        session: s,
        epoch,
        alive: AtomicBool::new(true),
        formats: links,
        task: AtomicU32::new(0),
        descriptors: Mutex::new(None),
        reading: Mutex::new(()),
        cached: Mutex::new(HashMap::new()),
    });
    if let Some(text) = text {
        lock(&offer.cached).insert(13, text);
    }
    let object: IDataObject = RemoteData {
        offer: offer.clone(),
        async_mode: AtomicBool::new(true),
        in_operation: AtomicBool::new(false),
    }
    .into();
    let _writing = Writing::new();
    clipboard_retry(|| unsafe { OleSetClipboard(&object) })?;
    *lock(&offer.session.error) = None;
    STATE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.owner = Some((object, offer));
            s.sequence = unsafe { GetClipboardSequenceNumber() };
        }
    });
    Ok(())
}
fn local_change() -> Result<()> {
    if WRITING.with(Cell::get) {
        return Ok(());
    }
    if WORKER
        .get()
        .and_then(|w| w.as_ref().ok())
        .is_some_and(|w| unsafe { GetClipboardOwner() }.is_ok_and(|h| h.0 as isize == w.hwnd))
    {
        return Ok(());
    }
    if sessions().is_empty() {
        return Ok(());
    }
    let (owner, sequence) = STATE.with(|s| {
        let s = s.borrow();
        let s = s.as_ref().unwrap();
        (s.owner.as_ref().map(|(o, _)| o.clone()), s.sequence)
    });
    let now = unsafe { GetClipboardSequenceNumber() };
    if now == sequence {
        return Ok(());
    }
    if owner.as_ref().is_some_and(is_current) {
        return Ok(());
    }
    let object = clipboard_retry(|| unsafe { OleGetClipboard() })?;
    let enumeration = clipboard_retry(|| unsafe { object.EnumFormatEtc(DATADIR_GET.0 as u32) })?;
    let mut ids = Vec::new();
    let mut has_files = false;
    for _ in 0..256 {
        let mut f = [FORMATETC::default()];
        let mut got = 0;
        if unsafe { enumeration.Next(&mut f, Some(&mut got)) } != S_OK || got != 1 {
            break;
        }
        if !f[0].ptd.is_null() {
            unsafe { CoTaskMemFree(Some(f[0].ptd.cast())) };
        }
        let id = f[0].cfFormat as u32;
        let name = formats::name(id);
        if id == 15 || name == "FileGroupDescriptorW" {
            has_files = true;
        }
        if f[0].tymed & (TYMED_HGLOBAL.0 | TYMED_ENHMF.0) as u32 != 0
            && formats::supported(id, &name, false)
            && !ids.iter().any(|(i, _)| *i == id)
        {
            ids.push((id, name));
        }
    }
    if has_files {
        ids = vec![
            (
                formats::register("FileGroupDescriptorW"),
                "FileGroupDescriptorW".into(),
            ),
            (formats::register("FileContents"), "FileContents".into()),
        ];
    }
    let source = Arc::new(LocalSource {
        object,
        files: has_files,
    });
    if unsafe { GetClipboardSequenceNumber() } != now {
        return Ok(());
    }
    retire_owner(false);
    STATE.with(|v| {
        if let Some(v) = v.borrow_mut().as_mut() {
            v.sequence = now;
        }
    });
    for session in sessions() {
        let links = formats::outgoing(
            &ids,
            session.platform.load(Ordering::Acquire),
            session.file_allowed(),
        );
        if links.is_empty() && !ids.is_empty() {
            STATE.with(|v| {
                if let Some(v) = v.borrow_mut().as_mut() {
                    v.published.remove(&session.id);
                }
            });
            continue;
        }
        STATE.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.published
                    .insert(session.id, (source.clone(), links.clone()));
            }
        });
        session.emit(
            session.epoch.load(Ordering::Acquire),
            request(
                session.next(),
                ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                    formats: links.into_iter().map(|f| f.wire).collect(),
                    has_action: 0,
                    drag_drop_action: None,
                }),
            ),
        )?;
        *lock(&session.error) = None;
    }
    Ok(())
}
struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) }
    }
}
fn format_etc(id: u32, index: i32, tymed: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: id as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed,
    }
}
fn get_medium(o: &IDataObject, id: u32, index: i32, tymed: u32) -> Result<Medium> {
    Ok(Medium(clipboard_retry(|| unsafe {
        o.GetData(&format_etc(id, index, tymed))
    })?))
}
fn global_bytes(h: HGLOBAL) -> Result<Vec<u8>> {
    unsafe {
        let size = GlobalSize(h);
        ensure!(size <= MAX_DATA, "剪贴板内容超过内存预算");
        if size == 0 {
            return Ok(Vec::new());
        }
        let ptr = GlobalLock(h);
        ensure!(!ptr.is_null(), "无法锁定剪贴板内容");
        let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(h);
        Ok(bytes)
    }
}
fn memory_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}
struct LocalSource {
    object: IDataObject,
    files: bool,
}
impl LocalSource {
    fn data(&self, format: &Format, platform: i32) -> Result<Vec<u8>> {
        ensure!(!self.files, "文件数据必须通过文件请求获取");
        let kind = if matches!(format.local, 14 | 0x8e) {
            TYMED_ENHMF
        } else {
            TYMED_HGLOBAL
        };
        let medium = get_medium(&self.object, format.local, -1, kind.0 as u32)?;
        let data = if kind == TYMED_ENHMF {
            unsafe {
                let h = medium.0.u.hEnhMetaFile;
                let n = GetEnhMetaFileBits(h, None);
                ensure!(n as usize <= MAX_DATA, "图元文件过大");
                let mut data = vec![0; n as usize];
                ensure!(
                    GetEnhMetaFileBits(h, Some(&mut data)) == n,
                    "无法读取图元文件"
                );
                data
            }
        } else {
            global_bytes(unsafe { medium.0.u.hGlobal })?
        };
        formats::convert(data, format, platform, true)
    }
    fn files(&self) -> Result<LocalFiles> {
        ensure!(self.files, "本次剪贴板不含文件");
        if let Ok(m) = get_medium(&self.object, 15, -1, TYMED_HGLOBAL.0 as u32) {
            let drop = HDROP(unsafe { m.0.u.hGlobal.0 });
            let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
            ensure!(count as usize <= MAX_FILES, "文件数量过多");
            let mut items = Vec::new();
            for i in 0..count {
                let size = unsafe { DragQueryFileW(drop, i, None) };
                ensure!(size > 0 && size < 32768, "无效的文件路径");
                let mut buf = vec![0u16; size as usize + 1];
                unsafe {
                    DragQueryFileW(drop, i, Some(&mut buf));
                }
                let path = PathBuf::from(String::from_utf16(&buf[..size as usize])?);
                ensure!(
                    std::fs::symlink_metadata(&path)?.file_attributes() & 0x400 == 0,
                    "不传输重解析点或链接"
                );
                let root = dunce::canonicalize(&path)?;
                let name = path
                    .file_name()
                    .ok_or_else(|| anyhow!("不能复制磁盘根目录"))?
                    .to_string_lossy()
                    .into_owned();
                collect_file(&root, &root, &name, &mut items)?;
            }
            return Ok(LocalFiles {
                items,
                object: None,
            });
        }
        let medium = get_medium(
            &self.object,
            formats::register("FileGroupDescriptorW"),
            -1,
            TYMED_HGLOBAL.0 as u32,
        )?;
        let data = global_bytes(unsafe { medium.0.u.hGlobal })?;
        let mut desc = parse_descriptors(&data)?;
        for (index, desc) in desc.iter_mut().enumerate() {
            let start = 4 + index * 592;
            let flags = u32::from_le_bytes(data[start..start + 4].try_into().unwrap());
            if flags & 0x40 == 0 && desc.file_attributes & 0x10 == 0 {
                let content = get_medium(
                    &self.object,
                    formats::register("FileContents"),
                    index as i32,
                    (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0) as u32,
                )?;
                desc.file_size = if content.0.tymed == TYMED_HGLOBAL.0 as u32 {
                    unsafe { GlobalSize(content.0.u.hGlobal) as u64 }
                } else {
                    let stream = unsafe { content.0.u.pstm.as_ref().cloned() }
                        .ok_or_else(|| anyhow!("文件流不可用"))?;
                    let mut stat = STATSTG::default();
                    unsafe {
                        stream.Stat(&mut stat, STATFLAG_NONAME)?;
                    }
                    stat.cbSize
                };
            }
        }
        Ok(LocalFiles {
            items: desc
                .into_iter()
                .map(|desc| LocalFile {
                    desc,
                    path: None,
                    root: None,
                })
                .collect(),
            object: Some(self.object.clone()),
        })
    }
}
struct LocalFile {
    desc: ClipboardFileDescriptor,
    path: Option<PathBuf>,
    root: Option<PathBuf>,
}
struct LocalFiles {
    items: Vec<LocalFile>,
    object: Option<IDataObject>,
}
pub(super) fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.encode_utf16().count() < 260
        && !s.contains(['\0', ':'])
        && !s.starts_with(['\\', '/'])
        && s.split(['\\', '/']).all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && !p.ends_with(['.', ' '])
                && !p.chars().any(|c| c < ' ' || "<>\"|?*".contains(c))
                && !matches!(
                    p.split('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
}
fn collect_file(path: &Path, root: &Path, name: &str, items: &mut Vec<LocalFile>) -> Result<()> {
    ensure!(
        items.len() < MAX_FILES && safe_name(name),
        "文件数量或名称不支持"
    );
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(meta.file_attributes() & 0x400 == 0, "不传输重解析点或链接");
    let canonical = dunce::canonicalize(path)?;
    ensure!(canonical.starts_with(root), "文件超出复制范围");
    items.push(LocalFile {
        desc: ClipboardFileDescriptor {
            file_name: name.replace('/', "\\"),
            file_attributes: meta.file_attributes(),
            last_write_time: meta.last_write_time(),
            file_size: if meta.is_dir() { 0 } else { meta.file_size() },
        },
        path: Some(canonical.clone()),
        root: Some(root.into()),
    });
    if meta.is_dir() {
        for entry in std::fs::read_dir(canonical)? {
            let e = entry?;
            collect_file(
                &e.path(),
                root,
                &format!("{}\\{}", name, e.file_name().to_string_lossy()),
                items,
            )?;
        }
    }
    Ok(())
}
impl LocalFiles {
    fn read(&self, r: &ClipboardFileContentsRequest) -> Result<Vec<u8>> {
        let f = self
            .items
            .get(r.list_index as usize)
            .ok_or_else(|| anyhow!("无效的文件索引"))?;
        ensure!(r.requested_len as usize <= FILE_BLOCK, "文件读取请求过大");
        if r.flags == 1 {
            return Ok(f.desc.file_size.to_le_bytes().to_vec());
        }
        ensure!(
            r.flags == 2 && f.desc.file_attributes & 0x10 == 0,
            "无效的文件读取类型"
        );
        let n =
            (r.requested_len as u64).min(f.desc.file_size.saturating_sub(r.pos_offset)) as usize;
        if let Some(path) = &f.path {
            ensure!(
                dunce::canonicalize(path)?.starts_with(f.root.as_ref().unwrap()),
                "文件超出原始复制范围"
            );
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(7)
                .custom_flags(0x00200000)
                .open(path)?;
            ensure!(
                file.metadata()?.file_attributes() & 0x400 == 0,
                "文件已变为链接"
            );
            // Validate the opened handle, not just the path before opening (junction races).
            let mut final_name = vec![0u16; 32768];
            let size = unsafe {
                windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW(
                    HANDLE(file.as_raw_handle()),
                    &mut final_name,
                    windows::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED,
                )
            } as usize;
            ensure!(
                size > 0 && size < final_name.len(),
                "无法确认复制文件的实际路径"
            );
            let final_path =
                dunce::simplified(Path::new(&String::from_utf16(&final_name[..size])?))
                    .to_path_buf();
            ensure!(
                final_path.starts_with(f.root.as_ref().unwrap()),
                "打开的文件超出原始复制范围"
            );
            file.seek(SeekFrom::Start(r.pos_offset))?;
            let mut data = vec![0; n];
            let mut done = 0;
            while done < n {
                let got = file.read(&mut data[done..])?;
                if got == 0 {
                    break;
                }
                done += got;
            }
            data.truncate(done);
            return Ok(data);
        }
        let source = self
            .object
            .as_ref()
            .ok_or_else(|| anyhow!("文件来源已关闭"))?;
        let medium = get_medium(
            source,
            formats::register("FileContents"),
            r.list_index as i32,
            (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0) as u32,
        )?;
        if medium.0.tymed == TYMED_HGLOBAL.0 as u32 {
            let bytes = global_bytes(unsafe { medium.0.u.hGlobal })?;
            let start = usize::try_from(r.pos_offset)?.min(bytes.len());
            return Ok(bytes[start..(start + n).min(bytes.len())].to_vec());
        }
        let stream =
            unsafe { medium.0.u.pstm.as_ref().cloned() }.ok_or_else(|| anyhow!("没有文件流"))?;
        unsafe {
            stream.Seek(i64::try_from(r.pos_offset)?, STREAM_SEEK_SET, None)?;
            let mut bytes = vec![0; n];
            let mut got = 0;
            stream
                .Read(bytes.as_mut_ptr().cast(), n as u32, Some(&mut got))
                .ok()?;
            bytes.truncate(got as usize);
            Ok(bytes)
        }
    }
}
fn serve(s: &Arc<Inner>, epoch: u64, id: i64, kind: ClipboardRequestKind) -> Result<()> {
    match kind {
        ClipboardRequestKind::FormatDataAsk(ask) => {
            let published = STATE.with(|v| {
                v.borrow()
                    .as_ref()
                    .and_then(|v| v.published.get(&s.id).cloned())
            });
            let result = (|| -> Result<Vec<u8>> {
                let (source, formats) = published.ok_or_else(|| anyhow!("原剪贴板已失效"))?;
                let format = formats
                    .iter()
                    .find(|f| {
                        f.wire.id == ask.format_id
                            && (ask.format_name.is_empty() || ask.format_name == f.wire.name)
                    })
                    .ok_or_else(|| anyhow!("未发布该剪贴板格式"))?;
                source.data(format, s.platform.load(Ordering::Acquire))
            })();
            match result {
                Ok(data) if !data.is_empty() => {
                    s.enqueue(Outbound::Blocks(epoch, id, ask.block_key, data))
                }
                _ => s.emit(
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
        ClipboardRequestKind::FileDescListRequest(r) => {
            let source = STATE.with(|v| {
                v.borrow()
                    .as_ref()
                    .and_then(|v| v.published.get(&s.id).map(|p| p.0.clone()))
            });
            let result = (|| -> Result<Arc<LocalFiles>> {
                ensure!(s.file_allowed(), "文件剪贴板已关闭");
                let source = source.ok_or_else(|| anyhow!("原文件剪贴板已失效"))?;
                let files = Arc::new(source.files()?);
                ensure!(!files.items.is_empty(), "文件列表为空");
                Ok(files)
            })();
            match result {
                Ok(files) => {
                    let inserted = STATE.with(|v| {
                        let mut v = v.borrow_mut();
                        let v = v.as_mut().unwrap();
                        if v.tasks.len() >= 32 {
                            return false;
                        }
                        v.tasks.insert((s.id, r.task_id), files.clone());
                        true
                    });
                    if !inserted {
                        return s.emit(
                            epoch,
                            response(
                                id,
                                ClipboardResponseKind::FileDescListResponse(
                                    ClipboardFileDescriptorListResponse {
                                        task_id: r.task_id,
                                        segment_count: 0,
                                        err: 2,
                                    },
                                ),
                            ),
                        );
                    }
                    // Stay below the SDK's 512 KiB message ceiling even with long UTF-8 names.
                    let mut segments = Vec::<Vec<ClipboardFileDescriptor>>::new();
                    let mut current = Vec::new();
                    let mut bytes = 0;
                    for item in &files.items {
                        let size = item.desc.encoded_len() + 8;
                        if current.len() == 1500 || bytes + size > 450000 {
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
                        ClipboardResponseKind::FileDescListResponse(
                            ClipboardFileDescriptorListResponse {
                                task_id: r.task_id,
                                segment_count: segments.len() as u32,
                                err: 1,
                            },
                        ),
                    ));
                    for (i, items) in segments.into_iter().enumerate() {
                        messages.push_back(request(
                            s.next(),
                            ClipboardRequestKind::DescSegment(ClipboardFileDescriptorSegment {
                                task_id: r.task_id,
                                segment_id: i as u32 + 1,
                                file_descs: items,
                            }),
                        ));
                    }
                    s.enqueue(Outbound::Packets(epoch, messages))?;
                    Ok(())
                }
                Err(_) => s.emit(
                    epoch,
                    response(
                        id,
                        ClipboardResponseKind::FileDescListResponse(
                            ClipboardFileDescriptorListResponse {
                                task_id: r.task_id,
                                segment_count: 0,
                                err: 2,
                            },
                        ),
                    ),
                ),
            }
        }
        ClipboardRequestKind::FileContentsRequest(r) => {
            let files = STATE.with(|v| {
                v.borrow()
                    .as_ref()
                    .and_then(|v| v.tasks.get(&(s.id, r.task_id)).cloned())
            });
            let result = if s.file_allowed() {
                files
                    .ok_or_else(|| anyhow!("文件任务已失效"))
                    .and_then(|f| f.read(&r))
            } else {
                Err(anyhow!("文件剪贴板已关闭"))
            };
            let (err, data) = match result {
                Ok(data) => (1, data),
                Err(_) => (2, Vec::new()),
            };
            s.emit(
                epoch,
                response(
                    id,
                    ClipboardResponseKind::FileContentsResponse(ClipboardFileContentsResponse {
                        task_id: r.task_id,
                        err,
                        data,
                        pos_offset: r.pos_offset,
                        list_index: r.list_index,
                    }),
                ),
            )
        }
        ClipboardRequestKind::CancelRequest(r) => {
            STATE.with(|v| {
                if let Some(v) = v.borrow_mut().as_mut() {
                    v.tasks.remove(&(s.id, r.task_id));
                }
            });
            s.emit(
                epoch,
                response(
                    id,
                    ClipboardResponseKind::CancelResponse(ClipboardFileCancelResponse {
                        task_id: r.task_id,
                        err: 1,
                    }),
                ),
            )
        }
        _ => Ok(()),
    }
}
fn parse_descriptors(bytes: &[u8]) -> Result<Vec<ClipboardFileDescriptor>> {
    ensure!(bytes.len() >= 4, "无效文件描述");
    let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    ensure!(
        count <= MAX_FILES && 4 + count * 592 <= bytes.len(),
        "无效文件描述长度"
    );
    let mut out = Vec::with_capacity(count);
    for b in bytes[4..4 + count * 592].chunks_exact(592) {
        let u32at = |i| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let name: Vec<u16> = b[72..592]
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .take_while(|c| *c != 0)
            .collect();
        let name = String::from_utf16(&name)?;
        ensure!(safe_name(&name), "不支持的文件名称");
        out.push(ClipboardFileDescriptor {
            file_name: name,
            file_attributes: u32at(36),
            last_write_time: u32at(56) as u64 | ((u32at(60) as u64) << 32),
            file_size: u32at(68) as u64 | ((u32at(64) as u64) << 32),
        });
    }
    Ok(out)
}
fn descriptor_bytes(items: &[ClipboardFileDescriptor]) -> Result<Vec<u8>> {
    ensure!(items.len() <= MAX_FILES, "文件数量过多");
    let mut bytes = vec![0; 4 + items.len() * 592];
    bytes[..4].copy_from_slice(&(items.len() as u32).to_le_bytes());
    for (b, f) in bytes[4..].chunks_exact_mut(592).zip(items) {
        ensure!(safe_name(&f.file_name), "不安全的远端文件名称");
        b[..4].copy_from_slice(&0x4064u32.to_le_bytes());
        b[36..40].copy_from_slice(&f.file_attributes.to_le_bytes());
        b[56..64].copy_from_slice(&f.last_write_time.to_le_bytes());
        b[64..68].copy_from_slice(&((f.file_size >> 32) as u32).to_le_bytes());
        b[68..72].copy_from_slice(&(f.file_size as u32).to_le_bytes());
        for (out, c) in b[72..]
            .chunks_exact_mut(2)
            .zip(f.file_name.replace('/', "\\").encode_utf16())
        {
            out.copy_from_slice(&c.to_le_bytes());
        }
    }
    Ok(bytes)
}
struct RemoteOffer {
    session: Arc<Inner>,
    epoch: u64,
    alive: AtomicBool,
    formats: Vec<Format>,
    task: AtomicU32,
    descriptors: Mutex<Option<Vec<ClipboardFileDescriptor>>>,
    reading: Mutex<()>,
    cached: Mutex<HashMap<u32, Vec<u8>>>,
}
impl RemoteOffer {
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
        if let Some(v) = existing {
            return Ok(v);
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
    }
}
fn com_error(e: anyhow::Error) -> windows::core::Error {
    windows::core::Error::new(E_FAIL, e.to_string())
}
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct RemoteData {
    offer: Arc<RemoteOffer>,
    async_mode: AtomicBool,
    in_operation: AtomicBool,
}
impl IDataObjectAsyncCapability_Impl for RemoteData_Impl {
    fn SetAsyncMode(&self, value: BOOL) -> windows::core::Result<()> {
        self.async_mode.store(value.as_bool(), Ordering::Release);
        Ok(())
    }
    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.async_mode.load(Ordering::Acquire).into())
    }
    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows::core::Result<()> {
        self.offer.valid().map_err(com_error)?;
        self.in_operation.store(true, Ordering::Release);
        Ok(())
    }
    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Acquire).into())
    }
    fn EndOperation(&self, result: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows::core::Result<()> {
        self.in_operation.store(false, Ordering::Release);
        if result.is_err() {
            self.offer.session.cancel_pending();
            let task = self.offer.task.swap(0, Ordering::AcqRel);
            *lock(&self.offer.descriptors) = None;
            if task != 0 {
                let _ = self.offer.session.enqueue(Outbound::Cleanup(request(
                    self.offer.session.next(),
                    ClipboardRequestKind::CancelRequest(ClipboardFileCancelRequest {
                        task_id: task,
                    }),
                )));
            }
        }
        Ok(())
    }
}
impl IDataObject_Impl for RemoteData_Impl {
    fn GetData(&self, p: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        self.QueryGetData(p).ok()?;
        let f = unsafe { &*p };
        self.offer.valid().map_err(com_error)?;
        let id = f.cfFormat as u32;
        if id == formats::register("FileContents") {
            let list = self.offer.list().map_err(com_error)?;
            let d = list
                .get(f.lindex as usize)
                .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            let stream: IStream = RemoteStream {
                offer: self.offer.clone(),
                index: f.lindex as u32,
                size: d.file_size,
                position: Mutex::new(0),
            }
            .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        let data = if id == formats::register("FileGroupDescriptorW") {
            descriptor_bytes(&self.offer.list().map_err(com_error)?).map_err(com_error)?
        } else {
            let cache = lock(&self.offer.cached).get(&id).cloned();
            if let Some(v) = cache {
                v
            } else {
                let f = self
                    .offer
                    .formats
                    .iter()
                    .find(|f| f.local == id)
                    .ok_or_else(|| windows::core::Error::from(DV_E_FORMATETC))?;
                let data = self
                    .offer
                    .session
                    .data(self.offer.epoch, &f.wire)
                    .and_then(|d| {
                        formats::convert(
                            d,
                            f,
                            self.offer.session.platform.load(Ordering::Acquire),
                            false,
                        )
                    })
                    .map_err(com_error)?;
                self.offer.valid().map_err(com_error)?;
                {
                    let mut cache = lock(&self.offer.cached);
                    if cache.values().map(Vec::len).sum::<usize>() + data.len() <= MAX_DATA {
                        cache.insert(id, data.clone());
                    }
                }
                data
            }
        };
        if matches!(id, 14 | 0x8e) {
            let metafile = unsafe { SetEnhMetaFileBits(&data) };
            if metafile.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            return Ok(STGMEDIUM {
                tymed: TYMED_ENHMF.0 as u32,
                u: STGMEDIUM_0 {
                    hEnhMetaFile: metafile,
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        memory_medium(&data)
    }
    fn QueryGetData(&self, p: *const FORMATETC) -> HRESULT {
        if p.is_null() {
            return E_POINTER;
        }
        let f = unsafe { &*p };
        if self.offer.valid().is_err() {
            return E_FAIL;
        }
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        let id = f.cfFormat as u32;
        if !self.offer.formats.iter().any(|v| v.local == id) {
            return DV_E_FORMATETC;
        }
        let kind = if id == formats::register("FileContents") {
            if f.lindex < 0 {
                return DV_E_LINDEX;
            }
            TYMED_ISTREAM
        } else if matches!(id, 14 | 0x8e) {
            TYMED_ENHMF
        } else {
            TYMED_HGLOBAL
        };
        if f.tymed & kind.0 as u32 == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            unsafe {
                (*out).ptd = std::ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }
    fn SetData(
        &self,
        _: *const FORMATETC,
        _: *const STGMEDIUM,
        _: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, dir: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dir != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let formats: Vec<_> = self
            .offer
            .formats
            .iter()
            .map(|f| {
                format_etc(
                    f.local,
                    -1,
                    if f.local == formats::register("FileContents") {
                        TYMED_ISTREAM.0
                    } else if matches!(f.local, 14 | 0x8e) {
                        TYMED_ENHMF.0
                    } else {
                        TYMED_HGLOBAL.0
                    } as u32,
                )
            })
            .collect();
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}
#[implement(IStream)]
struct RemoteStream {
    offer: Arc<RemoteOffer>,
    index: u32,
    size: u64,
    position: Mutex<u64>,
}
impl ISequentialStream_Impl for RemoteStream_Impl {
    fn Read(&self, p: *mut std::ffi::c_void, length: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if p.is_null() && length != 0 {
            return E_POINTER;
        }
        let result = (|| -> Result<u32> {
            self.offer.valid()?;
            let pos = *lock(&self.position);
            if pos >= self.size || length == 0 {
                return Ok(0);
            }
            self.offer.list()?;
            let _busy = self.offer.enter_read()?;
            let wanted = (length as u64).min(self.size - pos);
            let mut done = 0u64;
            while done < wanted {
                let data = self.offer.session.read_file(
                    self.offer.epoch,
                    self.offer.task(),
                    self.index,
                    pos + done,
                    (wanted - done).min(FILE_BLOCK as u64) as usize,
                    2,
                )?;
                self.offer.valid()?;
                if data.is_empty() {
                    break;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        p.cast::<u8>().add(done as usize),
                        data.len(),
                    );
                }
                done += data.len() as u64;
                *lock(&self.position) = pos + done;
                if !read.is_null() {
                    unsafe {
                        *read = done as u32;
                    }
                }
            }
            Ok(done as u32)
        })();
        match result {
            Ok(done) => {
                if !read.is_null() {
                    unsafe {
                        *read = done;
                    }
                }
                if done == length { S_OK } else { S_FALSE }
            }
            Err(e) => {
                self.offer.session.fail(e.to_string());
                E_FAIL
            }
        }
    }
    fn Write(&self, _: *const std::ffi::c_void, _: u32, w: *mut u32) -> HRESULT {
        if !w.is_null() {
            unsafe {
                *w = 0;
            }
        }
        STG_E_ACCESSDENIED
    }
}
impl IStream_Impl for RemoteStream_Impl {
    fn Seek(&self, delta: i64, origin: STREAM_SEEK, out: *mut u64) -> windows::core::Result<()> {
        let mut pos = lock(&self.position);
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *pos,
            STREAM_SEEK_END => self.size,
            _ => return Err(E_INVALIDARG.into()),
        };
        let new = base
            .checked_add_signed(delta)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        *pos = new;
        if !out.is_null() {
            unsafe {
                *out = new;
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(
        &self,
        target: Ref<IStream>,
        count: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> windows::core::Result<()> {
        let target = target.ok()?;
        let mut total = 0;
        let mut bytes = vec![0; FILE_BLOCK];
        while total < count {
            let n = (count - total).min(FILE_BLOCK as u64) as u32;
            let mut got = 0;
            self.Read(bytes.as_mut_ptr().cast(), n, &mut got).ok()?;
            if got == 0 {
                break;
            }
            let mut sent = 0;
            unsafe {
                target
                    .Write(bytes.as_ptr().cast(), got, Some(&mut sent))
                    .ok()?;
            }
            if sent != got {
                return Err(STG_E_WRITEFAULT.into());
            }
            total += got as u64;
        }
        if !read.is_null() {
            unsafe {
                *read = total;
            }
        }
        if !written.is_null() {
            unsafe {
                *written = total;
            }
        }
        Ok(())
    }
    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, p: *mut STATSTG, _: &STATFLAG) -> windows::core::Result<()> {
        if p.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *p = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.size,
                grfMode: STGM_READ,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        Ok(RemoteStream {
            offer: self.offer.clone(),
            index: self.index,
            size: self.size,
            position: Mutex::new(*lock(&self.position)),
        }
        .into())
    }
}
