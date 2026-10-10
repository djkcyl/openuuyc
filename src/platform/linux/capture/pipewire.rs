//! A PipeWire video stream consumer, loaded at run time.
//!
//! `libpipewire-0.3.so.0` is opened with dlopen rather than linked: every
//! desktop with the ScreenCast portal has it, but its development headers (and
//! the Rust bindings generated from them) track newer releases than long-term
//! distributions ship. The few entry points used here have been stable since
//! PipeWire 0.3, and the SPA format descriptions are written by hand.
//!
//! Frames are requested in shared memory (MemPtr/MemFd), read on PipeWire's
//! loop thread into BGRA and handed to the capture thread as the latest
//! picture; DMA-BUF import belongs with a GPU encoder.
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// --- SPA / PipeWire ABI (spa-0.2, pipewire-0.3) --------------------------------

#[repr(C)]
struct SpaList {
    next: *mut c_void,
    prev: *mut c_void,
}
#[repr(C)]
struct SpaCallbacks {
    funcs: *const c_void,
    data: *mut c_void,
}
#[repr(C)]
struct SpaHook {
    link: SpaList,
    cb: SpaCallbacks,
    removed: Option<unsafe extern "C" fn(*mut SpaHook)>,
    private: *mut c_void,
}
type Callback = Option<unsafe extern "C" fn(*mut c_void)>;
#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Callback,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    process: Callback,
    drained: Callback,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Callback,
}
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
}
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut SpaMeta,
    datas: *mut SpaData,
}
#[repr(C)]
struct SpaMeta {
    kind: u32,
    size: u32,
    data: *mut c_void,
}
#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}
#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}
#[repr(C)]
struct SpaMetaRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

const STREAM_EVENTS_VERSION: u32 = 2;
const DIRECTION_INPUT: u32 = 0;
const FLAG_AUTOCONNECT: u32 = 1 << 0;
const FLAG_MAP_BUFFERS: u32 = 1 << 2;
const STATE_ERROR: c_int = -1;
const STATE_UNCONNECTED: c_int = 0;
const STATE_STREAMING: c_int = 3;

const TYPE_ID: u32 = 3;
const TYPE_INT: u32 = 4;
const TYPE_RECTANGLE: u32 = 10;
const TYPE_FRACTION: u32 = 11;
const TYPE_OBJECT: u32 = 15;
const TYPE_CHOICE: u32 = 19;
const OBJECT_FORMAT: u32 = 0x40003;
const OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const OBJECT_PARAM_META: u32 = 0x40005;
const PARAM_ENUM_FORMAT: u32 = 3;
const PARAM_FORMAT: u32 = 4;
const PARAM_BUFFERS: u32 = 5;
const PARAM_META: u32 = 6;
const FORMAT_MEDIA_TYPE: u32 = 1;
const FORMAT_MEDIA_SUBTYPE: u32 = 2;
const FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const FORMAT_VIDEO_SIZE: u32 = 0x20003;
const FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const MEDIA_TYPE_VIDEO: u32 = 2;
const MEDIA_SUBTYPE_RAW: u32 = 1;
const VIDEO_RGBX: u32 = 7;
const VIDEO_BGRX: u32 = 8;
const VIDEO_RGBA: u32 = 11;
const VIDEO_BGRA: u32 = 12;
const BUFFERS_DATA_TYPE: u32 = 6;
const META_TYPE: u32 = 1;
const META_SIZE: u32 = 2;
const META_VIDEO_CROP: u32 = 2;
const DATA_MEM_PTR: u32 = 1;
const DATA_MEM_FD: u32 = 2;
const CHOICE_RANGE: u32 = 1;
const CHOICE_ENUM: u32 = 3;
const CHOICE_FLAGS: u32 = 4;

