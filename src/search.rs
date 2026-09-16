//! The search panel: a dark popup with an edit box and an owner-drawn result list.
//! Opens beside the dot, keyboard-first, closes on Esc or when it loses focus.
//!
//! Sizing: the panel fits what it shows (result count, answer length, status line) up to a
//! cap, animated ~150 ms on the pixel grid. A pixel grip in the bottom-right corner drags a
//! size; that size is saved (via the dot, which owns the config) and becomes the cap.
//! Closing keeps the view — query, results, streamed answer, scroll — and reopening restores
//! it; Esc on a restored panel (or a new query) clears it.
//!
//! Scrollbars: the result list and the answer box are created *without* WS_VSCROLL and the
//! panel paints pixel-art bars in a strip to their right (dark track, 1-unit orange border,
//! blocky thumb). The controls keep their native scrolling (LB_SETTOPINDEX / EM_LINESCROLL,
//! selection and caret tracking), so behaviour is unchanged; the panel only draws the bar and
//! turns wheel, track clicks and thumb drags into those messages. This is much smaller than
//! replacing the read-only EDIT with a custom control, and owner-painting *over* a child would
//! fight the control's own repaints — so the bar simply lives outside the control.

use crate::ask::{AskEngine, Job, WM_ASK_DONE, WM_ASK_STATUS, WM_ASK_TOKEN};
use crate::embed::Embedder;
use crate::store::{Hit, Store};
use crate::util::wide;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::SystemServices::{SS_ENDELLIPSIS, SS_LEFTNOWORDWRAP};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_SEARCH_CLOSED: u32 = 0x8010;
pub const WM_ASK_STARTED: u32 = 0x8014;
/// The user dragged the corner grip: wparam = width, lparam = height, both in 96-DPI px.
/// Sent to the dot, which owns the config.
pub const WM_PANEL_RESIZED: u32 = 0x8015;

const ID_EDIT: usize = 100;
const ID_LIST: usize = 101;
const ID_ANSWER: usize = 102;
const TIMER_DEBOUNCE: usize = 1;
const TIMER_RESIZE: usize = 2;
const TIMER_THINK: usize = 3;
/// Subclass ids for the two scrollable children.
const SC_LIST: usize = 2;
const SC_ANSWER: usize = 3;
const MAX_HITS: usize = 40;

// Logical (96-DPI) sizes.
const DEFAULT_W: i32 = 480;
/// Content sizing never grows the panel past this unless the user dragged it taller.
const DEFAULT_CAP_H: i32 = 540;
const MIN_W: i32 = 260;
const MIN_ROWS: i32 = 3;
const ANSWER_MIN_LINES: i32 = 2;
const ANSWER_MAX_LINES: i32 = 10;
const ANIM_MS: f32 = 150.0;

const BG: COLORREF = COLORREF(0x00140A18); // 0x00BBGGRR: very dark violet
const BG_EDIT: COLORREF = COLORREF(0x00241430);
const BG_SEL: COLORREF = COLORREF(0x00602878);
const BORDER: COLORREF = COLORREF(0x0040A0FF); // orange
const FG: COLORREF = COLORREF(0x00F0E8FF);
const FG_DIM: COLORREF = COLORREF(0x00A090B0);
const FG_KIND: COLORREF = COLORREF(0x0040A0FF);

