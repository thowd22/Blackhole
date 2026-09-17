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

/// The prepared build of the default model, published as release assets under
/// `PREPARED_BASE`: the same graph the installer ships (last-token logits, Gemm LM
/// head, logit-index input, one data file — see PACKAGING.md), so a downloaded model
/// runs exactly like an installed one. The data file is split into two parts
/// because GitHub caps release assets at 2 GiB; `JOINS` says how they go back together.
/// Order matters: the graph comes last, so a folder only ever holds a `.onnx` once
/// its data is complete (`models::list` treats any `.onnx` as a usable model).
#[allow(dead_code)]
pub const PREPARED_BASE: &str = "https://github.com/thowd22/Blackhole/releases/download/model-v1";
macro_rules! prepared {
    ($f:literal) => {
        concat!("https://github.com/thowd22/Blackhole/releases/download/model-v1/", $f)
    };
}
const ASSETS: &[Asset] = &[
    Asset { url: prepared!("tokenizer.json"), file: "tokenizer.json", sha256: "e7a95fce95bf5b0946d0ddb3f9d7caa030b7e850bbe92b0edb26bcf563e9f3d5", bytes: 9_117_040 },
    Asset { url: prepared!("model_q4f16.onnx.data.part0"), file: "model_q4f16.onnx.data.part0", sha256: "55a881ba24ab32caf622c740f120cc44f2dec6a7f6b35117c818b89c1d9b79e0", bytes: 1_992_294_400 },
    Asset { url: prepared!("model_q4f16.onnx.data.part1"), file: "model_q4f16.onnx.data.part1", sha256: "243a4d9dfa8748129662317f30bc3b5bd5d5bc18832abc3df440f27b433c6385", bytes: 780_861_440 },
    Asset { url: prepared!("model_q4f16.onnx"), file: "model_q4f16.onnx", sha256: "e7886798b945581ef80639300147b0d92b66c2c6e72db835834ecf8bbf6d62d3", bytes: 59_762_959 },
];

/// Files assembled from downloaded parts: (target, parts in order, sha256 of the whole, bytes).
const JOINS: &[(&str, &[&str], &str, u64)] =
    &[("model_q4f16.onnx.data", &["model_q4f16.onnx.data.part0", "model_q4f16.onnx.data.part1"], "24e16e8e966196a411b292f7f8251e10faffd8497a3b63b93981febcb2da513f", 2_773_155_840)];

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
    active_in(&list(extra_dir), &crate::config::load().model_name)
}

/// `active` over a list already in hand, so a caller that has both the models and the
/// config does not walk the model folders (or read config.json) a second time.
pub fn active_in(models: &[Model], want: &str) -> Option<Model> {
    if !want.is_empty() {
        if let Some(m) = models.iter().find(|m| m.name == want) {
            return Some(m.clone());
        }
    }
    models.iter().max_by_key(|m| m.bytes).cloned()
}

/// The one after `name` in `list` order (wraps); None when there are fewer than two.
pub fn next_after(extra_dir: &Path, name: &str) -> Option<Model> {
    next_after_in(&list(extra_dir), name)
}

/// `next_after` over a list already in hand.
pub fn next_after_in(models: &[Model], name: &str) -> Option<Model> {
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
        // The graph is the last asset: everything before it must be whole and joined.
        if a.file.ends_with(".onnx") {
            join_parts(&dir, hwnd)?;
        }
        let dest = dir.join(a.file);
        // A part whose joined file already exists counts as done.
        let joined_done = JOINS.iter().any(|(target, parts, _, bytes)| parts.contains(&a.file) && std::fs::metadata(dir.join(target)).map(|m| m.len() == *bytes).unwrap_or(false));
        if joined_done || (dest.exists() && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) == a.bytes && a.bytes > 0) {
            base += a.bytes;
            DONE_BYTES.store(base, Ordering::Relaxed);
            continue;
        }
        let part = dir.join(format!("{}.part", a.file));
        // A .part that is already the whole file came down in an earlier run that was
        // cancelled (or killed) between the last byte and the rename: verify it as it
        // stands rather than asking the server for a range it cannot serve.
        let whole = a.bytes > 0 && std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) == a.bytes;
        if whole {
            DONE_BYTES.store(base + a.bytes, Ordering::Relaxed);
        } else {
            crate::util::log(&format!("model download: {} -> {}", a.url, part.display()));
            fetch(a.url, &part, base, a.bytes, hwnd)?;
        }
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

