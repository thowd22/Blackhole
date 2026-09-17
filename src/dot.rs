//! The dot itself: a layered, topmost, tool window showing the sprite.
//! Handles drag-to-move, click-to-search, the summon hotkey, the context
//! menu (shared with the tray icon), speech bubbles and the tutorial.

use crate::ask::AskEngine;
use crate::bubble::{Bubble, WM_BUBBLE_CLICKED, WM_BUBBLE_EXPIRED};
use crate::config::{self, Config};
use crate::drop::{self, DropTarget, WM_DROP_ENTER, WM_DROP_LEAVE, WM_DROP_SWALLOW};
use crate::embed::Embedder;
use crate::ingest::{Input, Report};
use crate::search::{SearchWin, WM_ASK_FIRST_TOKEN, WM_ASK_STARTED, WM_PANEL_RESIZED, WM_SEARCH_CLOSED};
use crate::sprite::{self, Anim, Mood, SIZE, SPECK_SIZE};
use crate::startup;
use crate::store::Store;
use crate::tray::{self, WM_TRAY};
use crate::util::wide;
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE as WSIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Ole::{IDropTarget, RegisterDragDrop, RevokeDragDrop};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{ShellExecuteW, NINF_KEY, NIN_SELECT};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_INGEST_DONE: u32 = 0x8004;
/// Show a notification bubble; lparam = Box<String>. Used by ingest and (later) MCP.
pub const WM_NOTIFY: u32 = 0x8005;
/// Like WM_NOTIFY but quiet: no warp to the screen centre, gone after a few seconds
/// (confirmations of something the user just did, e.g. a screenshot).
pub const WM_NOTIFY_QUIET: u32 = 0x8017;
const WM_STARTUP: u32 = 0x8006;

const TIMER_ANIM: usize = 1;
/// Unload the LLM this long after the search panel closes.
const TIMER_UNLOAD: usize = 2;
const UNLOAD_AFTER_MS: u32 = 60_000;
/// Animation tick while nothing but the ring is moving (idle, listening…).
const IDLE_MS: u32 = 90;
/// Faster tick while specks or a mood transition are on screen (~20 fps).
const ACTIVE_MS: u32 = 33;
/// Every mood change eases over this long — fast but visible, never a snap.
const TRANSITION_MS: u32 = 150;
const HOTKEY_SUMMON: i32 = 1;
const HOTKEY_PASTE: i32 = 2;
const HOTKEY_SHOT: i32 = 3;
const HOTKEY_NOTE: i32 = 4;
/// Ctrl+Shift+S was taken; the screenshot key is Ctrl+Alt+S (menu label follows).


const MENU_SEARCH: usize = 1;
const MENU_PASTE: usize = 2;
const MENU_VAULT: usize = 3;
const MENU_SIZE_S: usize = 4;
const MENU_SIZE_M: usize = 5;
const MENU_SIZE_L: usize = 6;
const MENU_SHOW_DOT: usize = 7;
const MENU_CENTER_MSG: usize = 8;
const MENU_QUIT: usize = 9;
const MENU_START_LOGIN: usize = 10;
const MENU_TUTORIAL: usize = 11;
const MENU_SIZE_XL: usize = 12;
pub const MENU_THINK: usize = 13;
pub const MENU_VIEW_FILES: usize = 15;
pub const MENU_VIEW_NOTES: usize = 16;
const MENU_NEW_NOTE: usize = 17;
pub const MENU_NVIM_CONFIG: usize = 18;
pub const MENU_NVIM_BUILTIN: usize = 19;
const MENU_SETTINGS: usize = 20;
/// Settings tab: cycle to the next theme.
pub const MENU_THEME_NEXT: usize = 21;
/// Settings tab: toggle whether MCP clients may show bubbles.
pub const MENU_AGENT_NOTIFY: usize = 22;
pub const MENU_SCREENSHOT: usize = 14;
/// Settings tab: pick a new vault folder, move the vault there and restart.
pub const MENU_VAULT_FOLDER: usize = 40;
/// Settings tab: cycle copy / copy-small / reference for dropped files.
pub const MENU_STORE_POLICY: usize = 41;
/// Settings tab: switch to the next ask model found on disk.
pub const MENU_MODEL_NEXT: usize = 42;
/// Settings tab: start (or cancel) the default model download.
pub const MENU_MODEL_DOWNLOAD: usize = 43;
pub const MENU_CENTER_MSG_PUB: usize = MENU_CENTER_MSG;
pub const MENU_START_LOGIN_PUB: usize = MENU_START_LOGIN;
/// From the Settings tab: wparam = action index, lparam = boxed String combo. The dot
/// stores it, re-registers, and tells the panel to refresh (WM_SETTINGS_CHANGED).
pub const WM_SET_HOTKEY: u32 = 0x8018;
pub const WM_SETTINGS_CHANGED: u32 = 0x8019;
/// lparam: Box<Notice> — a bubble with an optional click action (MCP `notify`).
pub const WM_NOTICE: u32 = 0x801A;

/// What a clicked bubble does.
pub enum NoticeAction {
    /// Open the panel on the Files tab with this query.
    Search(String),
    /// Open this http(s) URL in the default browser.
    Url(String),
}

pub struct Notice {
    pub text: String,
    /// No warp to the screen centre, shorter stay.
    pub quiet: bool,
    pub action: Option<NoticeAction>,
    pub timeout_ms: Option<u32>,
}

/// First-run tutorial. Each step waits for the action it describes.
pub const TUTORIAL: &[&str] = &[
    "Hi! I'm Blackhole.\nDrop a file or some text on me to get started. Ctrl+Shift+V swallows the clipboard.",
    "Nice. Click me to search what I've eaten.",
    "Press Ctrl+Shift+Space anywhere to summon me to your mouse.",
    "Start a search with ? to ask me a question about your files.",
    "Right-click me for size, tray and settings.\nThat's it — I'll be here.",
    "Blackhole is open source:\ngithub.com/thowd22/Blackhole\nIssues and PRs are welcome.",
];
const TUTORIAL_DONE: u8 = TUTORIAL.len() as u8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Event {
    Swallow,
    SearchOpened,
    Summoned,
    Asked,
}

