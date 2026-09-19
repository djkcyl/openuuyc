//! Cached local information, never the simulated hardware registered with UU.
use super::LocalDisplayInfo;
use bytes::Bytes;
use std::sync::mpsc::{self, Receiver};

use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(super) struct LocalDiagnostics {
    pub rows: Vec<(String, String)>,
    pub graphics: Vec<String>,
    pub probe: Option<Vec<(String, String)>>,
    facts: Option<Receiver<Vec<(String, String)>>>,
    probing: Option<Receiver<Vec<(String, String)>>>,
}

impl LocalDiagnostics {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("local-hardware".into())
            .spawn(move || {
                let _ = tx.send(hardware());
            });
        Self {
            rows: Vec::new(),
            graphics: Vec::new(),
            probe: None,
            facts: Some(rx),
            probing: None,
        }
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.facts {
            match rx.try_recv() {
                Ok(rows) => {
                    self.rows = rows;
                    self.facts = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.rows = vec![("硬件信息".into(), "读取失败".into())];
                    self.facts = None;
                }
                Err(_) => {}
            }
        }
        if let Some(rx) = &self.probing {
            match rx.try_recv() {
                Ok(rows) => {
                    self.probe = Some(rows);
                    self.probing = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.probe = Some(vec![("配置探测".into(), "后台任务未完成".into())]);
                    self.probing = None;
                }
                Err(_) => {}
            }
        }
    }

    pub fn busy(&self) -> bool {
        self.probing.is_some()
    }
    pub fn probe(
        &mut self,
        display: LocalDisplayInfo,
        options: crate::media::ConnectionMediaOptions,
    ) {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.probe = None;
        self.probing = Some(rx);
        let _ = std::thread::Builder::new()
            .name("decoder-diagnostics".into())
            .spawn(move || {
                use crate::{decoder::NativeVideoDecoder, media::VideoCodec};
                let fps = options.frame_rate.value(display);
                let rows = [("H.264", VideoCodec::H264), ("H.265", VideoCodec::H265)]
                    .into_iter()
                    .map(|(label, codec)| {
                        let value = match NativeVideoDecoder::open(
                            codec,
                            display.width,
                            display.height,
                            fps,
                            options.hardware_decode,
                            Bytes::new(),
                        ) {
                            Ok(decoder) => format!(
                                "{} · {}×{} / {} FPS",
                                decoder.label(),
                                display.width,
                                display.height,
                                fps
                            ),
                            Err(error) => format!("配置不可用：{error:#}"),
                        };
                        (label.to_owned(), value)
                    })
                    .collect();
                let _ = tx.send(rows);
            });
    }
}

// Fixed read-only system queries, bounded and hidden; no shell interpolation
// of device/account data. Collection happens once, outside the UI thread.

fn read_command(mut command: Command) -> Option<String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return child
                    .wait_with_output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok());
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn hardware() -> Vec<(String, String)> {
    let mut rows = vec![(
        "运行平台".into(),
        format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH),
    )];

    #[cfg(windows)]
    {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $ErrorActionPreference='Stop'; $os = Get-CimInstance Win32_OperatingSystem; $cpu = Get-ItemProperty -LiteralPath 'HKLM:\\HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0'; @{os=($os.Caption + ' ' + $os.Version);cpu=$cpu.ProcessorNameString;memory=[math]::Round($os.TotalVisibleMemorySize / 1MB, 1)} | ConvertTo-Json -Compress"]);
        if let Some(text) = read_command(command).and_then(|s| {
            serde_json::from_str::<serde_json::Value>(s.trim_start_matches('\u{feff}')).ok()
        }) {
            for (key, label) in [("os", "操作系统"), ("cpu", "处理器")] {
                if let Some(value) = text[key].as_str() {
                    rows.push((label.into(), value.trim().into()));
                }
            }
            if let Some(value) = text["memory"].as_f64() {
                rows.push(("物理内存".into(), format!("{value:.1} GiB")));
            }
        } else {
            rows.push(("硬件信息".into(), "系统查询失败或超时".into()));
        }
    }
    #[cfg(not(windows))]
    {
        // procfs and os-release answer the same questions without a shell.
        if let Some(name) = os_release_name() {
            rows.push(("操作系统".into(), name));
        }
        if let Some(model) = proc_field("/proc/cpuinfo", "model name") {
            rows.push(("处理器".into(), model));
        }
        if let Some(total) = proc_field("/proc/meminfo", "MemTotal") {
            if let Some(kib) = total
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<f64>().ok())
            {
                rows.push(("物理内存".into(), format!("{:.1} GiB", kib / 1_048_576.0)));
            }
        }
    }

    rows
}

#[cfg(not(windows))]
fn os_release_name() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))?
        .trim_matches('"')
        .to_owned();
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    Some(match kernel {
        Some(kernel) => format!("{value} / Linux {}", kernel.trim()),
        None => value,
    })
}

#[cfg(not(windows))]
fn proc_field(path: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| line.split_once(':').filter(|(name, _)| name.trim() == key))
        .map(|(_, value)| value.trim().to_owned())
}
