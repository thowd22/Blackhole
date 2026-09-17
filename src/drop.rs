//! OLE drop target for the dot, plus clipboard reading (same data formats).

use crate::ingest::Input;
use crate::util::from_wide;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use windows::core::implement;
use windows::Win32::Foundation::{HGLOBAL, HWND, LPARAM, POINTL, WPARAM};
use windows::Win32::System::Com::{IDataObject, FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{IDropTarget, IDropTarget_Impl, ReleaseStgMedium, CF_DIB, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Messages posted to the dot so it can animate.
pub const WM_DROP_ENTER: u32 = 0x8001;
pub const WM_DROP_LEAVE: u32 = 0x8002;
pub const WM_DROP_SWALLOW: u32 = 0x8003;

#[implement(IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
    tx: Sender<Input>,
}

impl DropTarget {
    pub fn new(hwnd: HWND, tx: Sender<Input>) -> IDropTarget {
        DropTarget { hwnd, tx }.into()
    }
}

fn fmt(cf: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// `CFSTR_INETURLW`: what a browser puts on the clipboard (and in the drag) for a
/// link. Registered once; 0 if Windows refuses, which no format ever matches.
fn url_format() -> u16 {
    use std::sync::OnceLock;
    static F: OnceLock<u16> = OnceLock::new();
    *F.get_or_init(|| unsafe { RegisterClipboardFormatW(windows::core::w!("UniformResourceLocatorW")) as u16 })
}

fn acceptable(obj: &IDataObject) -> bool {
    unsafe {
        obj.QueryGetData(&fmt(CF_HDROP.0)).is_ok()
            || obj.QueryGetData(&fmt(CF_UNICODETEXT.0)).is_ok()
            || obj.QueryGetData(&fmt(url_format())).is_ok()
    }
}

unsafe fn files_from_hdrop(h: HDROP) -> Vec<PathBuf> {
    let n = DragQueryFileW(h, u32::MAX, None);
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let len = DragQueryFileW(h, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        DragQueryFileW(h, i, Some(&mut buf));
        out.push(PathBuf::from(from_wide(&buf)));
    }
    out
}

unsafe fn text_from_hglobal(h: HGLOBAL) -> Option<String> {
    let p = GlobalLock(h) as *const u16;
    if p.is_null() {
        return None;
    }
    let max = GlobalSize(h) / 2;
    let slice = std::slice::from_raw_parts(p, max);
    let s = from_wide(slice);
    let _ = GlobalUnlock(h);
    Some(s)
}

/// CF_DIB → RGB rows (top-down). Handles the common 24/32-bit uncompressed layouts.
unsafe fn dib_to_rgb(h: HGLOBAL) -> Option<(Vec<u8>, u32, u32)> {
    let p = GlobalLock(h) as *const u8;
    if p.is_null() {
        return None;
    }
    let size = GlobalSize(h);
    let bytes = std::slice::from_raw_parts(p, size);
    let result = (|| {
        if bytes.len() < 40 {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let i32_at = |o: usize| i32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let hdr = u32_at(0) as usize;
        let (w, h_signed) = (i32_at(4), i32_at(8));
        let bpp = u16::from_le_bytes(bytes[14..16].try_into().unwrap()) as usize;
        let compression = u32_at(16);
        let colors_used = u32_at(32) as usize;
        if w <= 0 || h_signed == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) {
            return None;
        }
        let (w, hgt) = (w as usize, h_signed.unsigned_abs() as usize);
        let bottom_up = h_signed > 0;
        // BI_BITFIELDS masks follow a 40-byte header; V4/V5 headers carry them inside.
        let offset = hdr + if compression == 3 && hdr < 52 { 12 } else { 0 } + colors_used * 4;
        let stride = (w * bpp / 8 + 3) / 4 * 4;
        if bytes.len() < offset + stride * hgt {
            return None;
        }
        let mut rgb = Vec::with_capacity(w * hgt * 3);
        for row in 0..hgt {
            let src_row = if bottom_up { hgt - 1 - row } else { row };
            let line = &bytes[offset + src_row * stride..offset + src_row * stride + w * bpp / 8];
            for px in line.chunks(bpp / 8) {
                rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        Some((rgb, w as u32, hgt as u32))
    })();
    let _ = GlobalUnlock(h);
    result
}

/// Pull files or text out of a data object (drag-and-drop source).
pub fn read_data_object(obj: &IDataObject) -> Option<Input> {
    unsafe {
        if let Ok(mut med) = obj.GetData(&fmt(CF_HDROP.0)) {
            let files = files_from_hdrop(HDROP(med.u.hGlobal.0));
            ReleaseStgMedium(&mut med);
            if !files.is_empty() {
                return Some(Input::Files(files));
            }
        }
        // A dragged link: the URL itself, which ingest turns into the page behind it.
        if url_format() != 0 {
            if let Ok(mut med) = obj.GetData(&fmt(url_format())) {
                let text = text_from_hglobal(med.u.hGlobal);
                ReleaseStgMedium(&mut med);
                if let Some(u) = text.as_deref().and_then(crate::web::bare_url) {
                    return Some(Input::Text(u));
                }
            }
        }
        if let Ok(mut med) = obj.GetData(&fmt(CF_UNICODETEXT.0)) {
            let text = text_from_hglobal(med.u.hGlobal);
            ReleaseStgMedium(&mut med);
            if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
                return Some(Input::Text(t));
            }
        }
    }
    None
}

/// Pull files or text off the clipboard.
pub fn read_clipboard(owner: HWND) -> Option<Input> {
    unsafe {
        OpenClipboard(Some(owner)).ok()?;
        let result = (|| {
            if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
                let h = GetClipboardData(CF_HDROP.0 as u32).ok()?;
                let files = files_from_hdrop(HDROP(h.0));
                if !files.is_empty() {
                    return Some(Input::Files(files));
                }
            }
            // A copied link ("copy link address"): swallow the page, not the text.
            if url_format() != 0 && IsClipboardFormatAvailable(url_format() as u32).is_ok() {
                if let Ok(h) = GetClipboardData(url_format() as u32) {
                    if let Some(u) = text_from_hglobal(HGLOBAL(h.0)).as_deref().and_then(crate::web::bare_url) {
                        return Some(Input::Text(u));
                    }
                }
            }
            if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
                let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
                let text = text_from_hglobal(HGLOBAL(h.0))?;
                if !text.trim().is_empty() {
                    return Some(Input::Text(text));
                }
            }
            // A copied picture (snipping tool, browser "copy image"): saved as PNG in the
            // vault's files folder and swallowed like a dropped image, OCR included.
            if IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok() {
                let h = GetClipboardData(CF_DIB.0 as u32).ok()?;
                if let Some((rgb, w, hgt)) = dib_to_rgb(HGLOBAL(h.0)) {
                    let t = windows::Win32::System::SystemInformation::GetLocalTime();
                    let stem = format!("Pasted image {:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
                    if let Ok(path) = crate::ingest::store_png(&rgb, w, hgt, &stem) {
                        return Some(Input::Files(vec![path]));
                    }
                }
            }
            None
        })();
        let _ = CloseClipboard();
        result
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: windows::core::Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let ok = pdataobj.as_ref().map(acceptable).unwrap_or(false);
        unsafe {
            // Paused: nothing is accepted, and the dot says so the moment something
            // is dragged over it rather than after the drop.
            if crate::config::paused() {
                *effect = DROPEFFECT_NONE;
                let _ = PostMessageW(Some(self.hwnd), crate::dot::WM_PAUSED, WPARAM(0), LPARAM(0));
                return Ok(());
            }
            *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
            if ok {
                let _ = PostMessageW(Some(self.hwnd), WM_DROP_ENTER, WPARAM(0), LPARAM(0));
            }
        }
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        unsafe { *effect = if crate::config::paused() { DROPEFFECT_NONE } else { DROPEFFECT_COPY } };
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_DROP_LEAVE, WPARAM(0), LPARAM(0));
        }
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: windows::core::Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            if crate::config::paused() {
                *effect = DROPEFFECT_NONE;
                let _ = PostMessageW(Some(self.hwnd), crate::dot::WM_PAUSED, WPARAM(0), LPARAM(0));
                return Ok(());
            }
        }
        let input = pdataobj.as_ref().and_then(read_data_object);
        unsafe {
            match input {
                Some(input) => {
                    *effect = DROPEFFECT_COPY;
                    let _ = self.tx.send(input);
                    let _ = PostMessageW(Some(self.hwnd), WM_DROP_SWALLOW, WPARAM(0), LPARAM(0));
                }
                None => {
                    *effect = DROPEFFECT_NONE;
                    let _ = PostMessageW(Some(self.hwnd), WM_DROP_LEAVE, WPARAM(0), LPARAM(0));
                }
            }
        }
        Ok(())
    }
}
