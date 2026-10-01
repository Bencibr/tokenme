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
    tauri_build::build()
}