struct Api {
    _library: libloading::Library,
    thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    context_destroy: unsafe extern "C" fn(*mut c_void),
    context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    properties_new: unsafe extern "C" fn(*const c_char, ...) -> *mut c_void,
    stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    stream_add_listener:
        unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const StreamEvents, *mut c_void),
    stream_connect:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const c_void, u32) -> c_int,
    stream_update_params: unsafe extern "C" fn(*mut c_void, *mut *const c_void, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    stream_destroy: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn load() -> Result<Api> {
    // SAFETY: loading the system PipeWire client library; the symbols below
    // are declared with their C signatures from pipewire-0.3.
    unsafe {
        let library = libloading::Library::new("libpipewire-0.3.so.0")
            .context("没有找到 PipeWire（libpipewire-0.3.so.0）")?;
        macro_rules! symbol {
            ($name:literal) => {
                *library
                    .get(concat!($name, "\0").as_bytes())
                    .with_context(|| format!("PipeWire 缺少 {}", $name))?
            };
        }
        let init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) = symbol!("pw_init");
        init(std::ptr::null_mut(), std::ptr::null_mut());
        Ok(Api {
            thread_loop_new: symbol!("pw_thread_loop_new"),
            thread_loop_get_loop: symbol!("pw_thread_loop_get_loop"),
            thread_loop_start: symbol!("pw_thread_loop_start"),
            thread_loop_stop: symbol!("pw_thread_loop_stop"),
            thread_loop_destroy: symbol!("pw_thread_loop_destroy"),
            thread_loop_lock: symbol!("pw_thread_loop_lock"),
            thread_loop_unlock: symbol!("pw_thread_loop_unlock"),
            context_new: symbol!("pw_context_new"),
            context_destroy: symbol!("pw_context_destroy"),
            context_connect_fd: symbol!("pw_context_connect_fd"),
            core_disconnect: symbol!("pw_core_disconnect"),
            properties_new: symbol!("pw_properties_new"),
            stream_new: symbol!("pw_stream_new"),
            stream_add_listener: symbol!("pw_stream_add_listener"),
            stream_connect: symbol!("pw_stream_connect"),
            stream_update_params: symbol!("pw_stream_update_params"),
            stream_dequeue_buffer: symbol!("pw_stream_dequeue_buffer"),
            stream_queue_buffer: symbol!("pw_stream_queue_buffer"),
            stream_disconnect: symbol!("pw_stream_disconnect"),
            stream_destroy: symbol!("pw_stream_destroy"),
            _library: library,
        })
    }
}

// --- SPA pod encoding ------------------------------------------------------------

/// Builds one SPA pod in an 8-byte aligned buffer, the layout `spa_pod_builder`
/// produces: every pod is `{u32 size, u32 type}` then its body, padded to 8.
#[derive(Default)]
struct Pod(Vec<u8>);
impl Pod {
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }
    fn pad(&mut self) {
        while self.0.len() % 8 != 0 {
            self.0.push(0);
        }
    }
    fn value(&mut self, kind: u32, body: &[u32]) {
        self.u32((body.len() * 4) as u32);
        self.u32(kind);
        for word in body {
            self.u32(*word);
        }
        self.pad();
    }
    /// A choice of `values`, each `child_words` 32-bit words of `child_type`.
    fn choice(&mut self, choice: u32, child_type: u32, child_words: usize, values: &[u32]) {
        self.u32((16 + values.len() * 4) as u32);
        self.u32(TYPE_CHOICE);
        self.u32(choice);
        self.u32(0);
        self.u32((child_words * 4) as u32);
        self.u32(child_type);
        for word in values {
            self.u32(*word);
        }
        self.pad();
    }
    /// An object with properties written by `props`, each started with `key`.
    fn object(kind: u32, id: u32, props: impl FnOnce(&mut Self)) -> Self {
        let mut pod = Self::default();
        pod.u32(0);
        pod.u32(TYPE_OBJECT);
        pod.u32(kind);
        pod.u32(id);
        props(&mut pod);
        let size = (pod.0.len() - 8) as u32;
        pod.0[..4].copy_from_slice(&size.to_ne_bytes());
        pod
    }
    fn key(&mut self, key: u32) {
        self.u32(key);
        self.u32(0);
    }
    /// The bytes, copied into 8-byte aligned storage as SPA requires.
    fn aligned(&self) -> Vec<u64> {
        let mut words = vec![0u64; self.0.len().div_ceil(8)];
        // SAFETY: `words` has at least as many bytes as the pod.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.0.as_ptr(),
                words.as_mut_ptr().cast::<u8>(),
                self.0.len(),
            );
        }
        words
    }
}

