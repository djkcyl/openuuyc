//! Device-scoped clipboard RPC with a process-wide platform adapter.
mod formats;
#[cfg(windows)]
mod native;
#[cfg(not(windows))]
#[path = "clipboard/native_linux.rs"]
mod native;
mod protocol;
use anyhow::{Result, anyhow, bail, ensure};
use prost::Message;
use protocol::*;
use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;

const BLOCK: usize = 128 * 1024;
const FILE_BLOCK: usize = 512000;
const MAX_DATA: usize = 128 * 1024 * 1024;
const MAX_FILES: usize = 100_000;
const MAX_PENDING: usize = 64;
pub(crate) fn shutdown() {
    native::shutdown();
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, PartialEq, Message)]
struct Header {
    #[prost(int64, tag = "1")]
    id: i64,
}
#[derive(Clone, PartialEq, Message)]
struct Request {
    #[prost(message, optional, tag = "1")]
    header: Option<Header>,
    #[prost(message, optional, tag = "9")]
    clip: Option<ClipboardRequest>,
    #[prost(message, optional, tag = "10")]
    text: Option<ClipboardTextChangeRequest>,
}
#[derive(Clone, PartialEq, Message)]
struct Response {
    #[prost(message, optional, tag = "1")]
    header: Option<Header>,
    #[prost(message, optional, tag = "5")]
    clip: Option<ClipboardResponse>,
    #[prost(message, optional, tag = "6")]
    text: Option<ClipboardTextChangeResponse>,
}
#[derive(Clone, PartialEq, Message)]
struct Envelope {
    #[prost(message, optional, tag = "21")]
    request: Option<Request>,
    #[prost(message, optional, tag = "22")]
    response: Option<Response>,
}
#[derive(Clone)]
pub(crate) struct Clipboard(Arc<Inner>);
pub(crate) struct Snapshot {
    pub enabled: bool,
    pub files: bool,
    pub active: bool,
    pub error: Option<String>,
}
struct Inner {
    id: u64,
    enabled: AtomicBool,
    files: AtomicBool,
    active: AtomicBool,
    allowed_files: AtomicBool,
    epoch: AtomicU64,
    platform: AtomicI32,
    serial: AtomicU32,
    sender: Mutex<Option<mpsc::Sender<Queued>>>,
    pending: Mutex<HashMap<i64, Pending>>,
    error: Mutex<Option<String>>,
}
static SEND_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct SendBudget(usize);
impl Drop for SendBudget {
    fn drop(&mut self) {
        SEND_BYTES.fetch_sub(self.0, Ordering::AcqRel);
    }
}
struct Queued {
    item: Outbound,
    budget: SendBudget,
}
enum Outbound {
    Packets(u64, std::collections::VecDeque<Envelope>),
    Cleanup(Envelope),
    Message(u64, Envelope),
    Blocks(u64, i64, String, Vec<u8>),
}
enum Value {
    Data(Vec<u8>),
    Files(Vec<ClipboardFileDescriptor>),
}
struct Waiter {
    value: Mutex<Option<Result<Value, String>>>,
    changed: Condvar,
}
impl Waiter {
    fn finish(&self, value: Result<Value, String>) {
        let mut slot = lock(&self.value);
        if slot.is_none() {
            *slot = Some(value);
        }
        self.changed.notify_all();
    }
}
enum Collect {
    Data {
        key: String,
        count: Option<usize>,
        next: i32,
        bytes: Vec<u8>,
    },
    Files {
        task: u32,
        count: Option<usize>,
        next: u32,
        items: Vec<ClipboardFileDescriptor>,
    },
    Read {
        task: u32,
        maximum: usize,
    },
}
struct Pending {
    waiter: Arc<Waiter>,
    collect: Collect,
}