pub struct Dot {
    hwnd: HWND,
    search: HWND,
    bubble: HWND,
    cfg: Config,
    tx: Sender<Input>,
    store: Arc<Mutex<Store>>,
    embedder: Arc<Embedder>,
    ask: Arc<AskEngine>,
    drop_target: Option<IDropTarget>,
    /// App start; sprite motion runs on milliseconds since then.
    t0: Instant,
    last_tick: Instant,
    /// Current animation timer period (IDLE_MS or ACTIVE_MS).
    tick_ms: u32,
    phase: f32,
    mood: Mood,
    /// The mood being eased away from, and when the change happened.
    prev_mood: Mood,
    mood_since: Instant,
    mood_until: Option<Instant>,
    pending: usize,
    /// Position before the last summon / centring, so it can go back.
    home: Option<(i32, i32)>,
    dragging: bool,
    drag_moved: bool,
    drag_origin: POINT,
    win_origin: POINT,
    pixels: Vec<u32>,
    /// 128×128 quarter-pixel speck overlay, composited over `pixels` at scale time.
    specks: Vec<u32>,
    /// What was last pushed to the screen, so identical frames cost nothing.
    shown: (Vec<u32>, Vec<u32>),
    dib: HBITMAP,
    dib_bits: *mut u32,
    mem_dc: HDC,
    /// Queued notification texts; one bubble at a time.
    /// Queued notification text and whether it is "quiet" (no centring, short).
    messages: VecDeque<Notice>,
    /// The action of the bubble on screen, run when its body is clicked.
    pending_action: Option<NoticeAction>,
    /// True while the visible bubble is a tutorial step.
    tutorial_showing: bool,
    taskbar_created: u32,
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut Dot> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Dot).as_mut()
}

/// Source-over of two premultiplied BGRA pixels.
fn blend_over(dst: u32, src: u32) -> u32 {
    let inv = 255 - (src >> 24);
    let ch = |shift: u32| ((src >> shift) & 255) + ((dst >> shift) & 255) * inv / 255;
    ch(24) << 24 | ch(16) << 16 | ch(8) << 8 | ch(0)
}

