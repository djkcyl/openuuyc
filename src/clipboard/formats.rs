use super::*;
use std::io::Cursor;
#[cfg(windows)]
use windows::{
    Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW},
    core::PCWSTR,
};
#[derive(Clone)]
pub(super) struct Format {
    pub wire: ClipboardFormat,
    pub local: u32,
}
#[cfg(windows)]
pub(super) fn register(name: &str) -> u32 {
    let s: Vec<u16> = name.encode_utf16().chain([0]).collect();
    unsafe { RegisterClipboardFormatW(PCWSTR(s.as_ptr())) }
}
#[cfg(windows)]
pub(super) fn name(id: u32) -> String {
    let mut s = [0u16; 1024];
    let n = unsafe { GetClipboardFormatNameW(id, &mut s) };
    String::from_utf16_lossy(&s[..n.max(0) as usize])
}

/// The wire protocol speaks Windows clipboard-format ids, so a process-local
/// registry stands in for the system one: the same name always maps to the same
/// id within this client, starting at CF_PRIVATEFIRST like Windows does.
#[cfg(not(windows))]
fn registry() -> &'static Mutex<(HashMap<String, u32>, HashMap<u32, String>)> {
    static REGISTRY: std::sync::OnceLock<Mutex<(HashMap<String, u32>, HashMap<u32, String>)>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

#[cfg(not(windows))]
pub(super) fn register(name: &str) -> u32 {
    if name.is_empty() {
        return 0;
    }
    let mut registry = lock(registry());
    if let Some(id) = registry.0.get(name) {
        return *id;
    }
    let id = 0xc000 + registry.0.len() as u32;
    if id > 0xffff {
        return 0;
    }
    registry.0.insert(name.to_owned(), id);
    registry.1.insert(id, name.to_owned());
    id
}

#[cfg(not(windows))]
pub(super) fn name(id: u32) -> String {
    if let Some(name) = standard_name(id) {
        return name.to_owned();
    }
    lock(registry()).1.get(&id).cloned().unwrap_or_default()
}

/// The predefined CF_* formats this client can name without the system.
#[cfg(not(windows))]
const fn standard_name(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "CF_TEXT",
        2 => "CF_BITMAP",
        3 => "CF_METAFILEPICT",
        4 => "CF_SYLK",
        5 => "CF_DIF",
        6 => "CF_TIFF",
        7 => "CF_OEMTEXT",
        8 => "CF_DIB",
        9 => "CF_PALETTE",
        10 => "CF_PENDATA",
        11 => "CF_RIFF",
        12 => "CF_WAVE",
        13 => "CF_UNICODETEXT",
        14 => "CF_ENHMETAFILE",
        15 => "CF_HDROP",
        16 => "CF_LOCALE",
        17 => "CF_DIBV5",
        _ => return None,
    })
}

