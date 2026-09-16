//! Embedded Neovim as the note editor: `nvim --embed` driven over msgpack-RPC, its
//! screen grid drawn by us in the panel's pixel font and palette (Neovim's
//! "external UI"). That is what makes syntax highlighting, treesitter, LSP and the
//! user's own muscle memory available inside a 480-px-wide pixel-art panel.
//!
//! Pieces: `Rpc` (process + request/notify + reader thread), `Host` (a child window
//! that owns the grid, paints it, and turns Win32 keys and mouse into `nvim_input`).
//! The reader thread batches `redraw` events up to each `flush` and posts them to the
//! host window; buffer changes arrive as `nvim_buf_lines_event` and are reported to
//! the panel, which autosaves like it does for the plain editor.

use rmpv::Value;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the host with a boxed `Vec<Event>` (a redraw batch ending in flush).
const WM_NVIM_EVENTS: u32 = 0x8040;
/// Posted to the host's parent: the buffer changed (wparam = changedtick).
pub const WM_NVIM_CHANGED: u32 = 0x8041;
/// Posted to the host's parent after a redraw: viewport/cursor may have moved.
pub const WM_NVIM_FLUSH: u32 = 0x8042;
/// Posted to the host's parent: Esc pressed while already in normal mode.
pub const WM_NVIM_ESCAPE: u32 = 0x8043;
/// Posted to the host's parent with a boxed String "action\targ": a `:w`/`:wq`/`:q`/`:Name`
/// from inside Neovim (its init file forwards them over RPC).
pub const WM_NVIM_CMD: u32 = 0x8044;
/// Posted to the host's parent with a boxed String: a Neovim message or the command
/// line being typed, for the status line (empty = back to the panel's own status).
pub const WM_NVIM_STATUS: u32 = 0x8045;

const NVIM_VERSION: &str = "0.12.5";
/// Neovim's initial buffer: the note being edited. Previews use a scratch buffer.
const NOTE_BUF: i64 = 1;

/// `<exe dir>\nvim\bin\nvim.exe` (the installer's layout), or NVIM on the path as a fallback.
pub fn find_nvim() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.parent()?.join("nvim").join("bin").join("nvim.exe");
    if beside.exists() {
        return Some(beside);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|p| p.join("nvim.exe")).find(|p| p.exists())
}

// ---------------------------------------------------------------------------
// RPC

struct Rpc {
    child: Child,
    stdin: Mutex<ChildStdin>,
    next_id: Mutex<u64>,
    pending: Arc<Mutex<HashMap<u64, Sender<Result<Value, Value>>>>>,
}

#[derive(Clone, Copy, Default)]
struct Attr {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

#[derive(Debug)]
enum Event {
    Resize(usize, usize),
    DefaultColors(u32, u32),
    HlAttr(u32, Attr),
    Line { row: usize, col: usize, cells: Vec<(String, u32, usize)> },
    Clear,
    Cursor(usize, usize),
    Scroll { top: usize, bot: usize, left: usize, right: usize, rows: i64 },
    Mode(String),
    Viewport { top: i64, bot: i64, lines: i64 },
    /// ext_messages / ext_cmdline: text for the panel's status line (None = clear).
    Status(Option<String>),
    Flush,
}

impl std::fmt::Debug for Attr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Attr")
    }
}

fn v_u64(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)).unwrap_or(0)
}
fn v_i64(v: &Value) -> i64 {
    // Buffer/window handles come back as msgpack EXT values wrapping the id.
    if let Value::Ext(_, bytes) = v {
        return rmpv::decode::read_value(&mut &bytes[..]).ok().map(|inner| v_i64(&inner)).unwrap_or(0);
    }
    v.as_i64().or_else(|| v.as_u64().map(|i| i as i64)).unwrap_or(0)
}
fn v_str(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_default()
}

/// [[attr_id, text], …] → the text.
fn chunks_text(v: &Value) -> String {
    v.as_array().cloned().unwrap_or_default().iter().filter_map(|c| c.as_array().and_then(|p| p.get(1)).map(v_str)).collect::<Vec<_>>().join("")
}

fn parse_attr(map: &Value) -> Attr {
    let mut a = Attr::default();
    if let Some(m) = map.as_map() {
        for (k, v) in m {
            match k.as_str().unwrap_or("") {
                "foreground" => a.fg = Some(v_u64(v) as u32),
                "background" => a.bg = Some(v_u64(v) as u32),
                "bold" => a.bold = v.as_bool().unwrap_or(false),
                "italic" => a.italic = v.as_bool().unwrap_or(false),
                "underline" | "undercurl" | "underdouble" | "underdotted" | "underdashed" => a.underline = v.as_bool().unwrap_or(false),
                "reverse" => a.reverse = v.as_bool().unwrap_or(false),
                _ => {}
            }
        }
    }
    a
}

