// Embed the icon and version info into the Windows exe (mingw windres).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=installer/blackhole.ico");
    println!("cargo:rerun-if-changed=build.rs");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let version = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    parts.resize(4, 0);
    let rc = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},{d}
PRODUCTVERSION {a},{b},{c},{d}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "thowd22\0"
      VALUE "FileDescription", "Blackhole — a black hole for your files\0"
      VALUE "FileVersion", "{version}\0"
      VALUE "InternalName", "blackhole\0"
      VALUE "LegalCopyright", "Built with Llama. See LICENSE-MODELS.txt\0"
      VALUE "OriginalFilename", "blackhole.exe\0"
      VALUE "ProductName", "Blackhole\0"
      VALUE "ProductVersion", "{version}\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = std::env::current_dir().unwrap().join("installer/blackhole.ico").display().to_string().replace('\\', "/"),
        a = parts[0], b = parts[1], c = parts[2], d = parts[3]
    );
    let rc_path = out.join("blackhole.rc");
    std::fs::write(&rc_path, rc).unwrap();
    let obj = out.join("blackhole_res.o");
    let status = std::process::Command::new("x86_64-w64-mingw32-windres")
        .args([rc_path.to_str().unwrap(), "-O", "coff", "-o", obj.to_str().unwrap()])
        .status()
        .expect("windres not found (apt install mingw-w64)");
    assert!(status.success(), "windres failed");
    println!("cargo:rustc-link-arg-bins={}", obj.display());
}
