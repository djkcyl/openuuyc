//! Windows display inventory. Protocol screen IDs and virtual-device ownership
//! belong to the host; Windows source/target IDs never stand in for either.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::mem::size_of;
use windows::{
    Win32::{
        Devices::Display::*,
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LUID},
        Graphics::Gdi::*,
    },
    core::PCWSTR,
};

pub(crate) const DPI_VALUES: [u32; 12] =
    [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dpi {
    pub current: u32,
    pub recommended: u32,
    pub supported: Vec<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub identity: String,
    pub name: String,
    pub source: String,
    pub adapter: u64,
    pub adapter_path: String,
    pub target: u32,
    pub source_adapter: u64,
    pub source_id: u32,
    pub active: bool,
    pub available: bool,
    pub modes: Vec<Mode>,
    pub dpi: Option<Dpi>,
}
pub(crate) struct Topology {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedTarget {
    pub identity: String,
    pub source_group: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: i32,
    pub refresh_numerator: u32,
    pub refresh_denominator: u32,
    pub scan_line_ordering: i32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedTopology {
    pub targets: Vec<SavedTarget>,
}

pub(crate) fn luid(value: LUID) -> u64 {
    (u64::from(value.HighPart as u32) << 32) | u64::from(value.LowPart)
}
pub(crate) fn native_luid(value: u64) -> LUID {
    LUID {
        LowPart: value as u32,
        HighPart: (value >> 32) as i32,
    }
}
fn text(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|c| *c == 0).unwrap_or(value.len())])
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

impl Topology {
    pub(crate) fn snapshot(&self) -> Result<SavedTopology> {
        let mut result = SavedTopology::default();
        for path in &self.paths {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0
                || !path.targetInfo.targetAvailable.as_bool()
            {
                continue;
            }
            let identity = target_identity(path)?;
            let mode = self
                .modes
                .get(unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize)
                .context("活动显示路径缺少源模式")?;
            ensure!(
                mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
                "无效源模式类型"
            );
            let source = unsafe { mode.Anonymous.sourceMode };
            let scan = self
                .modes
                .get(unsafe { path.targetInfo.Anonymous.modeInfoIdx } as usize)
                .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET)
                .map_or(path.targetInfo.scanLineOrdering.0, |m| unsafe {
                    m.Anonymous
                        .targetMode
                        .targetVideoSignalInfo
                        .scanLineOrdering
                        .0
                });
            ensure!((1..=3).contains(&scan), "活动显示模式缺少有效扫描方式");
            result.targets.push(SavedTarget {
                identity,
                source_group: format!(
                    "{:016X}/{}",
                    luid(path.sourceInfo.adapterId),
                    path.sourceInfo.id
                ),
                left: source.position.x,
                top: source.position.y,
                width: source.width,
                height: source.height,
                rotation: path.targetInfo.rotation.0,
                refresh_numerator: path.targetInfo.refreshRate.Numerator,
                refresh_denominator: path.targetInfo.refreshRate.Denominator,
                scan_line_ordering: scan,
            });
        }
        Ok(result)
    }
    pub(crate) fn query(active_only: bool) -> Result<Self> {
        let flags = if active_only {
            QDC_ONLY_ACTIVE_PATHS
        } else {
            QDC_ALL_PATHS
        };
        for _ in 0..3 {
            let (mut paths, mut modes) = (0, 0);
            let status = unsafe { GetDisplayConfigBufferSizes(flags, &mut paths, &mut modes) };
            ensure!(
                status == ERROR_SUCCESS,
                "读取显示拓扑大小失败：{}",
                status.0
            );
            ensure!(paths <= 4096 && modes <= 16384, "显示拓扑规模异常");
            let mut result = Self {
                paths: vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize],
                modes: vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize],
            };
            let status = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut paths,
                    result.paths.as_mut_ptr(),
                    &mut modes,
                    result.modes.as_mut_ptr(),
                    None,
                )
            };
            if status == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            ensure!(status == ERROR_SUCCESS, "读取显示拓扑失败：{}", status.0);
            result.paths.truncate(paths as usize);
            result.modes.truncate(modes as usize);
            return Ok(result);
        }
        bail!("显示拓扑正在变化，请稍后重试")
    }

    pub(crate) fn targets(&self) -> Result<Vec<Target>> {
        let mut targets: Vec<Target> = Vec::new();
        for path in &self.paths {
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            let status = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
            if status != 0 {
                // Inactive QDC_ALL_PATHS candidates can have no monitor attached.
                ensure!(
                    path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0,
                    "读取活动显示器身份失败：{status}"
                );
                continue;
            }
            let identity = text(&target.monitorDevicePath).to_ascii_lowercase();
            if identity.is_empty() {
                continue;
            }
            let active = path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0;
            if let Some(previous) = targets.iter().position(|t| t.identity == identity) {
                if targets[previous].active || !active {
                    continue;
                }
                targets.remove(previous);
            }
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            let source = if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } == 0 {
                text(&source.viewGdiDeviceName)
            } else {
                String::new()
            };
            let mut adapter = DISPLAYCONFIG_ADAPTER_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
                    size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: 0,
                },
                ..Default::default()
            };
            let adapter_path = if unsafe { DisplayConfigGetDeviceInfo(&mut adapter.header) } == 0 {
                text(&adapter.adapterDevicePath).to_ascii_lowercase()
            } else {
                String::new()
            };
            targets.push(Target {
                identity,
                name: text(&target.monitorFriendlyDeviceName),
                modes: if source.is_empty() {
                    Vec::new()
                } else {
                    modes(&source)?
                },
                dpi: if active {
                    dpi(path.sourceInfo.adapterId, path.sourceInfo.id).ok()
                } else {
                    None
                },
                source,
                adapter: luid(path.targetInfo.adapterId),
                adapter_path,
                target: path.targetInfo.id,
                source_adapter: luid(path.sourceInfo.adapterId),
                source_id: path.sourceInfo.id,
                active,
                available: path.targetInfo.targetAvailable.as_bool(),
            });
        }
        Ok(targets)
    }
}

