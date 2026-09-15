//! Pixel-art speech bubble anchored to the dot. One component, two uses:
//! the first-run tutorial and notifications (ingest problems, MCP messages).
//!
//! Drawn into a 32-bit DIB and shown with UpdateLayeredWindow. GDI text
//! zeroes the alpha channel where it draws, so the body mask is re-applied
//! after the text pass.

use crate::util::wide;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE as WSIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the dot when a bubble is clicked (wparam: 1 if the × was hit, 0 for the body).
pub const WM_BUBBLE_CLICKED: u32 = 0x8030;
/// Posted to the dot when a bubble timed out.
pub const WM_BUBBLE_EXPIRED: u32 = 0x8031;

const TIMER_DISMISS: usize = 1;
const MAX_TEXT_UNITS: i32 = 130; // wrap width in sprite units (× pixel scale)

const BG: u32 = 0x00_18_0C_1E; // dark violet
const BORDER: u32 = 0x00_FF_A0_40; // orange (0x00RRGGBB)
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
        b.render_and_place();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = KillTimer(Some(hwnd), TIMER_DISMISS);
        if let Some(ms) = timeout_ms {
            SetTimer(Some(hwnd), TIMER_DISMISS, ms, None);
        }
    }

    pub unsafe fn hide(hwnd: HWND) {
        let _ = KillTimer(Some(hwnd), TIMER_DISMISS);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }

    pub unsafe fn is_visible(hwnd: HWND) -> bool {
        IsWindowVisible(hwnd).as_bool()
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

        // Measure the wrapped text.
        let screen = GetDC(None);
        let old_font = SelectObject(screen, self.font.into());
        let mut text = wide(&self.text);
        let mut measure = RECT { left: 0, top: 0, right: MAX_TEXT_UNITS * u, bottom: 0 };
        DrawTextW(screen, &mut text, &mut measure, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
        SelectObject(screen, old_font);
        let _ = ReleaseDC(None, screen);
        let text_w = measure.right - measure.left;
        let text_h = measure.bottom - measure.top;

        let pad = 4 * u;
        let border = u;
        let tail_units = 4;
        let tail_h = tail_units * u;
        let close_units = 5; // the × glyph is 5×5 units
        let body_w = text_w + 2 * pad + 2 * border + (close_units + 2) * u;
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

        // Paint.
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
                    let c = if on_border(px, py) { BORDER } else { BG };
                    buf[(py * w + px) as usize] = 0xFF00_0000 | c;
                }
            }
        }
        // × in the top-right corner, border-coloured: two 1-unit diagonals.
        let cx0 = w - border - 2 * u - close_units * u;
        let cy0 = body_top + border + 2 * u;
        for i in 0..close_units {
            for (dx, dy) in [(i, i), (close_units - 1 - i, i)] {
                for yy in 0..u {
                    for xx in 0..u {
                        let px = cx0 + dx * u + xx;
                        let py = cy0 + dy * u + yy;
                        buf[(py * w + px) as usize] = 0xFF00_0000 | BORDER;
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
        let mut tr = RECT {
            left: border + pad,
            top: body_top + border + pad,
            right: border + pad + text_w,
            bottom: body_top + border + pad + text_h,
        };
        DrawTextW(self.dc, &mut text, &mut tr, DT_WORDBREAK | DT_NOPREFIX);
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

        let pos = POINT { x, y };
        let size = WSIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        let _ = UpdateLayeredWindow(self.hwnd, None, Some(&pos), Some(&size), Some(self.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
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
            if let Some(b) = state(hwnd) {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let c = b.close;
                let on_close = x >= c.left && x < c.right && y >= c.top && y < c.bottom;
                let dot = b.dot;
                Bubble::hide(hwnd);
                let _ = PostMessageW(Some(dot), WM_BUBBLE_CLICKED, WPARAM(on_close as usize), LPARAM(0));
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
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
