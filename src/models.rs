//! Ask-mode models: what is on disk, which one is active, and fetching the
//! default one when there is none.
//!
//! A model is a folder holding an ONNX graph (plus its external data and a
//! `tokenizer.json`). Blackhole looks beside the exe — that is where the
//! installer puts `qwen3-4b\` — and in the vault under `models\`, which is
//! where a downloaded model lands (the exe folder usually needs admin rights).
//!
//! The download talks WinHTTP directly: no HTTP crate, no TLS stack of our own.
//! It resumes with a `Range` header, reports progress into the Settings row, can
//! be cancelled, and verifies the sha256 pins from `ci/prepare-model.sh`.

use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Progress / completion of a model download; wparam = `DL_PROGRESS` | `DL_DONE` |
/// `DL_FAILED`, lparam = Box<String> (the bubble text) for the last two.
pub const WM_MODEL_DOWNLOAD: u32 = 0x8064;
pub const DL_PROGRESS: usize = 0;
pub const DL_DONE: usize = 1;
pub const DL_FAILED: usize = 2;

/// One downloadable file of the default model.
struct Asset {
    url: &'static str,
    file: &'static str,
    sha256: &'static str,
    bytes: u64,
}

/// Folder the default model is downloaded into (`<vault>\models\qwen3-4b`).
pub const DEFAULT_MODEL: &str = "qwen3-4b";

/// The same export, URLs and sha256 pins `ci/prepare-model.sh` uses. The installer
/// additionally runs `tools/*.py` over the graph (last-token logits, Gemm LM head,
/// a logit-index input, one data file); those are graph-only edits — the weights are
/// untouched — so this download is the same model, just without the speed-ups.
///
/// If a prepared build is ever published as a release asset, the convention is one
/// file per name under `PREPARED_BASE` (see below) and only this table changes.
const ASSETS: &[Asset] = &[
    Asset {
        url: "https://huggingface.co/onnx-community/Qwen3-4B-ONNX/resolve/main/onnx/model_q4f16.onnx",
        file: "model_q4f16.onnx",
        sha256: "d3e946f9e38577411b0251051c91a1f20c3c0c831e1cf76a13c58ce279d950de",
        bytes: 59_762_833,
    },
    Asset {
        url: "https://huggingface.co/onnx-community/Qwen3-4B-ONNX/resolve/main/onnx/model_q4f16.onnx_data",
        file: "model_q4f16.onnx_data",
        sha256: "050398248de4fce7b31b2d2caa909596e4c7aa0f35696270df46b8aaf2209fc8",
        bytes: 2_096_005_120,
    },
    Asset {
        url: "https://huggingface.co/onnx-community/Qwen3-4B-ONNX/resolve/main/onnx/model_q4f16.onnx_data_1",
        file: "model_q4f16.onnx_data_1",
        sha256: "363ff5e70ebeb5866afea8eb80b7bdfe22d94d735a8c4d2ebf90c75424c2a410",
        bytes: 677_150_720,
    },
    Asset {
        url: "https://huggingface.co/onnx-community/Qwen3-4B-ONNX/resolve/main/tokenizer.json",
        file: "tokenizer.json",
        sha256: "e7a95fce95bf5b0946d0ddb3f9d7caa030b7e850bbe92b0edb26bcf563e9f3d5",
        bytes: 9_117_040,
    },
];

/// Where a graph-optimised build would live if the project ever ships one as a
/// release asset: `<PREPARED_BASE>/<file>`, one file per name, sha256 in
/// `SHA256SUMS.txt` beside them. Nothing reads this yet; `ASSETS` is the source.
#[allow(dead_code)]
pub const PREPARED_BASE: &str = "https://github.com/thowd22/Blackhole/releases/download/model-v1";

/// A model folder found on disk.
#[derive(Clone, Debug)]
pub struct Model {
    /// Folder name (or file stem for a loose .onnx), e.g. "qwen3-4b".
    pub name: String,
    /// The graph to load.
    pub path: PathBuf,
    /// Size on disk of the model's files.
    pub bytes: u64,
}