impl Dot {
    pub fn create(tx: Sender<Input>, store: Arc<Mutex<Store>>, embedder: Arc<Embedder>, ask: Arc<AskEngine>) -> HWND {
        unsafe {
            let class = w!("BlackholeDot");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let cfg = config::load();
            crate::llm_ort::set_thinking(cfg.think);
            crate::theme::set_by_name(&cfg.theme);
            let dot = Box::new(Dot {
                hwnd: HWND::default(),
                search: HWND::default(),
                bubble: HWND::default(),
                tx,
                store,
                embedder,
                ask,
                drop_target: None,
                t0: Instant::now(),
                last_tick: Instant::now(),
                tick_ms: IDLE_MS,
                phase: 0.0,
                mood: Mood::Idle,
                prev_mood: Mood::Idle,
                mood_since: Instant::now(),
                mood_until: None,
                pending: 0,
                home: None,
                dragging: false,
                drag_moved: false,
                drag_origin: POINT::default(),
                win_origin: POINT::default(),
                pixels: vec![0; SIZE * SIZE],
                specks: vec![0; SPECK_SIZE * SPECK_SIZE],
                shown: (Vec::new(), Vec::new()),
                dib: HBITMAP::default(),
                dib_bits: std::ptr::null_mut(),
                mem_dc: HDC::default(),
                messages: VecDeque::new(),
                pending_action: None,
                tutorial_showing: false,
                taskbar_created: tray::taskbar_created_message(),
                cfg,
            });
            let (x, y, hidden) = (dot.cfg.x, dot.cfg.y, dot.cfg.hidden);
            let ptr = Box::into_raw(dot);
            let style = if hidden { WS_POPUP } else { WS_POPUP | WS_VISIBLE };
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!("Blackhole"),
                style,
                x, y, 64, 64,
                None, None, None,
                Some(ptr as *const _),
            )
            .unwrap_or_default()
        }
    }

    /// Integer pixel multiplier: user scale × DPI factor.
    unsafe fn unit(&self) -> i32 {
        let dpi = GetDpiForWindow(self.hwnd).max(96) as i32;
        let dpi_mul = ((dpi + 48) / 96).max(1);
        self.cfg.scale.clamp(1, 4) * dpi_mul
    }

    /// On-screen size in pixels.
    unsafe fn px_size(&self) -> i32 {
        SIZE as i32 * self.unit()
    }

    unsafe fn ensure_surface(&mut self, side: i32) {
        if !self.dib.is_invalid() {
            let mut bm = BITMAP::default();
            GetObjectW(self.dib.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
            if bm.bmWidth == side {
                return;
            }
            let _ = DeleteObject(self.dib.into());
            let _ = DeleteDC(self.mem_dc);
        }
        self.shown = (Vec::new(), Vec::new());
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: side,
                biHeight: -side, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        self.dib = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        self.dib_bits = bits as *mut u32;
        self.mem_dc = CreateCompatibleDC(None);
        SelectObject(self.mem_dc, self.dib.into());
    }

    unsafe fn redraw(&mut self) {
        let side = self.px_size();
        self.ensure_surface(side);
        if self.dib_bits.is_null() {
            return;
        }
        let anim = self.anim();
        sprite::render(&mut self.pixels, &anim);
        let specks = sprite::render_specks(&mut self.specks, &anim);
        if self.shown.0 == self.pixels && self.shown.1 == self.specks {
            return; // nothing moved since the last frame
        }
        self.shown.0.clone_from(&self.pixels);
        self.shown.1.clone_from(&self.specks);

        // Nearest-neighbour upscale straight into the DIB; the speck overlay
        // is sampled at twice the sprite resolution and composited on top.
        let mul = (side as usize) / SIZE;
        let dst = std::slice::from_raw_parts_mut(self.dib_bits, (side * side) as usize);
        for y in 0..side as usize {
            let sy = y / mul;
            let row = &self.pixels[sy * SIZE..sy * SIZE + SIZE];
            let over = &self.specks[(y * 4 / mul) * SPECK_SIZE..(y * 4 / mul + 1) * SPECK_SIZE];
            let out = &mut dst[y * side as usize..(y + 1) * side as usize];
            for (x, px) in out.iter_mut().enumerate() {
                *px = row[x / mul];
                if specks {
                    let s = over[x * 4 / mul];
                    if s >> 24 != 0 {
                        *px = blend_over(*px, s);
                    }
                }
            }
        }

        let r = self.rect();
        let pos = POINT { x: r.left, y: r.top };
        let size = WSIZE { cx: side, cy: side };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = UpdateLayeredWindow(self.hwnd, None, Some(&pos), Some(&size), Some(self.mem_dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
    }

    fn set_mood(&mut self, mood: Mood, for_ms: Option<u64>) {
        self.mood_until = for_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        self.change_mood(mood);
    }

    /// Switch moods, easing from the current one over TRANSITION_MS.
    fn change_mood(&mut self, mood: Mood) {
        if mood == self.mood {
            return;
        }
        let now = Instant::now();
        let elapsed = now - self.mood_since;
        let full = Duration::from_millis(TRANSITION_MS as u64);
        // Going straight back mid-transition (drag in, drag out): run the same
        // ease in reverse from where it is rather than restarting.
        self.mood_since = if mood == self.prev_mood && elapsed < full { now - (full - elapsed) } else { now };
        self.prev_mood = self.mood;
        self.mood = mood;
    }

    /// The mood to fall back to when a temporary one ends.
    unsafe fn base_mood(&self) -> Mood {
        if self.pending > 0 {
            Mood::Digesting
        } else if self.search_open() {
            Mood::Listening
        } else {
            Mood::Idle
        }
    }

    /// Eased progress of the current mood transition; 1 once it is over.
    fn blend(&self) -> f32 {
        let t = self.mood_since.elapsed().as_millis() as f32 / TRANSITION_MS as f32;
        if t >= 1.0 { 1.0 } else { t * t * (3.0 - 2.0 * t) }
    }

    fn anim(&self) -> Anim {
        Anim {
            t_ms: self.t0.elapsed().as_millis() as u32,
            phase: self.phase,
            mood: self.mood,
            prev: self.prev_mood,
            blend: self.blend(),
        }
    }

    unsafe fn tick(&mut self) {
        // Advance the ring by wall-clock time so its speed doesn't depend on the tick rate.
        let now = Instant::now();
        let dt = (now - self.last_tick).as_millis().min(250) as f32 / IDLE_MS as f32;
        self.last_tick = now;
        let blend = self.blend();
        let speed = sprite::speed(self.prev_mood) + (sprite::speed(self.mood) - sprite::speed(self.prev_mood)) * blend;
        self.phase = (self.phase + 0.12 * speed * dt) % (std::f32::consts::TAU * 30.0);
        if let Some(t) = self.mood_until {
            if now >= t {
                self.mood_until = None;
                let base = self.base_mood();
                self.change_mood(base);
            }
        }
        self.redraw();
        // Tick fast only while specks or a transition are moving; idle stays cheap.
        let busy = sprite::has_specks(self.mood) || self.blend() < 1.0;
        let want = if busy { ACTIVE_MS } else { IDLE_MS };
        if want != self.tick_ms {
            self.tick_ms = want;
            SetTimer(Some(self.hwnd), TIMER_ANIM, want, None);
        }
    }

    unsafe fn rect(&self) -> RECT {
        let mut r = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut r);
        r
    }

    unsafe fn search_open(&self) -> bool {
        !self.search.is_invalid() && IsWindowVisible(self.search).as_bool()
    }

    unsafe fn move_to(&mut self, x: i32, y: i32) {
        let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        self.cfg.x = x;
        self.cfg.y = y;
        if !self.bubble.is_invalid() {
            Bubble::follow(self.bubble, self.rect());
        }
    }

    unsafe fn set_hidden(&mut self, hidden: bool) {
        self.cfg.hidden = hidden;
        config::save(&self.cfg);
        let _ = ShowWindow(self.hwnd, if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE });
        if hidden {
            if self.search_open() {
                SearchWin::hide(self.search);
            }
            if !self.bubble.is_invalid() {
                Bubble::hide(self.bubble);
            }
        }
    }

    unsafe fn open_search(&mut self) {
        if self.cfg.hidden {
            self.set_hidden(false);
        }
        if self.search.is_invalid() {
            self.search = SearchWin::create(self.hwnd, self.store.clone(), self.embedder.clone(), self.ask.clone(), (self.cfg.panel_w, self.cfg.panel_h), self.cfg.notes_default, &self.cfg.nvim_init);
        }
        self.set_mood(Mood::Listening, None);
        SearchWin::show(self.search, self.rect());
        // Warm the LLM now; a `?` question a few seconds from now finds it loaded.
        let _ = KillTimer(Some(self.hwnd), TIMER_UNLOAD);
        self.ask.preload();
        self.tutorial_event(Event::SearchOpened);
    }

    unsafe fn toggle_search(&mut self) {
        if self.search_open() {
            SearchWin::hide(self.search);
        } else {
            self.open_search();
        }
    }

    /// Hotkey: warp to the cursor and open search; press again to go home.
    unsafe fn summon(&mut self) {
        if self.search_open() {
            SearchWin::hide(self.search);
            self.go_home();
            return;
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let side = self.px_size();
        if self.home.is_none() {
            self.home = Some((self.cfg.x, self.cfg.y));
        }
        if self.cfg.hidden {
            self.set_hidden(false);
        }
        self.move_to(pt.x - side / 2, pt.y - side / 2);
        self.set_mood(Mood::Satisfied, Some(250));
        self.redraw();
        self.open_search();
        self.tutorial_event(Event::Summoned);
    }

    /// (Re)register the four global hotkeys from the config. A key another program owns
    /// fails silently in Win32; that is logged and shown in the Settings tab.
    unsafe fn register_hotkeys(&mut self) {
        for (i, id) in [HOTKEY_SUMMON, HOTKEY_PASTE, HOTKEY_SHOT, HOTKEY_NOTE].into_iter().enumerate() {
            let _ = UnregisterHotKey(Some(self.hwnd), id);
            let text = self.cfg.hotkey(i);
            let ok = match crate::hotkeys::parse(&text) {
                Some(c) => RegisterHotKey(Some(self.hwnd), id, c.mods, c.vk).is_ok(),
                None => false,
            };
            crate::hotkeys::set_taken(i, !ok);
            if !ok {
                crate::util::log(&format!("hotkey {text} ({}) could not be registered: taken by another program or invalid", crate::hotkeys::ACTIONS[i].0));
            }
        }
    }

    /// Standard file dialog for an init.lua / init.vim; starts in %LOCALAPPDATA%\nvim.
    unsafe fn pick_nvim_config(&self) -> Option<String> {
        use windows::Win32::UI::Controls::Dialogs::*;
        let mut file = vec![0u16; 1024];
        let filter: Vec<u16> = "Neovim config (init.lua, init.vim)\0init.lua;init.vim;*.lua;*.vim\0All files\0*.*\0\0".encode_utf16().collect();
        let start = crate::util::wide(&dirs::data_local_dir().map(|d| d.join("nvim").display().to_string()).unwrap_or_default());
        let title = w!("Choose your Neovim config");
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: self.hwnd,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(file.as_mut_ptr()),
            nMaxFile: file.len() as u32,
            lpstrInitialDir: PCWSTR(start.as_ptr()),
            lpstrTitle: title,
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
            ..Default::default()
        };
        if !GetOpenFileNameW(&mut ofn).as_bool() {
            return None;
        }
        let path = crate::util::from_wide(&file);
        (!path.is_empty()).then_some(path)
    }

    /// Folder picker for the vault (the shell's browse dialog, new-style with an edit box).
    unsafe fn pick_folder(&self, title: &str) -> Option<String> {
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW, BROWSEINFOW, BIF_EDITBOX, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS};
        let t = wide(title);
        let bi = BROWSEINFOW {
            hwndOwner: self.hwnd,
            lpszTitle: PCWSTR(t.as_ptr()),
            ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
            ..Default::default()
        };
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let ok = SHGetPathFromIDListW(pidl, &mut buf).as_bool();
        CoTaskMemFree(Some(pidl as *const _));
        if !ok {
            return None;
        }
        let path = crate::util::from_wide(&buf);
        (!path.is_empty()).then_some(path)
    }

    /// Settings → "Vault folder": pick a folder, confirm, then hand the move to a fresh
    /// copy of ourselves (`--move-vault`) and quit, because vault.db is open right here.
    unsafe fn move_vault(&mut self) {
        let from = config::data_dir();
        let Some(picked) = self.pick_folder("Where should the vault live?") else { return };
        let mut to = std::path::PathBuf::from(&picked);
        // Picking a plain folder like D:\Data would strew vault.db over it: keep our
        // own folder inside unless the user picked one that is already a vault.
        if to != from && !to.join("vault.db").exists() && to.file_name().map(|n| n != "Blackhole").unwrap_or(true) {
            to = to.join("Blackhole");
        }
        if to == from {
            return;
        }
        let question = wide(&format!(
            "Move the vault from\n{}\n\nto\n{}\n\nBlackhole restarts to finish the move.",
            from.display(),
            to.display()
        ));
        if MessageBoxW(Some(self.hwnd), PCWSTR(question.as_ptr()), w!("Blackhole"), MB_OKCANCEL | MB_ICONQUESTION) != IDOK {
            return;
        }
        self.cfg.vault_dir = if to == config::base_dir() { String::new() } else { to.display().to_string() };
        config::save(&self.cfg);
        let Ok(exe) = std::env::current_exe() else { return };
        let spawned = std::process::Command::new(exe)
            .arg("--move-vault")
            .arg(from.as_os_str())
            .arg(to.as_os_str())
            .spawn();
        match spawned {
            Ok(_) => {
                crate::util::log(&format!("vault move: {} -> {}, restarting", from.display(), to.display()));
                let _ = DestroyWindow(self.hwnd);
            }
            Err(e) => {
                // Could not restart: leave the vault where it is.
                self.cfg.vault_dir = String::new();
                config::save(&self.cfg);
                let msg = wide(&format!("Could not restart to move the vault:\n{e}"));
                MessageBoxW(Some(self.hwnd), PCWSTR(msg.as_ptr()), w!("Blackhole"), MB_ICONERROR);
            }
        }
    }

    /// Ctrl+Shift+N: come to the mouse and open a fresh note on the Notes tab.
    unsafe fn new_note(&mut self) {
        if !self.search_open() {
            self.summon();
        }
        SearchWin::new_note_in(self.search);
    }

    unsafe fn go_home(&mut self) {
        if let Some((x, y)) = self.home.take() {
            self.move_to(x, y);
            config::save(&self.cfg);
        }
    }

    /// Warp to the centre of the monitor the cursor is on (for messages).
    unsafe fn center(&mut self) {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(mon, &mut mi);
        let w = mi.rcWork;
        let side = self.px_size();
        if self.home.is_none() {
            self.home = Some((self.cfg.x, self.cfg.y));
        }
        self.move_to((w.left + w.right) / 2 - side / 2, (w.top + w.bottom) / 2 - side / 2);
    }

    unsafe fn paste_clipboard(&mut self) {
        match drop::read_clipboard(self.hwnd) {
            Some(input) => {
                let _ = self.tx.send(input);
                self.swallow();
            }
            None => self.set_mood(Mood::Upset, Some(700)),
        }
    }

    /// Drag-region capture; the overlay posts WM_DROP_SWALLOW and WM_NOTIFY back when done.
    unsafe fn screenshot(&mut self) {
        let unit = self.unit();
        crate::screenshot::start(self.hwnd, self.tx.clone(), unit);
    }

    unsafe fn swallow(&mut self) {
        self.pending += 1;
        self.set_mood(Mood::Digesting, None);
        self.tutorial_event(Event::Swallow);
    }

    unsafe fn ingest_done(&mut self, report: Report) {
        if !report.errors.is_empty() {
            crate::util::log(&format!("ingest failures: {:?}", report.errors));
        }
        self.pending = self.pending.saturating_sub(1);
        if report.failed > 0 && report.added == 0 {
            self.set_mood(Mood::Upset, Some(1200));
        } else {
            self.set_mood(Mood::Satisfied, Some(700));
        }
        for e in report.errors {
            self.notify(format!("Couldn't swallow that: {e}"));
        }
        if report.updated > 0 {
            self.notify_quiet(if report.updated == 1 { "Already had that one — its text was re-read and updated".into() } else { format!("Already had {} of those — their text was re-read and updated", report.updated) });
        } else if report.duplicates > 0 && report.added == 0 {
            self.notify_quiet("Already swallowed that one".into());
        }
        let n = self.store.lock().unwrap().count();
        tray::set_tip(self.hwnd, &format!("Blackhole — {n} items inside"));
    }

    // ---- bubbles -------------------------------------------------------

    unsafe fn ensure_bubble(&mut self) {
        if self.bubble.is_invalid() {
            self.bubble = Bubble::create(self.hwnd);
        }
    }

    /// Queue a notification; shown when nothing else is up.
    unsafe fn notify(&mut self, text: String) {
        self.notice(Notice { text, quiet: false, action: None, timeout_ms: None });
    }

    unsafe fn notify_quiet(&mut self, text: String) {
        self.notice(Notice { text, quiet: true, action: None, timeout_ms: None });
    }

    unsafe fn notice(&mut self, n: Notice) {
        self.messages.push_back(n);
        self.show_next_message();
    }

    unsafe fn run_action(&mut self, action: NoticeAction) {
        match action {
            NoticeAction::Search(q) => {
                self.open_search();
                SearchWin::open_with_query(self.search, &q);
            }
            NoticeAction::Url(u) => {
                let w = wide(&u);
                ShellExecuteW(None, w!("open"), PCWSTR(w.as_ptr()), None, None, SW_SHOWNORMAL);
            }
        }
    }

    unsafe fn show_next_message(&mut self) {
        self.ensure_bubble();
        if Bubble::is_visible(self.bubble) {
            if !self.tutorial_showing || self.messages.is_empty() {
                return; // a message is up; the next one follows when it closes
            }
            // A notification pre-empts a (sticky) tutorial step; the step returns afterwards.
            Bubble::hide(self.bubble);
        }
        let Some(Notice { text, quiet, action, timeout_ms }) = self.messages.pop_front() else { return };
        self.pending_action = action;
        if self.cfg.hidden {
            self.set_hidden(false);
        }
        // Warp to the screen centre unless the user is mid-search beside the dot, or the
        // message only confirms something they just did here.
        if self.cfg.center_on_message && !self.search_open() && !quiet {
            self.center();
        }
        self.tutorial_showing = false;
        let unit = self.unit();
        let timeout = timeout_ms.map(|t| t.clamp(1000, 60000)).unwrap_or(if quiet { 2500 } else { (6000 + 40 * text.len() as u32).min(20000) }); // longer texts stay longer
        Bubble::show(self.bubble, &text, self.rect(), unit, Some(timeout));
        self.set_mood(Mood::Satisfied, Some(400));
    }

    /// `closed_x`: the × was clicked (skips the tour); a body click or timeout just hides.
    unsafe fn bubble_closed(&mut self, closed_x: bool) {
        if self.tutorial_showing {
            self.tutorial_showing = false;
            let step = self.cfg.tutorial_step;
            if closed_x || step + 1 >= TUTORIAL_DONE {
                self.cfg.tutorial_step = TUTORIAL_DONE;
                config::save(&self.cfg);
            } else if step >= 4 {
                // Informational card finished: move to the next one.
                self.cfg.tutorial_step = step + 1;
                config::save(&self.cfg);
            }
        }
        // A message may have been queued behind a bubble; go home after messages.
        if self.messages.is_empty() && !self.search_open() {
            self.go_home();
        }
        self.show_next_message();
        // Nothing left to say? Resume the tutorial where it was.
        if !Bubble::is_visible(self.bubble) && self.cfg.tutorial_step < TUTORIAL_DONE {
            self.show_tutorial_step();
        }
    }

    // ---- tutorial ------------------------------------------------------

    unsafe fn show_tutorial_step(&mut self) {
        let step = self.cfg.tutorial_step as usize;
        if step >= TUTORIAL.len() {
            return;
        }
        self.ensure_bubble();
        if self.cfg.hidden {
            self.set_hidden(false);
        }
        self.tutorial_showing = true;
        let unit = self.unit();
        // The closing cards are informational: they time out and chain instead of waiting for an action.
        let informational = step >= 4;
        let timeout = if informational { Some(12000) } else { None };
        Bubble::show(self.bubble, TUTORIAL[step], self.rect(), unit, timeout);
    }

    /// Advance the tutorial when the awaited action happens.
    unsafe fn tutorial_event(&mut self, ev: Event) {
        let step = self.cfg.tutorial_step;
        if step >= TUTORIAL_DONE {
            return;
        }
        let awaited = match step {
            0 => Event::Swallow,
            1 => Event::SearchOpened,
            2 => Event::Summoned,
            3 => Event::Asked,
            _ => return,
        };
        if ev != awaited {
            return;
        }
        self.cfg.tutorial_step = step + 1;
        config::save(&self.cfg);
        self.tutorial_showing = false;
        // Step 2 (click to search) opens the panel over the dot; wait a beat so the
        // bubble isn't shown under it — SearchWin sits beside the dot, bubble above.
        self.show_tutorial_step();
    }

    unsafe fn restart_tutorial(&mut self) {
        self.cfg.tutorial_step = 0;
        config::save(&self.cfg);
        self.show_tutorial_step();
    }

    // ---- menu ----------------------------------------------------------

    unsafe fn context_menu(&mut self) {
        let Ok(menu) = CreatePopupMenu() else { return };
        let count = self.store.lock().unwrap().count();
        let header = wide(&format!("{count} items inside · embeddings on {}", self.embedder.backend));
        let chk = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(header.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let labels: Vec<Vec<u16>> = [("Search", 0), ("Swallow clipboard", 1), ("Take screenshot", 2), ("New note", 3)]
            .iter()
            .map(|(l, i)| crate::util::wide(&format!("{l}\t{}", self.cfg.hotkey(*i))))
            .collect();
        let _ = AppendMenuW(menu, MF_STRING, MENU_SEARCH, PCWSTR(labels[0].as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_PASTE, PCWSTR(labels[1].as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_NEW_NOTE, PCWSTR(labels[3].as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_SCREENSHOT, PCWSTR(labels[2].as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_SETTINGS, w!("Settings…"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_VAULT, w!("Open vault folder"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_TUTORIAL, w!("Show tutorial"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING | chk(self.cfg.scale == 1), MENU_SIZE_S, w!("Small"));
        let _ = AppendMenuW(menu, MF_STRING | chk(self.cfg.scale == 2), MENU_SIZE_M, w!("Medium"));
        let _ = AppendMenuW(menu, MF_STRING | chk(self.cfg.scale == 3), MENU_SIZE_L, w!("Large"));
        let _ = AppendMenuW(menu, MF_STRING | chk(self.cfg.scale == 4), MENU_SIZE_XL, w!("Extra large"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING | chk(!self.cfg.hidden), MENU_SHOW_DOT, w!("Show dot"));
        let _ = AppendMenuW(menu, MF_STRING | chk(self.cfg.center_on_message), MENU_CENTER_MSG, w!("Center on new message"));
        let _ = AppendMenuW(menu, MF_STRING | chk(self.cfg.think), MENU_THINK, w!("Think before answering"));
        let view = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(view, MF_STRING | chk(!self.cfg.notes_default), MENU_VIEW_FILES, w!("Files"));
        let _ = AppendMenuW(view, MF_STRING | chk(self.cfg.notes_default), MENU_VIEW_NOTES, w!("Notes"));
        let _ = AppendMenuW(menu, MF_POPUP, view.0 as usize, w!("Default view"));
        let editor = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(editor, MF_STRING, MENU_NVIM_CONFIG, w!("Load Neovim config…"));
        let builtin = crate::util::wide(&if self.cfg.nvim_init.is_empty() { "Built-in config only".to_string() } else { format!("Built-in config only  (now: {})", self.cfg.nvim_init.rsplit(['\\', '/']).next().unwrap_or("")) });
        let _ = AppendMenuW(editor, MF_STRING | chk(self.cfg.nvim_init.is_empty()), MENU_NVIM_BUILTIN, PCWSTR(builtin.as_ptr()));
        let _ = AppendMenuW(menu, MF_POPUP, editor.0 as usize, w!("Editor"));
        let _ = AppendMenuW(menu, MF_STRING | chk(startup::enabled()), MENU_START_LOGIN, w!("Start at login"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(self.hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_LEFTALIGN, pt.x, pt.y, None, self.hwnd, None);
        let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }

    unsafe fn command(&mut self, id: usize) {
        self.command_inner(id);
        // The Settings tab mirrors these; let it redraw with the new values.
        if matches!(id, MENU_THINK | MENU_CENTER_MSG | MENU_VIEW_FILES | MENU_VIEW_NOTES | MENU_START_LOGIN | MENU_NVIM_CONFIG | MENU_NVIM_BUILTIN | MENU_THEME_NEXT | MENU_AGENT_NOTIFY | MENU_STORE_POLICY | MENU_MODEL_NEXT | MENU_MODEL_DOWNLOAD | MENU_VAULT_FOLDER) && !self.search.is_invalid() {
            let _ = PostMessageW(Some(self.search), WM_SETTINGS_CHANGED, WPARAM(0), LPARAM(0));
        }
    }

    unsafe fn command_inner(&mut self, id: usize) {
        match id {
            MENU_SEARCH => self.open_search(),
            MENU_PASTE => self.paste_clipboard(),
            MENU_SCREENSHOT => self.screenshot(),
            MENU_VAULT => {
                let dir = wide(&config::data_dir().display().to_string());
                ShellExecuteW(None, w!("open"), PCWSTR(dir.as_ptr()), None, None, SW_SHOWNORMAL);
            }
            MENU_TUTORIAL => self.restart_tutorial(),
            MENU_SIZE_S => self.set_scale(1),
            MENU_SIZE_M => self.set_scale(2),
            MENU_SIZE_L => self.set_scale(3),
            MENU_SIZE_XL => self.set_scale(4),
            MENU_SHOW_DOT => {
                let hidden = !self.cfg.hidden;
                self.set_hidden(hidden);
            }
            MENU_CENTER_MSG => {
                self.cfg.center_on_message = !self.cfg.center_on_message;
                config::save(&self.cfg);
            }
            MENU_THINK => {
                self.cfg.think = !self.cfg.think;
                crate::llm_ort::set_thinking(self.cfg.think);
                config::save(&self.cfg);
            }
            MENU_VIEW_FILES | MENU_VIEW_NOTES => {
                self.cfg.notes_default = id == MENU_VIEW_NOTES;
                config::save(&self.cfg);
                if !self.search.is_invalid() {
                    SearchWin::set_default_tab(self.search, self.cfg.notes_default);
                }
            }
            MENU_NEW_NOTE => self.new_note(),
            MENU_AGENT_NOTIFY => {
                self.cfg.agent_notify = !self.cfg.agent_notify;
                config::save(&self.cfg);
            }
            MENU_THEME_NEXT => {
                self.cfg.theme = crate::theme::next_name().to_string();
                crate::theme::set_by_name(&self.cfg.theme);
                config::save(&self.cfg);
                if !self.search.is_invalid() {
                    SearchWin::apply_theme(self.search);
                }
            }
            MENU_SETTINGS => {
                if !self.search_open() {
                    self.open_search();
                }
                SearchWin::open_settings(self.search);
            }
            MENU_NVIM_CONFIG => {
                if let Some(path) = self.pick_nvim_config() {
                    self.cfg.nvim_init = path;
                    config::save(&self.cfg);
                    if !self.search.is_invalid() {
                        SearchWin::set_nvim_init(self.search, &self.cfg.nvim_init);
                    }
                    self.notify_quiet("Neovim config loaded — the editor restarts with it".into());
                }
            }
            MENU_NVIM_BUILTIN => {
                self.cfg.nvim_init.clear();
                config::save(&self.cfg);
                if !self.search.is_invalid() {
                    SearchWin::set_nvim_init(self.search, "");
                }
            }
            MENU_STORE_POLICY => {
                let next = config::StorePolicy::parse(&self.cfg.store_policy).next();
                self.cfg.store_policy = next.as_str().to_string();
                config::set_store_policy(next);
                config::save(&self.cfg);
            }
            MENU_MODEL_NEXT => {
                let data = config::data_dir();
                let current = crate::models::active(&data).map(|m| m.name).unwrap_or_default();
                if let Some(next) = crate::models::next_after(&data, &current) {
                    self.cfg.model_name = next.name.clone();
                    config::save(&self.cfg);
                    self.ask.set_model(Some(next.path.clone()));
                    self.notify_quiet(format!("Ask model: {} ({})", next.name, next.size_text()));
                }
            }
            MENU_MODEL_DOWNLOAD => {
                if crate::models::downloading() {
                    crate::models::cancel();
                } else {
                    crate::models::start_download(self.hwnd.0 as usize);
                    self.notify_quiet(format!("Downloading the ask model ({}). It keeps going in the background.", crate::models::size_text(crate::models::default_size())));
                }
            }
            MENU_VAULT_FOLDER => self.move_vault(),
            MENU_START_LOGIN => startup::set(!startup::enabled()),
            MENU_QUIT => {
                let _ = DestroyWindow(self.hwnd);
            }
            _ => {}
        }
    }

    unsafe fn set_scale(&mut self, s: i32) {
        self.cfg.scale = s;
        config::save(&self.cfg);
        self.redraw();
        if !self.bubble.is_invalid() {
            Bubble::follow(self.bubble, self.rect());
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut Dot;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let d = &mut *ptr;
            d.hwnd = hwnd;
            let target = DropTarget::new(hwnd, d.tx.clone());
            let _ = RegisterDragDrop(hwnd, &target);
            d.drop_target = Some(target);
            d.register_hotkeys();
            SetTimer(Some(hwnd), TIMER_ANIM, IDLE_MS, None);
            let n = d.store.lock().unwrap().count();
            tray::add(hwnd, &format!("Blackhole — {n} items inside"));
            d.redraw();
            // Tutorial bubble once the window is up and positioned.
            let _ = PostMessageW(Some(hwnd), WM_STARTUP, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        WM_STARTUP => {
            if let Some(d) = state(hwnd) {
                if d.cfg.tutorial_step < TUTORIAL_DONE {
                    d.show_tutorial_step();
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            if let Some(d) = state(hwnd) {
                d.tick();
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(d) = state(hwnd) {
                d.dragging = true;
                d.drag_moved = false;
                let _ = GetCursorPos(&mut d.drag_origin);
                let r = d.rect();
                d.win_origin = POINT { x: r.left, y: r.top };
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(d) = state(hwnd) {
                if d.dragging {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let (dx, dy) = (pt.x - d.drag_origin.x, pt.y - d.drag_origin.y);
                    if d.drag_moved || dx.abs() > 3 || dy.abs() > 3 {
                        d.drag_moved = true;
                        d.move_to(d.win_origin.x + dx, d.win_origin.y + dy);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(d) = state(hwnd) {
                if d.dragging {
                    d.dragging = false;
                    let _ = ReleaseCapture();
                    if d.drag_moved {
                        d.home = None;
                        config::save(&d.cfg);
                    } else {
                        d.toggle_search();
                    }
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if let Some(d) = state(hwnd) {
                d.context_menu();
            }
            LRESULT(0)
        }
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lparam.
            let ev = (lparam.0 & 0xFFFF) as u32;
            if let Some(d) = state(hwnd) {
                if ev == WM_CONTEXTMENU || ev == WM_RBUTTONUP {
                    d.context_menu();
                } else if ev == NIN_SELECT || ev == NIN_SELECT | NINF_KEY || ev == WM_LBUTTONUP {
                    if d.cfg.hidden {
                        d.set_hidden(false);
                    }
                    d.open_search();
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if let Some(d) = state(hwnd) {
                d.command(wparam.0 & 0xFFFF);
            }
            LRESULT(0)
        }
        WM_SET_HOTKEY => {
            let combo = *Box::from_raw(lparam.0 as *mut String);
            if let Some(d) = state(hwnd) {
                let i = wparam.0.min(3);
                while d.cfg.hotkeys.len() < 4 {
                    let n = d.cfg.hotkeys.len();
                    d.cfg.hotkeys.push(crate::hotkeys::ACTIONS[n].1.to_string());
                }
                d.cfg.hotkeys[i] = combo;
                config::save(&d.cfg);
                d.register_hotkeys();
                if !d.search.is_invalid() {
                    let _ = PostMessageW(Some(d.search), WM_SETTINGS_CHANGED, WPARAM(0), LPARAM(0));
                }
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            if let Some(d) = state(hwnd) {
                match wparam.0 as i32 {
                    HOTKEY_SUMMON => d.summon(),
                    HOTKEY_PASTE => d.paste_clipboard(),
                    HOTKEY_SHOT => d.screenshot(),
                    HOTKEY_NOTE => d.new_note(),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_DROP_ENTER => {
            if let Some(d) = state(hwnd) {
                d.set_mood(Mood::Hungry, None);
            }
            LRESULT(0)
        }
        WM_DROP_LEAVE => {
            if let Some(d) = state(hwnd) {
                d.set_mood(Mood::Idle, Some(0));
            }
            LRESULT(0)
        }
        WM_DROP_SWALLOW => {
            if let Some(d) = state(hwnd) {
                d.swallow();
            }
            LRESULT(0)
        }
        WM_INGEST_DONE => {
            if let Some(d) = state(hwnd) {
                let report = Box::from_raw(lparam.0 as *mut Report);
                d.ingest_done(*report);
            }
            LRESULT(0)
        }
        WM_NOTIFY | WM_NOTIFY_QUIET => {
            let text = *Box::from_raw(lparam.0 as *mut String);
            if let Some(d) = state(hwnd) {
                if msg == WM_NOTIFY_QUIET { d.notify_quiet(text) } else { d.notify(text) }
            }
            LRESULT(0)
        }
        WM_BUBBLE_CLICKED | WM_BUBBLE_EXPIRED => {
            if let Some(d) = state(hwnd) {
                let action = d.pending_action.take();
                let closed_x = wparam.0 == 1;
                d.bubble_closed(msg == WM_BUBBLE_CLICKED && closed_x);
                if msg == WM_BUBBLE_CLICKED && !closed_x {
                    if let Some(a) = action {
                        d.run_action(a);
                    }
                }
            }
            LRESULT(0)
        }
        crate::models::WM_MODEL_DOWNLOAD => {
            let text = (lparam.0 != 0).then(|| *Box::from_raw(lparam.0 as *mut String));
            if let Some(d) = state(hwnd) {
                if wparam.0 == crate::models::DL_DONE {
                    // The model is on disk now: ask mode can use it without a restart.
                    d.ask.set_model(crate::llm_ort::find_model(&config::data_dir()));
                }
                if let Some(t) = text {
                    d.notify(t);
                }
                if !d.search.is_invalid() {
                    // Not WM_SETTINGS_CHANGED: that also takes the focus, and this
                    // ticks a few times a second while the download runs.
                    let _ = PostMessageW(Some(d.search), crate::models::WM_MODEL_DOWNLOAD, WPARAM(wparam.0), LPARAM(0));
                }
            }
            LRESULT(0)
        }
        WM_NOTICE => {
            let n = *Box::from_raw(lparam.0 as *mut Notice);
            if let Some(d) = state(hwnd) {
                d.notice(n);
            }
            LRESULT(0)
        }
        WM_SEARCH_CLOSED => {
            if let Some(d) = state(hwnd) {
                if matches!(d.mood, Mood::Listening | Mood::Thinking) {
                    d.set_mood(Mood::Idle, None);
                }
                // Panel closed: let the LLM go after a minute of not being needed.
                SetTimer(Some(hwnd), TIMER_UNLOAD, UNLOAD_AFTER_MS, None);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_UNLOAD => {
            if let Some(d) = state(hwnd) {
                // Still open (reopened without a close event) or mid-generation: try again next tick.
                if !d.search_open() && d.ask.unload() {
                    let _ = KillTimer(Some(hwnd), TIMER_UNLOAD);
                }
            }
            LRESULT(0)
        }
        WM_ASK_STARTED => {
            if let Some(d) = state(hwnd) {
                // Breathe while the model reads and reasons; the first token ends it.
                if d.search_open() {
                    d.set_mood(Mood::Thinking, None);
                }
                d.tutorial_event(Event::Asked);
            }
            LRESULT(0)
        }
        WM_ASK_FIRST_TOKEN => {
            if let Some(d) = state(hwnd) {
                if d.mood == Mood::Thinking {
                    let base = d.base_mood();
                    d.set_mood(base, None);
                }
            }
            LRESULT(0)
        }
        WM_PANEL_RESIZED => {
            if let Some(d) = state(hwnd) {
                d.cfg.panel_w = wparam.0 as i32;
                d.cfg.panel_h = lparam.0 as i32;
                config::save(&d.cfg);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            if let Some(d) = state(hwnd) {
                d.redraw();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(d) = state(hwnd) {
                let _ = RevokeDragDrop(hwnd);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_SUMMON);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_PASTE);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_SHOT);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_NOTE);
                tray::remove(hwnd);
                config::save(&d.cfg);
                if !d.search.is_invalid() {
                    let _ = DestroyWindow(d.search);
                }
                if !d.bubble.is_invalid() {
                    let _ = DestroyWindow(d.bubble);
                }
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            if let Some(d) = state(hwnd) {
                if msg == d.taskbar_created && msg != 0 {
                    let n = d.store.lock().unwrap().count();
                    tray::add(hwnd, &format!("Blackhole — {n} items inside"));
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}
