//! Explicit optional-driver installation/removal. Only the exact embedded
//! package and SudoVDA hardware identity are eligible for package removal.
use super::{recovery::Handle, virtual_driver::Driver};
use anyhow::{Context, Result, ensure};
use std::{mem::size_of, os::windows::process::CommandExt};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::*,
        Foundation::*,
        System::{Registry::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
    },
    core::{GUID, PCWSTR, w},
};

const CLASS: GUID = GUID::from_u128(0x4D36E968_E325_11CE_BFC1_08002BE10318);
const HARDWARE: &str = r"root\sudomaker\sudovda";
const FILES: &[(&str, &[u8])] = &[
    (
        "SudoVDA.inf",
        include_bytes!("../../../../assets/drivers/sudovda/SudoVDA.inf"),
    ),
    (
        "SudoVDA.dll",
        include_bytes!("../../../../assets/drivers/sudovda/SudoVDA.dll"),
    ),
    (
        "sudovda.cat",
        include_bytes!("../../../../assets/drivers/sudovda/sudovda.cat"),
    ),
    (
        "sudovda.cer",
        include_bytes!("../../../../assets/drivers/sudovda/sudovda.cer"),
    ),
];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Install,
    Uninstall,
}
impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "安装驱动",
            Self::Uninstall => "卸载驱动",
        }
    }
}

pub(crate) struct Status {
    pub label: &'static str,
    pub installed: bool,
    pub removable: bool,
}

fn nodes() -> Result<(DeviceSet, Vec<SP_DEVINFO_DATA>)> {
    let set = DeviceSet(unsafe {
        SetupDiGetClassDevsW(
            Some(&CLASS),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        )
    }?);
    let mut found = Vec::new();
    for index in 0..4096 {
        let mut info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        if let Err(error) = unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut info) } {
            if error.code() == windows::core::HRESULT::from_win32(ERROR_NO_MORE_ITEMS.0) {
                return Ok((set, found));
            }
            return Err(error.into());
        }
        let mut bytes = vec![0u8; 65536];
        let mut needed = 0;
        if unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                &info,
                SPDRP_HARDWAREID,
                None,
                Some(&mut bytes),
                Some(&mut needed),
            )
        }
        .is_err()
        {
            continue;
        }
        ensure!(needed as usize <= bytes.len(), "无效设备属性长度");
        let values: Vec<_> = bytes[..needed as usize]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        if values
            .split(|v| *v == 0)
            .any(|v| String::from_utf16_lossy(v).eq_ignore_ascii_case(HARDWARE))
        {
            found.push(info);
        }
    }
    anyhow::bail!("显示设备数量过多")
}