impl Model {
    /// "2.7 GB" / "780 MB".
    pub fn size_text(&self) -> String {
        size_text(self.bytes)
    }
}

pub fn size_text(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

/// Every model folder we can see: beside the exe (and one level under it), in the
/// vault, and in `<vault>\models\`. Sorted by name; duplicates by path removed.
pub fn list(extra_dir: &Path) -> Vec<Model> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            roots.push(d.to_path_buf());
        }
    }
    roots.push(extra_dir.to_path_buf());
    roots.push(extra_dir.join("models"));

    let mut out: Vec<Model> = Vec::new();
    for root in roots {
        // The folder itself (a loose graph) and one level of subfolders (the usual
        // layout: one folder per model, graph + external data + tokenizer.json).
        if let Some(m) = model_in(&root, false) {
            push(&mut out, m);
        }
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            if let Some(m) = model_in(&e, true) {
                push(&mut out, m);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn push(out: &mut Vec<Model>, m: Model) {
    if !out.iter().any(|x| x.path == m.path) {
        out.push(m);
    }
}

/// The model in `dir`, if any: the largest `.onnx` there. `own_folder` means the
/// folder belongs to the model, so its whole size counts; otherwise only the graph
/// and its external-data siblings do.
fn model_in(dir: &Path, own_folder: bool) -> Option<Model> {
    let mut best: Option<(u64, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        // A download in flight (or a cancelled one): half a model is not a model.
        if p.extension().map(|x| x.eq_ignore_ascii_case("part")).unwrap_or(false) {
            return None;
        }
        if p.extension().map(|x| x.eq_ignore_ascii_case("onnx")).unwrap_or(false) {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            if best.as_ref().map(|b| size > b.0).unwrap_or(true) {
                best = Some((size, p));
            }
        }
    }
    let (_, path) = best?;
    let name = if own_folder { dir.file_name()?.to_string_lossy().into_owned() } else { path.file_stem()?.to_string_lossy().into_owned() };
    let bytes = if own_folder { dir_size(dir) } else { graph_size(&path) };
    Some(Model { name, path, bytes })
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                dir_size(&p)
            } else {
                e.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// A loose graph plus its external data (`model.onnx_data`, `model.onnx.data`, …).
fn graph_size(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let (Some(dir), Some(stem)) = (path.parent(), path.file_name().map(|n| n.to_string_lossy().into_owned())) else { return total };
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n != stem && n.starts_with(&stem) {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// The model ask mode uses: the one named in the config if it is still there,
/// else the largest found (what Blackhole always did).
pub fn active(extra_dir: &Path) -> Option<Model> {
    let models = list(extra_dir);
    let want = crate::config::load().model_name;
    if !want.is_empty() {
        if let Some(m) = models.iter().find(|m| m.name == want) {
            return Some(m.clone());
        }
    }
    models.into_iter().max_by_key(|m| m.bytes)
}

/// The one after `name` in `list` order (wraps); None when there are fewer than two.
pub fn next_after(extra_dir: &Path, name: &str) -> Option<Model> {
    let models = list(extra_dir);
    if models.len() < 2 {
        return None;
    }
    let i = models.iter().position(|m| m.name == name).map(|i| (i + 1) % models.len()).unwrap_or(0);
    models.get(i).cloned()
}

// ---------------------------------------------------------------- download

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
static DONE_BYTES: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
static NOTE: RwLock<String> = RwLock::new(String::new());

pub fn downloading() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Row value while a download runs: "downloading 1.2 / 2.8 GB".
pub fn progress_text() -> String {
    if !downloading() {
        return NOTE.read().unwrap().clone();
    }
    let done = DONE_BYTES.load(Ordering::Relaxed);
    let total = TOTAL_BYTES.load(Ordering::Relaxed).max(1);
    let note = NOTE.read().unwrap().clone();
    if !note.is_empty() {
        return note;
    }
    format!("downloading {:.1} / {:.1} GB", done as f64 / 1e9, total as f64 / 1e9)
}

/// Total size of the default model, for the idle row ("2.8 GB").
pub fn default_size() -> u64 {
    ASSETS.iter().map(|a| a.bytes).sum()
}

pub fn cancel() {
    CANCEL.store(true, Ordering::Relaxed);
    *NOTE.write().unwrap() = "cancelling…".into();
}

/// Start the download on a background thread. `hwnd` gets `WM_MODEL_DOWNLOAD`
/// as it goes. A second call while one runs does nothing.
pub fn start_download(hwnd: usize) {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    CANCEL.store(false, Ordering::Relaxed);
    DONE_BYTES.store(0, Ordering::Relaxed);
    NOTE.write().unwrap().clear();
    std::thread::spawn(move || {
        let result = run_download(hwnd);
        ACTIVE.store(false, Ordering::Release);
        match result {
            Ok(dir) => {
                *NOTE.write().unwrap() = String::new();
                post(hwnd, DL_DONE, format!("Model ready — {} is in {}. Ask me something with ?", DEFAULT_MODEL, dir.display()));
            }
            Err(e) => {
                *NOTE.write().unwrap() = if CANCEL.load(Ordering::Relaxed) { String::new() } else { format!("failed: {e}") };
                let text =
                    if CANCEL.load(Ordering::Relaxed) { "Download cancelled — the part that came down is kept, click again to resume.".to_string() } else { format!("Model download failed: {e}") };
                post(hwnd, DL_FAILED, text);
            }
        }
    });
}

fn post(hwnd: usize, what: usize, text: String) {
    unsafe {
        let boxed = Box::into_raw(Box::new(text));
        if PostMessageW(Some(HWND(hwnd as *mut _)), WM_MODEL_DOWNLOAD, WPARAM(what), LPARAM(boxed as isize)).is_err() {
            drop(Box::from_raw(boxed));
        }
    }
}

fn post_progress(hwnd: usize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_MODEL_DOWNLOAD, WPARAM(DL_PROGRESS), LPARAM(0));
    }
}

/// BLACKHOLE_MODEL_URL: fetch this single file into `models\testdl\` instead of the
/// 2.8 GB model — how the download path is exercised without pulling the real thing.
fn assets() -> (String, Vec<Asset>) {
    if let Ok(url) = std::env::var("BLACKHOLE_MODEL_URL") {
        let file: String = url.rsplit('/').next().unwrap_or("model.onnx").to_string();
        let sha = std::env::var("BLACKHOLE_MODEL_SHA").unwrap_or_default();
        let bytes: u64 = std::env::var("BLACKHOLE_MODEL_BYTES").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        return ("testdl".to_string(), vec![Asset { url: leak(url), file: leak(file), sha256: leak(sha), bytes }]);
    }
    (DEFAULT_MODEL.to_string(), ASSETS.iter().map(|a| Asset { url: a.url, file: a.file, sha256: a.sha256, bytes: a.bytes }).collect())
}

fn run_download(hwnd: usize) -> Result<PathBuf, String> {
    let (folder, assets) = assets();
    let dir = crate::config::data_dir().join("models").join(&folder);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Files already here (a finished earlier run) count as done.
    let total: u64 = assets.iter().map(|a| a.bytes).sum();
    TOTAL_BYTES.store(total.max(1), Ordering::Relaxed);
    let mut base = 0u64;
    for a in &assets {
        let dest = dir.join(a.file);
        if dest.exists() && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == a.bytes && a.bytes > 0 {
            base += a.bytes;
            DONE_BYTES.store(base, Ordering::Relaxed);
            continue;
        }
        let part = dir.join(format!("{}.part", a.file));
        crate::util::log(&format!("model download: {} -> {}", a.url, part.display()));
        fetch(a.url, &part, base, hwnd)?;
        if !a.sha256.is_empty() {
            *NOTE.write().unwrap() = format!("checking {}…", a.file);
            post_progress(hwnd);
            let got = sha256_file(&part)?;
            NOTE.write().unwrap().clear();
            if got != a.sha256 {
                let _ = std::fs::remove_file(&part);
                return Err(format!("{} is not what we expected (sha256 {}…)", a.file, &got[..12.min(got.len())]));
            }
        }
        let _ = std::fs::remove_file(&dest);
        std::fs::rename(&part, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        base += std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(a.bytes);
        DONE_BYTES.store(base, Ordering::Relaxed);
        post_progress(hwnd);
    }
    Ok(dir)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if CANCEL.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

// ---------------------------------------------------------------- WinHTTP

/// A WinHTTP handle that closes itself.
struct Handle(*mut std::ffi::c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// `https://host/path` → (host, port, path, secure).
fn split_url(url: &str) -> Result<(String, u16, String, bool), String> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("not an http url: {url}"));
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(if secure { 443 } else { 80 })),
        None => (hostport.to_string(), if secure { 443 } else { 80 }),
    };
    Ok((host, port, path.to_string(), secure))
}

/// Download `url` into `part`, resuming from whatever is already there.
/// `base` is how many bytes of the whole job finished before this file.
fn fetch(url: &str, part: &Path, base: u64, hwnd: usize) -> Result<(), String> {
    let (host, port, path, secure) = split_url(url)?;
    let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let agent = crate::util::wide("Blackhole");
    let whost = crate::util::wide(&host);
    let verb = crate::util::wide("GET");
    let wpath = crate::util::wide(&path);
    unsafe {
        let session = Handle(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if session.0.is_null() {
            return Err("WinHttpOpen failed".into());
        }
        // Generous read timeout: the CDN can stall for a while on a big file.
        let _ = WinHttpSetTimeouts(session.0, 15_000, 20_000, 30_000, 60_000);
        let conn = Handle(WinHttpConnect(session.0, PCWSTR(whost.as_ptr()), port, 0));
        if conn.0.is_null() {
            return Err(format!("cannot reach {host}"));
        }
        let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let req = Handle(WinHttpOpenRequest(conn.0, PCWSTR(verb.as_ptr()), PCWSTR(wpath.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags));
        if req.0.is_null() {
            return Err("WinHttpOpenRequest failed".into());
        }
        if have > 0 {
            let header: Vec<u16> = format!("Range: bytes={have}-").encode_utf16().collect();
            let _ = WinHttpAddRequestHeaders(req.0, &header, WINHTTP_ADDREQ_FLAG_ADD);
        }
        WinHttpSendRequest(req.0, None, None, 0, 0, 0).map_err(|e| format!("send: {e}"))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| format!("no response: {e}"))?;

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut status as *mut u32 as *mut _), &mut len, &mut index)
            .map_err(|e| format!("status: {e}"))?;
        // 206 = the server honoured the resume; 200 with a resume means it did not,
        // so start the file again rather than appending to a stale prefix.
        let mut from = have;
        if status == 200 && have > 0 {
            from = 0;
        } else if status != 200 && status != 206 {
            return Err(format!("HTTP {status}"));
        }

        let mut file = if from > 0 {
            let mut f = std::fs::OpenOptions::new().write(true).open(part).map_err(|e| e.to_string())?;
            f.seek(SeekFrom::Start(from)).map_err(|e| e.to_string())?;
            f
        } else {
            std::fs::File::create(part).map_err(|e| e.to_string())?
        };

        let mut buf = vec![0u8; 256 * 1024];
        let mut got = from;
        let mut last_post = std::time::Instant::now();
        loop {
            if CANCEL.load(Ordering::Relaxed) {
                let _ = file.flush();
                return Err("cancelled".into());
            }
            let mut read: u32 = 0;
            WinHttpReadData(req.0, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut read).map_err(|e| format!("read: {e}"))?;
            if read == 0 {
                break;
            }
            file.write_all(&buf[..read as usize]).map_err(|e| e.to_string())?;
            got += read as u64;
            DONE_BYTES.store(base + got, Ordering::Relaxed);
            if last_post.elapsed().as_millis() > 400 {
                last_post = std::time::Instant::now();
                post_progress(hwnd);
            }
        }
        file.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}