fn target_identity(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String> {
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
    ensure!(status == 0, "读取显示目标身份失败：{status}");
    let identity = text(&target.monitorDevicePath).to_ascii_lowercase();
    ensure!(!identity.is_empty(), "显示目标身份为空");
    Ok(identity)
}

impl SavedTopology {
    /// Rebind saved monitor identities to current Windows IDs. Removed monitors
    /// are returned to the caller; their old integer target IDs are never reused.
    pub(crate) fn apply(&self) -> Result<Vec<String>> {
        use std::collections::{HashMap, HashSet};
        ensure!(
            !self.targets.is_empty() && self.targets.len() <= 64,
            "无效的目标显示布局"
        );
        let inventory = Topology::query(false)?;
        let current = Topology::query(true)?;
        let mut candidates = Vec::new();
        for path in current.paths.iter().copied().chain(inventory.paths) {
            if path.targetInfo.targetAvailable.as_bool() {
                if let Ok(identity) = target_identity(&path) {
                    candidates.push((identity, path));
                }
            }
        }
        let mut paths = Vec::new();
        let mut modes = Vec::new();
        let mut groups = HashMap::new();
        let mut used_sources = HashSet::new();
        let mut seen_targets = HashSet::new();
        let mut missing = Vec::new();
        for saved in &self.targets {
            ensure!(seen_targets.insert(&saved.identity), "布局包含重复显示目标");
            ensure!(
                (1..=16384).contains(&saved.width)
                    && (1..=16384).contains(&saved.height)
                    && (1..=4).contains(&saved.rotation)
                    && (1..=3).contains(&saved.scan_line_ordering)
                    && saved.refresh_denominator != 0,
                "无效的保存显示模式"
            );
            let group = groups.get(&saved.source_group).copied();
            let candidate = candidates
                .iter()
                .filter(|(identity, _)| identity == &saved.identity)
                .filter(|(_, p)| {
                    let key = (luid(p.sourceInfo.adapterId), p.sourceInfo.id);
                    group.map_or(!used_sources.contains(&key), |(g, _)| g == key)
                })
                .min_by_key(|(_, p)| p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0);
            let Some((_, candidate)) = candidate else {
                missing.push(saved.identity.clone());
                continue;
            };
            let mut path = *candidate;
            let source_key = (luid(path.sourceInfo.adapterId), path.sourceInfo.id);
            let index = if let Some((_, index)) = group {
                index
            } else {
                let index = modes.len() as u32;
                modes.push(DISPLAYCONFIG_MODE_INFO {
                    infoType: DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
                    id: path.sourceInfo.id,
                    adapterId: path.sourceInfo.adapterId,
                    Anonymous: DISPLAYCONFIG_MODE_INFO_0 {
                        sourceMode: DISPLAYCONFIG_SOURCE_MODE {
                            width: saved.width,
                            height: saved.height,
                            pixelFormat: DISPLAYCONFIG_PIXELFORMAT_32BPP,
                            position: windows::Win32::Foundation::POINTL {
                                x: saved.left,
                                y: saved.top,
                            },
                        },
                    },
                });
                groups.insert(saved.source_group.clone(), (source_key, index));
                used_sources.insert(source_key);
                index
            };
            path.flags = DISPLAYCONFIG_PATH_ACTIVE;
            path.sourceInfo.Anonymous.modeInfoIdx = index;
            path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            // Preserve complete timings for unchanged active targets. Inactive
            // possibilities returned by QDC_ALL_PATHS are not a substitute for
            // the active path's target-mode/scan/scaling metadata.
            if let Some(active) = current.paths.iter().find(|active| {
                active.targetInfo.adapterId == path.targetInfo.adapterId
                    && active.targetInfo.id == path.targetInfo.id
                    && active.sourceInfo.adapterId == path.sourceInfo.adapterId
                    && active.sourceInfo.id == path.sourceInfo.id
            }) {
                path.targetInfo.scaling = active.targetInfo.scaling;
                path.targetInfo.scanLineOrdering = active.targetInfo.scanLineOrdering;
                if let Some(source) = current
                    .modes
                    .get(unsafe { active.sourceInfo.Anonymous.modeInfoIdx } as usize)
                    && source.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                {
                    let source = unsafe { source.Anonymous.sourceMode };
                    let rate = active.targetInfo.refreshRate;
                    if source.width == saved.width
                        && source.height == saved.height
                        && active.targetInfo.rotation.0 == saved.rotation
                        && u64::from(rate.Numerator) * u64::from(saved.refresh_denominator)
                            == u64::from(saved.refresh_numerator) * u64::from(rate.Denominator)
                    {
                        if let Some(target) = current
                            .modes
                            .get(unsafe { active.targetInfo.Anonymous.modeInfoIdx } as usize)
                            && target.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
                        {
                            path.targetInfo.Anonymous.modeInfoIdx = modes.len() as u32;
                            modes.push(*target);
                        }
                    }
                }
            }
            if path.targetInfo.scaling.0 == 0 {
                path.targetInfo.scaling = DISPLAYCONFIG_SCALING_PREFERRED;
            }
            path.targetInfo.rotation = DISPLAYCONFIG_ROTATION(saved.rotation);
            path.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
                Numerator: saved.refresh_numerator,
                Denominator: saved.refresh_denominator,
            };
            // UNSPECIFIED is valid only together with a zero/zero refresh rate.
            // Restoring an explicit refresh rate also restores its scan order.
            path.targetInfo.scanLineOrdering =
                DISPLAYCONFIG_SCANLINE_ORDERING(saved.scan_line_ordering);
            paths.push(path);
        }
        ensure!(!paths.is_empty(), "原显示目标均不可用，保留当前布局");
        let flags = SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
        let status = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags | SDC_VALIDATE) };
        ensure!(status == 0, "显示驱动拒绝目标布局：{}", status);
        let status = unsafe {
            SetDisplayConfig(
                Some(&paths),
                Some(&modes),
                flags | SDC_APPLY | SDC_SAVE_TO_DATABASE,
            )
        };
        ensure!(status == 0, "应用显示布局失败：{}", status);
        Ok(missing)
    }
}