/// One `redraw` notification → events (only grid 1 is used: no multigrid).
fn parse_redraw(params: &Value, out: &mut Vec<Event>) {
    let Some(list) = params.as_array() else { return };
    for entry in list {
        let Some(e) = entry.as_array() else { continue };
        let name = e.first().and_then(|n| n.as_str()).unwrap_or("");
        for args in e.iter().skip(1) {
            let a = args.as_array().cloned().unwrap_or_default();
            match name {
                "grid_resize" if a.len() >= 3 => out.push(Event::Resize(v_u64(&a[1]) as usize, v_u64(&a[2]) as usize)),
                "default_colors_set" if a.len() >= 2 => out.push(Event::DefaultColors(v_u64(&a[0]) as u32, v_u64(&a[1]) as u32)),
                "hl_attr_define" if a.len() >= 2 => out.push(Event::HlAttr(v_u64(&a[0]) as u32, parse_attr(&a[1]))),
                "grid_line" if a.len() >= 4 => {
                    let mut cells = Vec::new();
                    let mut hl = 0u32;
                    for c in a[3].as_array().cloned().unwrap_or_default() {
                        let c = c.as_array().cloned().unwrap_or_default();
                        let text = c.first().map(v_str).unwrap_or_default();
                        if c.len() >= 2 {
                            hl = v_u64(&c[1]) as u32;
                        }
                        let rep = if c.len() >= 3 { v_u64(&c[2]) as usize } else { 1 };
                        cells.push((text, hl, rep.max(1)));
                    }
                    out.push(Event::Line { row: v_u64(&a[1]) as usize, col: v_u64(&a[2]) as usize, cells });
                }
                "grid_clear" => out.push(Event::Clear),
                "grid_cursor_goto" if a.len() >= 3 => out.push(Event::Cursor(v_u64(&a[1]) as usize, v_u64(&a[2]) as usize)),
                "grid_scroll" if a.len() >= 6 => out.push(Event::Scroll {
                    top: v_u64(&a[1]) as usize,
                    bot: v_u64(&a[2]) as usize,
                    left: v_u64(&a[3]) as usize,
                    right: v_u64(&a[4]) as usize,
                    rows: v_i64(&a[5]),
                }),
                "mode_change" if !a.is_empty() => out.push(Event::Mode(v_str(&a[0]))),
                "win_viewport" if a.len() >= 7 => out.push(Event::Viewport { top: v_i64(&a[2]), bot: v_i64(&a[3]), lines: v_i64(&a[6]) }),
                // Messages and the command line are drawn by the panel's status line, so the
                // whole grid is text (cmdheight 0) and long messages don't fight a 480-px panel.
                "msg_show" if a.len() >= 2 => {
                    let text = chunks_text(&a[1]);
                    if !text.trim().is_empty() {
                        out.push(Event::Status(Some(text)));
                    }
                }
                "msg_clear" => out.push(Event::Status(None)),
                "msg_history_show" if !a.is_empty() => {
                    let text: Vec<String> = a[0].as_array().cloned().unwrap_or_default().iter().filter_map(|e| e.as_array().and_then(|x| x.get(1)).map(chunks_text)).collect();
                    out.push(Event::Status(Some(text.join(" · "))));
                }
                "cmdline_show" if a.len() >= 4 => {
                    let firstc = v_str(&a[2]);
                    let prompt = v_str(&a[3]);
                    out.push(Event::Status(Some(format!("{prompt}{firstc}{}▏", chunks_text(&a[0])))));
                }
                "cmdline_hide" => out.push(Event::Status(None)),
                "flush" => out.push(Event::Flush),
                _ => {}
            }
        }
    }
}