#[derive(Clone, Copy, PartialEq)]
enum Ctl {
    List,
    Answer,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// Corner grip; the cursor's offset from the bottom-right corner at mouse-down.
    Corner(i32, i32),
    /// Scrollbar thumb; the cursor's offset from the thumb's top at mouse-down.
    Thumb(Ctl, i32),
}

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
    status_text: String,
    hits: Vec<Hit>,
    font: HFONT,
    font_small: HFONT,
    brush_bg: HBRUSH,
    brush_edit: HBRUSH,
    brush_sel: HBRUSH,
    brush_accent: HBRUSH,
    scale: f32,
    /// One line of answer text (font height + leading), physical px.
    line_h: i32,
    /// The answer area is part of the layout (ask mode). The EDIT itself is hidden while
    /// `thinking`, when the panel paints a pixel ellipsis in its place.
    answer_shown: bool,
    thinking: bool,
    think_frame: u32,
    /// Size dragged by the user, 96-DPI px (0 = default). The height caps content sizing.
    user_w: i32,
    user_h: i32,
    /// Current window size, physical px.
    cur_w: i32,
    cur_h: i32,
    /// Height animation from `anim_from` to `anim_to`, started at `anim_start`.
    animating: bool,
    anim_from: i32,
    anim_to: i32,
    anim_start: Instant,
    /// Client rects of the two scrollable children, from the last layout.
    list_rc: RECT,
    answer_rc: RECT,
    drag: Drag,
    /// Reopened with the previous view intact: Esc clears it instead of just closing.
    restored: bool,
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut SearchWin> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SearchWin;
    p.as_mut()
}

