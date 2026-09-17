//! ONNX Runtime bootstrap. The runtime (`onnxruntime.dll` + `DirectML.dll`)
//! is compiled into the exe and unpacked next to it — or into the vault's
//! `bin` folder if the exe's directory isn't writable — then loaded
//! dynamically. Nothing is linked at build time.

use crate::util::wide;
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::SetDllDirectoryW;

static ORT_DLL: &[u8] = include_bytes!("../runtime/onnxruntime.dll");
static DML_DLL: &[u8] = include_bytes!("../runtime/DirectML.dll");

/// Write `bytes` to `dir/name` unless an identical-size copy is already there.
fn unpack(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let path = dir.join(name);
    let same = std::fs::metadata(&path).map(|m| m.len() == bytes.len() as u64).unwrap_or(false);
    if !same {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

fn unpack_into(dir: &Path) -> std::io::Result<PathBuf> {
    let ort = unpack(dir, "onnxruntime.dll", ORT_DLL)?;
    unpack(dir, "DirectML.dll", DML_DLL)?;
    Ok(ort)
}

/// Unpack (if needed) and initialise ONNX Runtime with DirectML registered.
/// Returns the directory the runtime was loaded from.
pub fn init() -> anyhow::Result<PathBuf> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let candidates = exe_dir.into_iter().chain(std::iter::once(crate::config::data_dir().join("bin")));
    let mut last_err = None;
    for dir in candidates {
        match unpack_into(&dir) {
            Ok(dll) => {
                // DirectML.dll is loaded by onnxruntime.dll by name; make sure it is found.
                unsafe {
                    let _ = SetDllDirectoryW(PCWSTR(wide(&dir.display().to_string()).as_ptr()));
                }
                ort::init_from(&dll).map_err(|e| anyhow::anyhow!("load {}: {e}", dll.display()))?.with_name("blackhole").commit();
                return Ok(dir);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!("could not unpack the ONNX Runtime: {:?}", last_err))
}
