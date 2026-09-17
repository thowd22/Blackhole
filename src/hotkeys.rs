//! Global hotkeys as text ("Ctrl+Shift+Space"), parsed for RegisterHotKey and built
//! back from a key event when the user rebinds one in the Settings tab.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// The four actions, in the order the config and the Settings tab list them.
pub const ACTIONS: [(&str, &str); 4] = [
    ("Summon to mouse and search", "Ctrl+Shift+Space"),
    ("Swallow the clipboard", "Ctrl+Shift+V"),
    ("Take a screenshot", "Ctrl+Shift+S"),
    ("New note at the mouse", "Ctrl+Shift+N"),
];

/// Set by the dot when registration fails (another program owns the key); read by
/// the Settings tab to say so next to the binding.
pub static TAKEN: [AtomicBool; 4] = [AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)];

pub fn taken(i: usize) -> bool {
    TAKEN[i].load(Ordering::Relaxed)
}
pub fn set_taken(i: usize, v: bool) {
    TAKEN[i].store(v, Ordering::Relaxed);
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Combo {
    pub mods: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

const NAMED: &[(&str, VIRTUAL_KEY)] = &[
    ("Space", VK_SPACE), ("Tab", VK_TAB), ("Enter", VK_RETURN), ("Esc", VK_ESCAPE), ("Backspace", VK_BACK),
    ("Ins", VK_INSERT), ("Del", VK_DELETE), ("Home", VK_HOME), ("End", VK_END), ("PgUp", VK_PRIOR), ("PgDn", VK_NEXT),
    ("Left", VK_LEFT), ("Right", VK_RIGHT), ("Up", VK_UP), ("Down", VK_DOWN), ("Pause", VK_PAUSE), ("PrtSc", VK_SNAPSHOT),
    ("F1", VK_F1), ("F2", VK_F2), ("F3", VK_F3), ("F4", VK_F4), ("F5", VK_F5), ("F6", VK_F6),
    ("F7", VK_F7), ("F8", VK_F8), ("F9", VK_F9), ("F10", VK_F10), ("F11", VK_F11), ("F12", VK_F12),
    ("`", VK_OEM_3), ("-", VK_OEM_MINUS), ("=", VK_OEM_PLUS), ("[", VK_OEM_4), ("]", VK_OEM_6), ("\\", VK_OEM_5),
    (";", VK_OEM_1), ("'", VK_OEM_7), (",", VK_OEM_COMMA), (".", VK_OEM_PERIOD), ("/", VK_OEM_2),
];

/// "Ctrl+Alt+S" → Combo. Letters and digits, the names above; modifiers Ctrl, Shift, Alt, Win.
pub fn parse(text: &str) -> Option<Combo> {
    let mut mods = MOD_NOREPEAT;
    let mut vk = None;
    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "alt" => mods |= MOD_ALT,
            "win" | "super" => mods |= MOD_WIN,
            _ => {
                let up = part.to_ascii_uppercase();
                vk = if up.len() == 1 && up.as_bytes()[0].is_ascii_alphanumeric() {
                    Some(up.as_bytes()[0] as u32)
                } else {
                    NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(part)).map(|(_, k)| k.0 as u32)
                };
                vk?;
            }
        }
    }
    let vk = vk?;
    // A bare key would swallow ordinary typing; require Ctrl, Alt or Win (Shift alone is fine only with F-keys).
    let fkey = (VK_F1.0 as u32..=VK_F12.0 as u32).contains(&vk);
    if mods & (MOD_CONTROL | MOD_ALT | MOD_WIN) == HOT_KEY_MODIFIERS(0) && !fkey {
        return None;
    }
    Some(Combo { mods, vk })
}

pub fn format(c: Combo) -> String {
    let mut parts = Vec::new();
    if c.mods & MOD_CONTROL != HOT_KEY_MODIFIERS(0) { parts.push("Ctrl".to_string()); }
    if c.mods & MOD_ALT != HOT_KEY_MODIFIERS(0) { parts.push("Alt".to_string()); }
    if c.mods & MOD_SHIFT != HOT_KEY_MODIFIERS(0) { parts.push("Shift".to_string()); }
    if c.mods & MOD_WIN != HOT_KEY_MODIFIERS(0) { parts.push("Win".to_string()); }
    let key = if (b'A' as u32..=b'Z' as u32).contains(&c.vk) || (b'0' as u32..=b'9' as u32).contains(&c.vk) {
        (c.vk as u8 as char).to_string()
    } else {
        NAMED.iter().find(|(_, k)| k.0 as u32 == c.vk).map(|(n, _)| n.to_string()).unwrap_or_else(|| format!("VK{}", c.vk))
    };
    parts.push(key);
    parts.join("+")
}

/// A key press while rebinding: the key with whatever modifiers are held. None for a
/// modifier on its own or a combination that would not make a usable hotkey.
pub unsafe fn from_key(vk: u32) -> Option<Combo> {
    let is_mod = matches!(VIRTUAL_KEY(vk as u16), VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_MENU | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN);
    if is_mod {
        return None;
    }
    let mut mods = MOD_NOREPEAT;
    if GetKeyState(VK_CONTROL.0 as i32) < 0 { mods |= MOD_CONTROL; }
    if GetKeyState(VK_SHIFT.0 as i32) < 0 { mods |= MOD_SHIFT; }
    if GetKeyState(VK_MENU.0 as i32) < 0 { mods |= MOD_ALT; }
    if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 { mods |= MOD_WIN; }
    let c = Combo { mods, vk };
    parse(&format(c)) // round-trips only combinations we can name and re-register
}
