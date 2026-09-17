//! Drag-region screenshot tool (FEATURES §3.6). Ctrl+Shift+S or the menu grabs
//! the whole virtual screen into a bitmap *before* anything is shown, then puts
//! up an opaque topmost window painted with a dimmed copy of it. Dragging
//! un-dims the chosen region behind a 1-unit orange border with a live size
//! label; releasing crops that region out of the pristine capture, writes a PNG
//! under `<data dir>\captures\` and feeds it to the ingest worker like a
//! dropped file. Capturing first means the overlay can never photograph itself
//! and nothing flickers when it opens. Esc or a right click cancels.

use crate::drop::WM_DROP_SWALLOW;
use crate::ingest::Input;
use crate::util::wide;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Same palette as the speech bubble so the label reads as one family.
const BG: u32 = 0x00_18_0C_1E; // dark violet
const BORDER: u32 = 0x00_FF_A0_40; // orange (0x00RRGGBB)
const TEXT: COLORREF = COLORREF(0x00F0E8FF); // 0x00BBGGRR
/// Dim factor for everything outside the selection (~40 % darker).
const DIM_NUM: u32 = 6;
const DIM_DEN: u32 = 10;
/// A drag shorter than this in either direction is a slip, not a capture.
const MIN_SIDE: i32 = 2;

/// One overlay at a time; a second hotkey press while it is up does nothing.
static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Overlay {
    hwnd: HWND,
    dot: HWND,
    tx: Sender<Input>,
    /// Pixel scale for the border and label (the dot's unit).
    unit: i32,
    /// Virtual-screen size in physical pixels (client coords == bitmap coords).
    w: i32,
    h: i32,
    /// The pristine capture (0x00RRGGBB) and its dimmed twin.
    shot: Vec<u32>,
    dim: Vec<u32>,
    /// Working surface the window is painted from.
    dib: HBITMAP,
    dc: HDC,
    bits: *mut u32,
    font: HFONT,
    /// Drag anchor in bitmap coordinates; None until the button goes down.
    anchor: Option<POINT>,
    cursor: POINT,
    /// Everything the last frame drew over the dimmed base, to be restored.
    drawn: Option<RECT>,
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut Overlay> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay).as_mut()
}