fn inf_directory() -> Result<std::path::PathBuf> {
    Ok(
        std::path::PathBuf::from(std::env::var_os("SystemRoot").context("Windows目录不可用")?)
            .join("INF"),
    )
}
fn oem_inf(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.strip_prefix("oem")
        .and_then(|s| s.strip_suffix(".inf"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
fn matching_packages() -> Result<Vec<String>> {
    let mut packages = Vec::new();
    for entry in std::fs::read_dir(inf_directory()?)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if oem_inf(&name)
            && entry.metadata()?.len() == FILES[0].1.len() as u64
            && std::fs::read(entry.path())? == FILES[0].1
        {
            packages.push(name);
        }
    }
    Ok(packages)
}
fn bound_package(set: &DeviceSet, info: &SP_DEVINFO_DATA) -> Result<String> {
    let key = unsafe {
        SetupDiOpenDevRegKey(
            set.0,
            info,
            DICS_FLAG_GLOBAL.0,
            0,
            DIREG_DRV,
            KEY_QUERY_VALUE.0,
        )
    }?;
    let mut text = [0u16; 512];
    let mut bytes = size_of_val(&text) as u32;
    let result = unsafe {
        RegGetValueW(
            key,
            PCWSTR::null(),
            w!("InfPath"),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    result.ok()?;
    let end = text
        .iter()
        .position(|c| *c == 0)
        .context("驱动INF名称无效")?;
    let name = String::from_utf16_lossy(&text[..end]);
    ensure!(oem_inf(&name), "驱动不是可卸载的OEM包");
    Ok(name)
}

pub(crate) fn status() -> Result<Status> {
    let (_, nodes) = nodes()?;
    let installed = !nodes.is_empty();
    let ready = installed && Driver::device_instance().is_ok();
    let packages = matching_packages()?;
    Ok(Status {
        label: if ready {
            "已安装"
        } else if installed {
            "已安装，驱动未就绪"
        } else if !packages.is_empty() {
            "未启用，驱动包仍在系统中"
        } else {
            "未安装（可选）"
        },
        installed,
        removable: installed || !packages.is_empty(),
    })
}

/// Called after the product confirmation dialog, then lets Windows obtain UAC consent.
pub(crate) fn request(operation: Operation) -> Result<bool> {
    if operation == Operation::Uninstall {
        // Give the local UI the exact rejection before prompting for elevation.
        // The elevated process repeats this check under the display mutation lock.
        drop(removal_plan()?);
    }
    let executable = wide(&std::env::current_exe()?.to_string_lossy());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: match operation {
            Operation::Install => w!("display-driver-install"),
            Operation::Uninstall => w!("display-driver-uninstall"),
        },
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.context("驱动操作未启动（可能已取消管理员授权）")?;
    ensure!(!info.hProcess.is_invalid(), "驱动操作进程不可用");
    let process = Handle(info.hProcess);
    ensure!(
        unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_OBJECT_0,
        "无法等待驱动操作完成"
    );
    let mut code = 0;
    unsafe { GetExitCodeProcess(process.0, &mut code) }?;
    ensure!(
        code == 0 || code == 3010,
        "{}未完成，请查看日志（退出码 {code}）",
        operation.label()
    );
    Ok(code == 3010)
}

pub(crate) fn install() -> Result<bool> {
    let _serial = super::recovery::Serial::acquire()?;
    ensure!(
        nodes()?.1.is_empty(),
        "已有SudoVDA设备，保留现有安装；请先在设备管理器确认状态"
    );
    let directory = std::env::temp_dir().join(format!("OpenUUYC-driver-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory)?;
    let result = (|| -> Result<bool> {
        use std::io::Write;
        for (name, bytes) in FILES {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        // The confirmation explicitly covers both machine certificate stores.
        // Use the system executable and fixed arguments, never an install script.
        let certutil =
            std::path::PathBuf::from(std::env::var_os("SystemRoot").context("Windows目录不可用")?)
                .join("System32/certutil.exe");
        for store in ["Root", "TrustedPublisher"] {
            let output = std::process::Command::new(&certutil)
                .args(["-addstore", "-f", store])
                .arg(directory.join("sudovda.cer"))
                .creation_flags(0x08000000)
                .output()?;
            ensure!(output.status.success(), "无法安装驱动签名证书（{store}）");
        }
        let set = DeviceSet(unsafe { SetupDiCreateDeviceInfoList(Some(&CLASS), None) }?);
        let mut info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        unsafe {
            SetupDiCreateDeviceInfoW(
                set.0,
                w!("Display"),
                &CLASS,
                w!("SudoMaker Virtual Display Adapter"),
                None,
                DICD_GENERATE_ID,
                Some(&mut info),
            )
        }?;
        let hardware: Vec<u8> = HARDWARE
            .encode_utf16()
            .chain([0, 0])
            .flat_map(u16::to_le_bytes)
            .collect();
        unsafe {
            SetupDiSetDeviceRegistryPropertyW(set.0, &mut info, SPDRP_HARDWAREID, Some(&hardware))
        }?;
        unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&info)) }?;
        let inf = wide(&directory.join("SudoVDA.inf").to_string_lossy());
        let hardware = wide(HARDWARE);
        let mut reboot = windows::core::BOOL::default();
        let install = unsafe {
            UpdateDriverForPlugAndPlayDevicesW(
                None,
                PCWSTR(hardware.as_ptr()),
                PCWSTR(inf.as_ptr()),
                UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0),
                Some(&mut reboot),
            )
        };
        if let Err(error) = install {
            // Remove only the device node created by this invocation.
            let _ = unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set.0, Some(&info)) };
            return Err(error.into());
        }
        tracing::info!(
            reboot = reboot.as_bool(),
            "optional virtual display driver installed"
        );
        Ok(reboot.as_bool())
    })();
    for (name, _) in FILES {
        let _ = std::fs::remove_file(directory.join(name));
    }
    let _ = std::fs::remove_dir(&directory);
    result
}

/// Remove one verified SudoVDA node, then unused copies of our exact driver package.
/// Never force removal of a package used by another installed device.
fn removal_plan() -> Result<(DeviceSet, Option<SP_DEVINFO_DATA>, Vec<String>)> {
    let (set, nodes) = nodes()?;
    ensure!(
        nodes.len() <= 1,
        "存在多个SudoVDA设备，请在设备管理器确认后再卸载"
    );
    let packages = matching_packages()?;
    if let Some(info) = nodes.first() {
        let package = bound_package(&set, info)?;
        ensure!(
            packages.iter().any(|p| p.eq_ignore_ascii_case(&package)),
            "当前SudoVDA不是本程序内置版本，保留现有驱动；请在设备管理器卸载"
        );
        let mut instance = [0u16; 512];
        unsafe { SetupDiGetDeviceInstanceIdW(set.0, info, Some(&mut instance), None) }?;
        let end = instance
            .iter()
            .position(|c| *c == 0)
            .context("设备标识无效")?;
        let instance = String::from_utf16_lossy(&instance[..end]).to_ascii_lowercase();
        for target in super::topology::Topology::query(false)?
            .targets()?
            .iter()
            .filter(|t| t.available)
        {
            let path = target
                .adapter_path
                .trim_start_matches(r"\\?\")
                .trim_start_matches(r"\??\");
            let owner = path
                .split("#{")
                .next()
                .unwrap_or(path)
                .replace('#', r"\")
                .to_ascii_lowercase();
            ensure!(
                owner != instance,
                "SudoVDA仍有显示器，请先结束使用它的连接或程序"
            );
        }
        // Exclusive open rejects an owner which has not created a target yet.
        // Close the handle before PnP removal to avoid requiring a reboot ourselves.
        if Driver::device_instance().is_ok() {
            drop(Driver::open().context("虚拟显示驱动正在使用中，无法卸载")?);
        }
    }
    Ok((set, nodes.into_iter().next(), packages))
}

pub(crate) fn uninstall() -> Result<bool> {
    let _serial = super::recovery::Serial::acquire()?;
    let (set, node, packages) = removal_plan()?;
    let mut reboot = windows::core::BOOL::default();
    if let Some(info) = &node {
        unsafe { DiUninstallDevice(HWND::default(), set.0, info, 0, Some(&mut reboot)) }
            .context("卸载SudoVDA设备失败")?;
    }
    drop(set);
    if reboot.as_bool() {
        tracing::info!(
            "virtual display device removal requires reboot; package retained until device removal completes"
        );
        return Ok(true);
    }
    for package in packages {
        // The OS refuses deletion while any device still references this package.
        unsafe { SetupUninstallOEMInfW(PCWSTR(wide(&package).as_ptr()), 0, None) }
            .ok()
            .with_context(|| {
                format!("设备已移除，但驱动包{package}尚未卸载（可能仍被其他设备使用）")
            })?;
    }
    ensure!(!status()?.removable, "卸载尚未完成，请重新检查驱动状态");
    tracing::info!(
        "optional virtual display driver uninstalled; shared certificate trust retained"
    );
    Ok(false)
}
