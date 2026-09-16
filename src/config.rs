//! Persistent settings: dot position and scale. Stored as JSON next to the vault.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config { x: 200, y: 200, scale: 2, center_on_message: true, hidden: false, tutorial_step: 0, think: true }
    }
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
