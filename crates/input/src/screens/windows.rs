//! A Windows desktop's screens: where the monitors stand in the virtual
//! screen, and each panel's EDID.
//!
//! `EnumDisplayMonitors` gives the rectangles, in physical pixels only on a
//! thread that is per-monitor DPI aware — which the capture thread is, so the
//! enumeration takes the same awareness for its duration, or a scaled monitor
//! would come back shrunk and no crossing would land on it. A monitor's GDI
//! name (`\\.\DISPLAY1`) leads through the display configuration to its
//! device path, and the device path to the EDID the driver stored in the
//! registry.

use cursio_protocol::Screen;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiOpenDevRegKey, SetupDiOpenDeviceInterfaceW, DICS_FLAG_GLOBAL, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, DIREG_DEV, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, GUID_DEVINTERFACE_MONITOR,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, INVALID_HANDLE_VALUE, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::System::Registry::{RegCloseKey, RegQueryValueExW, KEY_READ};
use windows_sys::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use super::edid::Edid;
use crate::InputError;

/// One display path's names: the GDI name a monitor rectangle is reported
/// under, and the panel behind it.
struct Target {
    gdi_name: String,
    friendly: String,
    device_path: String,
    /// Wired inside the machine: a laptop's own panel.
    built_in: bool,
}

pub(super) fn screens() -> Result<Vec<Screen>, InputError> {
    // SAFETY: changes only this thread's awareness, restored below.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let monitors = monitors();
    // SAFETY: as above; `previous` came from the same call.
    unsafe { SetThreadDpiAwarenessContext(previous) };
    let monitors = monitors?;
    let targets = targets();
    Ok(monitors
        .into_iter()
        .map(|(gdi_name, rect)| {
            let target = targets.iter().find(|t| t.gdi_name == gdi_name);
            let edid = target.and_then(|t| edid(&t.device_path));
            let width = (rect.right - rect.left).max(0) as u32;
            let height = (rect.bottom - rect.top).max(0) as u32;
            // A laptop's own panel has no friendly name, and often no model
            // in its EDID either.
            let name = target
                .map(|t| t.friendly.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| edid.as_ref().and_then(|e| e.model.clone()))
                .or_else(|| {
                    target
                        .is_some_and(|t| t.built_in)
                        .then(|| "Built-in display".to_string())
                })
                .unwrap_or_else(|| gdi_name.trim_start_matches(r"\\.\").to_string());
            Screen {
                id: edid
                    .as_ref()
                    .map(Edid::id)
                    .unwrap_or_else(|| gdi_name.clone()),
                name,
                x: rect.left,
                y: rect.top,
                width,
                height,
                size_mm: edid
                    .and_then(|e| e.size_mm)
                    .map(|mm| super::oriented(mm, width, height)),
            }
        })
        .collect())
}

/// Every monitor's GDI name and rectangle in the virtual screen.
fn monitors() -> Result<Vec<(String, RECT)>, InputError> {
    unsafe extern "system" fn each(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        found: LPARAM,
    ) -> windows_sys::core::BOOL {
        // SAFETY: `found` is the Vec passed to EnumDisplayMonitors below,
        // alive for the whole enumeration.
        let found = unsafe { &mut *(found as *mut Vec<(String, RECT)>) };
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is sized for the extended structure, as cbSize says.
        if unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) } != 0 {
            found.push((wide(&info.szDevice), info.monitorInfo.rcMonitor));
        }
        1
    }
    let mut found: Vec<(String, RECT)> = Vec::new();
    // SAFETY: the callback only touches `found`, which outlives the call.
    let ok = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(each),
            &mut found as *mut _ as LPARAM,
        )
    };
    if ok == 0 {
        return Err(InputError::Unavailable(format!(
            "cannot list the monitors: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(found)
}

/// The active display paths' names. Empty when the display configuration
/// cannot be read: screens are then named by their GDI name, without a size.
fn targets() -> Vec<Target> {
    let (mut paths_len, mut modes_len) = (0u32, 0u32);
    // SAFETY: two valid out-parameters.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths_len, &mut modes_len) }
        != ERROR_SUCCESS
    {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); paths_len as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); modes_len as usize];
    // SAFETY: the buffers hold the lengths passed.
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths_len,
            paths.as_mut_ptr(),
            &mut modes_len,
            modes.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Vec::new();
    }
    paths.truncate(paths_len as usize);
    paths
        .iter()
        .filter_map(|path| {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of_val(&source) as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = std::mem::size_of_val(&target) as u32;
            target.header.adapterId = path.targetInfo.adapterId;
            target.header.id = path.targetInfo.id;
            // SAFETY: each packet is the structure its header names and sizes.
            let named = unsafe {
                DisplayConfigGetDeviceInfo(&mut source.header) == 0
                    && DisplayConfigGetDeviceInfo(&mut target.header) == 0
            };
            named.then(|| Target {
                gdi_name: wide(&source.viewGdiDeviceName),
                friendly: wide(&target.monitorFriendlyDeviceName),
                device_path: wide(&target.monitorDevicePath),
                built_in: matches!(
                    target.outputTechnology,
                    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
                        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
                ),
            })
        })
        .collect()
}

/// The EDID the monitor driver stored for the device at `path`, under the
/// device's own registry key.
fn edid(path: &str) -> Option<Edid> {
    let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // SAFETY: a class-devices query with no enumerator or window.
    let set = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_MONITOR,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == INVALID_HANDLE_VALUE as _ {
        return None;
    }
    let found = (|| {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: `path` is NUL-terminated; `interface` is sized.
        if unsafe { SetupDiOpenDeviceInterfaceW(set, path.as_ptr(), 0, &mut interface) } == 0 {
            return None;
        }
        let mut device = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        // Asked with no detail buffer, this fails for want of one but fills
        // in the device, which is all that is needed.
        // SAFETY: a null detail buffer of size 0 is the documented probe.
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &interface,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut device,
            )
        };
        // SAFETY: `device` was filled for this set.
        let key =
            unsafe { SetupDiOpenDevRegKey(set, &device, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ) };
        if std::ptr::eq(key, INVALID_HANDLE_VALUE) || key.is_null() {
            return None;
        }
        let name: Vec<u16> = "EDID".encode_utf16().chain(Some(0)).collect();
        let mut bytes = vec![0u8; 512];
        let mut len = bytes.len() as u32;
        // SAFETY: `bytes` holds `len` bytes; the key is closed right after.
        let status = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                &mut len,
            )
        };
        // SAFETY: `key` was opened above and is not used again.
        unsafe { RegCloseKey(key) };
        (status == ERROR_SUCCESS).then(|| Edid::parse(&bytes[..len as usize]))?
    })();
    // SAFETY: `set` came from SetupDiGetClassDevsW and is not used again.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    found
}

/// A NUL-terminated UTF-16 buffer as a string.
fn wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}