impl Rpc {
    /// Spawn `nvim --embed` with our init file; the reader thread posts to `host`.
    fn spawn(exe: &Path, init: &Path, host: usize) -> anyhow::Result<Rpc> {
        use std::os::windows::process::CommandExt;
        let mut child = Command::new(exe)
            .args(["--embed", "-u"])
            .arg(init)
            .args(["--cmd", "set noswapfile"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let pending: Arc<Mutex<HashMap<u64, Sender<Result<Value, Value>>>>> = Arc::new(Mutex::new(HashMap::new()));
        let pend2 = pending.clone();
        std::thread::spawn(move || {
            let mut rd = BufReader::new(stdout);
            let mut batch: Vec<Event> = Vec::new();
            loop {
                let msg = match rmpv::decode::read_value(&mut rd) {
                    Ok(m) => m,
                    Err(_) => break, // nvim exited
                };
                let Some(arr) = msg.as_array() else { continue };
                match arr.first().and_then(|t| t.as_u64()) {
                    Some(1) if arr.len() >= 4 => {
                        let id = v_u64(&arr[1]);
                        if let Some(tx) = pend2.lock().unwrap().remove(&id) {
                            let _ = tx.send(if arr[2].is_nil() { Ok(arr[3].clone()) } else { Err(arr[2].clone()) });
                        }
                    }
                    Some(2) if arr.len() >= 3 => {
                        let method = arr[1].as_str().unwrap_or("");
                        if method == "redraw" {
                            parse_redraw(&arr[2], &mut batch);
                            if matches!(batch.last(), Some(Event::Flush)) {
                                let boxed = Box::into_raw(Box::new(std::mem::take(&mut batch)));
                                unsafe {
                                    if PostMessageW(Some(HWND(host as *mut _)), WM_NVIM_EVENTS, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                                        drop(Box::from_raw(boxed));
                                    }
                                }
                            }
                        } else if method == "blackhole" {
                            let a = arr[2].as_array().cloned().unwrap_or_default();
                            let text = format!("{}\t{}", a.first().map(v_str).unwrap_or_default(), a.get(1).map(v_str).unwrap_or_default());
                            let boxed = Box::into_raw(Box::new(text));
                            unsafe {
                                if PostMessageW(Some(HWND(host as *mut _)), WM_NVIM_CMD, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                                    drop(Box::from_raw(boxed));
                                }
                            }
                        } else if method == "nvim_buf_lines_event" || method == "nvim_buf_changedtick_event" {
                            let tick = arr[2].as_array().and_then(|a| a.get(1)).map(v_u64).unwrap_or(0);
                            unsafe {
                                let _ = PostMessageW(Some(HWND(host as *mut _)), WM_NVIM_CHANGED, WPARAM(tick as usize), LPARAM(0));
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(Rpc { child, stdin: Mutex::new(stdin), next_id: Mutex::new(1), pending })
    }

    fn send(&self, msg: Value) -> anyhow::Result<()> {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &msg)?;
        let mut s = self.stdin.lock().unwrap();
        s.write_all(&buf)?;
        s.flush()?;
        Ok(())
    }

    fn notify(&self, method: &str, params: Vec<Value>) -> anyhow::Result<()> {
        self.send(Value::Array(vec![Value::from(2), Value::from(method), Value::Array(params)]))
    }

    /// Blocking request with a short timeout (the UI thread waits; nvim answers in µs).
    fn request(&self, method: &str, params: Vec<Value>) -> anyhow::Result<Value> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(Value::Array(vec![Value::from(0), Value::from(id), Value::from(method), Value::Array(params)]))?;
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => anyhow::bail!("nvim: {method}: {e}"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                anyhow::bail!("nvim: {method}: no reply")
            }
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Init file: the panel's look, line numbers, markdown by default, LSP if a server is around.

fn write_init(dir: &Path, user_init: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join("nvim-init.lua");
    let lua = r##"-- Generated by Blackhole on every editor start. Your own config, chosen from the
-- right-click menu ("Editor > Load Neovim config…"), is sourced at the end so it wins.
vim.o.number = true
vim.o.relativenumber = false
vim.o.numberwidth = 3
vim.o.wrap = true
vim.o.linebreak = true
vim.o.breakindent = true
vim.o.laststatus = 0
vim.o.showmode = false
vim.o.ruler = false
vim.o.showcmd = false
vim.o.cmdheight = 0
vim.o.swapfile = false
vim.o.backup = false
vim.o.writebackup = false
vim.o.undofile = false
vim.o.signcolumn = 'no'
vim.o.cursorline = true
vim.o.scrolloff = 2
vim.o.mouse = 'a'
vim.o.termguicolors = true
vim.o.clipboard = 'unnamedplus'
vim.o.expandtab = true
vim.o.shiftwidth = 2
vim.o.tabstop = 2
vim.o.fillchars = 'eob: '
vim.o.shortmess = 'aoOstTIcCF'
vim.o.timeoutlen = 400
vim.o.updatetime = 300
vim.cmd('syntax enable')

-- Blackhole's palette: dark violet ground, orange accent, dim lilac secondary text.
local bg, bg_dark, sel, accent, fg, dim = '#301424', '#180A14', '#782860', '#FFA040', '#FFE8F0', '#B090A0'
local green, red, lilac = '#78E08C', '#FF5040', '#C8A0E8'
local hl = function(g, o) vim.api.nvim_set_hl(0, g, o) end
hl('Normal', { fg = fg, bg = bg })
hl('NormalNC', { fg = fg, bg = bg })
hl('NormalFloat', { fg = fg, bg = bg_dark })
hl('FloatBorder', { fg = accent, bg = bg_dark })
hl('LineNr', { fg = dim, bg = bg })
hl('CursorLineNr', { fg = accent, bg = bg, bold = true })
hl('CursorLine', { bg = '#3A1A2E' })
hl('Visual', { bg = sel })
hl('Search', { fg = bg, bg = accent })
hl('IncSearch', { fg = bg, bg = green })
hl('CurSearch', { fg = bg, bg = green })
hl('MatchParen', { fg = accent, bold = true, underline = true })
hl('NonText', { fg = sel })
hl('Whitespace', { fg = sel })
hl('EndOfBuffer', { fg = bg })
hl('Comment', { fg = dim, italic = true })
hl('Title', { fg = accent, bold = true })
hl('Pmenu', { fg = fg, bg = bg_dark })
hl('PmenuSel', { fg = fg, bg = sel })
hl('PmenuSbar', { bg = bg_dark })
hl('PmenuThumb', { bg = accent })
hl('MsgArea', { fg = dim, bg = bg })
hl('ModeMsg', { fg = accent })
hl('MoreMsg', { fg = green })
hl('Question', { fg = green })
hl('ErrorMsg', { fg = red })
hl('WarningMsg', { fg = accent })
hl('Error', { fg = red })
hl('DiagnosticError', { fg = red })
hl('DiagnosticWarn', { fg = accent })
hl('DiagnosticInfo', { fg = lilac })
hl('DiagnosticHint', { fg = dim })
hl('DiagnosticUnderlineError', { undercurl = true, sp = red })
hl('DiagnosticUnderlineWarn', { undercurl = true, sp = accent })
hl('Directory', { fg = lilac })
hl('String', { fg = green })
hl('Constant', { fg = lilac })
hl('Number', { fg = lilac })
hl('Identifier', { fg = fg })
hl('Function', { fg = accent })
hl('Statement', { fg = accent, bold = true })
hl('Keyword', { fg = accent })
hl('Operator', { fg = fg })
hl('PreProc', { fg = lilac })
hl('Type', { fg = green })
hl('Special', { fg = lilac })
hl('Delimiter', { fg = dim })
hl('Todo', { fg = bg, bg = accent, bold = true })
hl('@markup.heading', { fg = accent, bold = true })
hl('@markup.heading.1.markdown', { fg = accent, bold = true })
hl('@markup.heading.2.markdown', { fg = accent, bold = true })
hl('@markup.strong', { bold = true })
hl('@markup.italic', { italic = true })
hl('@markup.strikethrough', { strikethrough = true, fg = dim })
hl('@markup.raw', { fg = green })
hl('@markup.raw.block', { fg = green })
hl('@markup.link', { fg = lilac, underline = true })
hl('@markup.link.url', { fg = lilac, underline = true })
hl('@markup.link.label', { fg = lilac })
hl('@markup.list', { fg = accent })
hl('@markup.list.checked', { fg = green })
hl('@markup.list.unchecked', { fg = dim })
hl('@markup.quote', { fg = dim, italic = true })
hl('@punctuation.special', { fg = dim })
hl('@label', { fg = lilac })

-- Notes are markdown unless they look like something else; treesitter colours them.
vim.api.nvim_create_autocmd({ 'BufEnter', 'BufNewFile' }, {
  callback = function()
    if vim.bo.filetype == '' then vim.bo.filetype = 'markdown' end
  end,
})
vim.api.nvim_create_autocmd('FileType', {
  callback = function(ev)
    pcall(vim.treesitter.start, ev.buf)
    -- LSP: any server the user has on the path for this filetype.
    local servers = { markdown = { 'marksman', 'server' }, lua = { 'lua-language-server' }, rust = { 'rust-analyzer' },
      python = { 'pyright-langserver', '--stdio' }, typescript = { 'typescript-language-server', '--stdio' },
      javascript = { 'typescript-language-server', '--stdio' }, go = { 'gopls' }, c = { 'clangd' }, cpp = { 'clangd' } }
    local cmd = servers[ev.match]
    if cmd and vim.fn.executable(cmd[1]) == 1 then
      vim.lsp.start({ name = cmd[1], cmd = cmd, root_dir = vim.fn.getcwd() })
    end
  end,
})
-- Blackhole owns the file side of things. :w saves the note, :wq / :x / ZZ save it and start a
-- fresh one, :q saves and closes the panel, :Name <title> names the note (":Name" alone goes
-- back to automatic titles). Notes never have a file name, so the built-ins are redirected.
local send = function(action, arg) vim.rpcnotify(1, 'blackhole', action, arg or '') end
vim.api.nvim_create_user_command('BhWrite', function() send('write') end, {})
vim.api.nvim_create_user_command('BhWriteQuit', function() send('wq') end, {})
vim.api.nvim_create_user_command('BhQuit', function() send('q') end, {})
vim.api.nvim_create_user_command('Name', function(o) send('name', o.args) end, { nargs = '?' })
vim.api.nvim_create_user_command('BhNew', function() send('new') end, {})
vim.api.nvim_create_user_command('Copy', function() send('copy') end, {})
vim.api.nvim_create_user_command('Ask', function(o) send('ask', o.args) end, { nargs = '+' })
vim.api.nvim_create_user_command('Tidy', function() send('tidy') end, {})
local redirect = { w = 'BhWrite', write = 'BhWrite', wq = 'BhWriteQuit', x = 'BhWriteQuit', xit = 'BhWriteQuit',
  wqa = 'BhWriteQuit', wqall = 'BhWriteQuit', q = 'BhQuit', quit = 'BhQuit', qa = 'BhQuit', qall = 'BhQuit',
  ['q!'] = 'BhQuit', ['wq!'] = 'BhWriteQuit', ['x!'] = 'BhWriteQuit', new = 'BhNew', enew = 'BhNew' }
for from, to in pairs(redirect) do
  local f = from:gsub('!', '')
  vim.cmd(string.format(
    "cnoreabbrev <expr> %s (getcmdtype() == ':' && getcmdline() ==# '%s') ? '%s' : '%s'", f, f, to, f))
end
vim.keymap.set('n', 'ZZ', '<Cmd>BhWriteQuit<CR>', { silent = true })
vim.keymap.set('n', 'ZQ', '<Cmd>BhQuit<CR>', { silent = true })
vim.keymap.set({ 'n', 'i' }, '<C-s>', '<Cmd>BhWrite<CR>', { silent = true })
vim.keymap.set({ 'n', 'i', 'v' }, '<C-S-c>', '<Cmd>Copy<CR>', { silent = true })
vim.api.nvim_create_user_command('Tag', function(o) send('tag', o.args) end, { nargs = '*' })

-- The user's own init, if one was chosen: its folder joins runtimepath/packpath so
-- lua/ modules and packs next to it load, then it is sourced last.
local user_init = [==[USER_INIT]==]
if user_init ~= '' then
  local dir = vim.fn.fnamemodify(user_init, ':h')
  vim.opt.runtimepath:prepend(dir)
  vim.opt.runtimepath:append(dir .. '/after')
  vim.opt.packpath:prepend(dir)
  vim.env.MYVIMRC = user_init
  local ok, err = pcall(function()
    if user_init:match('%.lua$') then dofile(user_init) else vim.cmd.source(user_init) end
  end)
  if not ok then vim.schedule(function() vim.notify('Blackhole: your Neovim config failed: ' .. tostring(err), vim.log.levels.WARN) end) end
end
"##;
    let lua = lua.replace("USER_INIT", &user_init.replace('\\', "/"));
    std::fs::write(&path, lua)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Host window: grid, painting, input.

#[derive(Clone)]
struct Cell {
    text: String,
    hl: u32,
}

pub struct Host {
    pub hwnd: HWND,
    rpc: Option<Arc<Rpc>>,
    cols: usize,
    rows: usize,
    grid: Vec<Cell>,
    attrs: HashMap<u32, Attr>,
    default_fg: u32,
    default_bg: u32,
    cursor: (usize, usize),
    mode: String,
    /// Viewport from `win_viewport`: (topline, botline, line_count), 0-based top.
    viewport: (i64, i64, i64),
    font: HFONT,
    font_bold: HFONT,
    font_italic: HFONT,
    cell_w: i32,
    cell_h: i32,
    /// Changedtick after our own `nvim_buf_set_lines`; events at or below it are not edits.
    own_tick: u64,
    mouse_down: bool,
    unit: i32,
    accent: COLORREF,
    /// First half of a UTF-16 surrogate pair from WM_CHAR, waiting for the second.
    high_surrogate: Option<u16>,
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut Host> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host).as_mut()
}

fn rgb(c: u32) -> COLORREF {
    // Neovim gives 0xRRGGBB; COLORREF is 0x00BBGGRR.
    COLORREF(((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF))
}

impl Host {
    /// Create the editor window as a child of `parent`. Returns None when nvim is not
    /// installed or fails to start (the panel then keeps its plain editor).
    pub fn create(parent: HWND, font_px: i32, unit: i32, accent: COLORREF, user_init: &str) -> Option<Host> {
        let exe = find_nvim()?;
        unsafe {
            let class = w!("BlackholeNvim");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_IBEAM).unwrap_or_default(),
                hbrBackground: HBRUSH::default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0), class, w!(""), WS_CHILD | WS_CLIPSIBLINGS,
                0, 0, 10, 10, Some(parent), None, None, None,
            ).ok()?;
            let mk = |weight: i32, italic: u32| {
                CreateFontW(-font_px, 0, 0, 0, weight, italic, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, (FIXED_PITCH.0 | FF_MODERN.0) as u32, w!("Consolas"))
            };
            let font = mk(FW_NORMAL.0 as i32, 0);
            let hdc = GetDC(Some(hwnd));
            let old = SelectObject(hdc, font.into());
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(hdc, &mut tm);
            SelectObject(hdc, old);
            ReleaseDC(Some(hwnd), hdc);
            let init = write_init(&crate::config::data_dir(), user_init).ok()?;
            let rpc = match Rpc::spawn(&exe, &init, hwnd.0 as usize) {
                Ok(r) => Arc::new(r),
                Err(e) => {
                    crate::util::log(&format!("nvim: could not start {}: {e}", exe.display()));
                    let _ = DestroyWindow(hwnd);
                    return None;
                }
            };
            let host = Host {
                hwnd,
                rpc: Some(rpc),
                cols: 0,
                rows: 0,
                grid: Vec::new(),
                attrs: HashMap::new(),
                default_fg: 0xFFE8F0,
                default_bg: 0x301424,
                cursor: (0, 0),
                mode: "normal".into(),
                viewport: (0, 0, 1),
                font,
                font_bold: mk(FW_BOLD.0 as i32, 0),
                font_italic: mk(FW_NORMAL.0 as i32, 1),
                cell_w: tm.tmAveCharWidth.max(1),
                cell_h: (tm.tmHeight + tm.tmExternalLeading).max(1),
                own_tick: 0,
                mouse_down: false,
                unit,
                accent,
                high_surrogate: None,
            };
            let boxed = Box::into_raw(Box::new(host));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, boxed as isize);
            let h = &mut *boxed;
            // Attach the UI at a provisional size; the real size follows the first layout.
            let mut opts = Vec::new();
            opts.push((Value::from("rgb"), Value::from(true)));
            opts.push((Value::from("ext_linegrid"), Value::from(true)));
            opts.push((Value::from("ext_messages"), Value::from(true)));
            opts.push((Value::from("ext_cmdline"), Value::from(true)));
            if let Some(rpc) = &h.rpc {
                let _ = rpc.request("nvim_ui_attach", vec![Value::from(40), Value::from(10), Value::Map(opts)]);
                // The note lives in the initial buffer (id 1); previews get their own buffer.
                let _ = rpc.request("nvim_buf_attach", vec![Value::from(NOTE_BUF), Value::from(false), Value::Map(vec![])]);
                crate::util::log(&format!("nvim: embedded {} from {}", NVIM_VERSION, exe.display()));
            }
            // Move out of the box: the window owns the state now; return a handle copy for the panel.
            Some(Host { hwnd, rpc: h.rpc.clone(), cols: 0, rows: 0, grid: Vec::new(), attrs: HashMap::new(), default_fg: 0, default_bg: 0, cursor: (0, 0), mode: String::new(), viewport: (0, 0, 1), font: HFONT::default(), font_bold: HFONT::default(), font_italic: HFONT::default(), cell_w: h.cell_w, cell_h: h.cell_h, own_tick: 0, mouse_down: false, unit, accent, high_surrogate: None })
        }
    }

    fn rpc(&self) -> Option<&Arc<Rpc>> {
        self.rpc.as_ref()
    }

    /// Replace the buffer with `text`; `insert` starts insert mode (new/empty notes).
    pub fn set_text(&self, text: &str, insert: bool) {
        let Some(rpc) = self.rpc() else { return };
        let lines: Vec<Value> = text.split('\n').map(|l| Value::from(l.trim_end_matches('\r'))).collect();
        self.end_preview();
        let _ = rpc.request("nvim_buf_set_lines", vec![Value::from(NOTE_BUF), Value::from(0), Value::from(-1), Value::from(false), Value::Array(lines)]);
        // One command each: `normal!` would swallow the rest of a `|`-joined line as keys.
        for cmd in ["stopinsert", "silent! normal! gg0", "setlocal nomodified"] {
            let _ = rpc.request("nvim_command", vec![Value::from(cmd)]);
        }
        if insert {
            let _ = rpc.request("nvim_command", vec![Value::from("startinsert")]);
        }
        let tick = rpc.request("nvim_buf_get_changedtick", vec![Value::from(NOTE_BUF)]).map(|v| v_u64(&v)).unwrap_or(0);
        unsafe {
            if let Some(h) = state(self.hwnd) {
                h.own_tick = tick;
            }
        }
    }

    pub fn text(&self) -> String {
        let Some(rpc) = self.rpc() else { return String::new() };
        match rpc.request("nvim_buf_get_lines", vec![Value::from(NOTE_BUF), Value::from(0), Value::from(-1), Value::from(false)]) {
            Ok(v) => v.as_array().map(|a| a.iter().map(v_str).collect::<Vec<_>>().join("\n")).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// Show read-only text (a search hit) in a scratch buffer with its filetype's colours
    /// and the query terms highlighted; `end_preview` returns to the note.
    pub fn preview(&self, text: &str, filetype: &str, terms: &[String]) {
        let Some(rpc) = self.rpc() else { return };
        let dbg = std::env::var_os("BLACKHOLE_NVIM_DEBUG").is_some();
        let existing = rpc.request("nvim_eval", vec![Value::from("get(g:, 'bh_preview', 0)")]);
        if dbg {
            crate::util::log(&format!("nvim preview: existing {existing:?}"));
        }
        let buf = match existing.map(|v| v_i64(&v)).unwrap_or(0) {
            0 => {
                let b = rpc.request("nvim_create_buf", vec![Value::from(false), Value::from(true)]);
                if dbg {
                    crate::util::log(&format!("nvim preview: create_buf {b:?}"));
                }
                let b = b.map(|v| v_i64(&v)).unwrap_or(0);
                let _ = rpc.request("nvim_set_var", vec![Value::from("bh_preview"), Value::from(b)]);
                b
            }
            b => b,
        };
        if buf == 0 {
            return;
        }
        let lines: Vec<Value> = text.split('\n').map(|l| Value::from(l.trim_end_matches('\r'))).collect();
        let r1 = rpc.request("nvim_set_option_value", vec![Value::from("modifiable"), Value::from(true), Value::Map(vec![(Value::from("buf"), Value::from(buf))])]);
        let r2 = rpc.request("nvim_buf_set_lines", vec![Value::from(buf), Value::from(0), Value::from(-1), Value::from(false), Value::Array(lines)]);
        let r3 = rpc.request("nvim_win_set_buf", vec![Value::from(0), Value::from(buf)]);
        if dbg {
            crate::util::log(&format!("nvim preview: buf {buf} modifiable {r1:?} lines {} win_set_buf {r3:?}", r2.is_ok()));
        }
        let ft = filetype.replace(|c: char| !c.is_ascii_alphanumeric(), "");
        for cmd in [
            "stopinsert".to_string(),
            format!("setlocal nomodifiable buftype=nofile bufhidden=hide noswapfile filetype={ft} nonumber"),
            "silent! call clearmatches()".to_string(),
            "silent! normal! gg0".to_string(),
        ] {
            let _ = rpc.request("nvim_command", vec![Value::from(cmd)]);
        }
        for t in terms.iter().filter(|t| t.len() >= 2).take(8) {
            let pat = format!("\\c\\V{}", t.replace('\\', "\\\\").replace('\'', "''"));
            let _ = rpc.request("nvim_command", vec![Value::from(format!("silent! call matchadd('Search', '{pat}')"))]);
        }
    }

    /// Back to the note buffer (no-op when it is already showing).
    pub fn end_preview(&self) {
        let Some(rpc) = self.rpc() else { return };
        let cur = rpc.request("nvim_get_current_buf", vec![]).map(|v| v_i64(&v)).unwrap_or(NOTE_BUF);
        if cur != NOTE_BUF {
            let _ = rpc.request("nvim_win_set_buf", vec![Value::from(0), Value::from(NOTE_BUF)]);
            let _ = rpc.request("nvim_command", vec![Value::from("silent! call clearmatches() | setlocal number")]);
        }
    }

    /// Cursor (1-based row, 0-based col) in the note, for remembering it per note.
    pub fn cursor(&self) -> Option<(i64, i64)> {
        let rpc = self.rpc()?;
        let v = rpc.request("nvim_win_get_cursor", vec![Value::from(0)]).ok()?;
        let a = v.as_array()?;
        Some((v_i64(a.first()?), v_i64(a.get(1)?)))
    }

    pub fn set_cursor(&self, pos: (i64, i64)) {
        if let Some(rpc) = self.rpc() {
            // Out-of-range positions error harmlessly (the note may have shrunk).
            let _ = rpc.request("nvim_win_set_cursor", vec![Value::from(0), Value::Array(vec![Value::from(pos.0), Value::from(pos.1)])]);
            let _ = rpc.request("nvim_command", vec![Value::from("normal! zz")]);
        }
    }

    /// Is this changedtick an edit by the user (not our own load)?
    pub fn is_user_change(&self, tick: u64) -> bool {
        unsafe { state(self.hwnd).map(|h| tick > h.own_tick).unwrap_or(true) }
    }

    /// (first visible line, lines per page, line count) for the panel's scrollbar.
    pub fn scroll_info(&self) -> (i32, i32, i32) {
        unsafe {
            match state(self.hwnd) {
                Some(h) => ((h.viewport.0 as i32).max(0), (h.rows as i32 - 1).max(1), h.viewport.2 as i32),
                None => (0, 1, 1),
            }
        }
    }

    pub fn scroll_to(&self, top: i32) {
        if let Some(rpc) = self.rpc() {
            let view = Value::Map(vec![(Value::from("topline"), Value::from(top + 1))]);
            let _ = rpc.request("nvim_call_function", vec![Value::from("winrestview"), Value::Array(vec![view])]);
        }
    }

    pub fn wheel(&self, up: bool) {
        if let Some(rpc) = self.rpc() {
            let _ = rpc.notify("nvim_input_mouse", vec![Value::from("wheel"), Value::from(if up { "up" } else { "down" }), Value::from(""), Value::from(0), Value::from(0), Value::from(0)]);
        }
    }

    #[allow(dead_code)]
    pub fn is_normal_mode(&self) -> bool {
        unsafe { state(self.hwnd).map(|h| h.mode == "normal").unwrap_or(true) }
    }

    /// Fit the grid to the window's current size.
    #[allow(dead_code)]
    pub fn resize_to_window(&self) {
        unsafe {
            let mut r = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut r);
            let (cw, ch) = state(self.hwnd).map(|h| (h.cell_w, h.cell_h)).unwrap_or((8, 16));
            let cols = ((r.right - r.left) / cw).max(10) as i64;
            let rows = ((r.bottom - r.top) / ch).max(3) as i64;
            if let Some(rpc) = self.rpc() {
                let _ = rpc.notify("nvim_ui_try_resize", vec![Value::from(cols), Value::from(rows)]);
            }
        }
    }

    // -- inside the window (the boxed state) --

    fn apply(&mut self, events: Vec<Event>) {
        for ev in events {
            match ev {
                Event::Resize(c, r) => {
                    self.cols = c;
                    self.rows = r;
                    self.grid = vec![Cell { text: " ".into(), hl: 0 }; c * r];
                }
                Event::DefaultColors(fg, bg) => {
                    self.default_fg = fg;
                    self.default_bg = bg;
                }
                Event::HlAttr(id, a) => {
                    self.attrs.insert(id, a);
                }
                Event::Line { row, col, cells } => {
                    let mut x = col;
                    for (text, hl, rep) in cells {
                        for _ in 0..rep {
                            if row < self.rows && x < self.cols {
                                self.grid[row * self.cols + x] = Cell { text: text.clone(), hl };
                            }
                            x += 1;
                        }
                    }
                }
                Event::Clear => {
                    for c in &mut self.grid {
                        *c = Cell { text: " ".into(), hl: 0 };
                    }
                }
                Event::Cursor(r, c) => self.cursor = (r, c),
                Event::Scroll { top, bot, left, right, rows } => {
                    let cols = self.cols;
                    if rows > 0 {
                        for y in top..bot.saturating_sub(rows as usize) {
                            for x in left..right.min(cols) {
                                let src = (y + rows as usize) * cols + x;
                                self.grid[y * cols + x] = self.grid[src].clone();
                            }
                        }
                    } else if rows < 0 {
                        let n = (-rows) as usize;
                        for y in (top + n..bot).rev() {
                            for x in left..right.min(cols) {
                                let src = (y - n) * cols + x;
                                self.grid[y * cols + x] = self.grid[src].clone();
                            }
                        }
                    }
                }
                Event::Mode(m) => self.mode = m,
                Event::Viewport { top, bot, lines } => self.viewport = (top, bot, lines),
                Event::Status(text) => {
                    let boxed = Box::into_raw(Box::new(text.unwrap_or_default()));
                    unsafe {
                        if PostMessageW(Some(GetParent(self.hwnd).unwrap_or_default()), WM_NVIM_STATUS, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                            drop(Box::from_raw(boxed));
                        }
                    }
                }
                Event::Flush => {}
            }
        }
    }

    unsafe fn paint(&self, hdc: HDC) {
        let mut rc = RECT::default();
        let _ = GetClientRect(self.hwnd, &mut rc);
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old_bmp = SelectObject(mem, bmp.into());
        let bg_brush = CreateSolidBrush(rgb(self.default_bg));
        FillRect(mem, &rc, bg_brush);
        let _ = DeleteObject(bg_brush.into());
        let mut old_font = SelectObject(mem, self.font.into());
        for row in 0..self.rows {
            let mut x = 0;
            while x < self.cols {
                let hl = self.grid[row * self.cols + x].hl;
                let mut end = x + 1;
                while end < self.cols && self.grid[row * self.cols + end].hl == hl {
                    end += 1;
                }
                let a = self.attrs.get(&hl).copied().unwrap_or_default();
                let (mut fg, mut bg) = (a.fg.unwrap_or(self.default_fg), a.bg.unwrap_or(self.default_bg));
                if a.reverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let font = if a.bold { self.font_bold } else if a.italic { self.font_italic } else { self.font };
                old_font = SelectObject(mem, font.into());
                SetBkColor(mem, rgb(bg));
                SetTextColor(mem, rgb(fg));
                let text: String = self.grid[row * self.cols + x..row * self.cols + end].iter().map(|c| if c.text.is_empty() { " " } else { c.text.as_str() }).collect();
                let wide: Vec<u16> = text.encode_utf16().collect();
                let r = RECT { left: (x as i32) * self.cell_w, top: (row as i32) * self.cell_h, right: (end as i32) * self.cell_w, bottom: (row as i32 + 1) * self.cell_h };
                let _ = ExtTextOutW(mem, r.left, r.top, ETO_OPAQUE | ETO_CLIPPED, Some(&r), PCWSTR(wide.as_ptr()), wide.len() as u32, None);
                if a.underline {
                    let ub = CreateSolidBrush(rgb(fg));
                    FillRect(mem, &RECT { left: r.left, top: r.bottom - self.unit, right: r.right, bottom: r.bottom }, ub);
                    let _ = DeleteObject(ub.into());
                }
                x = end;
            }
        }
        // Cursor: a block in normal/visual modes (text inverted), a bar while inserting.
        let (cr, cc) = self.cursor;
        if cr < self.rows && cc < self.cols {
            let r = RECT { left: (cc as i32) * self.cell_w, top: (cr as i32) * self.cell_h, right: (cc as i32 + 1) * self.cell_w, bottom: (cr as i32 + 1) * self.cell_h };
            let brush = CreateSolidBrush(self.accent);
            if self.mode.starts_with("insert") || self.mode.starts_with("cmdline") || self.mode == "replace" {
                FillRect(mem, &RECT { left: r.left, top: r.top, right: r.left + self.unit, bottom: r.bottom }, brush);
            } else {
                FillRect(mem, &r, brush);
                let cell = &self.grid[cr * self.cols + cc];
                SetBkMode(mem, TRANSPARENT);
                SetTextColor(mem, rgb(self.default_bg));
                let wide: Vec<u16> = (if cell.text.is_empty() { " " } else { cell.text.as_str() }).encode_utf16().collect();
                let _ = ExtTextOutW(mem, r.left, r.top, ETO_CLIPPED, Some(&r), PCWSTR(wide.as_ptr()), wide.len() as u32, None);
                SetBkMode(mem, OPAQUE);
            }
            let _ = DeleteObject(brush.into());
        }
        SelectObject(mem, old_font);
        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
    }

    fn input(&self, keys: &str) {
        if std::env::var_os("BLACKHOLE_NVIM_DEBUG").is_some() {
            crate::util::log(&format!("nvim input {keys:?} mode {}", self.mode));
        }
        if let Some(rpc) = &self.rpc {
            let _ = rpc.notify("nvim_input", vec![Value::from(keys)]);
        }
    }

    fn mouse(&self, button: &str, action: &str, pt: POINT) {
        if let Some(rpc) = &self.rpc {
            let (row, col) = ((pt.y / self.cell_h).max(0), (pt.x / self.cell_w).max(0));
            let mods = unsafe { key_mods() };
            let _ = rpc.notify("nvim_input_mouse", vec![Value::from(button), Value::from(action), Value::from(mods), Value::from(0), Value::from(row as i64), Value::from(col as i64)]);
        }
    }
}

/// "C", "S", "A" letters for the modifier string / key notation.
unsafe fn key_mods() -> String {
    let mut m = String::new();
    if GetKeyState(VK_CONTROL.0 as i32) < 0 { m.push('C'); }
    if GetKeyState(VK_SHIFT.0 as i32) < 0 { m.push('S'); }
    if GetKeyState(VK_MENU.0 as i32) < 0 { m.push('A'); }
    m
}

/// A WM_KEYDOWN worth sending as a `<...>` key; letters and symbols arrive as WM_CHAR.
unsafe fn special_key(vk: VIRTUAL_KEY) -> Option<&'static str> {
    Some(match vk {
        VK_ESCAPE => "Esc",
        VK_RETURN => "CR",
        VK_BACK => "BS",
        VK_TAB => "Tab",
        VK_DELETE => "Del",
        VK_INSERT => "Insert",
        VK_UP => "Up",
        VK_DOWN => "Down",
        VK_LEFT => "Left",
        VK_RIGHT => "Right",
        VK_HOME => "Home",
        VK_END => "End",
        VK_PRIOR => "PageUp",
        VK_NEXT => "PageDown",
        VK_F1 => "F1", VK_F2 => "F2", VK_F3 => "F3", VK_F4 => "F4", VK_F5 => "F5", VK_F6 => "F6",
        VK_F7 => "F7", VK_F8 => "F8", VK_F9 => "F9", VK_F10 => "F10", VK_F11 => "F11", VK_F12 => "F12",
        _ => return None,
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NVIM_EVENTS => {
            let events = *Box::from_raw(lparam.0 as *mut Vec<Event>);
            if let Some(h) = state(hwnd) {
                h.apply(events);
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = PostMessageW(Some(GetParent(hwnd).unwrap_or_default()), WM_NVIM_FLUSH, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_NVIM_CHANGED => {
            // Forwarded to the panel, which decides whether it was a user edit.
            let _ = PostMessageW(Some(GetParent(hwnd).unwrap_or_default()), WM_NVIM_CHANGED, wparam, LPARAM(0));
            LRESULT(0)
        }
        WM_NVIM_CMD => {
            if PostMessageW(Some(GetParent(hwnd).unwrap_or_default()), WM_NVIM_CMD, WPARAM(0), lparam).is_err() {
                drop(Box::from_raw(lparam.0 as *mut String));
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if let Some(h) = state(hwnd) {
                h.paint(hdc);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            if let Some(h) = state(hwnd) {
                let cols = (((lparam.0 & 0xFFFF) as i32) / h.cell_w).max(10) as i64;
                let rows = ((((lparam.0 >> 16) & 0xFFFF) as i32) / h.cell_h).max(3) as i64;
                if let Some(rpc) = &h.rpc {
                    let _ = rpc.notify("nvim_ui_try_resize", vec![Value::from(cols), Value::from(rows)]);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let Some(h) = state(hwnd) else { return LRESULT(0) };
            let vk = VIRTUAL_KEY(wparam.0 as u16);
            let mods = key_mods();
            let ctrl = mods.contains('C');
            let alt = mods.contains('A');
            let parent = GetParent(hwnd).unwrap_or_default();
            // Panel shortcuts: Ctrl+Tab switches tabs; Ctrl+N / Ctrl+Del in normal mode are the panel's.
            if vk == VK_TAB && ctrl {
                let _ = PostMessageW(Some(parent), WM_KEYDOWN, wparam, lparam);
                return LRESULT(0);
            }
            if h.mode == "normal" && ctrl && (vk == VK_N || vk == VK_DELETE) {
                let _ = PostMessageW(Some(parent), WM_KEYDOWN, wparam, lparam);
                return LRESULT(0);
            }
            if vk == VK_ESCAPE && h.mode == "normal" && !ctrl && !alt {
                let _ = PostMessageW(Some(parent), WM_NVIM_ESCAPE, WPARAM(0), LPARAM(0));
                return LRESULT(0);
            }
            if let Some(name) = special_key(vk) {
                let prefix: String = mods.chars().map(|c| format!("{c}-")).collect();
                h.input(&format!("<{prefix}{name}>"));
                return LRESULT(0);
            }
            if ctrl || alt {
                // Letters/digits with Ctrl or Alt never produce a useful WM_CHAR.
                let ch = MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_CHAR) as u8;
                if ch.is_ascii_alphanumeric() || b"[]\\^_-=;',./`".contains(&ch) {
                    let key = (ch as char).to_ascii_lowercase();
                    let prefix: String = mods.chars().filter(|c| *c != 'S').map(|c| format!("{c}-")).collect();
                    h.input(&format!("<{prefix}{key}>"));
                    return LRESULT(0);
                }
            }
            LRESULT(0)
        }
        WM_CHAR => {
            if let Some(h) = state(hwnd) {
                // Dead keys arrive already composed; IME results come through DefWindowProc as
                // WM_CHAR too; characters outside the BMP come as two surrogate halves.
                let c = wparam.0 as u32;
                let ch = match c {
                    0xD800..=0xDBFF => { h.high_surrogate = Some(c as u16); None }
                    0xDC00..=0xDFFF => h.high_surrogate.take().and_then(|hi| char::decode_utf16([hi, c as u16]).next().and_then(|r| r.ok())),
                    c if c >= 0x20 && c != 0x7F => char::from_u32(c),
                    _ => None,
                };
                if let Some(ch) = ch {
                    h.input(&ch.to_string().replace('<', "<lt>"));
                }
            }
            LRESULT(0)
        }
        WM_IME_STARTCOMPOSITION => {
            // Put the IME's composition window at the cursor cell.
            if let Some(h) = state(hwnd) {
                use windows::Win32::UI::Input::Ime::*;
                let ctx = ImmGetContext(hwnd);
                if !ctx.is_invalid() {
                    let (r, c) = h.cursor;
                    let cf = COMPOSITIONFORM { dwStyle: CFS_POINT, ptCurrentPos: POINT { x: (c as i32) * h.cell_w, y: (r as i32) * h.cell_h }, rcArea: RECT::default() };
                    let _ = ImmSetCompositionWindow(ctx, &cf);
                    let _ = ImmReleaseContext(hwnd, ctx);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let _ = SetFocus(Some(hwnd));
            if let Some(h) = state(hwnd) {
                let b = match msg { WM_LBUTTONDOWN => "left", WM_RBUTTONDOWN => "right", _ => "middle" };
                h.mouse_down = msg == WM_LBUTTONDOWN;
                SetCapture(hwnd);
                h.mouse(b, "press", lparam_point(lparam));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            if let Some(h) = state(hwnd) {
                let b = match msg { WM_LBUTTONUP => "left", WM_RBUTTONUP => "right", _ => "middle" };
                h.mouse_down = false;
                let _ = ReleaseCapture();
                h.mouse(b, "release", lparam_point(lparam));
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(h) = state(hwnd) {
                if h.mouse_down {
                    h.mouse("left", "drag", lparam_point(lparam));
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // The panel routes wheels (it also repaints the pixel bar).
            SendMessageW(GetParent(hwnd).unwrap_or_default(), msg, Some(wparam), Some(lparam))
        }
        WM_GETDLGCODE => LRESULT((DLGC_WANTALLKEYS | DLGC_WANTCHARS | DLGC_WANTARROWS | DLGC_WANTTAB) as isize),
        WM_SETFOCUS | WM_KILLFOCUS => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host;
            if !p.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let h = Box::from_raw(p);
                let _ = DeleteObject(h.font.into());
                let _ = DeleteObject(h.font_bold.into());
                let _ = DeleteObject(h.font_italic.into());
                drop(h); // drops the Rpc (kills nvim) once the panel's handle is gone too
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn lparam_point(lparam: LPARAM) -> POINT {
    POINT { x: (lparam.0 & 0xFFFF) as u16 as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32 }
}