impl SearchWin {
    /// `size` is the remembered dragged size from the config, (w, h) in 96-DPI px, 0 = default.
    pub fn create(dot: HWND, store: Arc<Mutex<Store>>, embedder: Arc<Embedder>, ask: Arc<AskEngine>, size: (i32, i32)) -> HWND {
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
                status_text: String::new(),
                hits: Vec::new(),
                font: HFONT::default(),
                font_small: HFONT::default(),
                brush_bg: CreateSolidBrush(BG),
                brush_edit: CreateSolidBrush(BG_EDIT),
                brush_sel: CreateSolidBrush(BG_SEL),
                brush_accent: CreateSolidBrush(BORDER),
                scale: 1.0,
                line_h: 20,
                answer_shown: false,
                thinking: false,
                think_frame: 0,
                user_w: size.0.max(0),
                user_h: size.1.max(0),
                cur_w: 10,
                cur_h: 10,
                animating: false,
                anim_from: 0,
                anim_to: 0,
                anim_start: Instant::now(),
                list_rc: RECT::default(),
                answer_rc: RECT::default(),
                drag: Drag::None,
                restored: false,
            });
            let ptr = Box::into_raw(boxed);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class,
                w!("Blackhole"),
                WS_POPUP | WS_CLIPCHILDREN,
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

    /// The pixel-art unit everything chunky (border, bars, grip) is drawn in.
    fn unit(&self) -> i32 {
        self.px(2).max(1)
    }

    fn snap(&self, v: i32) -> i32 {
        let u = self.unit();
        (v + u / 2) / u * u
    }

    fn row_height(&self) -> i32 {
        self.px(42)
    }

    fn pad(&self) -> i32 {
        self.px(8)
    }

    fn bar_w(&self) -> i32 {
        5 * self.unit()
    }

    fn grip_size(&self) -> i32 {
        6 * self.unit()
    }

    /// Everything but the answer box and the list: edit row, status row, paddings.
    fn fixed_h(&self) -> i32 {
        let pad = self.pad();
        pad + self.px(26) + pad + pad / 2 + self.px(16) + pad
    }

    /// Height the answer area wants (0 when not asking), including its gap below.
    unsafe fn answer_wanted_h(&self) -> i32 {
        if !self.answer_shown {
            return 0;
        }
        let lines = if self.thinking {
            ANSWER_MIN_LINES
        } else {
            (SendMessageW(self.answer, EM_GETLINECOUNT, None, None).0 as i32).clamp(ANSWER_MIN_LINES, ANSWER_MAX_LINES)
        };
        lines * self.line_h + self.px(6) + self.pad()
    }

    fn min_h(&self) -> i32 {
        let answer = if self.answer_shown { ANSWER_MIN_LINES * self.line_h + self.px(6) + self.pad() } else { 0 };
        self.fixed_h() + answer + MIN_ROWS * self.row_height()
    }

    unsafe fn layout_fonts(&mut self) {
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
        let hdc = GetDC(Some(self.hwnd));
        let old = SelectObject(hdc, self.font.into());
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        self.line_h = (tm.tmHeight + tm.tmExternalLeading).max(self.px(12));
        SelectObject(hdc, old);
        ReleaseDC(Some(self.hwnd), hdc);
    }

    /// Width of the status text in the small font, so the panel can widen rather than clip it.
    unsafe fn status_width(&self) -> i32 {
        let hdc = GetDC(Some(self.hwnd));
        let old = SelectObject(hdc, self.font_small.into());
        let mut text = wide(&self.status_text);
        let mut r = RECT::default();
        DrawTextW(hdc, &mut text, &mut r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
        SelectObject(hdc, old);
        ReleaseDC(Some(self.hwnd), hdc);
        r.right - r.left
    }

    /// The size the content wants: width from the drag/default (wider if the status clips),
    /// height fitting answer + results, clamped between the minimum and the cap.
    unsafe fn wanted_size(&self) -> (i32, i32) {
        let u = self.unit();
        let mut w = self.px(if self.user_w > 0 { self.user_w } else { DEFAULT_W });
        if self.user_w == 0 {
            // Default width only: widen so the status line does not clip. A dragged width
            // is the user's call and wins (the status then ends in an ellipsis).
            let need = self.status_width() + 2 * self.pad() + self.grip_size() + 2 * u;
            w = w.max(need).min(self.px(1200));
        }
        let cap = self.px(if self.user_h > 0 { self.user_h } else { DEFAULT_CAP_H });
        let rows = (self.hits.len() as i32).max(MIN_ROWS);
        let want = self.fixed_h() + self.answer_wanted_h() + rows * self.row_height();
        let h = want.min(cap).max(self.min_h());
        (self.snap(w), self.snap(h))
    }

    /// Position the children for the current size. The list takes what the answer leaves.
    unsafe fn layout_children(&mut self) {
        let (w, h) = (self.cur_w, self.cur_h);
        let (pad, u) = (self.pad(), self.unit());
        let edit_h = self.px(26);
        let status_h = self.px(16);
        let inner_w = w - 2 * pad - u - self.bar_w();
        let _ = SetWindowPos(self.edit, None, pad, pad, w - 2 * pad, edit_h, SWP_NOZORDER | SWP_NOACTIVATE);
        let mut y = pad + edit_h + pad;
        let answer_h = if self.answer_shown {
            self.answer_wanted_h().min(h - self.fixed_h() - MIN_ROWS * self.row_height()).max(0)
        } else {
            0
        };
        self.answer_rc = RECT { left: pad, top: y, right: pad + inner_w, bottom: y + (answer_h - pad).max(0) };
        let _ = SetWindowPos(self.answer, None, pad, y, inner_w, (answer_h - pad).max(0), SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = ShowWindow(self.answer, if self.answer_shown && !self.thinking { SW_SHOWNA } else { SW_HIDE });
        y += answer_h;
        let list_h = (h - y - pad / 2 - status_h - pad).max(0);
        self.list_rc = RECT { left: pad, top: y, right: pad + inner_w, bottom: y + list_h };
        let _ = SetWindowPos(self.list, None, pad, y, inner_w, list_h, SWP_NOZORDER | SWP_NOACTIVATE);
        // Owner-drawn rows are only repainted where newly exposed; ellipses need the new width.
        let _ = InvalidateRect(Some(self.list), None, true);
        let status_w = w - 2 * pad - self.grip_size() - u;
        let _ = SetWindowPos(self.status, None, pad, y + list_h + pad / 2, status_w, status_h, SWP_NOZORDER | SWP_NOACTIVATE);
    }

    /// Set the window size now, keeping the top-left corner unless that runs off the work area.
    unsafe fn apply_size(&mut self, w: i32, h: i32) {
        let mut r = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut r);
        let (mut x, mut y) = (r.left, r.top);
        if IsWindowVisible(self.hwnd).as_bool() {
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
            let work = mi.rcWork;
            if y + h > work.bottom {
                y = (work.bottom - h).max(work.top);
            }
            if x + w > work.right {
                x = (work.right - w).max(work.left);
            }
        }
        let _ = SetWindowPos(self.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        self.cur_w = w;
        self.cur_h = h;
        self.layout_children();
        let _ = InvalidateRect(Some(self.hwnd), None, true);
    }

    /// Fit the panel to its content; animated (§1.3 pace rule) when it is on screen.
    unsafe fn fit(&mut self, animate: bool) {
        let (w, h) = self.wanted_size();
        if !animate || !IsWindowVisible(self.hwnd).as_bool() {
            self.stop_anim();
            self.apply_size(w, h);
            return;
        }
        if w != self.cur_w {
            let cur_h = self.cur_h;
            self.apply_size(w, cur_h);
        }
        if h == self.cur_h {
            self.stop_anim();
            return;
        }
        if self.animating && h == self.anim_to {
            return; // already heading there
        }
        self.animating = true;
        self.anim_from = self.cur_h;
        self.anim_to = h;
        self.anim_start = Instant::now();
        SetTimer(Some(self.hwnd), TIMER_RESIZE, 15, None);
    }

    unsafe fn stop_anim(&mut self) {
        if self.animating {
            self.animating = false;
            let _ = KillTimer(Some(self.hwnd), TIMER_RESIZE);
        }
    }

    unsafe fn tick_resize(&mut self) {
        let t = (self.anim_start.elapsed().as_secs_f32() * 1000.0 / ANIM_MS).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3); // ease-out
        let h = if t >= 1.0 { self.anim_to } else { self.snap(self.anim_from + ((self.anim_to - self.anim_from) as f32 * e) as i32) };
        if h != self.cur_h {
            let w = self.cur_w;
            self.apply_size(w, h);
        }
        if t >= 1.0 {
            self.stop_anim();
        }
    }

    // ---- scrollbars ----

    unsafe fn ctl_visible(&self, c: Ctl) -> bool {
        match c {
            Ctl::List => true,
            Ctl::Answer => self.answer_shown && !self.thinking,
        }
    }

    /// (first visible line, lines per page, line count) — the control's own numbers.
    unsafe fn scroll_info(&self, c: Ctl) -> (i32, i32, i32) {
        match c {
            Ctl::List => {
                let count = SendMessageW(self.list, LB_GETCOUNT, None, None).0 as i32;
                let pos = SendMessageW(self.list, LB_GETTOPINDEX, None, None).0 as i32;
                let page = ((self.list_rc.bottom - self.list_rc.top) / self.row_height()).max(1);
                (pos, page, count)
            }
            Ctl::Answer => {
                let count = SendMessageW(self.answer, EM_GETLINECOUNT, None, None).0 as i32;
                let pos = SendMessageW(self.answer, EM_GETFIRSTVISIBLELINE, None, None).0 as i32;
                let page = ((self.answer_rc.bottom - self.answer_rc.top - self.px(6)) / self.line_h).max(1);
                (pos, page, count)
            }
        }
    }

    unsafe fn scroll_to(&self, c: Ctl, pos: i32) {
        let (cur, page, count) = self.scroll_info(c);
        let pos = pos.clamp(0, (count - page).max(0));
        if pos == cur {
            return;
        }
        match c {
            Ctl::List => {
                SendMessageW(self.list, LB_SETTOPINDEX, Some(WPARAM(pos as usize)), None);
            }
            Ctl::Answer => {
                SendMessageW(self.answer, EM_LINESCROLL, Some(WPARAM(0)), Some(LPARAM((pos - cur) as isize)));
            }
        }
        self.invalidate_bar(c);
    }

    unsafe fn scroll_by(&self, c: Ctl, lines: i32) {
        let (cur, _, _) = self.scroll_info(c);
        self.scroll_to(c, cur + lines);
    }

    fn bar_rect(&self, c: Ctl) -> RECT {
        let rc = match c {
            Ctl::List => self.list_rc,
            Ctl::Answer => self.answer_rc,
        };
        let u = self.unit();
        RECT { left: rc.right + u, top: rc.top, right: rc.right + u + self.bar_w(), bottom: rc.bottom }
    }

    /// The thumb, or None when the content fits (no bar is drawn then).
    unsafe fn thumb_rect(&self, c: Ctl) -> Option<RECT> {
        let (pos, page, count) = self.scroll_info(c);
        if count <= page {
            return None;
        }
        let u = self.unit();
        let bar = self.bar_rect(c);
        let inner_h = bar.bottom - bar.top - 2 * u;
        let th = (inner_h * page / count).max(3 * u).min(inner_h);
        let ty = bar.top + u + (inner_h - th) * pos / (count - page);
        Some(RECT { left: bar.left + u, top: ty, right: bar.right - u, bottom: ty + th })
    }

    unsafe fn invalidate_bar(&self, c: Ctl) {
        let r = self.bar_rect(c);
        let _ = InvalidateRect(Some(self.hwnd), Some(&r), true);
    }

    unsafe fn paint_bar(&self, hdc: HDC, c: Ctl) {
        if !self.ctl_visible(c) {
            return;
        }
        let Some(thumb) = self.thumb_rect(c) else { return };
        let bar = self.bar_rect(c);
        FillRect(hdc, &bar, self.brush_edit);
        self.frame_rect(hdc, bar);
        FillRect(hdc, &thumb, self.brush_accent);
        // A dark notch across the middle keeps the thumb readable as a grip.
        let u = self.unit();
        if thumb.bottom - thumb.top >= 5 * u {
            let mid = (thumb.top + thumb.bottom) / 2 / u * u;
            let notch = RECT { left: thumb.left, top: mid, right: thumb.right, bottom: mid + u };
            FillRect(hdc, &notch, self.brush_edit);
        }
    }

    /// One-unit orange frame just inside `r`.
    unsafe fn frame_rect(&self, hdc: HDC, r: RECT) {
        let u = self.unit();
        let b = self.brush_accent;
        FillRect(hdc, &RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + u }, b);
        FillRect(hdc, &RECT { left: r.left, top: r.bottom - u, right: r.right, bottom: r.bottom }, b);
        FillRect(hdc, &RECT { left: r.left, top: r.top, right: r.left + u, bottom: r.bottom }, b);
        FillRect(hdc, &RECT { left: r.right - u, top: r.top, right: r.right, bottom: r.bottom }, b);
    }

    fn grip_rect(&self) -> RECT {
        let (u, g) = (self.unit(), self.grip_size());
        RECT { left: self.cur_w - 2 * u - g, top: self.cur_h - 2 * u - g, right: self.cur_w - 2 * u, bottom: self.cur_h - 2 * u }
    }

    unsafe fn paint(&self, hdc: HDC) {
        let u = self.unit();
        let mut r = RECT::default();
        let _ = GetClientRect(self.hwnd, &mut r);
        self.frame_rect(hdc, r);
        // Corner grip: three diagonal runs of unit blocks, like a classic resize handle.
        let g = self.grip_rect();
        for gy in 0..6 {
            for gx in 0..6 {
                let d = gx + gy;
                if d >= 5 && d % 2 == 1 {
                    let cell = RECT { left: g.left + gx * u, top: g.top + gy * u, right: g.left + (gx + 1) * u, bottom: g.top + (gy + 1) * u };
                    FillRect(hdc, &cell, self.brush_accent);
                }
            }
        }
        self.paint_bar(hdc, Ctl::List);
        self.paint_bar(hdc, Ctl::Answer);
        if self.answer_shown && self.thinking {
            // The model is thinking: the answer box's place holds a ticking pixel ellipsis.
            FillRect(hdc, &self.answer_rc, self.brush_edit);
            let lit = self.think_frame % 4;
            for i in 0..3 {
                let x = self.answer_rc.left + 3 * u + i * 4 * u;
                let y = self.answer_rc.top + 3 * u;
                let dot = RECT { left: x, top: y, right: x + 2 * u, bottom: y + 2 * u };
                FillRect(hdc, &dot, if (i as u32) < lit { self.brush_accent } else { self.brush_sel });
            }
        }
    }

    unsafe fn set_thinking(&mut self, on: bool) {
        if self.thinking == on {
            return;
        }
        self.thinking = on;
        self.think_frame = 0;
        if on {
            SetTimer(Some(self.hwnd), TIMER_THINK, 250, None);
        } else {
            let _ = KillTimer(Some(self.hwnd), TIMER_THINK);
        }
        self.layout_children();
        let _ = InvalidateRect(Some(self.hwnd), None, true);
    }

    // ---- mouse on the panel itself: grip and bars ----

    unsafe fn mouse_down(&mut self, pt: POINT) {
        if PtInRect(&self.grip_rect(), pt).as_bool() {
            self.drag = Drag::Corner(self.cur_w - pt.x, self.cur_h - pt.y);
            SetCapture(self.hwnd);
            return;
        }
        for c in [Ctl::List, Ctl::Answer] {
            if !self.ctl_visible(c) || !PtInRect(&self.bar_rect(c), pt).as_bool() {
                continue;
            }
            let Some(t) = self.thumb_rect(c) else { return };
            if PtInRect(&t, pt).as_bool() {
                self.drag = Drag::Thumb(c, pt.y - t.top);
                SetCapture(self.hwnd);
            } else {
                let (_, page, _) = self.scroll_info(c);
                self.scroll_by(c, if pt.y < t.top { -page } else { page });
            }
            return;
        }
    }

    unsafe fn mouse_move(&mut self, pt: POINT) {
        match self.drag {
            Drag::Corner(dx, dy) => {
                let w = self.snap(pt.x + dx).max(self.px(MIN_W));
                let h = self.snap(pt.y + dy).max(self.min_h());
                if w != self.cur_w || h != self.cur_h {
                    self.stop_anim();
                    self.apply_size(w, h);
                }
                self.user_w = (w as f32 / self.scale).round() as i32;
                self.user_h = (h as f32 / self.scale).round() as i32;
            }
            Drag::Thumb(c, off) => {
                let (_, page, count) = self.scroll_info(c);
                let Some(t) = self.thumb_rect(c) else { return };
                let u = self.unit();
                let bar = self.bar_rect(c);
                let span = (bar.bottom - bar.top - 2 * u) - (t.bottom - t.top);
                if span > 0 {
                    let pos = ((pt.y - off - bar.top - u) * (count - page) + span / 2) / span;
                    self.scroll_to(c, pos);
                }
            }
            Drag::None => {}
        }
    }

    unsafe fn mouse_up(&mut self) {
        if let Drag::Corner(..) = self.drag {
            let _ = PostMessageW(Some(self.dot), WM_PANEL_RESIZED, WPARAM(self.user_w as usize), LPARAM(self.user_h as isize));
        }
        self.drag = Drag::None;
        let _ = ReleaseCapture();
    }

    // ---- ask mode ----

    /// Everything after a leading `?` is a question for ask mode.
    fn question_of(q: &str) -> Option<&str> {
        q.trim_start().strip_prefix('?').map(str::trim)
    }

    unsafe fn cancel_job(&mut self) {
        if let Some(j) = self.job.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        self.set_thinking(false);
    }

    unsafe fn start_ask(&mut self) {
        let q = self.query_text();
        let Some(question) = Self::question_of(&q).filter(|s| !s.is_empty()) else { return };
        self.cancel_job();
        self.answer_text.clear();
        let _ = SetWindowTextW(self.answer, w!(""));
        self.answer_shown = true;
        self.set_thinking(true);
        let id = self.next_job;
        self.next_job += 1;
        self.set_status("thinking…");
        self.job = Some(self.ask.ask(question.to_string(), self.hwnd, id));
        self.fit(true);
        let _ = PostMessageW(Some(self.dot), WM_ASK_STARTED, WPARAM(0), LPARAM(0));
    }

    unsafe fn set_status(&mut self, text: &str) {
        self.status_text = text.to_string();
        let _ = SetWindowTextW(self.status, PCWSTR(wide(text).as_ptr()));
    }

    unsafe fn on_ask_message(&mut self, msg: u32, id: u64, text: String) {
        if self.job.as_ref().map(|j| j.id) != Some(id) {
            return; // from a cancelled/older job
        }
        match msg {
            WM_ASK_TOKEN => {
                self.set_thinking(false);
                self.answer_text.push_str(&text);
                let _ = SetWindowTextW(self.answer, PCWSTR(wide(&self.answer_text).as_ptr()));
                let len = self.answer_text.encode_utf16().count();
                SendMessageW(self.answer, EM_SETSEL, Some(WPARAM(len)), Some(LPARAM(len as isize)));
                SendMessageW(self.answer, EM_SCROLLCARET, None, None);
            }
            WM_ASK_STATUS => self.set_status(&text),
            WM_ASK_DONE => {
                self.set_thinking(false);
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
        self.fit(true);
    }

    // ---- open / close ----

    /// Show beside the dot (to the right, or left/above if that runs off-screen).
    /// A previous view (non-empty query or an answer) comes back exactly as it was.
    pub unsafe fn show(hwnd: HWND, dot_rect: RECT) {
        let Some(s) = state(hwnd) else { return };
        s.layout_fonts();
        s.restored = !s.query_text().trim().is_empty() || s.answer_shown;
        if s.restored {
            s.fit(false);
        } else {
            s.refresh();
        }
        let (w, h) = (s.cur_w, s.cur_h);

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
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(s.edit));
        let len = GetWindowTextLengthW(s.edit) as usize;
        SendMessageW(s.edit, EM_SETSEL, Some(WPARAM(len)), Some(LPARAM(len as isize)));
    }

    /// Hide, keeping the view (and any running generation) for the next open.
    pub unsafe fn hide(hwnd: HWND) {
        if IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
            if let Some(s) = state(hwnd) {
                s.stop_anim();
                s.drag = Drag::None;
                let _ = PostMessageW(Some(s.dot), WM_SEARCH_CLOSED, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Forget the previous view: empty query, recent items, no answer.
    unsafe fn clear_view(&mut self) {
        self.cancel_job();
        self.answer_text.clear();
        let _ = SetWindowTextW(self.answer, w!(""));
        self.answer_shown = false;
        self.restored = false;
        let _ = SetWindowTextW(self.edit, w!(""));
        let _ = KillTimer(Some(self.hwnd), TIMER_DEBOUNCE);
        self.refresh();
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
            self.answer_shown = false;
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
        if self.job.is_none() {
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
        self.fit(true);
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
        self.invalidate_bar(Ctl::List);
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

fn lparam_point(lparam: LPARAM) -> POINT {
    POINT { x: (lparam.0 & 0xFFFF) as u16 as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32 }
}

unsafe extern "system" fn edit_subclass(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, refdata: usize) -> LRESULT {
    let parent = HWND(refdata as *mut _);
    match msg {
        WM_KEYDOWN => {
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            let key = VIRTUAL_KEY(wparam.0 as u16);
            if let Some(s) = state(parent) {
                let page = s.scroll_info(Ctl::List).1;
                match key {
                    VK_ESCAPE if s.job.is_some() => { s.cancel_job(); s.set_status("stopped"); s.fit(true); return LRESULT(0); }
                    // Esc on a restored view forgets it; otherwise the panel just closes (and keeps the view).
                    VK_ESCAPE if s.restored => { s.clear_view(); SearchWin::hide(parent); return LRESULT(0); }
                    VK_ESCAPE => { SearchWin::hide(parent); return LRESULT(0); }
                    VK_DOWN => { s.move_sel(1); return LRESULT(0); }
                    VK_UP => { s.move_sel(-1); return LRESULT(0); }
                    VK_NEXT => { s.move_sel(page); return LRESULT(0); }
                    VK_PRIOR => { s.move_sel(-page); return LRESULT(0); }
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
        WM_MOUSEWHEEL => return SendMessageW(parent, msg, Some(wparam), Some(lparam)),
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// The list and the answer box: the wheel goes to the panel (which scrolls them and repaints
/// the bar), and anything that may have moved their content redraws the bar afterwards.
unsafe extern "system" fn scroll_subclass(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, id: usize, refdata: usize) -> LRESULT {
    let parent = HWND(refdata as *mut _);
    if msg == WM_MOUSEWHEEL {
        return SendMessageW(parent, msg, Some(wparam), Some(lparam));
    }
    let r = DefSubclassProc(hwnd, msg, wparam, lparam);
    let moved = matches!(
        msg,
        WM_KEYDOWN | WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_VSCROLL | WM_SETTEXT | WM_SIZE
            | LB_SETTOPINDEX | LB_SETCURSEL | LB_RESETCONTENT | LB_ADDSTRING
            | EM_LINESCROLL | EM_SCROLLCARET | EM_SETSEL
    );
    if moved {
        if let Some(s) = state(parent) {
            s.invalidate_bar(if id == SC_LIST { Ctl::List } else { Ctl::Answer });
        }
    }
    r
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
            // No WS_VSCROLL on the list or the answer box: the panel paints their bars.
            s.list = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("LISTBOX"), w!(""),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE((LBS_OWNERDRAWFIXED | LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32),
                0, 0, 10, 10, Some(hwnd), Some(HMENU(ID_LIST as *mut _)), None, None,
            ).unwrap_or_default();
            s.answer = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("EDIT"), w!(""),
                WS_CHILD | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32),
                0, 0, 10, 10, Some(hwnd), Some(HMENU(ID_ANSWER as *mut _)), None, None,
            ).unwrap_or_default();
            s.status = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("STATIC"), w!(""),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_LEFTNOWORDWRAP.0 | SS_ENDELLIPSIS.0),
                0, 0, 10, 10, Some(hwnd), None, None, None,
            ).unwrap_or_default();
            let _ = SetWindowSubclass(s.edit, Some(edit_subclass), 1, hwnd.0 as usize);
            let _ = SetWindowSubclass(s.list, Some(scroll_subclass), SC_LIST, hwnd.0 as usize);
            let _ = SetWindowSubclass(s.answer, Some(scroll_subclass), SC_ANSWER, hwnd.0 as usize);
            s.layout_fonts();
            s.fit(false);
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            let hdc = HDC(wparam.0 as *mut _);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            if let Some(s) = state(hwnd) {
                FillRect(hdc, &r, s.brush_bg);
            }
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if let Some(s) = state(hwnd) {
                s.paint(hdc);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
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
                    s.restored = false; // a new query supersedes the restored view
                    SetTimer(Some(hwnd), TIMER_DEBOUNCE, 90, None);
                } else if id == ID_LIST && code == LBN_DBLCLK {
                    s.open_selected(false);
                } else if id == ID_LIST && code == LBN_SELCHANGE {
                    let _ = SetFocus(Some(s.edit));
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if let Some(s) = state(hwnd) {
                match wparam.0 {
                    TIMER_DEBOUNCE => {
                        let _ = KillTimer(Some(hwnd), TIMER_DEBOUNCE);
                        s.refresh();
                    }
                    TIMER_RESIZE => s.tick_resize(),
                    TIMER_THINK => {
                        s.think_frame = s.think_frame.wrapping_add(1);
                        let r = s.answer_rc;
                        let _ = InvalidateRect(Some(hwnd), Some(&r), true);
                    }
                    _ => {}
                }
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
        WM_LBUTTONDOWN => {
            if let Some(s) = state(hwnd) {
                s.mouse_down(lparam_point(lparam));
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(s) = state(hwnd) {
                s.mouse_move(lparam_point(lparam));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(s) = state(hwnd) {
                s.mouse_up();
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            if let Some(s) = state(hwnd) {
                s.drag = Drag::None;
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(s) = state(hwnd) {
                // Screen coords here (also when forwarded by a child): over the answer box or
                // its bar scrolls the answer, anywhere else the list — 3 lines a notch, like native.
                let mut pt = lparam_point(lparam);
                let _ = ScreenToClient(hwnd, &mut pt);
                let over_answer = s.ctl_visible(Ctl::Answer) && (PtInRect(&s.answer_rc, pt).as_bool() || PtInRect(&s.bar_rect(Ctl::Answer), pt).as_bool());
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                s.scroll_by(if over_answer { Ctl::Answer } else { Ctl::List }, -(delta / 120) * 3);
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                if let Some(s) = state(hwnd) {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let _ = ScreenToClient(hwnd, &mut pt);
                    if PtInRect(&s.grip_rect(), pt).as_bool() {
                        SetCursor(LoadCursorW(None, IDC_SIZENWSE).ok());
                        return LRESULT(1);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
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
