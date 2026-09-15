//! Start-at-login via the per-user Run registry key.

use crate::util::wide;
use windows::core::w;
use windows::Win32::System::Registry::*;

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: windows::core::PCWSTR = w!("Blackhole");

unsafe fn open(write: bool) -> Option<HKEY> {
    let mut key = HKEY::default();
    let sam = if write { KEY_SET_VALUE | KEY_QUERY_VALUE } else { KEY_QUERY_VALUE };
    let err = RegCreateKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, None, REG_OPTION_NON_VOLATILE, sam, None, &mut key, None);
    err.is_ok().then_some(key)
}

pub fn enabled() -> bool {
    unsafe {
        let Some(key) = open(false) else { return false };
        let mut size = 0u32;
        let err = RegQueryValueExW(key, VALUE, None, None, None, Some(&mut size));
        let _ = RegCloseKey(key);
        err.is_ok()
    }
}

pub fn set(enabled: bool) {
    unsafe {
        let Some(key) = open(true) else { return };
        if enabled {
            if let Ok(exe) = std::env::current_exe() {
                let cmd = wide(&format!("\"{}\"", exe.display()));
                let bytes: &[u8] = std::slice::from_raw_parts(cmd.as_ptr() as *const u8, cmd.len() * 2);
                let _ = RegSetValueExW(key, VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, VALUE);
        }
        let _ = RegCloseKey(key);
    }
}