fn enum_format() -> Pod {
    Pod::object(OBJECT_FORMAT, PARAM_ENUM_FORMAT, |p| {
        p.key(FORMAT_MEDIA_TYPE);
        p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
        p.key(FORMAT_MEDIA_SUBTYPE);
        p.value(TYPE_ID, &[MEDIA_SUBTYPE_RAW]);
        p.key(FORMAT_VIDEO_FORMAT);
        p.choice(
            CHOICE_ENUM,
            TYPE_ID,
            1,
            &[VIDEO_BGRX, VIDEO_BGRX, VIDEO_BGRA, VIDEO_RGBX, VIDEO_RGBA],
        );
        p.key(FORMAT_VIDEO_SIZE);
        p.choice(
            CHOICE_RANGE,
            TYPE_RECTANGLE,
            2,
            &[1920, 1080, 1, 1, 16384, 16384],
        );
        p.key(FORMAT_VIDEO_FRAMERATE);
        p.choice(CHOICE_RANGE, TYPE_FRACTION, 2, &[60, 1, 0, 1, 1000, 1]);
    })
}

fn buffers() -> Pod {
    Pod::object(OBJECT_PARAM_BUFFERS, PARAM_BUFFERS, |p| {
        p.key(BUFFERS_DATA_TYPE);
        p.choice(
            CHOICE_FLAGS,
            TYPE_INT,
            1,
            &[(1 << DATA_MEM_PTR) | (1 << DATA_MEM_FD)],
        );
    })
}

fn crop_meta() -> Pod {
    Pod::object(OBJECT_PARAM_META, PARAM_META, |p| {
        p.key(META_TYPE);
        p.value(TYPE_ID, &[META_VIDEO_CROP]);
        p.key(META_SIZE);
        p.value(TYPE_INT, &[size_of::<SpaMetaRegion>() as u32]);
    })
}

/// The negotiated `(video format, width, height)` from a Format object.
///
/// # Safety
/// `pod` must point at a valid SPA pod.
unsafe fn parse_format(pod: *const c_void) -> Option<(u32, u32, u32)> {
    let read =
        |offset: usize| unsafe { pod.cast::<u8>().add(offset).cast::<u32>().read_unaligned() };
    let size = read(0) as usize;
    if read(4) != TYPE_OBJECT || size < 8 {
        return None;
    }
    let end = 8 + size;
    let mut offset = 16;
    let (mut format, mut width, mut height) = (None, None, None);
    while offset + 16 <= end {
        let key = read(offset);
        let value_size = read(offset + 8) as usize;
        let mut value_type = read(offset + 12);
        let mut value = offset + 16;
        if value_type == TYPE_CHOICE {
            // A fixed property may still come wrapped in a None choice; its
            // first element is the value.
            value_type = read(value + 12);
            value += 16;
        }
        match (key, value_type) {
            (FORMAT_VIDEO_FORMAT, TYPE_ID) => format = Some(read(value)),
            (FORMAT_VIDEO_SIZE, TYPE_RECTANGLE) => {
                width = Some(read(value));
                height = Some(read(value + 4));
            }
            _ => {}
        }
        offset += 16 + value_size.next_multiple_of(8);
    }
    Some((format?, width?, height?))
}

// --- Stream ----------------------------------------------------------------------

