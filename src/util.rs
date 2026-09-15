//! Small helpers shared across the Win32 code.

use std::time::{SystemTime, UNIX_EPOCH};

/// Null-terminated UTF-16 for passing to Win32 `W` functions.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a UTF-16 buffer up to the first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Truncate a string on a char boundary.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Append a line to `%LOCALAPPDATA%\Blackhole\log.txt` (best effort, for diagnosing the running app).
pub fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crate::config::data_dir().join("log.txt")) {
        let _ = writeln!(f, "{} {msg}", now_secs());
    }
}
