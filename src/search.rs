//! The search panel: a dark popup with an edit box and an owner-drawn result list.
//! Opens beside the dot, keyboard-first, closes on Esc or when it loses focus.

use crate::ask::{AskEngine, Job, WM_ASK_DONE, WM_ASK_STATUS, WM_ASK_TOKEN};
use crate::embed::Embedder;
use crate::store::{Hit, Store};
use std::sync::atomic::Ordering;
use crate::util::wide;
use std::sync::{Arc, Mutex};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_SEARCH_CLOSED: u32 = 0x8010;
pub const WM_ASK_STARTED: u32 = 0x8014;
/// Posted to the dot once per question when the first answer token (or the end) arrives.
pub const WM_ASK_FIRST_TOKEN: u32 = 0x8015;

const ID_EDIT: usize = 100;
const ID_LIST: usize = 101;
const ID_ANSWER: usize = 102;
const ANSWER_LINES: i32 = 6;
const TIMER_DEBOUNCE: usize = 1;
const MAX_HITS: usize = 40;
const ROWS_VISIBLE: i32 = 8;

const BG: COLORREF = COLORREF(0x00140A18); // 0x00BBGGRR: very dark violet
const BG_EDIT: COLORREF = COLORREF(0x00241430);
const BG_SEL: COLORREF = COLORREF(0x00602878);
const BORDER: COLORREF = COLORREF(0x0040A0FF); // orange
const FG: COLORREF = COLORREF(0x00F0E8FF);
const FG_DIM: COLORREF = COLORREF(0x00A090B0);
const FG_KIND: COLORREF = COLORREF(0x0040A0FF);

pub struct SearchWin {
    pub hwnd: HWND,
    edit: HWND,
    list: HWND,
    answer: HWND,
    status: HWND,
    dot: HWND,
    store: Arc<Mutex<Store>>,
    embedder: Arc<Embedder>,
    ask: Arc<AskEngine>,
    /// In-flight generation, if any; stale messages carry an older id.
    job: Option<Job>,
    next_job: u64,
    answer_text: String,
    hits: Vec<Hit>,
    font: HFONT,
    font_small: HFONT,
    brush_bg: HBRUSH,
    brush_edit: HBRUSH,
    brush_sel: HBRUSH,
    scale: f32,
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut SearchWin> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SearchWin;
    p.as_mut()
}