pub(crate) fn modes(source: &str) -> Result<Vec<Mode>> {
    let source = wide(source);
    let mut result = Vec::new();
    for index in 0..16384 {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplaySettingsExW(
                PCWSTR(source.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(index),
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
        }
        .as_bool()
        {
            return Ok(result);
        }
        if mode.dmPelsWidth == 0 || mode.dmPelsHeight == 0 || mode.dmDisplayFrequency <= 1 {
            continue;
        }
        let mode = Mode {
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
            hz: mode.dmDisplayFrequency,
        };
        if !result.contains(&mode) {
            result.push(mode);
        }
    }
    bail!("显示模式列表过大")
}

#[repr(C)]
#[derive(Default)]
struct GetDpi {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    minimum: i32,
    current: i32,
    maximum: i32,
}
#[repr(C)]
#[derive(Default)]
struct SetDpi {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    relative: i32,
}

fn dpi_raw(adapter: LUID, id: u32) -> Result<GetDpi> {
    let mut value = GetDpi {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-3),
            size: size_of::<GetDpi>() as u32,
            adapterId: adapter,
            id,
        },
        ..Default::default()
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut value.header) };
    ensure!(status == 0, "读取DPI范围失败：{status}");
    ensure!(
        value.minimum <= 0
            && value.maximum >= 0
            && (value.minimum..=value.maximum).contains(&value.current),
        "无效DPI范围"
    );
    Ok(value)
}
fn dpi(adapter: LUID, id: u32) -> Result<Dpi> {
    let value = dpi_raw(adapter, id)?;
    let base = value.minimum.checked_neg().context("无效推荐DPI索引")?;
    let current = base.checked_add(value.current).context("无效当前DPI索引")?;
    let last = base.checked_add(value.maximum).context("无效最大DPI索引")?;
    let recommended = *DPI_VALUES.get(base as usize).context("未知推荐DPI")?;
    let current = *DPI_VALUES.get(current as usize).context("未知当前DPI")?;
    ensure!(last >= 0, "无效DPI上限");
    Ok(Dpi {
        current,
        recommended,
        supported: DPI_VALUES[..=(last as usize).min(DPI_VALUES.len() - 1)].to_vec(),
    })
}

