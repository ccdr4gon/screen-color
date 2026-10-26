//! The "apply at sign-in" option: an entry in the user's Run key that runs `screen-color restore`.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};

use crate::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Task Manager's Startup apps page records the entries it turned off.
const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const ENTRY: &str = "Screen Color";

/// Whether Windows will run `restore` when the user signs in.
pub fn is_enabled() -> bool {
    let (run, entry) = (wide(RUN_KEY), wide(ENTRY));
    let mut size = 0;
    let listed = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, run.as_ptr(), entry.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size)
    } == ERROR_SUCCESS;
    listed && !turned_off_in_task_manager()
}

fn turned_off_in_task_manager() -> bool {
    let (approved, entry) = (wide(APPROVED_KEY), wide(ENTRY));
    let mut flags = [0u8; 64];
    let mut size = flags.len() as u32;
    let found = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            approved.as_ptr(),
            entry.as_ptr(),
            RRF_RT_REG_BINARY,
            null_mut(),
            flags.as_mut_ptr().cast(),
            &mut size,
        )
    } == ERROR_SUCCESS;
    // The first byte is even for entries that are on and odd for ones turned off.
    found && size > 0 && flags[0] & 1 == 1
}

/// Adds or removes the sign-in entry that runs this exe's `restore` command.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let (run, approved, entry) = (wide(RUN_KEY), wide(APPROVED_KEY), wide(ENTRY));
    // Clear any "turned off" mark from Task Manager, so the choice made here takes effect.
    unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, approved.as_ptr(), entry.as_ptr()) };
    let error = if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("Couldn't find this program's path: {e}"))?;
        let command = wide(&format!("\"{}\" restore", exe.display()));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                run.as_ptr(),
                entry.as_ptr(),
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * size_of::<u16>()) as u32,
            )
        }
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, run.as_ptr(), entry.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            other => other,
        }
    };
    if error == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("Couldn't change the sign-in setting (error {error})."))
    }
}