impl SearchWin {
    pub fn create(dot: HWND, store: Arc<Mutex<Store>>, embedder: Arc<Embedder>, ask: Arc<AskEngine>) -> HWND {
        unsafe {
            let class = w!("BlackholeSearch");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH::default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let boxed = Box::new(SearchWin {
                hwnd: HWND::default(),
                edit: HWND::default(),
                list: HWND::default(),
                answer: HWND::default(),
                status: HWND::default(),
                dot,
                store,
                embedder,
                ask,
                job: None,
                next_job: 1,
                answer_text: String::new(),
                hits: Vec::new(),
                font: HFONT::default(),
                font_small: HFONT::default(),
                brush_bg: CreateSolidBrush(BG),
                brush_edit: CreateSolidBrush(BG_EDIT),
                brush_sel: CreateSolidBrush(BG_SEL),
                scale: 1.0,
            });
            let ptr = Box::into_raw(boxed);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class,
                w!("Blackhole"),
                WS_POPUP,
                0, 0, 10, 10,
                None, None, None,
                Some(ptr as *const _),
            )
            .unwrap_or_default();
            hwnd
        }
    }

    fn px(&self, v: i32) -> i32 {
        (v as f32 * self.scale).round() as i32
    }

    fn row_height(&self) -> i32 {
        self.px(42)
    }

    unsafe fn layout(&mut self) {
        let dpi = GetDpiForWindow(self.hwnd);
        self.scale = dpi as f32 / 96.0;
        if !self.font.is_invalid() {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.font_small.into());
        }
        self.font = CreateFontW(
            -self.px(15), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
            (FIXED_PITCH.0 | FF_MODERN.0) as u32, w!("Consolas"),
        );
        self.font_small = CreateFontW(
            -self.px(12), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
            (FIXED_PITCH.0 | FF_MODERN.0) as u32, w!("Consolas"),
        );
        SendMessageW(self.edit, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
        SendMessageW(self.answer, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
        SendMessageW(self.status, WM_SETFONT, Some(WPARAM(self.font_small.0 as usize)), Some(LPARAM(1)));
        SendMessageW(self.list, LB_SETITEMHEIGHT, Some(WPARAM(0)), Some(LPARAM(self.row_height() as isize)));

        let w = self.px(480);
        let pad = self.px(8);
        let edit_h = self.px(26);
        let status_h = self.px(16);
        let asking = IsWindowVisible(self.answer).as_bool();
        let answer_h = if asking { self.px(20) * ANSWER_LINES + pad } else { 0 };
        let list_h = self.row_height() * ROWS_VISIBLE;
        let h = pad + edit_h + pad + answer_h + list_h + pad / 2 + status_h + pad;
        let _ = SetWindowPos(self.hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = SetWindowPos(self.edit, None, pad, pad, w - 2 * pad, edit_h, SWP_NOZORDER | SWP_NOACTIVATE);
        let mut y = pad + edit_h + pad;
        if asking {
            let _ = SetWindowPos(self.answer, None, pad, y, w - 2 * pad, answer_h - pad, SWP_NOZORDER | SWP_NOACTIVATE);
            y += answer_h;
        }
        let _ = SetWindowPos(self.list, None, pad, y, w - 2 * pad, list_h, SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = SetWindowPos(self.status, None, pad, y + list_h + pad / 2, w - 2 * pad, status_h, SWP_NOZORDER | SWP_NOACTIVATE);
    }

    /// Everything after a leading `?` is a question for ask mode.
    fn question_of(q: &str) -> Option<&str> {
        q.trim_start().strip_prefix('?').map(str::trim)
    }

    unsafe fn set_answer_visible(&mut self, visible: bool) {
        if IsWindowVisible(self.answer).as_bool() == visible {
            return;
        }
        let _ = ShowWindow(self.answer, if visible { SW_SHOW } else { SW_HIDE });
        // Keep the top edge where it is; the panel grows downward.
        let mut r = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut r);
        self.layout();
        let mut nr = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut nr);
        let _ = SetWindowPos(self.hwnd, None, r.left, r.top, nr.right - nr.left, nr.bottom - nr.top, SWP_NOZORDER | SWP_NOACTIVATE);
    }

    unsafe fn cancel_job(&mut self) {
        if let Some(j) = self.job.take() {
            j.cancel.store(true, Ordering::Relaxed);
            let _ = PostMessageW(Some(self.dot), WM_ASK_FIRST_TOKEN, WPARAM(0), LPARAM(0)); // stop the dot's thinking
        }
    }

    unsafe fn start_ask(&mut self) {
        let q = self.query_text();
        let Some(question) = Self::question_of(&q).filter(|s| !s.is_empty()) else { return };
        self.cancel_job();
        self.answer_text.clear();
        let _ = SetWindowTextW(self.answer, w!(""));
        self.set_answer_visible(true);
        let id = self.next_job;
        self.next_job += 1;
        self.set_status("thinking…");
        self.job = Some(self.ask.ask(question.to_string(), self.hwnd, id));
        let _ = PostMessageW(Some(self.dot), WM_ASK_STARTED, WPARAM(0), LPARAM(0));
    }

    unsafe fn set_status(&self, text: &str) {
        let _ = SetWindowTextW(self.status, PCWSTR(wide(text).as_ptr()));
    }

    unsafe fn on_ask_message(&mut self, msg: u32, id: u64, text: String) {
        if self.job.as_ref().map(|j| j.id) != Some(id) {
            return; // from a cancelled/older job
        }
        match msg {
            WM_ASK_TOKEN => {
                if self.answer_text.is_empty() {
                    let _ = PostMessageW(Some(self.dot), WM_ASK_FIRST_TOKEN, WPARAM(0), LPARAM(0));
                }
                self.answer_text.push_str(&text);
                let _ = SetWindowTextW(self.answer, PCWSTR(wide(&self.answer_text).as_ptr()));
                let len = self.answer_text.encode_utf16().count();
                SendMessageW(self.answer, EM_SETSEL, Some(WPARAM(len)), Some(LPARAM(len as isize)));
                SendMessageW(self.answer, EM_SCROLLCARET, None, None);
            }
            WM_ASK_STATUS => self.set_status(&text),
            WM_ASK_DONE => {
                let _ = PostMessageW(Some(self.dot), WM_ASK_FIRST_TOKEN, WPARAM(0), LPARAM(0));
                if self.answer_text.is_empty() {
                    let _ = SetWindowTextW(self.answer, PCWSTR(wide(&text).as_ptr()));
                    self.set_status("");
                } else {
                    self.set_status(&text);
                }
                self.job = None;
            }
            _ => {}
        }
    }

    /// Show beside the dot (to the right, or left/above if that runs off-screen).
    pub unsafe fn show(hwnd: HWND, dot_rect: RECT) {
        let Some(s) = state(hwnd) else { return };
        s.layout();
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let (w, h) = (r.right - r.left, r.bottom - r.top);

        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = MonitorFromPoint(POINT { x: dot_rect.left, y: dot_rect.top }, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(mon, &mut mi);
        let work = mi.rcWork;

        let gap = s.px(6);
        let mut x = dot_rect.right + gap;
        let mut y = dot_rect.top;
        if x + w > work.right {
            x = dot_rect.left - gap - w;
        }
        if x < work.left {
            x = work.left;
        }
        if y + h > work.bottom {
            y = work.bottom - h;
        }
        if y < work.top {
            y = work.top;
        }
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
        let _ = SetWindowTextW(s.edit, w!(""));
        s.refresh();
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(s.edit));
    }

    pub unsafe fn hide(hwnd: HWND) {
        if IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
            if let Some(s) = state(hwnd) {
                s.cancel_job();
                s.set_answer_visible(false);
                let _ = PostMessageW(Some(s.dot), WM_SEARCH_CLOSED, WPARAM(0), LPARAM(0));
            }
        }
    }

    unsafe fn query_text(&self) -> String {
        let len = GetWindowTextLengthW(self.edit) as usize;
        let mut buf = vec![0u16; len + 1];
        GetWindowTextW(self.edit, &mut buf);
        crate::util::from_wide(&buf)
    }

    unsafe fn refresh(&mut self) {
        let raw = self.query_text();
        let asking = Self::question_of(&raw).is_some();
        if !asking {
            self.cancel_job();
            self.set_answer_visible(false);
        }
        let q = Self::question_of(&raw).unwrap_or(&raw).to_string();
        // Embed outside the store lock; the worker may be holding it to write.
        let qvec = if q.trim().is_empty() { None } else { self.embedder.embed(&q).ok() };
        let (hits, total, absent) = {
            let store = self.store.lock().unwrap();
            let absent = !q.trim().is_empty() && store.absent(&q, qvec.as_deref());
            (if absent { Vec::new() } else { store.search(&q, qvec.as_deref(), MAX_HITS) }, store.count(), absent)
        };
        self.hits = hits;
        SendMessageW(self.list, LB_RESETCONTENT, None, None);
        for i in 0..self.hits.len() {
            SendMessageW(self.list, LB_ADDSTRING, Some(WPARAM(0)), Some(LPARAM(i as isize)));
        }
        if !self.hits.is_empty() {
            SendMessageW(self.list, LB_SETCURSEL, Some(WPARAM(0)), None);
        }
        if self.job.is_some() {
            return; // the ask worker owns the status line for now
        }
        let status = if q.trim().is_empty() {
            format!("{total} items inside · recent   ↵ open  ^↵ reveal  ^C copy  Del forget  ? ask")
        } else if absent {
            "nothing in the vault mentions that".to_string()
        } else if asking {
            format!("↵ to ask · {} related items", self.hits.len())
        } else {
            format!("{} of {total} items   ↵ open  ^↵ reveal  ^C copy  Del forget", self.hits.len())
        };
        self.set_status(&status);
    }

    unsafe fn selected(&self) -> Option<&Hit> {
        let i = SendMessageW(self.list, LB_GETCURSEL, None, None).0;
        if i < 0 {
            return None;
        }
        self.hits.get(i as usize)
    }

    unsafe fn move_sel(&self, delta: i32) {
        if self.hits.is_empty() {
            return;
        }
        let cur = SendMessageW(self.list, LB_GETCURSEL, None, None).0 as i32;
        let next = (cur + delta).clamp(0, self.hits.len() as i32 - 1);
        SendMessageW(self.list, LB_SETCURSEL, Some(WPARAM(next as usize)), None);
    }

    unsafe fn open_selected(&self, reveal: bool) {
        let Some(hit) = self.selected() else { return };
        match (&hit.source, reveal) {
            (Some(path), true) => {
                let args = wide(&format!("/select,\"{path}\""));
                ShellExecuteW(None, w!("open"), w!("explorer.exe"), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
            }
            (Some(path), false) => {
                ShellExecuteW(None, w!("open"), PCWSTR(wide(path).as_ptr()), None, None, SW_SHOWNORMAL);
            }
            (None, _) => self.copy_selected(),
        }
        SearchWin::hide(self.hwnd);
    }

    unsafe fn copy_selected(&self) {
        let Some(hit) = self.selected() else { return };
        let text = self.store.lock().unwrap().content(hit.id).unwrap_or_default();
        set_clipboard_text(self.hwnd, &text);
    }

    unsafe fn forget_selected(&mut self) {
        let Some(hit) = self.selected() else { return };
        let id = hit.id;
        let _ = self.store.lock().unwrap().delete(id);
        self.refresh();
    }

    unsafe fn draw_item(&self, dis: &DRAWITEMSTRUCT) {
        if dis.itemID == u32::MAX {
            return;
        }
        let Some(hit) = self.hits.get(dis.itemID as usize) else { return };
        let selected = (dis.itemState.0 & ODS_SELECTED.0) != 0;
        let hdc = dis.hDC;
        FillRect(hdc, &dis.rcItem, if selected { self.brush_sel } else { self.brush_bg });
        SetBkMode(hdc, TRANSPARENT);

        let pad = self.px(6);
        let mut r = dis.rcItem;
        r.left += pad;
        r.right -= pad;
        r.top += self.px(4);

        // kind tag + title
        let old = SelectObject(hdc, self.font.into());
        SetTextColor(hdc, FG_KIND);
        let via = match hit.via { "sem" => " ≈", "both" => " ≈=", _ => "" };
        let mut tag = wide(&format!("[{}]{via}", hit.kind));
        let mut tag_r = r;
        DrawTextW(hdc, &mut tag, &mut tag_r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
        DrawTextW(hdc, &mut tag, &mut r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);
        let mut title_r = r;
        title_r.left = tag_r.right + pad;
        SetTextColor(hdc, FG);
        let mut title = wide(&hit.title);
        DrawTextW(hdc, &mut title, &mut title_r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);

        // snippet, with FTS highlight markers stripped (rendered plain for now)
        SelectObject(hdc, self.font_small.into());
        SetTextColor(hdc, FG_DIM);
        let mut snip_r = r;
        snip_r.top += self.px(18);
        let clean: String = hit.snippet.chars().filter(|c| *c != '\u{1}' && *c != '\u{2}').map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
        let mut snip = wide(&clean);
        DrawTextW(hdc, &mut snip, &mut snip_r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
        SelectObject(hdc, old);
    }
}

unsafe fn set_clipboard_text(owner: HWND, text: &str) {
    let data = wide(text);
    let bytes = data.len() * 2;
    if OpenClipboard(Some(owner)).is_err() {
        return;
    }
    let _ = EmptyClipboard();
    if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
        let p = GlobalLock(h) as *mut u16;
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            let _ = GlobalUnlock(h);
            let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(windows::Win32::Foundation::HANDLE(h.0)));
        }
    }
    let _ = CloseClipboard();
}