/// Grab the screen and open the overlay. `unit` is the dot's pixel scale.
pub fn start(dot: HWND, tx: Sender<Input>, unit: i32) {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let origin = POINT { x: GetSystemMetrics(SM_XVIRTUALSCREEN), y: GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);

        // One DIB serves twice: BitBlt the desktop into it, then keep it as the paint surface.
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut raw, None, 0).unwrap_or_default();
        if dib.is_invalid() || raw.is_null() {
            ACTIVE.store(false, Ordering::SeqCst);
            return;
        }
        let dc = CreateCompatibleDC(None);
        SelectObject(dc, dib.into());
        let screen = GetDC(None);
        // CAPTUREBLT: include layered windows (tooltips, the dot itself) like the eye sees them.
        let ok = BitBlt(dc, 0, 0, w, h, Some(screen), origin.x, origin.y, SRCCOPY | CAPTUREBLT).is_ok();
        let _ = ReleaseDC(None, screen);
        let _ = GdiFlush();
        if !ok {
            let _ = DeleteDC(dc);
            let _ = DeleteObject(dib.into());
            ACTIVE.store(false, Ordering::SeqCst);
            return;
        }
        let bits = raw as *mut u32;
        let n = (w * h) as usize;
        let shot: Vec<u32> = std::slice::from_raw_parts(bits, n).to_vec();
        let dim: Vec<u32> = shot.iter().map(|&p| dim_px(p)).collect();
        std::ptr::copy_nonoverlapping(dim.as_ptr(), bits, n);

        let font = CreateFontW(
            -(7 * unit),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            NONANTIALIASED_QUALITY,
            (FIXED_PITCH.0 | FF_MODERN.0) as u32,
            w!("Consolas"),
        );

        let class = w!("BlackholeCapture");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(), ..Default::default() };
        RegisterClassW(&wc);
        let ov = Box::new(Overlay { hwnd: HWND::default(), dot, tx, unit: unit.max(1), w, h, shot, dim, dib, dc, bits, font, anchor: None, cursor: POINT::default(), drawn: None });
        let ptr = Box::into_raw(ov);
        // Not a tool window: it needs the keyboard for Esc, so it activates normally.
        let hwnd = CreateWindowExW(WS_EX_TOPMOST, class, w!("Blackhole screenshot"), WS_POPUP, origin.x, origin.y, w, h, None, None, None, Some(ptr as *const _)).unwrap_or_default();
        if hwnd.is_invalid() {
            drop(Box::from_raw(ptr));
            let _ = DeleteDC(dc);
            let _ = DeleteObject(dib.into());
            ACTIVE.store(false, Ordering::SeqCst);
            return;
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn dim_px(p: u32) -> u32 {
    let c = |v: u32| (v & 0xFF) * DIM_NUM / DIM_DEN;
    c(p >> 16) << 16 | c(p >> 8) << 8 | c(p)
}

fn norm(a: POINT, b: POINT) -> RECT {
    RECT { left: a.x.min(b.x), top: a.y.min(b.y), right: a.x.max(b.x), bottom: a.y.max(b.y) }
}

fn union(a: RECT, b: RECT) -> RECT {
    RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
}

impl Overlay {
    fn clip(&self, r: RECT) -> RECT {
        RECT { left: r.left.clamp(0, self.w), top: r.top.clamp(0, self.h), right: r.right.clamp(0, self.w), bottom: r.bottom.clamp(0, self.h) }
    }

    /// Bitmap coordinates of the mouse from a client-area lparam.
    fn pt(lparam: LPARAM) -> POINT {
        POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 }
    }

    unsafe fn fill(&mut self, r: RECT, colour: u32) {
        let r = self.clip(r);
        for y in r.top..r.bottom {
            let row = std::slice::from_raw_parts_mut(self.bits.add((y * self.w) as usize), self.w as usize);
            row[r.left as usize..r.right as usize].fill(colour);
        }
    }

    /// Copy rows of `src` back into the surface for a rect.
    unsafe fn blit(&mut self, r: RECT, from_shot: bool) {
        let r = self.clip(r);
        let src = if from_shot { &self.shot } else { &self.dim };
        for y in r.top..r.bottom {
            let start = (y * self.w + r.left) as usize;
            let len = (r.right - r.left) as usize;
            std::ptr::copy_nonoverlapping(src.as_ptr().add(start), self.bits.add(start), len);
        }
    }

    /// The selection so far (exclusive right/bottom), if a drag is in progress.
    fn selection(&self) -> Option<RECT> {
        let a = self.anchor?;
        let mut r = norm(a, self.cursor);
        r.right += 1;
        r.bottom += 1;
        Some(self.clip(r))
    }

    /// Redraw the selection (un-dimmed region, border, size label) over the dimmed base.
    unsafe fn refresh(&mut self) {
        let u = self.unit;
        let mut dirty = self.drawn.take();
        if let Some(old) = dirty {
            self.blit(old, false);
        }
        if let Some(sel) = self.selection() {
            // Border sits just outside the selection so the pixels being captured stay visible.
            let outer = RECT { left: sel.left - u, top: sel.top - u, right: sel.right + u, bottom: sel.bottom + u };
            self.fill(outer, BORDER);
            self.blit(sel, true);

            // Label: "412×188" in a bubble-styled box beside the cursor, kept on screen.
            let text = format!("{}\u{d7}{}", sel.right - sel.left, sel.bottom - sel.top);
            let mut wtext = wide(&text);
            let old_font = SelectObject(self.dc, self.font.into());
            let mut m = RECT::default();
            DrawTextW(self.dc, &mut wtext, &mut m, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
            let (tw, th) = (m.right - m.left, m.bottom - m.top);
            let pad = 2 * u;
            let bw = tw + 2 * pad + 2 * u;
            let bh = th + 2 * pad + 2 * u;
            let mut bx = self.cursor.x + 6 * u;
            let mut by = self.cursor.y + 6 * u;
            if bx + bw > self.w {
                bx = self.cursor.x - 6 * u - bw;
            }
            if by + bh > self.h {
                by = self.cursor.y - 6 * u - bh;
            }
            let bx = bx.clamp(0, (self.w - bw).max(0));
            let by = by.clamp(0, (self.h - bh).max(0));
            let label = RECT { left: bx, top: by, right: bx + bw, bottom: by + bh };
            self.fill(label, BORDER);
            self.fill(RECT { left: bx + u, top: by + u, right: bx + bw - u, bottom: by + bh - u }, BG);
            SetBkMode(self.dc, TRANSPARENT);
            SetTextColor(self.dc, TEXT);
            let mut tr = RECT { left: bx + u + pad, top: by + u + pad, right: bx + u + pad + tw, bottom: by + u + pad + th };
            DrawTextW(self.dc, &mut wtext, &mut tr, DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(self.dc, old_font);
            let _ = GdiFlush();

            let now = union(outer, label);
            dirty = Some(dirty.map_or(now, |d| union(d, now)));
            self.drawn = Some(now);
        }
        if let Some(d) = dirty {
            let d = self.clip(d);
            let _ = InvalidateRect(Some(self.hwnd), Some(&d), false);
            let _ = UpdateWindow(self.hwnd);
        }
    }

    unsafe fn paint(&mut self) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(self.hwnd, &mut ps);
        let r = ps.rcPaint;
        let _ = BitBlt(hdc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(self.dc), r.left, r.top, SRCCOPY);
        let _ = EndPaint(self.hwnd, &ps);
    }

    /// Button released: crop, encode and hand over on a worker thread, then close.
    unsafe fn finish(&mut self) {
        let Some(sel) = self.selection() else { return self.close() };
        let (cw, ch) = (sel.right - sel.left, sel.bottom - sel.top);
        if cw < MIN_SIDE || ch < MIN_SIDE {
            return self.close();
        }
        let mut rgb = Vec::with_capacity((cw * ch * 3) as usize);
        for y in sel.top..sel.bottom {
            for &p in &self.shot[(y * self.w + sel.left) as usize..(y * self.w + sel.right) as usize] {
                rgb.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
        }
        // The clipboard gets the same pixels, so a capture can go straight into a chat or
        // editor. Must happen on this (window) thread; the overlay is still alive here.
        if let Err(e) = self.copy_to_clipboard(&sel) {
            crate::util::log(&format!("screenshot: clipboard: {e}"));
        }
        let tx = self.tx.clone();
        let dot = self.dot.0 as usize;
        std::thread::spawn(move || {
            let dot = HWND(dot as *mut _);
            match save_png(&rgb, cw as u32, ch as u32) {
                Ok(path) => {
                    // Swallow first so the dot is already digesting when WM_INGEST_DONE lands.
                    let _ = PostMessageW(Some(dot), WM_DROP_SWALLOW, WPARAM(0), LPARAM(0));
                    let _ = tx.send(Input::Files(vec![path]));
                    notify(dot, format!("Swallowed a screenshot, {cw}\u{d7}{ch} — it's on the clipboard too"));
                }
                Err(e) => {
                    crate::util::log(&format!("screenshot: {e}"));
                    notify(dot, format!("Couldn't save the screenshot: {e}"));
                }
            }
        });
        self.close();
    }

    unsafe fn close(&mut self) {
        let _ = DestroyWindow(self.hwnd);
    }

    /// CF_DIB (32-bit BGRX, bottom-up) of the selection — the format every Windows
    /// app pastes; the 0x00RRGGBB pixels of the capture are already BGRX in memory.
    unsafe fn copy_to_clipboard(&self, sel: &RECT) -> Result<(), String> {
        let (cw, ch) = (sel.right - sel.left, sel.bottom - sel.top);
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: cw,
            biHeight: ch, // positive = bottom-up
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (cw * ch * 4) as u32,
            ..Default::default()
        };
        let bytes = std::mem::size_of::<BITMAPINFOHEADER>() + (cw * ch * 4) as usize;
        let hmem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
        let dst = GlobalLock(hmem) as *mut u8;
        if dst.is_null() {
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(&header as *const _ as *const u8, dst, std::mem::size_of::<BITMAPINFOHEADER>());
        let px = dst.add(std::mem::size_of::<BITMAPINFOHEADER>()) as *mut u32;
        for (row, y) in (sel.top..sel.bottom).rev().enumerate() {
            let src = &self.shot[(y * self.w + sel.left) as usize..(y * self.w + sel.right) as usize];
            std::ptr::copy_nonoverlapping(src.as_ptr(), px.add(row * cw as usize), cw as usize);
        }
        let _ = GlobalUnlock(hmem);
        OpenClipboard(Some(self.hwnd)).map_err(|e| e.to_string())?;
        let r = EmptyClipboard().and_then(|_| SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(hmem.0))).map(|_| ()));
        let _ = CloseClipboard();
        r.map_err(|e| e.to_string())
    }
}