/// File names arriving from a Windows peer must stay valid Windows names, even
/// when this client writes them on a filesystem that would accept more.
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
pub(super) fn file_format(id: u32, name: &str) -> bool {
    id == 15
        || [
            "FileGroupDescriptorW",
            "FileGroupDescriptor",
            "FileContents",
            "FileName",
            "FileNameW",
            "Shell IDList Array",
            "Preferred DropEffect",
            "DropDescription",
            "Shell Object Offsets",
            "DataObjectAttributes",
            "DataObjectAttributesRequiringElevation",
            "public.file-url",
        ]
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}
pub(super) fn supported(id: u32, name: &str, files: bool) -> bool {
    if name.eq_ignore_ascii_case("DataObject") || name.eq_ignore_ascii_case("Ole Private Data") {
        return false;
    }
    if file_format(id, name) {
        return files
            && matches!(
                name,
                "FileGroupDescriptorW" | "FileContents" | "public.file-url"
            );
    }
    matches!(
        id,
        1 | 4 | 5 | 6 | 7 | 8 | 10 | 11 | 12 | 13 | 14 | 16 | 17 | 0x81 | 0x8e
    ) || (id >= 0xc000 && !name.is_empty())
}
pub(super) fn incoming(f: ClipboardFormat, platform: i32, files: bool) -> Option<Format> {
    let local = if platform == 4 {
        match f.name.as_str() {
            "public.utf8-plain-text" | "public.utf16-plain-text" | "public.plain-text" => 13,
            "public.tiff" => 8,
            "public.html" => register("HTML Format"),
            "public.file-url" if files => register("FileGroupDescriptorW"),
            _ => return None,
        }
    } else if f.name.is_empty() {
        f.id
    } else {
        register(&f.name)
    };
    if local == 0 || (!supported(local, &name(local), files) && platform != 4) {
        return None;
    }
    Some(Format { wire: f, local })
}
pub(super) fn outgoing(ids: &[(u32, String)], platform: i32, files: bool) -> Vec<Format> {
    let mut out = Vec::new();
    for (id, n) in ids {
        if !supported(*id, n, files) {
            continue;
        }
        if platform != 4 {
            out.push(Format {
                wire: ClipboardFormat {
                    id: *id,
                    name: n.clone(),
                },
                local: *id,
            });
            continue;
        }
        let names: &[&str] = match *id {
            13 => &["public.utf8-plain-text", "public.utf16-plain-text"],
            8 => &["public.tiff"],
            _ if n == "HTML Format" => &["public.html"],
            _ if n == "FileGroupDescriptorW" => &["public.file-url"],
            _ => &[],
        };
        for n in names {
            out.push(Format {
                wire: ClipboardFormat {
                    id: out.len() as u32 + 1,
                    name: (*n).into(),
                },
                local: *id,
            });
        }
    }
    out
}
fn utf16(bytes: &[u8]) -> Result<String> {
    ensure!(bytes.len() % 2 == 0, "无效的Unicode剪贴板");
    let mut words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    while words.last() == Some(&0) {
        words.pop();
    }
    if words.first() == Some(&0xfeff) {
        words.remove(0);
    }
    Ok(String::from_utf16(&words)?)
}
pub(super) fn unicode(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn field(bytes: &[u8], key: &str) -> Option<usize> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    head.lines()
        .find_map(|l| l.strip_prefix(key)?.trim().parse().ok())
}
fn windows_lines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}
pub(super) fn convert(data: Vec<u8>, f: &Format, platform: i32, outbound: bool) -> Result<Vec<u8>> {
    if platform != 4 {
        return Ok(data);
    }
    let out = match f.wire.name.as_str() {
        "public.utf8-plain-text" | "public.plain-text" => {
            if outbound {
                utf16(&data)?.replace("\r\n", "\n").into_bytes()
            } else {
                unicode(&windows_lines(
                    std::str::from_utf8(&data)?
                        .trim_end_matches('\0')
                        .trim_start_matches('\u{feff}'),
                ))
            }
        }
        "public.utf16-plain-text" => {
            if outbound {
                let s = utf16(&data)?.replace("\r\n", "\n");
                s.encode_utf16().flat_map(u16::to_le_bytes).collect()
            } else {
                unicode(&windows_lines(&utf16(&data)?))
            }
        }
        "public.html" => {
            if outbound {
                let start =
                    field(&data, "StartHTML:").ok_or_else(|| anyhow!("HTML剪贴板缺少起始位置"))?;
                let end =
                    field(&data, "EndHTML:").ok_or_else(|| anyhow!("HTML剪贴板缺少结束位置"))?;
                ensure!(start <= end && end <= data.len(), "无效的HTML剪贴板范围");
                data[start..end].to_vec()
            } else {
                let html = std::str::from_utf8(&data)?.trim_end_matches('\0');
                let before = "<html><body><!--StartFragment-->";
                let after = "<!--EndFragment--></body></html>";
                let header = |a, b, c, d| {
                    format!(
                        "Version:1.0\r\nStartHTML:{a:010}\r\nEndHTML:{b:010}\r\nStartFragment:{c:010}\r\nEndFragment:{d:010}\r\n"
                    )
                };
                let len = header(0, 0, 0, 0).len();
                let mut result = header(
                    len,
                    len + before.len() + html.len() + after.len(),
                    len + before.len(),
                    len + before.len() + html.len(),
                )
                .into_bytes();
                result.extend_from_slice(before.as_bytes());
                result.extend_from_slice(html.as_bytes());
                result.extend_from_slice(after.as_bytes());
                result.push(0);
                result
            }
        }
        "public.tiff" => {
            let (bytes, kind) = if outbound {
                (dib_to_bmp(&data)?, image::ImageFormat::Bmp)
            } else {
                (data, image::ImageFormat::Tiff)
            };
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes), kind);
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(MAX_DATA as u64);
            limits.max_image_width = Some(32768);
            limits.max_image_height = Some(32768);
            reader.limits(limits);
            let image = reader.decode()?;
            let mut out = Cursor::new(Vec::new());
            image.write_to(
                &mut out,
                if outbound {
                    image::ImageFormat::Tiff
                } else {
                    image::ImageFormat::Bmp
                },
            )?;
            let data = out.into_inner();
            if outbound {
                data
            } else {
                ensure!(data.len() >= 14, "无效的位图输出");
                data[14..].to_vec()
            }
        }
        _ => data,
    };
    ensure!(out.len() <= MAX_DATA, "转换后的剪贴板内容过大");
    Ok(out)
}
fn dib_to_bmp(d: &[u8]) -> Result<Vec<u8>> {
    ensure!(d.len() >= 40, "无效的DIB");
    let u32at = |i| u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
    let header = u32at(0) as usize;
    ensure!(
        matches!(header, 40 | 52 | 56 | 108 | 124) && header <= d.len(),
        "不支持的DIB头"
    );
    let bits = u16::from_le_bytes([d[14], d[15]]);
    let compression = u32at(16);
    let used = u32at(32) as usize;
    let palette = if used != 0 {
        used
    } else if bits <= 8 {
        1usize << bits
    } else {
        0
    };
    let masks = if header == 40 && compression == 3 {
        12
    } else if header == 40 && compression == 6 {
        16
    } else {
        0
    };
    let offset = header
        .checked_add(masks)
        .and_then(|v| v.checked_add(palette.checked_mul(4)?))
        .ok_or_else(|| anyhow!("DIB尺寸溢出"))?;
    ensure!(offset <= d.len(), "无效的DIB像素偏移");
    let mut out = Vec::with_capacity(14 + d.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((14 + d.len()) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&((14 + offset) as u32).to_le_bytes());
    out.extend_from_slice(d);
    Ok(out)
}
