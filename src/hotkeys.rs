//! Global hotkeys as text ("Ctrl+Shift+Space"), parsed for RegisterHotKey and built
//! back from a key event when the user rebinds one in the Settings tab.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// The four actions, in the order the config and the Settings tab list them.
pub const ACTIONS: [(&str, &str); 4] =
    [("Summon to mouse and search", "Ctrl+Shift+Space"), ("Swallow the clipboard", "Ctrl+Shift+V"), ("Take a screenshot", "Ctrl+Shift+S"), ("New note at the mouse", "Ctrl+Shift+N")];

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
    ("Space", VK_SPACE),
    ("Tab", VK_TAB),
    ("Enter", VK_RETURN),
    ("Esc", VK_ESCAPE),
    ("Backspace", VK_BACK),
    ("Ins", VK_INSERT),
    ("Del", VK_DELETE),
    ("Home", VK_HOME),
    ("End", VK_END),
    ("PgUp", VK_PRIOR),
    ("PgDn", VK_NEXT),
    ("Left", VK_LEFT),
    ("Right", VK_RIGHT),
    ("Up", VK_UP),
    ("Down", VK_DOWN),
    ("Pause", VK_PAUSE),
    ("PrtSc", VK_SNAPSHOT),
    ("F1", VK_F1),
    ("F2", VK_F2),
    ("F3", VK_F3),
    ("F4", VK_F4),
    ("F5", VK_F5),
    ("F6", VK_F6),
    ("F7", VK_F7),
    ("F8", VK_F8),
    ("F9", VK_F9),
    ("F10", VK_F10),
    ("F11", VK_F11),
    ("F12", VK_F12),
    ("`", VK_OEM_3),
    ("-", VK_OEM_MINUS),
    ("=", VK_OEM_PLUS),
    ("[", VK_OEM_4),
    ("]", VK_OEM_6),
    ("\\", VK_OEM_5),
    (";", VK_OEM_1),
    ("'", VK_OEM_7),
    (",", VK_OEM_COMMA),
    (".", VK_OEM_PERIOD),
    ("/", VK_OEM_2),
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
    if c.mods & MOD_CONTROL != HOT_KEY_MODIFIERS(0) {
        parts.push("Ctrl".to_string());
    }
    if c.mods & MOD_ALT != HOT_KEY_MODIFIERS(0) {
        parts.push("Alt".to_string());
    }
    if c.mods & MOD_SHIFT != HOT_KEY_MODIFIERS(0) {
        parts.push("Shift".to_string());
    }
    if c.mods & MOD_WIN != HOT_KEY_MODIFIERS(0) {
        parts.push("Win".to_string());
    }
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
    if GetKeyState(VK_CONTROL.0 as i32) < 0 {
        mods |= MOD_CONTROL;
    }
    if GetKeyState(VK_SHIFT.0 as i32) < 0 {
        mods |= MOD_SHIFT;
    }
    if GetKeyState(VK_MENU.0 as i32) < 0 {
        mods |= MOD_ALT;
    }
    if GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0 {
        mods |= MOD_WIN;
    }
    let c = Combo { mods, vk };
    parse(&format(c)) // round-trips only combinations we can name and re-register
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_binding_parses() {
        for (name, binding) in ACTIONS {
            let c = parse(binding).unwrap_or_else(|| panic!("{name}: {binding} does not parse"));
            assert_eq!(format(c), binding, "{name}");
        }
    }

    #[test]
    fn round_trips_in_canonical_order() {
        for text in ["Ctrl+Shift+Space", "Ctrl+Alt+S", "Ctrl+Alt+Shift+Win+F7", "Win+V", "Alt+F4", "Ctrl+9", "Ctrl+Shift+PgDn", "Ctrl+["] {
            let c = parse(text).unwrap_or_else(|| panic!("{text} does not parse"));
            assert_eq!(format(c), text);
        }
    }

    #[test]
    fn modifier_spelling_and_case_are_forgiving() {
        assert_eq!(parse("control+shift+space"), parse("Ctrl+Shift+Space"));
        assert_eq!(parse("CTRL + ALT + s"), parse("Ctrl+Alt+S"));
        assert_eq!(parse("super+v"), parse("Win+V"));
        assert_eq!(parse("Ctrl+Shift+ESC"), parse("Ctrl+Shift+Esc"));
    }

    #[test]
    fn a_bare_key_would_swallow_typing_and_is_refused() {
        assert!(parse("S").is_none());
        assert!(parse("Shift+S").is_none());
        assert!(parse("Space").is_none());
    }

    #[test]
    fn function_keys_may_stand_alone() {
        assert!(parse("F5").is_some());
        assert!(parse("Shift+F5").is_some());
        assert_eq!(format(parse("Shift+F12").unwrap()), "Shift+F12");
    }

    #[test]
    fn nonsense_does_not_parse() {
        assert!(parse("").is_none());
        assert!(parse("Ctrl+").is_none());
        assert!(parse("Ctrl+Shift").is_none());
        assert!(parse("Ctrl+Banana").is_none());
        assert!(parse("Ctrl++").is_none());
    }

    #[test]
    fn modifiers_are_flags_not_an_order() {
        assert_eq!(parse("Shift+Ctrl+Alt+K"), parse("Alt+Ctrl+Shift+K"));
    }

    #[test]
    fn norepeat_is_always_set_so_a_held_key_fires_once() {
        let c = parse("Ctrl+Shift+Space").unwrap();
        assert!(c.mods & MOD_NOREPEAT != HOT_KEY_MODIFIERS(0));
    }

    #[test]
    fn unknown_virtual_keys_format_as_vk_and_do_not_round_trip() {
        let c = Combo { mods: MOD_CONTROL | MOD_NOREPEAT, vk: 0xFE };
        assert_eq!(format(c), "Ctrl+VK254");
        assert!(parse(&format(c)).is_none());
    }

    #[test]
    fn taken_flags_default_to_free() {
        // Index 3 only, so the flag this test writes is its own.
        set_taken(3, true);
        assert!(taken(3));
        set_taken(3, false);
        assert!(!taken(3));
    }
}