impl Target {
    pub(crate) fn revalidate(&self) -> Result<Self> {
        Topology::query(false)?
            .targets()?
            .into_iter()
            .find(|t| t.identity == self.identity && t.available)
            .context("目标显示器已断开")
    }
    pub(crate) fn set_dpi(&self, requested: u32, active: impl Fn() -> bool) -> Result<()> {
        let target = self.revalidate()?;
        ensure!(target.active, "目标显示器未启用");
        let adapter = native_luid(target.source_adapter);
        let value = dpi_raw(adapter, target.source_id)?;
        let index = DPI_VALUES
            .iter()
            .position(|v| *v == requested)
            .context("不支持的DPI比例")? as i32;
        let relative = index.checked_add(value.minimum).context("无效DPI偏移")?;
        ensure!(
            (value.minimum..=value.maximum).contains(&relative),
            "DPI超出显示器支持范围"
        );
        if value.current == relative {
            return Ok(());
        }
        let value = SetDpi {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-4),
                size: size_of::<SetDpi>() as u32,
                adapterId: adapter,
                id: target.source_id,
            },
            relative,
        };
        ensure!(active(), "显示操作已取消");
        let status = unsafe { DisplayConfigSetDeviceInfo(&value.header) };
        ensure!(status == 0, "应用DPI失败：{status}");
        Ok(())
    }
    pub(crate) fn set_resolution(
        &self,
        width: u32,
        height: u32,
        active: impl Fn() -> bool,
    ) -> Result<()> {
        let target = self.revalidate()?;
        ensure!(target.active, "目标显示器未启用");
        ensure!(
            target
                .modes
                .iter()
                .any(|m| m.width == width && m.height == height),
            "不支持的物理分辨率"
        );
        let source = wide(&target.source);
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        ensure!(
            unsafe {
                EnumDisplaySettingsExW(
                    PCWSTR(source.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
            }
            .as_bool(),
            "读取当前显示模式失败"
        );
        if (mode.dmPelsWidth, mode.dmPelsHeight) == (width, height) {
            return Ok(());
        }
        mode.dmPelsWidth = width;
        mode.dmPelsHeight = height;
        mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        let status = unsafe {
            ChangeDisplaySettingsExW(PCWSTR(source.as_ptr()), Some(&mode), None, CDS_TEST, None)
        };
        ensure!(
            status == DISP_CHANGE_SUCCESSFUL,
            "显示驱动拒绝分辨率：{}",
            status.0
        );
        // Apply once; an ambiguous failure is never automatically replayed.
        ensure!(active(), "显示操作已取消");
        let status = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(source.as_ptr()),
                Some(&mode),
                None,
                CDS_UPDATEREGISTRY,
                None,
            )
        };
        ensure!(
            status == DISP_CHANGE_SUCCESSFUL,
            "应用分辨率失败：{}",
            status.0
        );
        Ok(())
    }
}
