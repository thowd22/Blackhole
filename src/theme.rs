//! UI themes for the panel and the embedded editor. The dot itself keeps its own
//! colours (it is the app's identity); the panel, its editor and the Neovim palette
//! follow the chosen theme. Colours are 0xRRGGBB; `cr()` gives GDI's COLORREF.

use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::COLORREF;

pub struct Theme {
    pub name: &'static str,
    /// Panel ground, input/editor ground, selection, accent (frames, tags, cursor).
    pub bg: u32,
    pub bg_edit: u32,
    pub bg_sel: u32,
    pub accent: u32,
    pub fg: u32,
    pub fg_dim: u32,
    /// Extra colours for the editor palette.
    pub green: u32,
    pub red: u32,
    pub lilac: u32,
    pub cursor_line: u32,
}

pub const THEMES: [Theme; 8] = [
    Theme { name: "Blackhole", bg: 0x180A14, bg_edit: 0x301424, bg_sel: 0x782860, accent: 0xFFA040, fg: 0xFFE8F0, fg_dim: 0xB090A0, green: 0x78E08C, red: 0xFF5040, lilac: 0xC8A0E8, cursor_line: 0x3A1A2E },
    Theme { name: "Dracula", bg: 0x1E1F29, bg_edit: 0x282A36, bg_sel: 0x44475A, accent: 0xBD93F9, fg: 0xF8F8F2, fg_dim: 0x6272A4, green: 0x50FA7B, red: 0xFF5555, lilac: 0xFF79C6, cursor_line: 0x2F3140 },
    Theme { name: "Gruvbox", bg: 0x1D2021, bg_edit: 0x282828, bg_sel: 0x504945, accent: 0xFE8019, fg: 0xEBDBB2, fg_dim: 0x928374, green: 0xB8BB26, red: 0xFB4934, lilac: 0xD3869B, cursor_line: 0x32302F },
    Theme { name: "Nord", bg: 0x2E3440, bg_edit: 0x3B4252, bg_sel: 0x4C566A, accent: 0x88C0D0, fg: 0xECEFF4, fg_dim: 0x8891A8, green: 0xA3BE8C, red: 0xBF616A, lilac: 0xB48EAD, cursor_line: 0x434C5E },
    Theme { name: "Catppuccin", bg: 0x181825, bg_edit: 0x1E1E2E, bg_sel: 0x45475A, accent: 0xCBA6F7, fg: 0xCDD6F4, fg_dim: 0x9399B2, green: 0xA6E3A1, red: 0xF38BA8, lilac: 0xF5C2E7, cursor_line: 0x313244 },
    Theme { name: "One Dark", bg: 0x21252B, bg_edit: 0x282C34, bg_sel: 0x3E4451, accent: 0x61AFEF, fg: 0xABB2BF, fg_dim: 0x5C6370, green: 0x98C379, red: 0xE06C75, lilac: 0xC678DD, cursor_line: 0x2C313A },
    Theme { name: "Tokyo Night", bg: 0x16161E, bg_edit: 0x1A1B26, bg_sel: 0x33467C, accent: 0x7AA2F7, fg: 0xC0CAF5, fg_dim: 0x565F89, green: 0x9ECE6A, red: 0xF7768E, lilac: 0xBB9AF7, cursor_line: 0x292E42 },
    Theme { name: "Solarized Dark", bg: 0x002B36, bg_edit: 0x073642, bg_sel: 0x586E75, accent: 0x268BD2, fg: 0xEEE8D5, fg_dim: 0x839496, green: 0x859900, red: 0xDC322F, lilac: 0x6C71C4, cursor_line: 0x0A3D49 },
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);

pub fn current() -> &'static Theme {
    &THEMES[CURRENT.load(Ordering::Relaxed).min(THEMES.len() - 1)]
}

pub fn index_of(name: &str) -> Option<usize> {
    THEMES.iter().position(|t| t.name.eq_ignore_ascii_case(name.trim()))
}

/// Select by name; unknown names fall back to the default.
pub fn set_by_name(name: &str) {
    CURRENT.store(index_of(name).unwrap_or(0), Ordering::Relaxed);
}

/// The theme after the current one (the Settings row cycles).
pub fn next_name() -> &'static str {
    THEMES[(CURRENT.load(Ordering::Relaxed) + 1) % THEMES.len()].name
}

/// 0xRRGGBB → COLORREF (0x00BBGGRR).
pub fn cr(rgb: u32) -> COLORREF {
    COLORREF(((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF))
}

pub fn hex(rgb: u32) -> String {
    format!("#{rgb:06X}")
}
