//! Finding the active displays.

use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::Display::*;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LUID};
use windows_sys::Win32::Graphics::Gdi::*;

use crate::from_wide;

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    /// 1-based, as shown by `list`
    pub index: usize,
    /// e.g. \\.\DISPLAY2, what the gamma API takes
    pub gdi_name: String,
    /// monitor name from its EDID
    pub name: String,
    pub connection: String,
    pub internal: bool,
    pub primary: bool,
    /// stable per monitor and port; the key for saved settings
    pub device_path: String,
}

impl Display {
    pub fn kind(&self) -> String {
        let kind = if self.internal { "built-in" } else { "external" };
        if self.primary { format!("{kind}, primary") } else { kind.to_string() }
    }

    pub fn label(&self) -> String {
        let details = if self.internal {
            self.kind()
        } else {
            format!("{}, {}", self.connection, self.kind())
        };
        format!("{}: {} ({details})", self.index, self.name)
    }
}

fn connection_name(technology: u32) -> String {
    let name = match technology {
        0xFFFF_FFFF => "Other",
        0 => "VGA",
        1 => "S-Video",
        2 => "Composite",
        3 => "Component",
        4 => "DVI",
        5 => "HDMI",
        6 => "LVDS",
        8 => "D-Jpn",
        9 => "SDI",
        10 => "DisplayPort",
        11 => "Embedded DisplayPort",
        12 => "UDI",
        13 => "Embedded UDI",
        14 => "SDTV dongle",
        15 => "Miracast",
        16 => "Indirect (wired)",
        17 => "Indirect (virtual)",
        18 => "DisplayPort (USB tunnel)",
        0x8000_0000 => "Internal",
        other => return format!("type {other}"),
    };
    name.to_string()
}

fn is_internal(technology: u32) -> bool {
    matches!(technology, 6 | 11 | 13 | 0x8000_0000)
}

fn primary_gdi_name() -> Option<String> {
    let mut device: DISPLAY_DEVICEW = unsafe { zeroed() };
    device.cb = size_of::<DISPLAY_DEVICEW>() as u32;
    let mut i = 0;
    while unsafe { EnumDisplayDevicesW(null(), i, &mut device, 0) } != 0 {
        if device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0 {
            return Some(from_wide(&device.DeviceName));
        }
        i += 1;
    }
    None
}

/// Sends a DisplayConfigGetDeviceInfo request; `T` starts with its header.
fn device_info<T>(mut request: T, kind: DISPLAYCONFIG_DEVICE_INFO_TYPE, adapter_id: LUID, id: u32) -> Option<T> {
    let header = &mut request as *mut T as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER;
    unsafe {
        *header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: kind,
            size: size_of::<T>() as u32,
            adapterId: adapter_id,
            id,
        };
        (DisplayConfigGetDeviceInfo(header) == 0).then_some(request)
    }
}

fn title_case(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut chars = lower.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Active displays, ordered by their \\.\DISPLAYn number.
pub fn list_displays() -> Result<Vec<Display>, String> {
    let (mut n_paths, mut n_modes) = (0u32, 0u32);
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
    let mut error = ERROR_SUCCESS;
    // Buffer sizes can change if a display is plugged in between the two calls.
    for _ in 0..3 {
        error = unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes) };
        if error != ERROR_SUCCESS {
            break;
        }
        paths = vec![unsafe { zeroed() }; n_paths as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![unsafe { zeroed() }; n_modes as usize];
        error = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut n_paths,
                paths.as_mut_ptr(),
                &mut n_modes,
                modes.as_mut_ptr(),
                null_mut(),
            )
        };
        if error != ERROR_INSUFFICIENT_BUFFER {
            break;
        }
    }
    if error != ERROR_SUCCESS {
        return Err(format!("Couldn't read the display configuration (error {error})."));
    }
    paths.truncate(n_paths as usize);

    let primary = primary_gdi_name();
    let mut displays = Vec::new();
    for path in &paths {
        let source = path.sourceInfo;
        let target = path.targetInfo;
        let Some(source) = device_info::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(
            unsafe { zeroed() },
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            source.adapterId,
            source.id,
        ) else {
            continue;
        };
        let Some(target) = device_info::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(
            unsafe { zeroed() },
            DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            target.adapterId,
            target.id,
        ) else {
            continue;
        };
        let gdi_name = from_wide(&source.viewGdiDeviceName);
        let technology = target.outputTechnology as u32;
        let name = from_wide(&target.monitorFriendlyDeviceName);
        let device_path = from_wide(&target.monitorDevicePath);
        displays.push(Display {
            index: 0,
            name: if name.is_empty() {
                title_case(gdi_name.trim_start_matches(['\\', '.']))
            } else {
                name
            },
            connection: connection_name(technology),
            internal: is_internal(technology),
            primary: primary.as_deref() == Some(gdi_name.as_str()),
            device_path: if device_path.is_empty() { gdi_name.clone() } else { device_path },
            gdi_name,
        });
    }

    displays.sort_by_key(|d| {
        let digits: String = d.gdi_name.chars().filter(char::is_ascii_digit).collect();
        digits.parse::<u32>().unwrap_or(0)
    });
    for (i, display) in displays.iter_mut().enumerate() {
        display.index = i + 1;
    }
    Ok(displays)
}

/// External displays if there are any, otherwise the primary one.
pub fn default_displays(displays: &[Display]) -> Vec<&Display> {
    let external: Vec<&Display> = displays.iter().filter(|d| !d.internal).collect();
    if !external.is_empty() {
        return external;
    }
    displays.iter().find(|d| d.primary).or(displays.first()).into_iter().collect()
}
