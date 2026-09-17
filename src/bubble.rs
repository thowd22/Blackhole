//! Pixel-art speech bubble anchored to the dot. One component, two uses:
//! the first-run tutorial and notifications (ingest problems, MCP messages).
//!
//! Drawn into a 32-bit DIB and shown with UpdateLayeredWindow. GDI text
//! zeroes the alpha channel where it draws, so the body mask is re-applied
//! after the text pass.

use crate::util::wide;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE as WSIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the dot when a bubble is clicked (wparam: 1 if the × was hit, 0 for the body).
pub const WM_BUBBLE_CLICKED: u32 = 0x8030;
/// Posted to the dot when a bubble timed out.
pub const WM_BUBBLE_EXPIRED: u32 = 0x8031;

const TIMER_DISMISS: usize = 1;
/// Steps the fade-in / fade-out (§1.3 pace rule: ~150 ms, stepped).
const TIMER_FADE: usize = 2;
const FADE_STEPS: i32 = 4;
const FADE_STEP_MS: u32 = 40;
const MAX_TEXT_UNITS: i32 = 130; // wrap width in sprite units (× pixel scale)
/// A bubble never grows past this many lines; longer texts (an agent dumping a
/// report through MCP `notify`) get the panel's pixel scrollbar instead.
const MAX_LINES: i32 = 12;
/// Wheel notch = this many lines, like the panel.
const WHEEL_LINES: i32 = 3;

const TEXT: COLORREF = COLORREF(0x00F0E8FF); // 0x00BBGGRR

pub struct Bubble {
    hwnd: HWND,
    dot: HWND,
    text: String,
    #[allow(dead_code)]
    sticky: bool,
    unit: i32,
    /// Where the tail points (screen coords) and which way the bubble sits.
    anchor: RECT,
    dib: HBITMAP,
    dc: HDC,
    font: HFONT,
    /// The × close box, in window coordinates.
    close: RECT,
    /// Where the rendered bubble sits when fully shown, and whether it is above the dot.
    pos: POINT,
    size: WSIZE,
    above: bool,
    /// Fade progress 0..=FADE_STEPS (FADE_STEPS = fully shown) and its direction.
    fade: i32,
    fading_out: bool,
    /// Overflow: total wrapped lines, how many are shown, the first one shown, and
    /// the height of one line. `lines > visible` is what puts the scrollbar up.
    lines: i32,
    visible_lines: i32,
    scroll: i32,
    line_h: i32,
    /// The scrollbar, in window coordinates (empty when the text fits).
    bar: RECT,
    /// The dismiss timeout, restarted whenever the text is scrolled.
    timeout: Option<u32>,
}

/// While a scrollable bubble is up, the wheel has to reach it even though the
/// bubble never takes focus (it is a WS_EX_NOACTIVATE tool window). A low-level
/// mouse hook, alive only for as long as such a bubble is on screen, hands wheel
/// notches over the bubble to it and swallows them so nothing behind scrolls.
static HOOK: AtomicIsize = AtomicIsize::new(0);
static HOOK_WND: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn wheel_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 as u32 == WM_MOUSEWHEEL {
        let hwnd = HWND(HOOK_WND.load(Ordering::Relaxed) as *mut _);
        if !hwnd.is_invalid() && IsWindowVisible(hwnd).as_bool() {
            let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            if PtInRect(&r, ms.pt).as_bool() {
                let delta = (ms.mouseData >> 16) as i16 as i32;
                let _ = PostMessageW(Some(hwnd), WM_MOUSEWHEEL, WPARAM((delta as u32 as usize) << 16), LPARAM(0));
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Install the wheel hook while `on`, remove it otherwise. Idempotent.
unsafe fn set_wheel_hook(hwnd: HWND, on: bool) {
    let cur = HOOK.load(Ordering::Relaxed);
    if on {
        HOOK_WND.store(hwnd.0 as isize, Ordering::Relaxed);
        if cur == 0 {
            if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(wheel_hook), None, 0) {
                HOOK.store(h.0 as isize, Ordering::Relaxed);
            }
        }
    } else if cur != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(cur as *mut _));
        HOOK.store(0, Ordering::Relaxed);
    }
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut Bubble> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bubble).as_mut()
}