impl Clipboard {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(Arc::new(Inner {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            enabled: AtomicBool::new(false),
            files: AtomicBool::new(true),
            active: AtomicBool::new(false),
            allowed_files: AtomicBool::new(true),
            epoch: AtomicU64::new(1),
            platform: AtomicI32::new(1),
            serial: AtomicU32::new(0x4000_0000),
            sender: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            error: Mutex::new(None),
        }))
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            enabled: self.0.enabled.load(Ordering::Acquire),
            files: self.0.files.load(Ordering::Acquire),
            active: self.0.active.load(Ordering::Acquire),
            error: lock(&self.0.error).clone(),
        }
    }
    pub fn protocol_error(&self, error: String) {
        self.0.fail(error);
        self.0.cancel_pending();
    }
    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        if enabled {
            native::start()?;
        }
        self.0.enabled.store(enabled, Ordering::Release);
        if !enabled {
            self.suspend();
        }
        Ok(())
    }
    pub fn set_files(&self, enabled: bool) {
        if self.0.files.swap(enabled, Ordering::AcqRel) != enabled {
            self.suspend();
        }
    }
    pub fn platform(&self, platform: i32) {
        self.0.platform.store(platform, Ordering::Release);
    }
    pub fn policy(&self, ready: bool, files: bool) {
        let files = files && self.0.files.load(Ordering::Acquire);
        if self.0.allowed_files.swap(files, Ordering::AcqRel) != files {
            self.suspend();
        }
        let active = ready && self.0.enabled.load(Ordering::Acquire);
        if active && !self.0.active.swap(true, Ordering::AcqRel) {
            if let Err(e) = native::post(native::Command::Activate(Arc::downgrade(&self.0))) {
                self.0.active.store(false, Ordering::Release);
                self.0.fail(e.to_string());
            }
        } else if !active {
            self.suspend();
        }
    }
    pub fn suspend(&self) {
        if self.0.active.swap(false, Ordering::AcqRel) {
            self.0.epoch.fetch_add(1, Ordering::AcqRel);
            self.0.cancel_pending();
            let _ = native::post(native::Command::Remove(self.0.id));
        }
    }
    pub(crate) async fn run_sender(&self, channel: Arc<RTCDataChannel>) {
        let (tx, mut rx) = mpsc::channel(32);
        *lock(&self.0.sender) = Some(tx);
        let mut streams =
            std::collections::VecDeque::<(u64, String, Vec<u8>, usize, SendBudget)>::new();
        let mut packets = std::collections::VecDeque::<(
            u64,
            std::collections::VecDeque<Envelope>,
            SendBudget,
        )>::new();
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let result = tokio::select! {
                biased;
                queued=rx.recv()=> {
                    let Some(Queued{item,budget})=queued else{break;};
                    match item {
                        Outbound::Message(epoch,_)|Outbound::Blocks(epoch,_,_,_)|Outbound::Packets(epoch,_) if !self.0.valid(epoch)=>Ok(()),
                        Outbound::Cleanup(msg)=>channel.send_text_bytes(&bytes::Bytes::from(msg.encode_to_vec())).await.map(|_|()).map_err(Into::into),
                        Outbound::Message(epoch,msg)=>self.0.send(&channel,epoch,msg).await,
                        Outbound::Blocks(epoch,id,key,data)=> {
                            let count=data.len().div_ceil(BLOCK) as i32;
                            let result=self.0.send(&channel,epoch,response(id,ClipboardResponseKind::FormatDataConfirm(ClipboardFormatDataConfirm{err:1,block_key:key.clone(),block_count:count}))).await;
                            if result.is_ok(){streams.push_back((epoch,key,data,0,budget));}
                            result
                        },
                        Outbound::Packets(epoch,messages)=>{packets.push_back((epoch,messages,budget));Ok(())},
                    }
                },
                _=tick.tick(),if !streams.is_empty() || !packets.is_empty()=> {
                    if let Some((epoch,mut messages,budget))=packets.pop_front() {
                        if !self.0.valid(epoch){continue;}
                        let msg=messages.pop_front().expect("nonempty packet series");
                        let result=self.0.send(&channel,epoch,msg).await;
                        if result.is_ok() && !messages.is_empty(){packets.push_back((epoch,messages,budget));}
                        result
                    }else if let Some((epoch,key,data,offset,budget))=streams.pop_front() {
                        if !self.0.valid(epoch){continue;}
                        let end=(offset+BLOCK).min(data.len());
                        let msg=request(self.0.next(),ClipboardRequestKind::DataBlock(ClipboardDataBlock{block_key:key.clone(),block_id:(offset/BLOCK+1) as i32,data:data[offset..end].to_vec()}));
                        let result=self.0.send(&channel,epoch,msg).await;
                        if result.is_ok() && end<data.len(){streams.push_back((epoch,key,data,end,budget));}
                        result
                    }else{Ok(())}
                },
            };
            if let Err(error) = result {
                if self.0.active.load(Ordering::Acquire) {
                    self.0.fail(error.to_string());
                    self.0.cancel_pending();
                }
            }
        }
    }
    /// Called before general RPC dispatch; never treats ordinary video replies as clipboard data.
    pub fn receive(&self, bytes: &[u8]) -> Result<bool> {
        let e = Envelope::decode(bytes)?;
        if let Some(req) = e.request {
            if req.clip.is_none() && req.text.is_none() {
                return Ok(false);
            }
            ensure!(bytes.len() < 524288, "clipboard message too large");
            if !self.0.active.load(Ordering::Acquire) {
                return Ok(true);
            }
            let id = req.header.map_or(0, |h| h.id);
            if let Some(kind) = req.clip.and_then(|v| v.which) {
                match kind {
                    ClipboardRequestKind::DataBlock(block) => self.0.block(id, block)?,
                    ClipboardRequestKind::DescSegment(segment) => self.0.segment(id, segment)?,
                    ClipboardRequestKind::FormatList(list) => {
                        ensure!(
                            list.has_action == 0 && list.drag_drop_action.is_none(),
                            "file drag-and-drop is not enabled"
                        );
                        ensure!(
                            list.formats.len() <= 256
                                && list.formats.iter().all(|f| f.name.len() <= 1024),
                            "invalid clipboard format list"
                        );
                        native::post(native::Command::Offer(
                            Arc::downgrade(&self.0),
                            self.0.epoch.load(Ordering::Acquire),
                            list.formats,
                        ))?;
                    }
                    kind => native::post(native::Command::Request(
                        Arc::downgrade(&self.0),
                        self.0.epoch.load(Ordering::Acquire),
                        id,
                        kind,
                    ))?,
                }
            } else if let Some(text) = req.text {
                ensure!(text.data.len() <= MAX_DATA, "clipboard text too large");
                native::post(native::Command::Text(
                    Arc::downgrade(&self.0),
                    self.0.epoch.load(Ordering::Acquire),
                    id,
                    text.data,
                ))?;
            }
            return Ok(true);
        }
        if let Some(res) = e.response {
            if res.clip.is_none() && res.text.is_none() {
                return Ok(false);
            }
            ensure!(bytes.len() < 524288, "clipboard response too large");
            if let Some(kind) = res.clip.and_then(|v| v.which) {
                self.0.response(res.header.map_or(0, |h| h.id), kind)?;
            }
            return Ok(true);
        }
        Ok(false)
    }
}
impl Inner {
    fn next(&self) -> i64 {
        self.serial.fetch_add(1, Ordering::Relaxed) as i64
    }
    fn valid(&self, epoch: u64) -> bool {
        self.active.load(Ordering::Acquire) && self.epoch.load(Ordering::Acquire) == epoch
    }
    fn file_allowed(&self) -> bool {
        self.allowed_files.load(Ordering::Acquire)
    }
    fn fail(&self, text: String) {
        *lock(&self.error) = Some(text);
    }
    fn cancel_pending(&self) {
        for (_, p) in lock(&self.pending).drain() {
            p.waiter.finish(Err("剪贴板操作已取消".into()));
        }
    }
    fn enqueue(&self, item: Outbound) -> Result<()> {
        let size = match &item {
            Outbound::Cleanup(m) | Outbound::Message(_, m) => m.encoded_len(),
            Outbound::Blocks(_, _, _, v) => v.len(),
            Outbound::Packets(_, v) => v.iter().map(Message::encoded_len).sum(),
        };
        SEND_BYTES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(size)
                    .filter(|n| *n <= MAX_DATA + 4 * 1024 * 1024)
            })
            .map_err(|_| anyhow!("剪贴板发送超过内存预算"))?;
        let queued = Queued {
            item,
            budget: SendBudget(size),
        };
        lock(&self.sender)
            .as_ref()
            .ok_or_else(|| anyhow!("剪贴板通道未就绪"))?
            .try_send(queued)
            .map_err(|_| anyhow!("剪贴板发送队列已满或关闭"))
    }
    fn emit(&self, epoch: u64, msg: Envelope) -> Result<()> {
        ensure!(self.valid(epoch), "剪贴板同步已暂停");
        self.enqueue(Outbound::Message(epoch, msg))
    }
    async fn send(&self, channel: &RTCDataChannel, epoch: u64, msg: Envelope) -> Result<()> {
        ensure!(self.valid(epoch), "剪贴板同步已暂停");
        let data = msg.encode_to_vec();
        ensure!(data.len() < 524288, "剪贴板消息过大");
        // A bounded producer plus the SCTP queue provides backpressure; no retry of an uncertain send.
        channel.send_text_bytes(&bytes::Bytes::from(data)).await?;
        Ok(())
    }
    fn begin(
        &self,
        epoch: u64,
        kind: ClipboardRequestKind,
        collect: Collect,
    ) -> Result<(i64, Arc<Waiter>)> {
        let id = self.next();
        let waiter = Arc::new(Waiter {
            value: Mutex::new(None),
            changed: Condvar::new(),
        });
        {
            let mut p = lock(&self.pending);
            ensure!(p.len() < MAX_PENDING, "剪贴板请求过多");
            p.insert(
                id,
                Pending {
                    waiter: waiter.clone(),
                    collect,
                },
            );
        }
        if let Err(e) = self.emit(epoch, request(id, kind)) {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        Ok((id, waiter))
    }
    fn wait(&self, epoch: u64, id: i64, waiter: Arc<Waiter>) -> Result<Value> {
        loop {
            let completed = { lock(&waiter.value).take() };
            if let Some(value) = completed {
                lock(&self.pending).remove(&id);
                return value.map_err(anyhow::Error::msg);
            }
            if !self.valid(epoch) {
                lock(&self.pending).remove(&id);
                bail!("剪贴板操作已取消");
            }
            native::pump();
            let guard = lock(&waiter.value);
            if guard.is_none() {
                drop(waiter.changed.wait_timeout(guard, Duration::from_millis(5)));
            }
        }
    }
    fn data(&self, epoch: u64, format: &ClipboardFormat) -> Result<Vec<u8>> {
        let key = format!("openuuyc-{}-{}", self.id, self.next());
        let (id, w) = self.begin(
            epoch,
            ClipboardRequestKind::FormatDataAsk(ClipboardFormatDataAsk {
                format_id: format.id,
                format_name: format.name.clone(),
                block_key: key.clone(),
            }),
            Collect::Data {
                key,
                count: None,
                next: 1,
                bytes: Vec::new(),
            },
        )?;
        match self.wait(epoch, id, w)? {
            Value::Data(v) => Ok(v),
            _ => bail!("剪贴板响应类型错误"),
        }
    }
    fn descriptors(&self, epoch: u64, task: u32) -> Result<Vec<ClipboardFileDescriptor>> {
        ensure!(self.file_allowed(), "文件剪贴板已关闭");
        let (id, w) = self.begin(
            epoch,
            ClipboardRequestKind::FileDescListRequest(ClipboardFileDescriptorListRequest {
                task_id: task,
            }),
            Collect::Files {
                task,
                count: None,
                next: 1,
                items: Vec::new(),
            },
        )?;
        match self.wait(epoch, id, w)? {
            Value::Files(v) => Ok(v),
            _ => bail!("文件列表响应类型错误"),
        }
    }
    fn read_file(
        &self,
        epoch: u64,
        task: u32,
        index: u32,
        offset: u64,
        length: usize,
        flags: u32,
    ) -> Result<Vec<u8>> {
        ensure!(self.file_allowed(), "文件剪贴板已关闭");
        ensure!(length <= FILE_BLOCK, "文件读取过大");
        // The official producer correlates by task_id. Serialize reads for each remote OLE object.
        let (id, w) = self.begin(
            epoch,
            ClipboardRequestKind::FileContentsRequest(ClipboardFileContentsRequest {
                task_id: task,
                list_index: index,
                flags,
                pos_offset: offset,
                requested_len: length as u32,
            }),
            Collect::Read {
                task,
                maximum: length,
            },
        )?;
        match self.wait(epoch, id, w)? {
            Value::Data(v) => Ok(v),
            _ => bail!("文件内容响应类型错误"),
        }
    }
    fn response(&self, id: i64, r: ClipboardResponseKind) -> Result<()> {
        let mut pending = lock(&self.pending);
        match r {
            ClipboardResponseKind::FormatDataConfirm(v) => {
                if let Some(p) = pending.get_mut(&id) {
                    if let Collect::Data { key, count, .. } = &mut p.collect {
                        if v.err != 1
                            || v.block_key != *key
                            || v.block_count <= 0
                            || v.block_count as usize > MAX_DATA / BLOCK
                        {
                            p.waiter.finish(Err("远端无法提供剪贴板内容".into()));
                        } else {
                            *count = Some(v.block_count as usize);
                        }
                    }
                }
            }
            ClipboardResponseKind::FileDescListResponse(v) => {
                for p in pending.values_mut() {
                    if let Collect::Files { task, count, .. } = &mut p.collect {
                        if *task == v.task_id {
                            if v.err != 1
                                || v.segment_count == 0
                                || v.segment_count as usize > MAX_FILES
                            {
                                p.waiter.finish(Err("远端无法提供文件列表".into()));
                            } else {
                                *count = Some(v.segment_count as usize);
                            }
                        }
                    }
                }
            }
            ClipboardResponseKind::FileContentsResponse(v) => {
                if let Some(p) = pending
                    .values_mut()
                    .find(|p| matches!(p.collect,Collect::Read{task,..} if task==v.task_id))
                {
                    if let Collect::Read { maximum, .. } = p.collect {
                        p.waiter.finish(if v.err == 1 && v.data.len() <= maximum {
                            Ok(Value::Data(v.data))
                        } else {
                            Err("远端文件读取失败".into())
                        });
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn block(&self, id: i64, b: ClipboardDataBlock) -> Result<()> {
        let epoch = self.epoch.load(Ordering::Acquire);
        let mut ok = false;
        {
            let mut pending = lock(&self.pending);
            let used = pending
                .values()
                .map(|p| match &p.collect {
                    Collect::Data { bytes, .. } => bytes.len(),
                    _ => 0,
                })
                .sum::<usize>();
            for p in pending.values_mut() {
                if let Collect::Data {
                    key,
                    count,
                    next,
                    bytes,
                } = &mut p.collect
                {
                    if *key == b.block_key {
                        if used.saturating_add(b.data.len()) > MAX_DATA
                            || count.is_none()
                            || b.block_id != *next
                            || b.data.len() > BLOCK
                            || bytes.len() + b.data.len() > MAX_DATA
                            || b.data.is_empty()
                        {
                            p.waiter.finish(Err("无效的剪贴板数据块".into()));
                            break;
                        }
                        bytes.extend_from_slice(&b.data);
                        *next += 1;
                        ok = true;
                        if (*next - 1) as usize == count.unwrap() {
                            p.waiter.finish(Ok(Value::Data(std::mem::take(bytes))));
                        }
                        break;
                    }
                }
            }
        }
        self.emit(
            epoch,
            response(
                id,
                ClipboardResponseKind::DataBlockConfirm(ClipboardDataBlockConfirm {
                    block_key: b.block_key,
                    block_id: b.block_id,
                    err: if ok { 1 } else { 2 },
                }),
            ),
        )
    }
    fn segment(&self, id: i64, s: ClipboardFileDescriptorSegment) -> Result<()> {
        let mut ok = false;
        {
            let mut p = lock(&self.pending);
            for p in p.values_mut() {
                if let Collect::Files {
                    task,
                    count,
                    next,
                    items,
                } = &mut p.collect
                {
                    if *task == s.task_id {
                        if count.is_none()
                            || s.segment_id != *next
                            || items.len() + s.file_descs.len() > MAX_FILES
                            || s.file_descs
                                .iter()
                                .any(|f| !native::safe_name(&f.file_name))
                        {
                            p.waiter.finish(Err("无效的剪贴板文件列表".into()));
                            break;
                        }
                        items.extend(s.file_descs);
                        *next += 1;
                        ok = true;
                        if (*next - 1) as usize == count.unwrap() {
                            p.waiter.finish(Ok(Value::Files(std::mem::take(items))));
                        }
                        break;
                    }
                }
            }
        }
        self.emit(
            self.epoch.load(Ordering::Acquire),
            response(
                id,
                ClipboardResponseKind::DescSegmentConfirm(ClipboardFileDescriptorSegmentConfirm {
                    task_id: s.task_id,
                    segment_id: s.segment_id,
                    err: if ok { 1 } else { 2 },
                }),
            ),
        )
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.cancel_pending();
        let _ = native::post(native::Command::Remove(self.id));
    }
}
fn request(id: i64, kind: ClipboardRequestKind) -> Envelope {
    Envelope {
        request: Some(Request {
            header: Some(Header { id }),
            clip: Some(ClipboardRequest { which: Some(kind) }),
            text: None,
        }),
        response: None,
    }
}
fn response(id: i64, kind: ClipboardResponseKind) -> Envelope {
    Envelope {
        request: None,
        response: Some(Response {
            header: Some(Header { id }),
            clip: Some(ClipboardResponse { which: Some(kind) }),
            text: None,
        }),
    }
}