unsafe fn notify(dot: HWND, text: String) {
    let boxed = Box::into_raw(Box::new(text));
    if PostMessageW(Some(dot), crate::dot::WM_NOTIFY_QUIET, WPARAM(0), LPARAM(boxed as isize)).is_err() {
        drop(Box::from_raw(boxed));
    }
}

/// `<data dir>\captures\<yyyy-mm-dd HH-MM-SS>.png`; a second capture in the same
/// second gets a numeric suffix rather than overwriting.
fn save_png(rgb: &[u8], w: u32, h: u32) -> Result<PathBuf, String> {
    let dir = crate::config::data_dir().join("captures");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let t = unsafe { GetLocalTime() };
    let stamp = format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    let mut path = dir.join(format!("{stamp}.png"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("{stamp} ({n}).png"));
    }
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(path)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut Overlay;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            (*ptr).hwnd = hwnd;
            LRESULT(0)
        }
        WM_PAINT => {
            if let Some(o) = state(hwnd) {
                o.paint();
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // the surface covers everything
        WM_LBUTTONDOWN => {
            if let Some(o) = state(hwnd) {
                let p = Overlay::pt(lparam);
                o.anchor = Some(p);
                o.cursor = p;
                SetCapture(hwnd);
                o.refresh();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(o) = state(hwnd) {
                if o.anchor.is_some() {
                    o.cursor = Overlay::pt(lparam);
                    o.refresh();
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(o) = state(hwnd) {
                if o.anchor.is_some() {
                    o.cursor = Overlay::pt(lparam);
                    let _ = ReleaseCapture();
                    o.finish();
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if let Some(o) = state(hwnd) {
                o.close();
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            if let Some(o) = state(hwnd) {
                o.close();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !ptr.is_null() {
                let o = Box::from_raw(ptr);
                let _ = DeleteObject(o.font.into());
                let _ = DeleteDC(o.dc);
                let _ = DeleteObject(o.dib.into());
            }
            ACTIVE.store(false, Ordering::SeqCst);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