/// One captured picture as the stream delivered it, converted to BGRA.
pub(super) struct Picture {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[derive(Default)]
struct State {
    picture: Option<Picture>,
    sequence: u64,
    format: Option<(u32, u32, u32)>,
    streaming: bool,
    error: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// What the loop thread's callbacks reach through their `data` pointer.
struct Callbacks {
    api: &'static Api,
    stream: *mut c_void,
    shared: Arc<Shared>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let mut guard = lock(&callbacks.shared.state);
    match state {
        STATE_STREAMING => guard.streaming = true,
        STATE_ERROR => {
            let text = if error.is_null() {
                "未知错误".to_owned()
            } else {
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            guard.error = Some(format!("PipeWire 画面流出错：{text}"));
        }
        STATE_UNCONNECTED if guard.streaming => {
            guard.error = Some("屏幕共享已结束".into());
        }
        _ => {}
    }
    drop(guard);
    callbacks.shared.changed.notify_all();
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    if param.is_null() || id != PARAM_FORMAT {
        return;
    }
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let format = unsafe { parse_format(param) };
    lock(&callbacks.shared.state).format = format;
    tracing::debug!(?format, "PipeWire screencast format negotiated");
    // Ask for buffers in memory the CPU can read, and for the crop region.
    let buffers = buffers().aligned();
    let meta = crop_meta().aligned();
    let mut params = [buffers.as_ptr().cast::<c_void>(), meta.as_ptr().cast()];
    unsafe {
        (callbacks.api.stream_update_params)(callbacks.stream, params.as_mut_ptr(), 2);
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let api = callbacks.api;
    // Only the newest buffer matters; return any older ones unread.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let buffer = unsafe { (api.stream_dequeue_buffer)(callbacks.stream) };
        if buffer.is_null() {
            break;
        }
        if !newest.is_null() {
            unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
        }
        newest = buffer;
    }
    if newest.is_null() {
        return;
    }
    let format = lock(&callbacks.shared.state).format;
    let picture = format.and_then(|format| unsafe { read_picture((*newest).buffer, format) });
    unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
    if let Some(picture) = picture {
        let mut state = lock(&callbacks.shared.state);
        state.picture = Some(picture);
        state.sequence += 1;
        drop(state);
        callbacks.shared.changed.notify_all();
    }
}

/// Copy a buffer's pixels out as tightly packed BGRA with opaque alpha.
///
/// # Safety
/// `buffer` must be a buffer the stream just dequeued.
unsafe fn read_picture(
    buffer: *mut SpaBuffer,
    (format, width, height): (u32, u32, u32),
) -> Option<Picture> {
    let buffer = unsafe { &*buffer };
    if buffer.n_datas == 0 || buffer.datas.is_null() {
        return None;
    }
    let data = unsafe { &*buffer.datas };
    if data.data.is_null() || data.chunk.is_null() {
        // DMA-BUF, which was not offered, or an empty buffer.
        return None;
    }
    let chunk = unsafe { &*data.chunk };
    let stride = if chunk.stride > 0 {
        chunk.stride as usize
    } else {
        width as usize * 4
    };
    let (mut x, mut y, mut w, mut h) = (0usize, 0usize, width as usize, height as usize);
    for index in 0..buffer.n_metas as usize {
        let meta = unsafe { &*buffer.metas.add(index) };
        if meta.kind == META_VIDEO_CROP
            && meta.size as usize >= size_of::<SpaMetaRegion>()
            && !meta.data.is_null()
        {
            let region = unsafe { &*meta.data.cast::<SpaMetaRegion>() };
            if region.width > 0 && region.height > 0 {
                x = region.x.max(0) as usize;
                y = region.y.max(0) as usize;
                w = (region.width as usize).min(width as usize - x.min(width as usize));
                h = (region.height as usize).min(height as usize - y.min(height as usize));
            }
        }
    }
    let base = (chunk.offset % data.maxsize.max(1)) as usize;
    let needed = base + (y + h).saturating_sub(1) * stride + (x + w) * 4;
    if w == 0 || h == 0 || needed > data.maxsize as usize {
        return None;
    }
    let source =
        unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), data.maxsize as usize) };
    let swap = matches!(format, VIDEO_RGBX | VIDEO_RGBA);
    let mut pixels = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = base + (y + row) * stride + x * 4;
        pixels.extend_from_slice(&source[start..start + w * 4]);
    }
    for pixel in pixels.chunks_exact_mut(4) {
        if swap {
            pixel.swap(0, 2);
        }
        pixel[3] = 0xff;
    }
    Some(Picture {
        width: w as u32,
        height: h as u32,
        pixels,
    })
}

/// A video stream from one PipeWire node, on its own loop thread.
pub(super) struct Stream {
    api: &'static Api,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    _hook: Box<SpaHook>,
    _events: Box<StreamEvents>,
    callbacks: *mut Callbacks,
    shared: Arc<Shared>,
    delivered: u64,
}

// The raw handles are only used under the thread loop's lock or after it stopped.
unsafe impl Send for Stream {}

