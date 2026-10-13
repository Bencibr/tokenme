//! WER writes the dump out of process, including native heap-corruption aborts.
//! Installation configures the machine policy; startup reports its actual state.
//! No SEH handler attempts to allocate/log from an already corrupted heap.

use std::fs::File;
use std::io::Read;
use sha2::{Digest, Sha256};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RRF_RT_REG_EXPAND_SZ};

const WER: &str = "SOFTWARE\\Microsoft\\Windows\\Windows Error Reporting\\LocalDumps";

fn read_number(key: &str, name: &str) -> Result<u32, u32> {
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut value = 0u32; let mut bytes = std::mem::size_of_val(&value) as u32;
    let status = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD,
        std::ptr::null_mut(), (&mut value as *mut u32).cast(), &mut bytes) };
    if status == 0 { Ok(value) } else { Err(status) }
}

fn read_folder(key: &str) -> Option<std::path::PathBuf> {
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let name: Vec<u16> = "DumpFolder\0".encode_utf16().collect(); let mut bytes = 0u32;
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let status = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), flags,
        std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) };
    if status != 0 || bytes < 2 || bytes > 65536 { return None }
    let mut buffer = vec![0u16; (bytes as usize + 1) / 2];
    let status = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), flags,
        std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut bytes) };
    if status != 0 { return None }
    let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Some(std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

pub(crate) fn init() {
    let Some(data) = dirs::data_local_dir() else { return };
    let dumps = data.join("tokenme").join("crashes");
    if let Err(error) = std::fs::create_dir_all(&dumps) {
        crate::logging::error(&format!("crash reporting: dump directory unavailable ({:?})", error.kind()));
    }
    let exe = std::env::current_exe().ok();
    let image = exe.as_ref().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or("TokenMe.exe");
    let key = format!("{WER}\\{image}");
    let app_type = read_number(&key, "DumpType"); let global_type = read_number(WER, "DumpType");
    let dump_type = app_type.as_ref().ok().copied().or_else(|| global_type.as_ref().ok().copied());
    let folder = read_folder(&key).or_else(|| read_folder(WER)).unwrap_or_else(|| data.join("CrashDumps"));
    let build: serde_json::Value = serde_json::from_str(env!("TOKENME_BUILD_DIAGNOSTICS")).unwrap_or_default();
    let record = serde_json::json!({
        "pid": std::process::id(), "image": image, "exe_sha256": exe.as_ref().and_then(|p| file_hash(p)),
        "build": build, "wer_full_dump": dump_type == Some(2), "wer_application_query_status": app_type.err(),
        "wer_policy_source": if app_type.is_ok() { "application" } else if global_type.is_ok() { "global" } else { "not_configured" },
        "dump_directory": folder, "dump_directory_exists": folder.is_dir(),
        "dump_count": read_number(&key, "DumpCount").ok().or_else(|| read_number(WER, "DumpCount").ok()),
        "quota_trace": "quota.log", "pageheap": "explicit diagnostic switch only",
    });
    crate::logging::info(&format!("diagnostics: {record}"));
    if dump_type != Some(2) {
        crate::logging::info("crash reporting: full dump policy is not active; install/repair with administrator rights or use windows-crash-dumps.ps1");
    }
}

fn file_hash(path: &std::path::Path) -> Option<String> {
    let mut file = File::open(path).ok()?; let mut hash = Sha256::new(); let mut bytes = [0u8; 16 * 1024];
    loop { let read = file.read(&mut bytes).ok()?; if read == 0 { break } hash.update(&bytes[..read]); }
    Some(format!("{:x}", hash.finalize()))
}