unsafe extern "system" fn edit_subclass(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, refdata: usize) -> LRESULT {
    let parent = HWND(refdata as *mut _);
    match msg {
        WM_KEYDOWN => {
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            let key = VIRTUAL_KEY(wparam.0 as u16);
            if let Some(s) = state(parent) {
                match key {
                    VK_ESCAPE if s.job.is_some() => { s.cancel_job(); s.set_status("stopped"); return LRESULT(0); }
                    VK_ESCAPE => { SearchWin::hide(parent); return LRESULT(0); }
                    VK_DOWN => { s.move_sel(1); return LRESULT(0); }
                    VK_UP => { s.move_sel(-1); return LRESULT(0); }
                    VK_NEXT => { s.move_sel(ROWS_VISIBLE); return LRESULT(0); }
                    VK_PRIOR => { s.move_sel(-ROWS_VISIBLE); return LRESULT(0); }
                    VK_RETURN if SearchWin::question_of(&s.query_text()).is_some() => { s.start_ask(); return LRESULT(0); }
                    VK_RETURN => { s.open_selected(ctrl); return LRESULT(0); }
                    VK_DELETE if !s.query_text().is_empty() && ctrl => { s.forget_selected(); return LRESULT(0); }
                    VK_DELETE if s.query_text().is_empty() => { s.forget_selected(); return LRESULT(0); }
                    _ if key == VK_C && ctrl => {
                        // Copy the selected item unless there is a text selection in the box.
                        let sel = SendMessageW(hwnd, EM_GETSEL, None, None).0 as u32;
                        if (sel & 0xFFFF) == (sel >> 16) {
                            s.copy_selected();
                            return LRESULT(0);
                        }
                    }
                    _ => {}
                }
            }
        }
        WM_CHAR if wparam.0 == 27 || wparam.0 == 13 => return LRESULT(0), // swallow beep
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut SearchWin;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            let s = &mut *ptr;
            s.hwnd = hwnd;
            s.edit = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("EDIT"), w!(""),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                0, 0, 10, 10, Some(hwnd), Some(HMENU(ID_EDIT as *mut _)), None, None,
            ).unwrap_or_default();
            s.list = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("LISTBOX"), w!(""),
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE((LBS_OWNERDRAWFIXED | LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32),
                0, 0, 10, 10, Some(hwnd), Some(HMENU(ID_LIST as *mut _)), None, None,
            ).unwrap_or_default();
            s.answer = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("EDIT"), w!(""),
                WS_CHILD | WS_VSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32),
                0, 0, 10, 10, Some(hwnd), Some(HMENU(ID_ANSWER as *mut _)), None, None,
            ).unwrap_or_default();
            s.status = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("STATIC"), w!(""),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(0x0C),
                0, 0, 10, 10, Some(hwnd), None, None, None,
            ).unwrap_or_default();
            let _ = SetWindowSubclass(s.edit, Some(edit_subclass), 1, hwnd.0 as usize);
            s.layout();
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            let hdc = HDC(wparam.0 as *mut _);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            if let Some(s) = state(hwnd) {
                FillRect(hdc, &r, s.brush_bg);
                let pen = CreatePen(PS_SOLID, s.px(2), BORDER);
                let old = SelectObject(hdc, pen.into());
                let oldb = SelectObject(hdc, GetStockObject(NULL_BRUSH));
                let _ = Rectangle(hdc, r.left, r.top, r.right, r.bottom);
                SelectObject(hdc, oldb);
                SelectObject(hdc, old);
                let _ = DeleteObject(pen.into());
            }
            LRESULT(1)
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORLISTBOX => {
            let hdc = HDC(wparam.0 as *mut _);
            if let Some(s) = state(hwnd) {
                let ctl = HWND(lparam.0 as *mut _);
                let is_edit = msg == WM_CTLCOLOREDIT || ctl == s.answer;
                SetTextColor(hdc, if is_edit { FG } else { FG_DIM });
                SetBkColor(hdc, if is_edit { BG_EDIT } else { BG });
                return LRESULT(if is_edit { s.brush_edit.0 } else { s.brush_bg.0 } as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ASK_TOKEN | WM_ASK_STATUS | WM_ASK_DONE => {
            let text = *Box::from_raw(lparam.0 as *mut String);
            if let Some(s) = state(hwnd) {
                s.on_ask_message(msg, wparam.0 as u64, text);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as usize;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if let Some(s) = state(hwnd) {
                if id == ID_EDIT && code == EN_CHANGE {
                    SetTimer(Some(hwnd), TIMER_DEBOUNCE, 90, None);
                } else if id == ID_LIST && code == LBN_DBLCLK {
                    s.open_selected(false);
                } else if id == ID_LIST && code == LBN_SELCHANGE {
                    let _ = SetFocus(Some(s.edit));
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_DEBOUNCE => {
            let _ = KillTimer(Some(hwnd), TIMER_DEBOUNCE);
            if let Some(s) = state(hwnd) {
                s.refresh();
            }
            LRESULT(0)
        }
        WM_DRAWITEM => {
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            if let Some(s) = state(hwnd) {
                s.draw_item(dis);
            }
            LRESULT(1)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                SearchWin::hide(hwnd);
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if let Some(s) = state(hwnd) {
                let _ = SetFocus(Some(s.edit));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            SearchWin::hide(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
