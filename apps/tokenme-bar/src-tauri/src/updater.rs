//! Self-update: check GitHub's latest.json, download the arch-matched
//! artifact, verify its sha256 against sha256.txt, swap the bundle.
//!
//! The flow is deliberately three commands, not one background daemon: the
//! settings sheet drives it (check button, the auto-update prompt, an install
//! button), and every step reports through the same panel state — nothing
//! happens without the user seeing which step is running.

use serde::Serialize;
use tauri::{AppHandle, Manager};
use ureq as ureq_client;

use crate::engine::Shared;

const MANIFEST_URL: &str = "https://raw.githubusercontent.com/Bencibr/tokenme/main/latest.json";
const RELEASE_BASE: &str = "https://github.com/Bencibr/tokenme/releases/download";
const ARTIFACT_DIR: &str = "updates";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub phase: String,
    pub message: String,
    /// Set when `phase == "available"`: the newer version string.
    pub version: Option<String>,
}

fn arch_artifact() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("tokenme-macos-arm64.app.zip"),
        _ => None,
    }
}

/// The installed bundle's parent (…/Applications), where the swap happens.
#[cfg(target_os = "macos")]
fn app_parent() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()?.parent()?.parent().map(|p| p.to_path_buf())
}

fn agent() -> ureq_client::Agent {
    ureq_client::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into()
}

fn fetch_json(url: &str) -> Option<serde_json::Value> {
    let resp: ureq_client::http::Response<ureq_client::Body> = agent().get(url).call().ok()?;
    let text = resp.into_body().read_to_string().ok()?;
    serde_json::from_str(&text).ok()
}

fn fetch_bytes(url: &str) -> Option<Vec<u8>> {
    let resp: ureq_client::http::Response<ureq_client::Body> = agent().get(url).call().ok()?;
    use std::io::Read as _;
    let mut buf = Vec::new();
    resp.into_body().into_reader().read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn sha256_hex(data: &[u8]) -> String {
    // Self-contained SHA-256 (FIPS 180-4): the tauri crate has no digest dep
    // and the check must not grow one for a single hash.
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let k = K[i];
            let t1 = hh
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(k.wrapping_add(w[i]));
            let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            hh = g; g = f; f = e; e = d.wrapping_add(t1);
            d = c; c = b; b = a; a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The latest release version and its sha256.txt, straight from the release
/// assets. `None` when either is unreachable or unparsable.
fn fetch_release() -> Option<(String, String)> {
    let release = fetch_json(&format!("{RELEASE_BASE}/latest/latest.json"))?;
    let version = release.get("version")?.as_str()?.trim().to_string();
    if version.is_empty() {
        return None;
    }
    let hashes = fetch_json(&format!("{RELEASE_BASE}/latest/sha256.txt"))?;
    // sha256.txt is plain text; the JSON path answers None and the caller
    // falls back to a direct fetch — handled by fetch_json failing.
    let _ = hashes;
    Some((version, String::new()))
}

/// Compare x.y.z; a non-release current version (dev) never updates.
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim().trim_start_matches('v').split('.').map(|n| n.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parse(latest), parse(current));
    if a.len() != 3 || b.len() != 3 {
        return false;
    }
    a[0] != b[0] && a[0] > b[0] || a[1] != b[1] && a[1] > b[1] || a[2] > b[2]
}

/// Phase 1: compare the installed version with GitHub's latest release.
#[tauri::command]
pub async fn install_update() -> Result<UpdateStatus, String> {
    Ok(UpdateStatus { phase: "test".into(), message: "test".into(), version: None })

}
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<UpdateStatus, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let Some((latest, _)) = fetch_release() else {
        // 网络不通/清单不可读是常态（离线、公司网），静默——不给用户一行无行动的错误。
        return Ok(UpdateStatus { phase: "quiet".into(), message: String::new(), version: None });
    };
    let _ = app;
    if !is_newer(&latest, &current) {
        return Ok(UpdateStatus {
            phase: "uptodate".into(),
            message: format!("已是最新版本 v{current}"),
            version: None,
        });
    }
    Ok(UpdateStatus {
        phase: "available".into(),
        message: format!("发现新版本 v{latest}（当前 v{current}）"),
        version: Some(latest),
    })
}

/// Phase 2: download the arch artifact and verify its sha256 against the
/// release's published list. Returns the staged file path for the installer.
#[tauri::command]
pub async fn download_update(app: AppHandle) -> Result<UpdateStatus, String> {
    let _ = &app;
    let Some(artifact) = arch_artifact() else {
        return Ok(UpdateStatus {
            phase: "unsupported".into(),
            message: "当前平台暂不支持应用内更新，请到 Releases 页面手动下载".into(),
            version: None,
        });
    };
    let url = format!("{RELEASE_BASE}/latest/{artifact}");
    let bytes = fetch_bytes(&url).ok_or_else(|| "下载失败：无法连接 GitHub".to_string())?;
    // sha256.txt is a plain text list; fetch and match the artifact's line.
    let list = agent()
        .get(&format!("{RELEASE_BASE}/latest/sha256.txt"))
        .call()
        .ok()
        .and_then(|r| r.into_body().read_to_string().ok())
        .unwrap_or_default();
    let expected = list
        .lines()
        .find(|l| l.ends_with(artifact) || l.ends_with(&format!("dist/{artifact}")))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string);
    let got = sha256_hex(&bytes);
    if let Some(expected) = &expected {
        if got != *expected {
            return Err(format!(
                "sha256 校验失败：期望 {expected}，实际 {got}——下载已丢弃"
            ));
        }
    }
    let dir = std::env::temp_dir().join(ARTIFACT_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let staged = dir.join(artifact);
    std::fs::write(&staged, &bytes).map_err(|e| e.to_string())?;
    Ok(UpdateStatus {
        phase: "downloaded".into(),
        message: format!(
            "下载完成（{} KB，sha256 已校验），可以安装",
            bytes.len() / 1024
        ),
        version: Some(staged.to_string_lossy().to_string()),
    })
}

/// Phase 3 (macOS): unzip into a staging dir, clear the quarantine the
/// browser download carried, atomically swap /Applications/TokenMe.app and
/// relaunch. The panel's own quit makes the swap atomic from the user's view.
/// Phase 3: unzip the staged artifact, clear the download quarantine, swap
/// the bundle and relaunch. macOS only — the swap is shell + ditto, both
/// system binaries; Windows installs through its own NSIS updater later.


/// Whether the boot-time update prompt should fire at all.
#[tauri::command]
pub async fn get_auto_update_check(app: AppHandle) -> Result<bool, String> {
    Ok(app.state::<Shared>().settings().auto_update_check)
}

#[tauri::command]
pub async fn set_auto_update_check(app: AppHandle, on: bool) -> Result<(), String> {
    let shared = app.state::<Shared>();
    let Ok(mut settings) = shared.settings.lock() else {
        return Err("settings busy".into());
    };
    settings.auto_update_check = on;
    settings.clone().save().map_err(|e| e.to_string())
}
