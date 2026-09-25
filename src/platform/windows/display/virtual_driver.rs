//! SudoVDA's public 0.2.1 IOCTL contract. This adapter never installs drivers,
//! changes certificate trust, or removes devices by a display-name heuristic.
use super::topology::{luid, native_luid};
use anyhow::{Context, Result, ensure};
use std::{mem::size_of, ptr};
use uuid::Uuid;
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::*,
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE},
        Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING},
        System::IO::DeviceIoControl,
    },
    core::{GUID, PCWSTR},
};

const INTERFACE: GUID = GUID::from_u128(0xe5bcc234_1e0c_418a_a0d4_ef8b7501414d);
const fn ioctl(function: u32) -> u32 {
    (0x22 << 16) | (function << 2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Output {
    pub adapter: u64,
    pub target: u32,
}

struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}
pub(crate) struct Driver(HANDLE);
// A host display owner serializes IOCTLs. The OS handle has no COM apartment.
unsafe impl Send for Driver {}
impl Drop for Driver {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[repr(C)]
#[derive(Default)]
struct Add {
    width: u32,
    height: u32,
    hz: u32,
    monitor: GUID,
    name: [u8; 14],
    serial: [u8; 14],
}
#[repr(C)]
#[derive(Default)]
struct Added {
    adapter: windows::Win32::Foundation::LUID,
    target: u32,
}
#[repr(C)]
#[derive(Default)]
pub(crate) struct Watchdog {
    pub timeout: u32,
    pub countdown: u32,
}

impl Driver {
    fn interface() -> Result<(Vec<u16>, String)> {
        let devices = DeviceSet(
            unsafe {
                SetupDiGetClassDevsW(
                    Some(&INTERFACE),
                    PCWSTR::null(),
                    None,
                    DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
                )
            }
            .context("未找到虚拟显示驱动")?,
        );
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        unsafe { SetupDiEnumDeviceInterfaces(devices.0, None, &INTERFACE, 0, &mut interface) }
            .context("SudoVDA尚未安装或未启动")?;
        let mut other = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        match unsafe { SetupDiEnumDeviceInterfaces(devices.0, None, &INTERFACE, 1, &mut other) } {
            Ok(()) => anyhow::bail!("存在多个SudoVDA设备，无法确定驱动拥有者"),
            Err(error)
                if error.code()
                    == windows::core::HRESULT::from_win32(
                        windows::Win32::Foundation::ERROR_NO_MORE_ITEMS.0,
                    ) =>
            {
                ()
            }
            Err(error) => return Err(error.into()),
        }
        let mut length = 0;
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                devices.0,
                &interface,
                None,
                0,
                Some(&mut length),
                None,
            )
        };
        ensure!((8..=65536).contains(&length), "无效驱动接口长度");
        // u64 storage provides the alignment required by the platform detail structure.
        let mut storage = vec![0u64; (length as usize).div_ceil(8)];
        let detail = storage
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        let mut info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        unsafe {
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(
                devices.0,
                &interface,
                Some(detail),
                length,
                None,
                Some(&mut info),
            )?;
        }
        let path = unsafe { ptr::addr_of!((*detail).DevicePath).cast::<u16>() };
        let offset = path as usize - storage.as_ptr() as usize;
        let path = unsafe { std::slice::from_raw_parts(path, (length as usize - offset) / 2) };
        let end = path
            .iter()
            .position(|c| *c == 0)
            .context("驱动接口路径无效")?;
        let path = path[..=end].to_vec();
        let mut instance = [0u16; 512];
        unsafe { SetupDiGetDeviceInstanceIdW(devices.0, &info, Some(&mut instance), None) }?;
        let end = instance
            .iter()
            .position(|c| *c == 0)
            .context("驱动设备标识无效")?;
        Ok((
            path,
            String::from_utf16_lossy(&instance[..end]).to_ascii_lowercase(),
        ))
    }
    pub(crate) fn device_instance() -> Result<String> {
        Ok(Self::interface()?.1)
    }
    pub(crate) fn open() -> Result<Self> {
        let (path, _) = Self::interface()?;
        // No sharing: another controller must not own the global driver watchdog
        // or change its render adapter while this host owns virtual monitors.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .context("打开虚拟显示驱动失败；可能正被其他程序占用")?;
        let driver = Self(handle);
        let mut version = [0u8; 4];
        let returned = driver.control(0x8ff, &[], &mut version)?;
        ensure!(
            returned == version.len() && version[..3] == [0, 2, 1],
            "不支持的虚拟显示驱动协议：{:?}",
            version
        );
        Ok(driver)
    }
    fn control(&self, command: u32, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let mut returned = 0;
        unsafe {
            DeviceIoControl(
                self.0,
                ioctl(command),
                (!input.is_empty()).then_some(input.as_ptr().cast()),
                input.len() as u32,
                (!output.is_empty()).then_some(output.as_mut_ptr().cast()),
                output.len() as u32,
                Some(&mut returned),
                None,
            )
        }
        .context("虚拟显示驱动请求失败")?;
        ensure!(returned as usize <= output.len(), "驱动返回长度超出缓冲区");
        Ok(returned as usize)
    }
    pub(crate) fn add(&self, id: Uuid, width: u32, height: u32, hz: u32) -> Result<Output> {
        ensure!(
            (2..=16384).contains(&width)
                && (2..=16384).contains(&height)
                && (1..=1000).contains(&hz),
            "无效虚拟显示模式"
        );
        let mut request = Add {
            width,
            height,
            hz: hz * 1000,
            monitor: GUID::from_u128(id.as_u128()),
            ..Default::default()
        };
        request.name[..8].copy_from_slice(b"OpenUUYC");
        let serial = id.simple().to_string();
        request.serial[..13].copy_from_slice(&serial.as_bytes()[..13]);
        // All ABI structures are fully initialized and have no uninitialized padding.
        let input = unsafe {
            std::slice::from_raw_parts(ptr::addr_of!(request).cast::<u8>(), size_of::<Add>())
        };
        let mut output = Added::default();
        let buffer = unsafe {
            std::slice::from_raw_parts_mut(
                ptr::addr_of_mut!(output).cast::<u8>(),
                size_of::<Added>(),
            )
        };
        ensure!(
            self.control(0x800, input, buffer)? == size_of::<Added>(),
            "缺少虚拟显示目标身份"
        );
        Ok(Output {
            adapter: luid(output.adapter),
            target: output.target,
        })
    }
    pub(crate) fn remove(&self, id: Uuid) -> Result<()> {
        let id = GUID::from_u128(id.as_u128());
        let input = unsafe {
            std::slice::from_raw_parts(ptr::addr_of!(id).cast::<u8>(), size_of::<GUID>())
        };
        self.control(0x801, input, &mut [])?;
        Ok(())
    }
    pub(crate) fn render_adapter(&self, adapter: u64) -> Result<()> {
        let adapter = native_luid(adapter);
        let input = unsafe {
            std::slice::from_raw_parts(ptr::addr_of!(adapter).cast::<u8>(), size_of_val(&adapter))
        };
        self.control(0x802, input, &mut [])?;
        Ok(())
    }
    pub(crate) fn watchdog(&self) -> Result<Watchdog> {
        let mut output = Watchdog::default();
        let buffer = unsafe {
            std::slice::from_raw_parts_mut(
                ptr::addr_of_mut!(output).cast::<u8>(),
                size_of::<Watchdog>(),
            )
        };
        ensure!(
            self.control(0x803, &[], buffer)? == size_of::<Watchdog>(),
            "缺少虚拟显示看门狗状态"
        );
        Ok(output)
    }
    pub(crate) fn ping(&self) -> Result<()> {
        self.control(0x888, &[], &mut [])?;
        Ok(())
    }
}
