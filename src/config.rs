//! Persistent settings: dot position and scale. Stored as JSON next to the vault.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

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
    /// Where the vault (vault.db, files\, captures\, models\) lives when the user moved
    /// it; empty = beside config.json. `config.json` itself always stays in the base dir,
    /// so the app can find the vault again after a move. See `data_dir`.
    #[serde(default)]
    pub vault_dir: String,
    /// What a dropped file leaves behind: "copy" (our own copy under `<vault>\files\`),
    /// "reference" (never copy; the original path is the only path) or "copy-small"
    /// (copy below `COPY_SMALL_MAX`). See `ingest::stored_copy`.
    #[serde(default = "copy")]
    pub store_policy: String,
    /// Folder name of the ask model to use (`models::list`); empty = the largest found.
    #[serde(default)]
    pub model_name: String,
}

/// Files this size or smaller are copied into the vault under the "copy-small" policy.
pub const COPY_SMALL_MAX: u64 = 25 * 1024 * 1024;

/// What a dropped file leaves behind in the vault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StorePolicy {
    /// Always keep our own copy (the default, and what Blackhole always did).
    Copy,
    /// Never copy: the item points at the original file only.
    Reference,
    /// Copy files up to `COPY_SMALL_MAX`, reference anything bigger.
    CopySmall,
}

impl StorePolicy {
    pub fn parse(s: &str) -> StorePolicy {
        match s {
            "reference" => StorePolicy::Reference,
            "copy-small" => StorePolicy::CopySmall,
            _ => StorePolicy::Copy,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            StorePolicy::Copy => "copy",
            StorePolicy::Reference => "reference",
            StorePolicy::CopySmall => "copy-small",
        }
    }
    /// The next one in the Settings row's cycle.
    pub fn next(self) -> StorePolicy {
        match self {
            StorePolicy::Copy => StorePolicy::CopySmall,
            StorePolicy::CopySmall => StorePolicy::Reference,
            StorePolicy::Reference => StorePolicy::Copy,
        }
    }
    /// Value shown in the Settings row.
    pub fn label(self) -> &'static str {
        match self {
            StorePolicy::Copy => "always",
            StorePolicy::Reference => "never",
            StorePolicy::CopySmall => "under 25 MB",
        }
    }
    pub fn copies(self, size: u64) -> bool {
        match self {
            StorePolicy::Copy => true,
            StorePolicy::Reference => false,
            StorePolicy::CopySmall => size <= COPY_SMALL_MAX,
        }
    }
}

fn copy() -> String {
    "copy".to_string()
}

/// The ingest worker asks for this per file; cached so it costs nothing, and
/// refreshed by `set_store_policy` when the Settings row changes it.
static POLICY: RwLock<Option<StorePolicy>> = RwLock::new(None);

pub fn store_policy() -> StorePolicy {
    if let Some(p) = *POLICY.read().unwrap() {
        return p;
    }
    let p = StorePolicy::parse(&load().store_policy);
    *POLICY.write().unwrap() = Some(p);
    p
}

pub fn set_store_policy(p: StorePolicy) {
    *POLICY.write().unwrap() = Some(p);
}

fn yes() -> bool {
    true
}

impl Config {
    /// The binding for action `i`: the configured text or the built-in default.
    pub fn hotkey(&self, i: usize) -> String {
        self.hotkeys.get(i).filter(|s| !s.trim().is_empty()).cloned().unwrap_or_else(|| crate::hotkeys::ACTIONS[i].1.to_string())
    }
}

impl Default for Config {
    fn default() -> Self {
        Config { x: 200, y: 200, scale: 2, center_on_message: true, hidden: false, tutorial_step: 0, think: true, notes_default: false, panel_w: 0, panel_h: 0, nvim_init: String::new(), hotkeys: Vec::new(), theme: String::new(), agent_notify: true, vault_dir: String::new(), store_policy: copy(), model_name: String::new() }
    }
}

/// Where `config.json` lives: `%LOCALAPPDATA%\Blackhole`, created on demand. This never
/// moves — it is how Blackhole finds a vault the user relocated (`Config::vault_dir`).
/// BLACKHOLE_DATA_DIR: run a second, isolated instance (testing) with its own config.
pub fn base_dir() -> PathBuf {
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

/// Resolved once (it is asked for on every ingest) and updated by `set_vault_dir`.
static VAULT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// The vault folder: `vault_dir` from the config when the user moved it, else `base_dir`.
pub fn data_dir() -> PathBuf {
    if let Some(d) = VAULT.read().unwrap().clone() {
        return d;
    }
    let base = base_dir();
    let want = load().vault_dir;
    // A vault on a drive that is not plugged in right now: fall back rather than
    // silently starting an empty one somewhere unexpected.
    let unreachable = !want.trim().is_empty() && std::fs::create_dir_all(&want).is_err();
    let dir = if want.trim().is_empty() || unreachable { base.clone() } else { PathBuf::from(&want) };
    // Cache before logging: `util::log` writes into the vault and would come back here.
    *VAULT.write().unwrap() = Some(dir.clone());
    if unreachable {
        crate::util::log(&format!("vault folder {want} is not reachable; using {}", base.display()));
    }
    dir
}

/// Point `data_dir` at `dir` for the rest of this run (after a move).
pub fn set_vault_dir(dir: &Path) {
    *VAULT.write().unwrap() = Some(dir.to_path_buf());
}

fn config_path() -> PathBuf {
    base_dir().join("config.json")
}

/// Move a vault's contents from one folder to another, leaving `config.json` behind.
///
/// A rename is what we want: instant, atomic, and it fails while the old process still
/// has vault.db open — which is exactly the signal to wait. `copy_ok` (only after the
/// caller has waited long enough) allows the copy + delete a different volume needs.
pub fn move_vault(from: &Path, to: &Path, copy_ok: bool) -> Result<(), String> {
    if from == to {
        return Ok(());
    }
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for e in entries.flatten() {
        let name = e.file_name();
        // config.json stays behind: it is what records where the vault went.
        if name.eq_ignore_ascii_case("config.json") {
            continue;
        }
        let src = e.path();
        let dst = to.join(&name);
        if dst.exists() {
            return Err(format!("{} already exists", dst.display()));
        }
        let Err(err) = std::fs::rename(&src, &dst) else { continue };
        if !copy_ok {
            return Err(format!("{} is still in use ({err})", src.display()));
        }
        // Different volume: copy, then delete — and leave nothing half-copied behind
        // if it goes wrong, so the next attempt starts from a clean folder.
        let copied = copy_tree(&src, &dst).map_err(|e| format!("{}: {e}", src.display())).and_then(|()| {
            if src.is_dir() { std::fs::remove_dir_all(&src) } else { std::fs::remove_file(&src) }.map_err(|e| format!("{}: copied, but the original could not be removed ({e})", src.display()))
        });
        if let Err(e) = copied {
            let _ = if dst.is_dir() { std::fs::remove_dir_all(&dst) } else { std::fs::remove_file(&dst) };
            return Err(e);
        }
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)?.flatten() {
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
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
