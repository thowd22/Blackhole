//! Persistent settings: dot position and scale. Stored as JSON next to the vault.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub x: i32,
    pub y: i32,
    /// Integer pixel scale applied on top of the DPI scale (1 = 32px at 96 DPI).
    pub scale: i32,
    /// Warp the dot to the centre of the screen when a message arrives.
    #[serde(default = "yes")]
    pub center_on_message: bool,
    /// Dot hidden (tray only).
    #[serde(default)]
    pub hidden: bool,
    /// First-run tutorial progress; see `dot::TUTORIAL`.
    #[serde(default)]
    pub tutorial_step: u8,
    /// Let the model reason before answering (models with a think mode; ~5 s extra).
    #[serde(default = "yes")]
    pub think: bool,
    /// The panel opens on the Notes tab instead of Files.
    #[serde(default)]
    pub notes_default: bool,
    /// The user's own Neovim init file, sourced after Blackhole's (empty = built-in only).
    #[serde(default)]
    pub nvim_init: String,
    /// Global hotkeys in `hotkeys::ACTIONS` order ("Ctrl+Shift+Space", …); empty = default.
    #[serde(default)]
    pub hotkeys: Vec<String>,
    /// Panel/editor theme name (see `theme::THEMES`); empty = Blackhole.
    #[serde(default)]
    pub theme: String,
    /// Agents may show speech bubbles through the MCP `notify` tool.
    #[serde(default = "yes")]
    pub agent_notify: bool,
    /// Search panel size the user dragged, in 96-DPI px (0 = default); see `search.rs`.
    #[serde(default)]
    pub panel_w: i32,
    #[serde(default)]
    pub panel_h: i32,
    /// How opaque the dot is when nothing is happening, in percent (100 = solid).
    /// It eases back to 100 on hover, a bubble or the panel.
    #[serde(default = "hundred")]
    pub idle_opacity: i32,
    /// Shrink to a few pixels after a few seconds of being left alone.
    #[serde(default)]
    pub shy: bool,
    /// Snap flush to the work-area edges and corners while dragging.
    #[serde(default = "yes")]
    pub snap: bool,
    /// Swallowing is paused: drops, paste, screenshots and MCP `put` are refused.
    #[serde(default)]
    pub paused: bool,
    /// Dot sprite colours (see `sprite::PALETTES`); empty = Ember.
    #[serde(default)]
    pub dot_palette: String,
}

fn yes() -> bool {
    true
}

fn hundred() -> i32 {
    100
}

impl Config {
    /// The binding for action `i`: the configured text or the built-in default.
    pub fn hotkey(&self, i: usize) -> String {
        self.hotkeys.get(i).filter(|s| !s.trim().is_empty()).cloned().unwrap_or_else(|| crate::hotkeys::ACTIONS[i].1.to_string())
    }
}

impl Default for Config {
    fn default() -> Self {
        Config { x: 200, y: 200, scale: 2, center_on_message: true, hidden: false, tutorial_step: 0, think: true, notes_default: false, panel_w: 0, panel_h: 0, nvim_init: String::new(), hotkeys: Vec::new(), theme: String::new(), agent_notify: true, idle_opacity: 100, shy: false, snap: true, paused: false, dot_palette: String::new() }
    }
}

/// Mirror of `Config::paused` for the paths that are asked many times a second
/// (drag-over, drop, screenshot); the dot keeps it in step with the file.
static PAUSED: AtomicBool = AtomicBool::new(false);

pub fn set_paused(v: bool) {
    PAUSED.store(v, Ordering::Relaxed);
}

pub fn paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

/// `%LOCALAPPDATA%\Blackhole`, created on demand.
pub fn data_dir() -> PathBuf {
    // BLACKHOLE_DATA_DIR: run a second, isolated instance (testing) with its own vault/config.
    if let Some(d) = std::env::var_os("BLACKHOLE_DATA_DIR") {
        let dir = PathBuf::from(d);
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("Blackhole");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn load() -> Config {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(config_path(), s);
    }
}
