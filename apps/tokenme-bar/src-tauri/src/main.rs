// Prevents an extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    eprintln!("tokenme v{} build {} starting", env!("CARGO_PKG_VERSION"), env!("TOKENME_BUILD_ID"));
    tokenme_bar::run()
}
