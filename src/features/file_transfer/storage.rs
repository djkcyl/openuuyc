use super::{
    protocol::*,
    service::{PartialFile, Record},
};
use anyhow::{Context, Result, ensure};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_FILES: usize = 100_000;
pub(super) fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    let value = unsafe {
        windows::Win32::UI::Shell::SHGetKnownFolderPath(
            id,
            windows::Win32::UI::Shell::KF_FLAG_DEFAULT,
            None,
        )
    }
    .ok()?;
    let path = unsafe { value.to_string() }.ok().map(PathBuf::from);
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast())) };
    path.filter(|p| p.is_dir())
}
pub(super) fn safe_relative(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty() && name.encode_utf16().count() < 30_000,
        "文件名为空或过长"
    );
    let mut result = PathBuf::new();
    for part in name.split(['\\', '/']) {
        ensure!(
            !part.is_empty()
                && !matches!(part, "." | "..")
                && !part.ends_with(['.', ' '])
                && !part.chars().any(|c| c < ' ' || ":<>\"|?*".contains(c)),
            "远端返回了不安全的文件路径"
        );
        let stem = part.split('.').next().unwrap_or("").to_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
            ) && !(stem.starts_with("COM") || stem.starts_with("LPT"))
                .then(|| &stem[3..])
                .is_some_and(|n| matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )),
            "不能使用设备名作为文件名"
        );
        result.push(part);
    }
    Ok(result)
}
pub(super) fn validate_manifest(files: &[FileInfo]) -> Result<u64> {
    ensure!(files.len() <= MAX_FILES, "文件清单过大");
    let mut names = std::collections::HashSet::new();
    let mut total = 0u64;
    for f in files {
        safe_relative(&f.rel_path)?;
        ensure!(
            names.insert(f.rel_path.replace('/', "\\").to_lowercase()),
            "文件清单包含重复路径"
        );
        total = total.checked_add(f.size).context("文件大小溢出")?;
        ensure!(f.size <= i64::MAX as u64, "文件大小超出可寻址范围");
    }
    Ok(total)
}
pub(super) fn compress<T: Message>(v: &T) -> Result<Vec<u8>> {
    ensure!(v.encoded_len() <= 32 * 1024 * 1024, "文件清单过大");
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&v.encode_to_vec())?;
    Ok(enc.finish()?)
}
pub(super) fn decompress<T: Message + Default>(v: &[u8]) -> Result<T> {
    let mut bytes = Vec::new();
    flate2::read::ZlibDecoder::new(v)
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 32 * 1024 * 1024, "解压后的文件清单过大");
    Ok(T::decode(bytes.as_slice())?)
}
pub(super) fn modified(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |v| v.as_secs())
}
fn not_link(p: &Path) -> Result<std::fs::Metadata> {
    let m = std::fs::symlink_metadata(p)?;
    ensure!(
        m.file_attributes() & 0x400 == 0,
        "不允许通过链接或重解析点传输：{}",
        p.display()
    );
    Ok(m)
}
pub(super) fn canonical_dir(p: &Path) -> Result<PathBuf> {
    ensure!(p.is_absolute(), "请选择完整本地目录");
    ensure!(not_link(p)?.is_dir(), "本地路径不是目录");
    Ok(dunce::canonicalize(p)?)
}
fn underneath(path: &Path, root: &Path) -> bool {
    let a = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>();
    let b = root
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>();
    a.starts_with(&b)
}
fn check_handle(f: &File, root: &Path) -> Result<()> {
    ensure!(
        f.metadata()?.file_attributes() & 0x400 == 0,
        "打开的文件已变为链接"
    );
    let mut path = vec![0u16; 32768];
    let n = unsafe {
        windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW(
            windows::Win32::Foundation::HANDLE(f.as_raw_handle()),
            &mut path,
            windows::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED,
        )
    } as usize;
    ensure!(n > 0 && n < path.len(), "无法核对文件实际位置");
    let actual = PathBuf::from(String::from_utf16(&path[..n])?);
    ensure!(
        underneath(dunce::simplified(&actual), root),
        "文件实际位置超出已选择目录"
    );
    Ok(())
}
pub(super) fn scan(
    source: &Path,
    cancel: &CancellationToken,
) -> Result<(PathBuf, String, Vec<FileInfo>)> {
    let m = not_link(source)?;
    let source = dunce::canonicalize(source)?;
    let folder = if m.is_dir() {
        source
            .file_name()
            .context("请选择文件夹而非整个磁盘")?
            .to_str()
            .context("文件夹名称无效")?
            .to_owned()
    } else {
        String::new()
    };
    let root = if m.is_dir() {
        source.clone()
    } else {
        source.parent().context("文件没有父目录")?.to_owned()
    };
    let mut files = Vec::new();
    let mut pending = vec![source];
    let mut visited = 0usize;
    while let Some(path) = pending.pop() {
        ensure!(!cancel.is_cancelled(), "已取消扫描");
        visited += 1;
        ensure!(visited <= MAX_FILES * 2, "目录包含过多项目");
        let m = not_link(&path)?;
        if m.is_dir() {
            for e in std::fs::read_dir(&path)? {
                pending.push(e?.path());
            }
        } else if m.is_file() {
            let name = path
                .strip_prefix(&root)?
                .to_str()
                .context("文件名称无效")?
                .replace('\\', "/");
            files.push(FileInfo {
                rel_path: name,
                size: m.len(),
                modified_time: modified(&m),
            });
            ensure!(files.len() <= MAX_FILES, "文件数量过多");
        }
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    validate_manifest(&files)?;
    Ok((root, folder, files))
}
pub(super) fn open_source(root: &Path, item: &FileInfo) -> Result<File> {
    let path = root.join(safe_relative(&item.rel_path)?);
    let f = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(0x00200000)
        .open(&path)?;
    check_handle(&f, root)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.len() == item.size && modified(&m) == item.modified_time,
        "源文件已变化，不能继续原任务"
    );
    Ok(f)
}
// Hold ancestor directory handles without FILE_SHARE_DELETE until finalization.
// This prevents a selected parent being swapped for a junction during a write.
pub(super) fn parents(root: &Path, relative: &Path, create: bool) -> Result<Vec<File>> {
    let mut locks = Vec::new();
    let mut p = root.to_path_buf();
    let lock_dir = |p: &Path| -> Result<File> {
        ensure!(not_link(p)?.is_dir(), "目标父路径不是目录");
        let f = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .custom_flags(0x02200000)
            .open(p)?;
        check_handle(&f, root)?;
        Ok(f)
    };
    locks.push(lock_dir(&p)?);
    if let Some(parent) = relative.parent() {
        for c in parent.components() {
            p.push(c);
            if create && !p.exists() {
                std::fs::create_dir(&p)?;
            }
            locks.push(lock_dir(&p)?);
        }
    }
    Ok(locks)
}
pub(super) struct Receiving {
    pub file: tokio::fs::File,
    pub partial: PartialFile,
    pub position: u64,
    pub _locks: Vec<File>,
}
pub(super) fn prepare(
    root: &Path,
    key: &str,
    item: &FileInfo,
    policy: i32,
    old: Option<&PartialFile>,
) -> Result<Option<Receiving>> {
    ensure!(uuid::Uuid::parse_str(key).is_ok(), "续传任务身份无效");
    let rel = safe_relative(&item.rel_path)?;
    let mut locks = parents(root, &rel, true)?;
    if let Some(old) = old {
        ensure!(old.info == *item, "源文件已变化，不能续传");
        if old.done || old.skipped {
            return Ok(None);
        }
        let target_rel = safe_relative(&old.target)?;
        locks.extend(parents(root, &target_rel, false)?);
        let temp = temp_path(root, &target_rel, key);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(1)
            .custom_flags(0x00200000)
            .open(temp)
            .context("续传临时文件已丢失")?;
        check_handle(&file, root)?;
        let n = file.metadata()?.len();
        ensure!(n <= item.size, "续传临时文件大小无效");
        return Ok(Some(Receiving {
            file: tokio::fs::File::from_std(file),
            partial: old.clone(),
            position: n,
            _locks: locks,
        }));
    }
    let mut target = rel.clone();
    if root.join(&target).exists() {
        ensure!(
            not_link(&root.join(&target))?.is_file(),
            "目标已存在且不是普通文件"
        );
        if policy == 3 {
            return Ok(None);
        }
        if policy == 2 {
            let stem = target
                .file_stem()
                .and_then(|s| s.to_str())
                .context("文件名无效")?
                .to_owned();
            let ext = target
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| format!(".{s}"))
                .unwrap_or_default();
            for n in 1..10000 {
                target.set_file_name(format!("{stem} ({n}){ext}"));
                if !root.join(&target).exists() {
                    break;
                }
            }
            ensure!(!root.join(&target).exists(), "无法分配不重名的文件名");
        }
    }
    let temp = temp_path(root, &target, key);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(1)
        .custom_flags(0x00200000)
        .open(temp)?;
    check_handle(&file, root)?;
    Ok(Some(Receiving {
        file: tokio::fs::File::from_std(file),
        partial: PartialFile {
            info: item.clone(),
            target: target.to_str().context("目标文件名无效")?.into(),
            done: false,
            skipped: false,
        },
        position: 0,
        _locks: locks,
    }))
}
fn temp_path(root: &Path, relative: &Path, key: &str) -> PathBuf {
    let mut p = root.join(relative);
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    p.set_file_name(format!(".{name}.{key}.downloading"));
    p
}
pub(super) async fn finish(
    mut v: Receiving,
    root: &Path,
    key: &str,
    policy: i32,
) -> Result<PartialFile> {
    ensure!(
        v.position == v.partial.info.size,
        "收到的文件长度与清单不一致"
    );
    v.file.sync_all().await?;
    let file = v.file.into_std().await;
    if let Some(time) =
        UNIX_EPOCH.checked_add(std::time::Duration::from_secs(v.partial.info.modified_time))
    {
        file.set_modified(time)?;
    }
    drop(file);
    let relative = safe_relative(&v.partial.target)?;
    let target = root.join(&relative);
    let source = temp_path(root, &relative, key);
    if target.exists() {
        ensure!(not_link(&target)?.is_file(), "目标已经变成目录或链接");
    }
    use std::os::windows::ffi::OsStrExt;
    let from = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        windows::Win32::Storage::FileSystem::MoveFileExW(
            windows::core::PCWSTR(from.as_ptr()),
            windows::core::PCWSTR(to.as_ptr()),
            windows::Win32::Storage::FileSystem::MOVE_FILE_FLAGS(if policy == 1 {
                1 | 8
            } else {
                8
            }),
        )
    }
    .context("无法完成目标文件保存")?;
    v.partial.done = true;
    Ok(v.partial)
}
pub(super) fn cleanup(root: &Path, key: &str, items: &[PartialFile]) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(key).is_ok(), "续传任务身份无效");
    for item in items.iter().filter(|i| !i.done && !i.skipped) {
        let rel = safe_relative(&item.target)?;
        let _locks = parents(root, &rel, false)?;
        let p = temp_path(root, &rel, key);
        match std::fs::symlink_metadata(&p) {
            Ok(m) => {
                ensure!(
                    m.is_file() && m.file_attributes() & 0x400 == 0,
                    "临时路径已被替换"
                );
                std::fs::remove_file(p)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[derive(Clone)]
pub(crate) struct Store(PathBuf);
impl Store {
    pub(crate) fn new(account: &str, device: &str) -> Result<Self> {
        crate::account::api::validate_device_id(device)?;
        ensure!(!account.is_empty(), "无法确认当前账号");
        Ok(Self(
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("本地配置目录不可用")?)
                .join("OpenUUYC/file-transfer")
                .join(format!("{:x}", Sha256::digest(account)))
                .join(format!("{device}.json")),
        ))
    }
    pub(super) fn load(&self) -> Result<Vec<Record>> {
        let f = match File::open(&self.0) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut b = Vec::new();
        f.take(64 * 1024 * 1024 + 1).read_to_end(&mut b)?;
        ensure!(b.len() <= 64 * 1024 * 1024, "传输历史过大");
        let records: Vec<Record> = serde_json::from_slice(&b)?;
        ensure!(records.len() <= 256, "传输历史过多");
        for r in &records {
            ensure!(uuid::Uuid::parse_str(&r.key).is_ok(), "传输记录身份无效");
            validate_manifest(&r.files)?;
            for f in &r.partial {
                safe_relative(&f.target)?;
            }
        }
        Ok(records)
    }
    pub(super) fn save(&self, r: &[Record]) -> Result<()> {
        ensure!(r.len() <= 256, "传输记录已满，请清理已完成记录");
        let b = serde_json::to_vec(r)?;
        ensure!(b.len() <= 64 * 1024 * 1024, "传输记录过大");
        std::fs::create_dir_all(self.0.parent().context("历史路径无效")?)?;
        let temp = self
            .0
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            f.write_all(&b)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&temp, &self.0)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}
