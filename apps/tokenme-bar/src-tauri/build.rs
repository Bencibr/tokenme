fn main() {
    // A monotonically increasing build id, bumped by scripts/build-macos.sh.
    // It rides the version everywhere the panel can be asked "which build is
    // this?" — the settings footer, the tray tooltip, the first log line.
    let id = std::fs::read_to_string("BUILD_ID")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    println!("cargo:rustc-env=TOKENME_BUILD_ID={id}");
    println!("cargo:rerun-if-changed=BUILD_ID");
    let diagnostics = std::fs::read_to_string("resources/diagnostics/build-info.json").unwrap_or_else(|_| "{}".into());
    // One-line JSON is produced before a Windows build; no host paths or keys.
    println!("cargo:rustc-env=TOKENME_BUILD_DIAGNOSTICS={}", diagnostics.trim());
    println!("cargo:rerun-if-changed=resources/diagnostics/build-info.json");
    tauri_build::build()
}
