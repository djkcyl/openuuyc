//! Bounded, read-only live logs for one application launch and its viewer children.
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender},
};

const CHUNK: u64 = 128 * 1024;
const MAX_LINE: usize = 16 * 1024;
const BATCH_BYTES: usize = 512 * 1024;

pub struct Request {
    pub directory: PathBuf,
    pub session: String,
    pub explicit_file: Option<PathBuf>,
    pub clear: bool,
}

pub struct Batch {
    pub lines: Vec<String>,
    pub error: Option<String>,
}

pub struct Reader {
    pub requests: SyncSender<Request>,
    pub batches: Receiver<Batch>,
}

impl Reader {
    pub fn start() -> io::Result<Self> {
        let (requests, rx) = mpsc::sync_channel::<Request>(1);
        let (tx, batches) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("log-viewer".into())
            .spawn(move || {
                let mut session = SessionTail::default();
                while let Ok(request) = rx.recv() {
                    let batch = session.poll(&request);
                    if tx.send(batch).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { requests, batches })
    }
}

// This UUID is propagated only to viewer children, never inferred from PID or time.
fn session_files(request: &Request) -> io::Result<Vec<PathBuf>> {
    if let Some(path) = &request.explicit_file {
        return Ok(vec![path.clone()]);
    }
    let suffix = format!("-{}.", request.session);
    let mut files = Vec::new();
    for entry in fs::read_dir(&request.directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if super::files::managed_name(&name)
            && name.contains(&suffix)
            && entry.file_type()?.is_file()
        {
            files.push(entry.path());
        }
    }
    files.sort_unstable();
    Ok(files)
}

pub fn timestamp(line: &str) -> chrono::DateTime<chrono::Utc> {
    line.split_whitespace()
        .nth(1)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.to_utc())
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
}

#[derive(Default)]
struct Source {
    tail: Tail,
    included: bool,
}

#[derive(Default)]
struct SessionTail {
    sources: BTreeMap<PathBuf, Source>,
    next: usize,
    cleared_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl SessionTail {
    fn poll(&mut self, request: &Request) -> Batch {
        let mut batch = Batch {
            lines: Vec::new(),
            error: None,
        };
        let files = match session_files(request) {
            Ok(files) => files,
            Err(e) => {
                batch.error = Some(format!("读取日志失败：{e}"));
                return batch;
            }
        };
        self.sources
            .retain(|path, _| files.binary_search(path).is_ok());
        if request.clear {
            self.cleared_at = Some(chrono::Utc::now());
        }
        let prefix = format!("session={} ", request.session);
        let mut consumed = 0;
        for index in 0..files.len() {
            let position = (self.next + index) % files.len();
            let path = &files[position];
            let source = self.sources.entry(path.clone()).or_default();
            let mut lines = Vec::new();
            match source.tail.read(path, request.clear, &mut lines) {
                Ok(bytes) => consumed += bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    batch.error = Some(format!("读取日志失败：{e}"));
                    continue;
                }
            }
            if request.clear {
                source.included = false;
            }
            for line in lines {
                if line.starts_with("session=") {
                    source.included = line.starts_with(&prefix);
                }
                if !source.included {
                    continue;
                }
                let line = line.strip_prefix(&prefix).unwrap_or(&line);
                if self.cleared_at.is_none_or(|at| timestamp(line) > at) {
                    batch.lines.push(line.to_owned());
                }
            }
            if !request.clear && consumed >= BATCH_BYTES {
                self.next = (position + 1) % files.len();
                break;
            }
        }
        batch
    }
}

#[derive(Default)]
struct Tail {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
    skipping: bool,
}

impl Tail {
    fn read(&mut self, path: &Path, clear: bool, lines: &mut Vec<String>) -> io::Result<usize> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        if self.path != path || len < self.offset {
            self.path = path.to_owned();
            self.offset = len.saturating_sub(CHUNK);
            self.pending.clear();
            self.skipping = self.offset != 0;
        }
        if clear {
            self.offset = len;
            self.pending.clear();
            self.skipping = if len != 0 {
                file.seek(SeekFrom::Start(len - 1))?;
                let mut last = [0];
                file.read_exact(&mut last)?;
                last[0] != b'\n'
            } else {
                false
            };
            return Ok(0);
        }
        // TRACE bursts must not leave an ever-growing backlog in the viewer.
        if len.saturating_sub(self.offset) > CHUNK * 4 {
            self.offset = len.saturating_sub(CHUNK);
            self.pending.clear();
            self.skipping = true;
            lines.push("… 日志过多，已跳至最新内容 …".into());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(CHUNK).read_to_end(&mut bytes)?;
        let consumed = bytes.len();
        self.offset += consumed as u64;
        for byte in bytes {
            if byte == b'\n' {
                if !self.skipping {
                    if self.pending.last() == Some(&b'\r') {
                        self.pending.pop();
                    }
                    lines.push(String::from_utf8_lossy(&self.pending).into_owned());
                }
                self.pending.clear();
                self.skipping = false;
            } else if !self.skipping {
                self.pending.push(byte);
                if self.pending.len() >= MAX_LINE {
                    lines.push(format!(
                        "{} … [行已截断]",
                        String::from_utf8_lossy(&self.pending)
                    ));
                    self.pending.clear();
                    self.skipping = true;
                }
            }
        }
        Ok(consumed)
    }
}
