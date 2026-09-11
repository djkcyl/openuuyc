//! Session-wide TRACE capture in bounded memory, independent of the disk filter.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const MAX_EVENT: usize = 16 * 1024;
const MAX_CLIENTS: usize = 64;
thread_local! { static DISPLAYING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub fn capture_enabled() -> bool {
    !DISPLAYING.get()
}
pub fn displaying<T>(show: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            DISPLAYING.set(self.0);
        }
    }
    let _restore = Restore(DISPLAYING.replace(true));
    show()
}

#[derive(Clone)]
pub struct BoundedWriter(pub tracing_appender::non_blocking::NonBlocking);
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write_all(&bytes[..bytes.len().min(MAX_EVENT)])?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capacity {
    pub lines: usize,
    pub mib: usize,
}
impl Default for Capacity {
    fn default() -> Self {
        Self {
            lines: 5000,
            mib: 2,
        }
    }
}
impl Capacity {
    fn validate(self) -> Result<Self> {
        if !(100..=100_000).contains(&self.lines) || !(1..=64).contains(&self.mib) {
            bail!("缓冲区范围：100–100000 条，1–64 MiB");
        }
        Ok(self)
    }
}

pub struct Batch {
    pub cursor: u64,
    pub first: u64,
    pub lines: Vec<(u64, Arc<str>)>,
    pub capacity: Capacity,
    pub error: Option<String>,
}

struct Buffer {
    lines: VecDeque<(u64, Arc<str>)>,
    bytes: usize,
    cursor: u64,
    capacity: Capacity,
    error: Option<String>,
}
impl Buffer {
    fn trim(&mut self) {
        while self.lines.len() > self.capacity.lines || self.bytes > self.capacity.mib * 1048576 {
            self.bytes -= self.lines.pop_front().unwrap().1.len();
        }
    }
    fn push(&mut self, text: &[u8]) {
        let text = String::from_utf8_lossy(text);
        let text = text
            .strip_prefix("session=")
            .and_then(|text| text.split_once(' ').map(|(_, text)| text))
            .unwrap_or(&text);
        // Preserve complete multiline records, including their level and timestamp header.
        let text: Arc<str> = text.trim_end_matches(['\r', '\n']).into();
        self.cursor += 1;
        self.bytes += text.len();
        self.lines.push_back((self.cursor, text));
        self.trim();
    }
}

#[derive(Clone)]
pub struct View {
    buffer: Option<Arc<Mutex<Buffer>>>,
    config: PathBuf,
    pub endpoint: String,
}
impl View {
    pub fn read(&self, after: u64) -> Option<Batch> {
        let buffer = self.buffer.as_ref()?.try_lock().ok()?;
        Some(Batch {
            cursor: buffer.cursor,
            first: buffer
                .lines
                .front()
                .map_or(buffer.cursor + 1, |line| line.0),
            lines: buffer
                .lines
                .iter()
                .filter(|line| line.0 > after)
                .cloned()
                .collect(),
            capacity: buffer.capacity,
            error: buffer.error.clone(),
        })
    }
    pub fn set_capacity(&self, capacity: Capacity) -> Result<()> {
        let capacity = capacity.validate()?;
        let buffer = self.buffer.as_ref().context("实时日志收集器不可用")?;
        super::files::save_config(&self.config, &serde_json::to_vec_pretty(&capacity)?)?;
        let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
        buffer.capacity = capacity;
        buffer.error = None;
        buffer.trim();
        Ok(())
    }
}

pub struct CollectorGuard {
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}
impl Drop for CollectorGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

