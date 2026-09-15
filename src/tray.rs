//! System tray icon: a home for the app when the dot is hidden, and the
//! entry point for start-at-login. The icon is the sprite itself.

use crate::sprite::{self, Mood, SIZE};
use crate::util::wide;
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_TRAY: u32 = 0x8020;
const TRAY_ID: u32 = 1;

/// Build an HICON from the idle sprite (32×32 ARGB).
unsafe fn make_icon() -> HICON {
    let mut px = vec![0u32; SIZE * SIZE];
    sprite::render(&mut px, 0, 0.0, Mood::Idle);
    // Un-premultiply so the icon's straight-alpha ARGB is right.
    for p in px.iter_mut() {
        let a = *p >> 24;
        if a > 0 && a < 255 {
            let un = |c: u32| ((c * 255 / a).min(255)) as u32;
            *p = a << 24 | un((*p >> 16) & 255) << 16 | un((*p >> 8) & 255) << 8 | un(*p & 255);
        }
    }
    let color = CreateBitmap(SIZE as i32, SIZE as i32, 1, 32, Some(px.as_ptr() as *const _));
    let mask = CreateBitmap(SIZE as i32, SIZE as i32, 1, 1, None);
    let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info).unwrap_or_default();
    let _ = DeleteObject(color.into());
    let _ = DeleteObject(mask.into());
    icon
}

fn data(hwnd: HWND, tip: &str) -> NOTIFYICONDATAW {
    let mut d = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    let t = wide(tip);
    let n = t.len().min(127);
    d.szTip[..n].copy_from_slice(&t[..n]);
    d
}

pub unsafe fn add(hwnd: HWND, tip: &str) {
    let mut d = data(hwnd, tip);
    d.hIcon = make_icon();
    let _ = Shell_NotifyIconW(NIM_ADD, &d);
    d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
}

pub unsafe fn set_tip(hwnd: HWND, tip: &str) {
    let mut d = data(hwnd, tip);
    d.uFlags = NIF_TIP | NIF_SHOWTIP;
    let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
}

pub unsafe fn remove(hwnd: HWND) {
    let d = data(hwnd, "");
    let _ = Shell_NotifyIconW(NIM_DELETE, &d);
}

/// Explorer restarts drop all tray icons; it broadcasts this so apps re-add theirs.
pub unsafe fn taskbar_created_message() -> u32 {
    RegisterWindowMessageW(w!("TaskbarCreated"))
}
