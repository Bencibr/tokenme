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
    tauri_build::build();
    link_manifest_into_tests();
}

/// `cargo test` links the same dependency graph the app does, and some of it
/// (`notify-rust`'s dialog fallback) imports `TaskDialogIndirect` — which
/// comctl32 only exports under the v6 side-by-side assembly. tauri-build puts
/// the manifest in `resource.lib` and attaches it with `rustc-link-arg-bins`,
/// so a test binary carried no manifest at all and died before `main` with
/// STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139) on Windows.
///
/// The same resource archive is right there in our OUT_DIR (tauri-build wrote
/// it during this run); attaching it to test targets too makes `cargo test`
/// runnable without touching the app's own link. Absent (a target that bundles
/// no resources) this is a no-op.
fn link_manifest_into_tests() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let Ok(out) = std::env::var("OUT_DIR") else {
        return;
    };
    let resource_lib = std::path::Path::new(&out).join("resource.lib");
    if resource_lib.exists() {
        println!("cargo:rustc-link-arg={}", resource_lib.display());
    }
}