/// Concatenate downloaded parts into their target (streaming, then sha256-checked);
/// parts are deleted afterwards. Nothing to do when the target is already whole.
fn join_parts(dir: &Path, hwnd: usize) -> Result<(), String> {
    for (target, parts, sha, bytes) in JOINS {
        let out = dir.join(target);
        if std::fs::metadata(&out).map(|m| m.len() == *bytes).unwrap_or(false) {
            for p in *parts {
                let _ = std::fs::remove_file(dir.join(p));
            }
            continue;
        }
        if !parts.iter().all(|p| dir.join(p).exists()) {
            continue;
        }
        *NOTE.write().unwrap() = format!("joining {target}…");
        post_progress(hwnd);
        let tmp = dir.join(format!("{target}.part"));
        {
            let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?);
            for p in *parts {
                let mut r = std::fs::File::open(dir.join(p)).map_err(|e| format!("{p}: {e}"))?;
                std::io::copy(&mut r, &mut w).map_err(|e| format!("joining {p}: {e}"))?;
            }
            use std::io::Write;
            w.flush().map_err(|e| e.to_string())?;
        }
        *NOTE.write().unwrap() = format!("checking {target}…");
        post_progress(hwnd);
        let got = sha256_file(&tmp)?;
        NOTE.write().unwrap().clear();
        if got != *sha {
            let _ = std::fs::remove_file(&tmp);
            for p in *parts {
                let _ = std::fs::remove_file(dir.join(p));
            }
            return Err(format!("{target} did not join cleanly (sha256 {}…) — the parts were removed; try again", &got[..12]));
        }
        std::fs::rename(&tmp, &out).map_err(|e| format!("{}: {e}", out.display()))?;
        for p in *parts {
            let _ = std::fs::remove_file(dir.join(p));
        }
    }
    Ok(())
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
/// `base` is how many bytes of the whole job finished before this file, `expected`
/// the file's full length (0 when unknown).
fn fetch(url: &str, part: &Path, base: u64, expected: u64, hwnd: usize) -> Result<(), String> {
    let mut have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    // A prefix that is already the whole file is not a resume point: `Range: bytes=len-`
    // is unsatisfiable, so the file would fail with HTTP 416 forever. Start it again.
    if expected > 0 && have >= expected {
        have = 0;
    }
    match fetch_from(url, part, base, have, hwnd) {
        // The server refused the range anyway (a stale or overlong .part): take the
        // whole file instead of leaving the download stuck on 416.
        Err(FetchErr::Range) if have > 0 => fetch_from(url, part, base, 0, hwnd).map_err(FetchErr::text),
        other => other.map_err(FetchErr::text),
    }
}

/// Why a `fetch_from` attempt stopped. `Range` is worth retrying from zero.
enum FetchErr {
    Range,
    Other(String),
}

impl FetchErr {
    fn text(self) -> String {
        match self {
            FetchErr::Range => "HTTP 416".to_string(),
            FetchErr::Other(s) => s,
        }
    }
}

impl From<String> for FetchErr {
    fn from(s: String) -> FetchErr {
        FetchErr::Other(s)
    }
}

impl From<&str> for FetchErr {
    fn from(s: &str) -> FetchErr {
        FetchErr::Other(s.to_string())
    }
}

/// One GET, resuming at `have` bytes (0 = from the start, truncating `part`).
fn fetch_from(url: &str, part: &Path, base: u64, have: u64, hwnd: usize) -> Result<(), FetchErr> {
    let (host, port, path, secure) = split_url(url)?;
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
            return Err(format!("cannot reach {host}").into());
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
        if status == 416 {
            return Err(FetchErr::Range);
        }
        if status == 200 && have > 0 {
            from = 0;
        } else if status != 200 && status != 206 {
            return Err(format!("HTTP {status}").into());
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