impl Bubble {
    pub fn create(dot: HWND) -> HWND {
        unsafe {
            let class = w!("BlackholeBubble");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let b = Box::new(Bubble {
                hwnd: HWND::default(),
                dot,
                text: String::new(),
                sticky: false,
                unit: 2,
                anchor: RECT::default(),
                dib: HBITMAP::default(),
                dc: HDC::default(),
                font: HFONT::default(),
                close: RECT::default(),
                pos: POINT::default(),
                size: WSIZE::default(),
                above: true,
                fade: 0,
                fading_out: false,
                lines: 1,
                visible_lines: 1,
                scroll: 0,
                line_h: 1,
                bar: RECT::default(),
                timeout: None,
            });
            let ptr = Box::into_raw(b);
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!("Blackhole"),
                WS_POPUP,
                0, 0, 10, 10,
                None, None, None,
                Some(ptr as *const _),
            )
            .unwrap_or_default()
        }
    }

    /// Show `text` pointing at `anchor` (the dot's screen rect). `unit` is the
    /// dot's pixel scale so the bubble's pixels match. `timeout_ms == None`
    /// keeps it up until clicked or hidden.
    pub unsafe fn show(hwnd: HWND, text: &str, anchor: RECT, unit: i32, timeout_ms: Option<u32>) {
        let Some(b) = state(hwnd) else { return };
        b.text = text.to_string();
        b.anchor = anchor;
        b.unit = unit.max(1);
        b.sticky = timeout_ms.is_none();
        // Pop in from the dot: a few alpha steps while it slides into place. A
        // bubble caught mid fade-out just turns around.
        if !IsWindowVisible(hwnd).as_bool() {
            b.fade = 0;
        }
        b.fading_out = false;
        b.scroll = 0;
        b.timeout = timeout_ms;
        b.render_and_place();
        let scrollable = b.lines > b.visible_lines;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), TIMER_FADE, FADE_STEP_MS, None);
        let _ = KillTimer(Some(hwnd), TIMER_DISMISS);
        if let Some(ms) = timeout_ms {
            SetTimer(Some(hwnd), TIMER_DISMISS, ms, None);
        }
        set_wheel_hook(hwnd, scrollable);
    }

    /// The dot's colours changed: redraw with the new border if one is on screen.
    pub unsafe fn recolour(hwnd: HWND) {
        if let Some(b) = state(hwnd) {
            if IsWindowVisible(hwnd).as_bool() {
                b.render_and_place();
            }
        }
    }

    /// Scroll a long text by `lines`, keeping it on screen a while longer.
    unsafe fn scroll_by(&mut self, lines: i32) {
        let max = (self.lines - self.visible_lines).max(0);
        let next = (self.scroll + lines).clamp(0, max);
        if next == self.scroll {
            return;
        }
        self.scroll = next;
        self.render_and_place();
        if let Some(ms) = self.timeout {
            SetTimer(Some(self.hwnd), TIMER_DISMISS, ms, None);
        }
    }

    /// Fade out, then hide. Reads as hidden (`is_visible`) straight away.
    pub unsafe fn hide(hwnd: HWND) {
        let _ = KillTimer(Some(hwnd), TIMER_DISMISS);
        let Some(b) = state(hwnd) else { return };
        if !IsWindowVisible(hwnd).as_bool() || b.fading_out {
            return;
        }
        b.fading_out = true;
        SetTimer(Some(hwnd), TIMER_FADE, FADE_STEP_MS, None);
    }

    pub unsafe fn is_visible(hwnd: HWND) -> bool {
        IsWindowVisible(hwnd).as_bool() && state(hwnd).is_some_and(|b| !b.fading_out)
    }

    /// One fade step; hides the window once a fade-out completes.
    unsafe fn fade_step(&mut self) {
        self.fade = if self.fading_out { self.fade - 1 } else { self.fade + 1 };
        if self.fade <= 0 {
            self.fade = 0;
            self.fading_out = false;
            let _ = KillTimer(Some(self.hwnd), TIMER_FADE);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            set_wheel_hook(self.hwnd, false);
            return;
        }
        if self.fade >= FADE_STEPS {
            self.fade = FADE_STEPS;
            let _ = KillTimer(Some(self.hwnd), TIMER_FADE);
        }
        self.present();
    }

    /// Push the rendered DIB to the screen at the current fade step: alpha
    /// rises in FADE_STEPS steps while the bubble slides one unit per step
    /// away from the dot into its final place.
    unsafe fn present(&self) {
        let remaining = FADE_STEPS - self.fade;
        let slide = remaining * self.unit * if self.above { 1 } else { -1 };
        let pos = POINT { x: self.pos.x, y: self.pos.y + slide };
        let alpha = (255 * self.fade / FADE_STEPS) as u8;
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), pos.x, pos.y, self.size.cx, self.size.cy, SWP_NOACTIVATE);
        let _ = UpdateLayeredWindow(self.hwnd, None, Some(&pos), Some(&self.size), Some(self.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
    }

    /// The dot moved: follow it.
    pub unsafe fn follow(hwnd: HWND, anchor: RECT) {
        if let Some(b) = state(hwnd) {
            if IsWindowVisible(hwnd).as_bool() {
                b.anchor = anchor;
                b.render_and_place();
            }
        }
    }

    unsafe fn ensure_font(&mut self) {
        if !self.font.is_invalid() {
            let _ = DeleteObject(self.font.into());
        }
        self.font = CreateFontW(
            -(7 * self.unit), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, NONANTIALIASED_QUALITY,
            (FIXED_PITCH.0 | FF_MODERN.0) as u32, w!("Consolas"),
        );
    }

    unsafe fn render_and_place(&mut self) {
        let u = self.unit;
        self.ensure_font();

        // Measure the wrapped text, and one line of it, so an overflowing text can be
        // capped at MAX_LINES and scrolled instead of growing off the screen.
        let screen = GetDC(None);
        let old_font = SelectObject(screen, self.font.into());
        let mut text = wide(&self.text);
        let mut measure = RECT { left: 0, top: 0, right: MAX_TEXT_UNITS * u, bottom: 0 };
        DrawTextW(screen, &mut text, &mut measure, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
        let mut one = wide("Ag");
        let mut line = RECT { left: 0, top: 0, right: MAX_TEXT_UNITS * u, bottom: 0 };
        DrawTextW(screen, &mut one, &mut line, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(screen, old_font);
        let _ = ReleaseDC(None, screen);
        let text_w = measure.right - measure.left;
        let full_h = measure.bottom - measure.top;
        self.line_h = (line.bottom - line.top).max(1);
        self.lines = ((full_h + self.line_h / 2) / self.line_h).max(1);
        self.visible_lines = self.lines.min(MAX_LINES);
        self.scroll = self.scroll.clamp(0, (self.lines - self.visible_lines).max(0));
        let text_h = self.visible_lines * self.line_h;
        let scrollable = self.lines > self.visible_lines;

        let pad = 4 * u;
        let border = u;
        let tail_units = 4;
        let tail_h = tail_units * u;
        let close_units = 5; // the × glyph is 5×5 units
        // The scrollbar gets its own column on the far right, the same 3-unit bar the
        // panel draws; the × moves left of it.
        let bar_col = if scrollable { 4 * u } else { 0 };
        let body_w = text_w + 2 * pad + 2 * border + (close_units + 2) * u + bar_col;
        let body_h = text_h + 2 * pad + 2 * border;

        // Bubble above the dot if there is room, else below (tail flips).
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = MonitorFromPoint(POINT { x: self.anchor.left, y: self.anchor.top }, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(mon, &mut mi);
        let work = mi.rcWork;
        let dot_cx = (self.anchor.left + self.anchor.right) / 2;
        let above = self.anchor.top - (body_h + tail_h) - u >= work.top;
        let total_h = body_h + tail_h;

        let mut x = dot_cx - body_w / 2;
        x = x.clamp(work.left, (work.right - body_w).max(work.left));
        let y = if above { self.anchor.top - u - total_h } else { self.anchor.bottom + u };
        // Tail x within the bubble, aimed at the dot centre.
        let tail_cx = (dot_cx - x).clamp(tail_units * u + border + u, body_w - tail_units * u - border - u);

        // Paint. The dot's palette owns these: the border is its accent, the body a
        // very dark version of its coolest ring colour.
        let border_col = crate::sprite::accent();
        let bg = crate::sprite::shade();
        let w = body_w;
        let h = total_h;
        let mut buf = vec![0u32; (w * h) as usize];
        let body_top = if above { 0 } else { tail_h };
        let body_bottom = body_top + body_h; // exclusive
        let in_body = |px: i32, py: i32| -> bool {
            if py >= body_top && py < body_bottom {
                // Notched corners (one unit) for the pixel look.
                let cx = px < u || px >= w - u;
                let cy = py < body_top + u || py >= body_bottom - u;
                return !(cx && cy);
            }
            // Tail: a staircase of rows narrowing towards the dot.
            let (row, dir_ok) = if above {
                (py - body_bottom, py >= body_bottom)
            } else {
                (body_top - 1 - py, py < body_top)
            };
            if !dir_ok || row < 0 || row >= tail_h {
                return false;
            }
            let step = row / u; // 0 nearest the body
            let half = (tail_units - step) * u;
            px >= tail_cx - half && px < tail_cx + half
        };
        let on_border = |px: i32, py: i32| -> bool {
            // Border = body pixels that have a non-body neighbour within `border` px.
            for dy in -border..=border {
                for dx in -border..=border {
                    if !in_body(px + dx, py + dy) {
                        return true;
                    }
                }
            }
            false
        };
        for py in 0..h {
            for px in 0..w {
                if in_body(px, py) {
                    let c = if on_border(px, py) { border_col } else { bg };
                    buf[(py * w + px) as usize] = 0xFF00_0000 | c;
                }
            }
        }
        // Pixel scrollbar in the right-hand column: same shape as the panel's
        // (one-unit accent frame, accent thumb with a dark grip notch).
        self.bar = RECT::default();
        if scrollable {
            let text_top = body_top + border + pad;
            let bar = RECT { left: w - border - u - 3 * u, top: text_top, right: w - border - u, bottom: text_top + text_h };
            let fill = |buf: &mut Vec<u32>, r: RECT, c: u32| {
                for py in r.top.max(0)..r.bottom.min(h) {
                    for px in r.left.max(0)..r.right.min(w) {
                        buf[(py * w + px) as usize] = 0xFF00_0000 | c;
                    }
                }
            };
            fill(&mut buf, bar, bg);
            fill(&mut buf, RECT { left: bar.left, top: bar.top, right: bar.right, bottom: bar.top + u }, border_col);
            fill(&mut buf, RECT { left: bar.left, top: bar.bottom - u, right: bar.right, bottom: bar.bottom }, border_col);
            fill(&mut buf, RECT { left: bar.left, top: bar.top, right: bar.left + u, bottom: bar.bottom }, border_col);
            fill(&mut buf, RECT { left: bar.right - u, top: bar.top, right: bar.right, bottom: bar.bottom }, border_col);
            let inner = bar.bottom - bar.top - 2 * u;
            let th = (inner * self.visible_lines / self.lines).max(3 * u).min(inner);
            let ty = bar.top + u + (inner - th) * self.scroll / (self.lines - self.visible_lines);
            let thumb = RECT { left: bar.left + u, top: ty, right: bar.right - u, bottom: ty + th };
            fill(&mut buf, thumb, border_col);
            if th >= 5 * u {
                let mid = (thumb.top + thumb.bottom) / 2 / u * u;
                fill(&mut buf, RECT { left: thumb.left, top: mid, right: thumb.right, bottom: mid + u }, bg);
            }
            self.bar = bar;
        }
        // × in the top-right corner, border-coloured: two 1-unit diagonals.
        let cx0 = w - border - 2 * u - close_units * u - bar_col;
        let cy0 = body_top + border + 2 * u;
        for i in 0..close_units {
            for (dx, dy) in [(i, i), (close_units - 1 - i, i)] {
                for yy in 0..u {
                    for xx in 0..u {
                        let px = cx0 + dx * u + xx;
                        let py = cy0 + dy * u + yy;
                        buf[(py * w + px) as usize] = 0xFF00_0000 | border_col;
                    }
                }
            }
        }
        // Generous hit box around the glyph.
        self.close = RECT { left: cx0 - 2 * u, top: cy0 - 2 * u, right: cx0 + close_units * u + 2 * u, bottom: cy0 + close_units * u + 2 * u };

        // DIB + GDI text.
        if !self.dib.is_invalid() {
            let _ = DeleteObject(self.dib.into());
            let _ = DeleteDC(self.dc);
        }
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        self.dib = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        self.dc = CreateCompatibleDC(None);
        SelectObject(self.dc, self.dib.into());
        let px = bits as *mut u32;
        std::ptr::copy_nonoverlapping(buf.as_ptr(), px, buf.len());

        let old = SelectObject(self.dc, self.font.into());
        SetBkMode(self.dc, TRANSPARENT);
        SetTextColor(self.dc, TEXT);
        let top = body_top + border + pad;
        // Scrolling moves the whole text up by whole lines; the clip box keeps the
        // lines above and below it out of the bubble.
        let mut tr = RECT {
            left: border + pad,
            top: top - self.scroll * self.line_h,
            right: border + pad + text_w,
            bottom: top + (self.lines - self.scroll) * self.line_h,
        };
        IntersectClipRect(self.dc, border + pad, top, border + pad + text_w, top + text_h);
        DrawTextW(self.dc, &mut text, &mut tr, DT_WORDBREAK | DT_NOPREFIX);
        SelectClipRgn(self.dc, None);
        SelectObject(self.dc, old);
        let _ = GdiFlush();

        // Restore alpha: opaque inside the body, clear outside.
        let out = std::slice::from_raw_parts_mut(px, buf.len());
        for py in 0..h {
            for pxx in 0..w {
                let i = (py * w + pxx) as usize;
                out[i] = if in_body(pxx, py) { 0xFF00_0000 | (out[i] & 0x00FF_FFFF) } else { 0 };
            }
        }

        self.pos = POINT { x, y };
        self.size = WSIZE { cx: w, cy: h };
        self.above = above;
        self.present();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut Bubble;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            (*ptr).hwnd = hwnd;
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(b) = state(hwnd).filter(|b| !b.fading_out) {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                // A click on the scrollbar pages the text rather than dismissing.
                let bar = b.bar;
                if bar.right > bar.left && x >= bar.left - 2 && x < bar.right + 2 && y >= bar.top && y < bar.bottom {
                    let page = b.visible_lines.max(1);
                    let mid = b.bar.top + (b.bar.bottom - b.bar.top) / 2;
                    b.scroll_by(if y < mid { -page } else { page });
                    return LRESULT(0);
                }
                let c = b.close;
                let on_close = x >= c.left && x < c.right && y >= c.top && y < c.bottom;
                let dot = b.dot;
                Bubble::hide(hwnd);
                let _ = PostMessageW(Some(dot), WM_BUBBLE_CLICKED, WPARAM(on_close as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(b) = state(hwnd).filter(|b| !b.fading_out) {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
                b.scroll_by(-(delta / 120) * WHEEL_LINES);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_DISMISS => {
            if let Some(b) = state(hwnd) {
                let dot = b.dot;
                Bubble::hide(hwnd);
                let _ = PostMessageW(Some(dot), WM_BUBBLE_EXPIRED, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_FADE => {
            if let Some(b) = state(hwnd) {
                b.fade_step();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
