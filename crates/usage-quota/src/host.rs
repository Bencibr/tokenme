//! Is the tool's host application running?
//!
//! The quota probe for a tool whose host has exited is a vendor call nobody
//! wants: the user closed the IDE, yet the panel kept asking the vendor for a
//! number that cannot change. `host_running` answers per tool from the process
//! table, and a tool with no known process names answers `true` — the
//! conservative default keeps probing (CLI-only tools can run inside any
//! terminal, and a wrong "not running" would silently freeze a live row).
//!
//! The names are base process names, compared case-insensitively with or
//! without the platform's `.exe` suffix.

use std::collections::HashSet;

/// `(tool id, process base names)` — the host application whose presence means
/// "the user is using this tool right now".
pub const HOST_PROCESSES: &[(&str, &[&str])] = &[
    ("qoder", &["Qoder"]),
    ("atomcode", &["atomcode"]),
    ("funide", &["FunIDE"]),
    ("dsh", &["DSH Desktop", "dsh-desktop"]),
    ("zcode", &["ZCode", "zcode"]),
    ("antigravity", &["Antigravity"]),
    ("workbuddy", &["WorkBuddy AI", "WorkBuddy"]),
    ("catpaw", &["CatPawAI", "CatPaw"]),
    ("cline", &["Cline", "cline", "cline-app", "code-sidecar"]),
    ("codex", &["codex", "Codex"]),
    ("claude", &["claude", "Claude"]),
    ("joycode", &["JoyCode"]),
    ("agnes", &["AgnesCode", "agnesd"]),
    ("crow5", &["Crow5", "crow5-cli"]),
    // Cola's billing token sits in auth.json regardless, but with no host
    // running nothing is spending against it — the bundled cola-server counts
    // as alive too (it outlives the window on some exits).
    ("cola", &["Cola", "cola-server"]),
    // Trae's main binary ships as the generic "Electron"; its helpers are the
    // only processes that name the product, and they live and die with it.
    ("trae", &["Trae Helper", "Trae Helper (GPU)", "Trae Helper (Renderer)", "Trae Helper (Plugin)"]),
];

/// Whether the tool's host application is running. A tool with no mapping is
/// always "running": the probe keeps going rather than guessing a stop.
pub fn host_running(tool: &str, alive: &dyn Fn(&str) -> bool) -> bool {
    let Some((_, names)) = HOST_PROCESSES.iter().find(|(id, _)| *id == tool) else {
        return true;
    };
    names.iter().any(|n| alive(n))
}

#[cfg(windows)]
pub fn running_process_names() -> HashSet<String> {
    use std::mem;

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;

    #[repr(C)]
    struct ProcessEntryW {
        dw_size: u32,
        cnt_usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        cnt_threads: u32,
        parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        exe_file: [u16; 260],
    }

    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntryW) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntryW) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    let mut names = HashSet::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == -1 {
            return names;
        }
        let mut entry = ProcessEntryW {
            dw_size: mem::size_of::<ProcessEntryW>() as u32,
            cnt_usage: 0,
            process_id: 0,
            default_heap_id: 0,
            module_id: 0,
            cnt_threads: 0,
            parent_process_id: 0,
            pc_pri_class_base: 0,
            dw_flags: 0,
            exe_file: [0; 260],
        };
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry.exe_file.iter().position(|c| *c == 0).unwrap_or(0);
                names.insert(String::from_utf16_lossy(&entry.exe_file[..len]).to_lowercase());
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    names
}

#[cfg(not(windows))]
pub fn running_process_names() -> HashSet<String> {
    let mut names = HashSet::new();
    for tool_names in HOST_PROCESSES.iter().flat_map(|(_, names)| names.iter()) {
        if let Ok(output) = std::process::Command::new("pgrep")
            .args(["-x", tool_names])
            .output()
        {
            // pgrep answers 0 with pids when the process runs.
            if output.status.success() && !output.stdout.is_empty() {
                names.insert(tool_names.to_lowercase());
            }
        }
    }
    names
}

/// Case-insensitive, `.exe`-tolerant match of a running process name — the
/// suffix may sit on either side or both.
pub fn matches(alive: &HashSet<String>, name: &str) -> bool {
    let lower = name.to_lowercase();
    let bare = lower.trim_end_matches(".exe");
    alive.contains(&lower)
        || alive.contains(&format!("{lower}.exe"))
        || alive.iter().any(|p| {
            let bare_p = p.trim_end_matches(".exe").to_lowercase();
            bare_p == bare || bare_p == lower
        })
}

/// Convenience over [`host_running`] + [`running_process_names`]: one snapshot
/// answers every tool in the pass.
pub fn any_host_running(tool: &str, alive: &HashSet<String>) -> bool {
    host_running(tool, &|name| matches(alive, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmapped_tool_is_always_running() {
        let empty: HashSet<String> = HashSet::new();
        assert!(host_running("pi", &|_| false), "a CLI tool runs in any terminal");
    }

    #[test]
    fn a_mapped_tool_follows_its_process() {
        let mut alive: HashSet<String> = HashSet::new();
        alive.insert("qoder.exe".to_string());
        assert!(any_host_running("qoder", &alive));
        assert!(!any_host_running("atomcode", &alive));
    }

    #[test]
    fn names_match_case_and_extension_insensitively() {
        let mut alive: HashSet<String> = HashSet::new();
        alive.insert("FunIDE".to_string());
        assert!(matches(&alive, "funide"));
        assert!(matches(&alive, "funide.exe"));
    }

    /// Cola gates on either the Electron main process or the bundled
    /// cola-server (exact names, never substrings — "Cola Helper" does not
    /// count on its own once the main process is gone).
    #[test]
    fn cola_follows_its_host_or_server() {
        let mut alive: HashSet<String> = HashSet::new();
        assert!(!any_host_running("cola", &alive), "closed means paused");
        alive.insert("cola-server".to_string());
        assert!(any_host_running("cola", &alive));
        alive.clear();
        alive.insert("Cola".to_string());
        assert!(any_host_running("cola", &alive));
        alive.clear();
        alive.insert("Cola Helper (Renderer)".to_string());
        assert!(!any_host_running("cola", &alive), "helpers alone are not the host");
    }
}