impl Stream {
    /// Connect to `node` on the PipeWire remote behind `fd` (from the portal).
    pub fn connect(fd: std::os::fd::OwnedFd, node: u32) -> Result<Self> {
        use std::os::fd::IntoRawFd;
        let api = api()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let name = CString::new("openuuyc-capture")?;
        // SAFETY: the PipeWire calls follow the documented setup sequence; every
        // handle is checked and owned by the returned value, which tears them
        // down in reverse order.
        unsafe {
            let thread_loop = (api.thread_loop_new)(name.as_ptr(), std::ptr::null());
            ensure!(!thread_loop.is_null(), "无法创建 PipeWire 线程");
            let context = (api.context_new)(
                (api.thread_loop_get_loop)(thread_loop),
                std::ptr::null_mut(),
                0,
            );
            if context.is_null() {
                (api.thread_loop_destroy)(thread_loop);
                bail!("无法创建 PipeWire 上下文");
            }
            let mut this = Self {
                api,
                thread_loop,
                context,
                core: std::ptr::null_mut(),
                stream: std::ptr::null_mut(),
                _hook: Box::new(std::mem::zeroed()),
                _events: Box::new(StreamEvents {
                    version: STREAM_EVENTS_VERSION,
                    destroy: None,
                    state_changed: Some(on_state_changed),
                    control_info: None,
                    io_changed: None,
                    param_changed: Some(on_param_changed),
                    add_buffer: None,
                    remove_buffer: None,
                    process: Some(on_process),
                    drained: None,
                    command: None,
                    trigger_done: None,
                }),
                callbacks: std::ptr::null_mut(),
                shared: shared.clone(),
                delivered: 0,
            };
            ensure!(
                (api.thread_loop_start)(thread_loop) >= 0,
                "无法启动 PipeWire 线程"
            );
            (api.thread_loop_lock)(thread_loop);
            let result = (|| -> Result<()> {
                this.core =
                    (api.context_connect_fd)(context, fd.into_raw_fd(), std::ptr::null_mut(), 0);
                ensure!(!this.core.is_null(), "无法连接屏幕共享的 PipeWire 远端");
                let key = |s: &str| CString::new(s).unwrap();
                let (media_type, video, category, capture, role, screen) = (
                    key("media.type"),
                    key("Video"),
                    key("media.category"),
                    key("Capture"),
                    key("media.role"),
                    key("Screen"),
                );
                let props = (api.properties_new)(
                    media_type.as_ptr(),
                    video.as_ptr(),
                    category.as_ptr(),
                    capture.as_ptr(),
                    role.as_ptr(),
                    screen.as_ptr(),
                    std::ptr::null::<c_char>(),
                );
                this.stream = (api.stream_new)(this.core, name.as_ptr(), props);
                ensure!(!this.stream.is_null(), "无法创建 PipeWire 画面流");
                this.callbacks = Box::into_raw(Box::new(Callbacks {
                    api,
                    stream: this.stream,
                    shared,
                }));
                (api.stream_add_listener)(
                    this.stream,
                    &mut *this._hook,
                    &*this._events,
                    this.callbacks.cast(),
                );
                let format = enum_format().aligned();
                let mut params = [format.as_ptr().cast::<c_void>()];
                let connected = (api.stream_connect)(
                    this.stream,
                    DIRECTION_INPUT,
                    node,
                    FLAG_AUTOCONNECT | FLAG_MAP_BUFFERS,
                    params.as_mut_ptr(),
                    1,
                );
                ensure!(connected >= 0, "连接 PipeWire 画面流失败：{connected}");
                Ok(())
            })();
            (api.thread_loop_unlock)(thread_loop);
            result.map(|()| this)
        }
    }

    /// The next picture newer than the last one returned, waiting up to
    /// `timeout`. `None` when nothing new arrived.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(error) = &state.error {
                bail!("{error}");
            }
            if state.sequence != self.delivered
                && let Some(picture) = state.picture.take()
            {
                self.delivered = state.sequence;
                return Ok(Some(picture));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let api = self.api;
        // SAFETY: teardown in reverse order of setup; the loop is stopped
        // before the callback data it could still call into is freed.
        unsafe {
            (api.thread_loop_lock)(self.thread_loop);
            if !self.stream.is_null() {
                (api.stream_disconnect)(self.stream);
                (api.stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (api.core_disconnect)(self.core);
            }
            (api.thread_loop_unlock)(self.thread_loop);
            (api.thread_loop_stop)(self.thread_loop);
            (api.context_destroy)(self.context);
            (api.thread_loop_destroy)(self.thread_loop);
            if !self.callbacks.is_null() {
                drop(Box::from_raw(self.callbacks));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_parses_back() {
        // A fixed format written the way enum_format writes a choice's first
        // element must be read by parse_format.
        let pod = Pod::object(OBJECT_FORMAT, PARAM_FORMAT, |p| {
            p.key(FORMAT_MEDIA_TYPE);
            p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
            p.key(FORMAT_VIDEO_FORMAT);
            p.value(TYPE_ID, &[VIDEO_BGRX]);
            p.key(FORMAT_VIDEO_SIZE);
            p.value(TYPE_RECTANGLE, &[2560, 1440]);
        })
        .aligned();
        assert_eq!(
            unsafe { parse_format(pod.as_ptr().cast()) },
            Some((VIDEO_BGRX, 2560, 1440))
        );
        let choices = enum_format().aligned();
        let size = unsafe { choices.as_ptr().cast::<u32>().read() } as usize;
        assert_eq!((size + 8) % 8, 0);
    }
}
