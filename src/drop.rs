//! OLE drop target for the dot, plus clipboard reading (same data formats).

use crate::ingest::Input;
use crate::util::from_wide;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use windows::core::implement;
use windows::Win32::Foundation::{HGLOBAL, HWND, LPARAM, POINTL, WPARAM};
use windows::Win32::System::Com::{IDataObject, FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{IDropTarget, IDropTarget_Impl, ReleaseStgMedium, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE};
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

fn acceptable(obj: &IDataObject) -> bool {
    unsafe { obj.QueryGetData(&fmt(CF_HDROP.0)).is_ok() || obj.QueryGetData(&fmt(CF_UNICODETEXT.0)).is_ok() }
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
            if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
                let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
                let text = text_from_hglobal(HGLOBAL(h.0))?;
                if !text.trim().is_empty() {
                    return Some(Input::Text(text));
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
            *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
            if ok {
                let _ = PostMessageW(Some(self.hwnd), WM_DROP_ENTER, WPARAM(0), LPARAM(0));
            }
        }
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        unsafe { *effect = DROPEFFECT_COPY };
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