struct MemoryWriter(Arc<Mutex<Buffer>>);
impl Write for MemoryWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(&bytes[..bytes.len().min(MAX_EVENT)]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ChildWriter(Option<TcpStream>);
impl Write for ChildWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let bounded = &bytes[..bytes.len().min(MAX_EVENT)];
        let result = (|| {
            let stream = self.0.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
            stream.write_all(&(bounded.len() as u32).to_le_bytes())?;
            stream.write_all(bounded)?;
            Ok(bytes.len())
        })();
        if result.is_err() {
            self.0 = None;
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct Capture {
    pub writer: Box<dyn Write + Send>,
    pub view: View,
    pub guard: Option<CollectorGuard>,
}

pub fn unavailable(config_directory: &Path, error: anyhow::Error) -> Capture {
    Capture {
        writer: Box::new(io::sink()),
        guard: None,
        view: View {
            endpoint: String::new(),
            config: config_directory.join("live-logging.json"),
            buffer: Some(Arc::new(Mutex::new(Buffer {
                lines: VecDeque::new(),
                bytes: 0,
                cursor: 0,
                capacity: Capacity::default(),
                error: Some(format!("实时日志不可用：{error:#}")),
            }))),
        },
    }
}

pub fn start(config_directory: &Path) -> Result<Capture> {
    let config = config_directory.join("live-logging.json");
    if let Ok(endpoint) = std::env::var("OPENUUYC_LIVE_LOG_ENDPOINT") {
        let (port, token) = endpoint.split_once(':').context("无效的实时日志端点")?;
        let nonce = uuid::Uuid::parse_str(token)?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port.parse::<u16>()?));
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
        stream.set_write_timeout(Some(Duration::from_millis(250)))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_nodelay(true)?;
        stream.write_all(nonce.as_bytes())?;
        let mut accepted = [0];
        stream.read_exact(&mut accepted)?;
        if accepted != [1] {
            bail!("实时日志收集器验证失败");
        }
        return Ok(Capture {
            writer: Box::new(ChildWriter(Some(stream))),
            view: View {
                buffer: None,
                config,
                endpoint,
            },
            guard: None,
        });
    }
    let (capacity, error) = match super::read_settings(&config) {
        Ok(Some(bytes)) => match serde_json::from_slice::<Capacity>(&bytes)
            .map_err(anyhow::Error::from)
            .and_then(Capacity::validate)
        {
            Ok(capacity) => (capacity, None),
            Err(error) => (
                Capacity::default(),
                Some(format!("缓冲区设置无效：{error}")),
            ),
        },
        Ok(None) => (Capacity::default(), None),
        Err(error) => (
            Capacity::default(),
            Some(format!("读取缓冲区设置失败：{error}")),
        ),
    };
    let buffer = Arc::new(Mutex::new(Buffer {
        lines: VecDeque::new(),
        bytes: 0,
        cursor: 0,
        capacity,
        error,
    }));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let nonce = uuid::Uuid::new_v4();
    let endpoint = format!("{}:{nonce}", listener.local_addr()?.port());
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = stop.clone();
    let collected = buffer.clone();
    let task = std::thread::Builder::new()
        .name("live-log-collector".into())
        .spawn(move || {
            // Never log the collector's own IO back into the collection channel.
            let _quiet =
                tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default());
            let mut clients: Vec<Client> = Vec::new();
            while !task_stop.load(Ordering::Acquire) {
                for _ in 0..MAX_CLIENTS {
                    match listener.accept() {
                        Ok((stream, _)) if clients.len() < MAX_CLIENTS => {
                            if stream.set_nonblocking(true).is_ok() {
                                clients.push(Client {
                                    stream,
                                    bytes: Vec::new(),
                                    authenticated: false,
                                    started: Instant::now(),
                                });
                            }
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
                clients.retain_mut(|client| client.receive(nonce.as_bytes(), &collected).is_ok());
                std::thread::sleep(Duration::from_millis(5));
            }
        })?;
    Ok(Capture {
        writer: Box::new(MemoryWriter(buffer.clone())),
        view: View {
            buffer: Some(buffer),
            config,
            endpoint,
        },
        guard: Some(CollectorGuard {
            stop,
            task: Some(task),
        }),
    })
}

struct Client {
    stream: TcpStream,
    bytes: Vec<u8>,
    authenticated: bool,
    started: Instant,
}
impl Client {
    fn receive(&mut self, nonce: &[u8; 16], buffer: &Mutex<Buffer>) -> io::Result<()> {
        if !self.authenticated && self.started.elapsed() > Duration::from_secs(2) {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut scratch = [0_u8; 8192];
        // Bound work per child so one TRACE burst cannot starve other viewers.
        for _ in 0..16 {
            match self.stream.read(&mut scratch) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(size) => self.bytes.extend_from_slice(&scratch[..size]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if !self.authenticated {
                if self.bytes.len() < 16 {
                    continue;
                }
                if &self.bytes[..16] != nonce {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                self.bytes.drain(..16);
                self.stream.write_all(&[1])?;
                self.authenticated = true;
            }
            while self.bytes.len() >= 4 {
                let size = u32::from_le_bytes(self.bytes[..4].try_into().unwrap()) as usize;
                if size == 0 || size > MAX_EVENT {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                if self.bytes.len() < size + 4 {
                    break;
                }
                buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(&self.bytes[4..4 + size]);
                self.bytes.drain(..4 + size);
            }
        }
        Ok(())
    }
}

pub fn timestamp(line: &str) -> chrono::DateTime<chrono::Utc> {
    line.split_whitespace()
        .nth(1)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.to_utc())
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
}
