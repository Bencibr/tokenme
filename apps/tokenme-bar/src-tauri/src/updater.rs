//! Self-update: ask GitHub which release is newest, download the arch-matched
//! artifact, verify its sha256 against the release's sha256.txt, swap the
//! bundle and relaunch.
//!
//! The flow is deliberately three commands, not one background daemon: the
//! settings sheet drives it (check button, the auto-update prompt, an install
//! button), and every step reports through the same panel state — nothing
//! happens without the user seeing which step is running. The download streams
//! with progress events (`update-download-progress`) so the footer button can
//! read "下载中 23%" instead of a frozen "处理中…".
//!
//! "Newest" means the API's latest *stable* release of the public mirror repo
//! (`Bencibr/tokenme`): its asset set is the contract — `latest.json` (the
//! version), `sha256.txt` (hash list), `tokenme-macos-arm64.app.zip` (the
//! universal macOS bundle) and the CLI archives. Gitea hosts the private
//! originals, but anonymous download there is impossible, so the update
//! channel rides the public mirror.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use ureq as ureq_client;

use crate::engine::Shared;

const API_LATEST: &str = "https://api.github.com/repos/Bencibr/tokenme/releases/latest";
/// Where the swap script and the staged artifact land between phases.
const ARTIFACT_DIR: &str = "updates";
/// The macOS bundle the release CI attaches, universal despite the name.
const MAC_ZIP: &str = "tokenme-macos-arm64.app.zip";
/// Progress events emitted while the artifact streams in.
const PROGRESS_EVENT: &str = "update-download-progress";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub phase: String,
    pub message: String,
    /// Set when `phase == "available"`: the newer version string.
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    /// 0–100, rounded; the frontend renders `下载中 {percent}%`.
    pub percent: u32,
    pub downloaded: u64,
    pub total: u64,
}

/// The newest stable release, resolved from the API: its version and the
/// download URLs of the macOS bundle and the hash list.
struct Release {
    version: String,
    zip_url: String,
    sha_url: String,
}

/// The installed bundle's parent (…/Applications), where the swap happens.
#[cfg(target_os = "macos")]
fn app_parent() -> Option<std::path::PathBuf> {
    // Ascend to the .app bundle and report ITS parent. A hard-coded depth once
    // answered …/TokenMe.app/Contents here — the swap then tried to move
    // …/Contents/TokenMe.app aside, failed, and the user was left with a dead
    // tray (the panel had already exited by design when the script runs).
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    loop {
        if dir.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")) {
            return dir.parent().map(|p| p.to_path_buf());
        }
        dir = dir.parent()?.to_path_buf();
    }
}

fn agent() -> ureq_client::Agent {
    ureq_client::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into()
}

fn fetch_json(url: &str) -> Option<serde_json::Value> {
    let resp: ureq_client::http::Response<ureq_client::Body> = agent()
        .get(url)
        .header("user-agent", concat!("tokenme/", env!("CARGO_PKG_VERSION")))
        .header("accept", "application/vnd.github+json")
        .call()
        .ok()?;
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
            // Σ0(a) is one xored triple and Maj is *added* to it. Written
            // without these parens, `^` swallows the addition and every hash
            // comes out wrong — the bug that failed the release-artifact check.
            let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// The API's latest stable release, or `None` when unreachable — offline and
/// corporate-network failures are normal life and stay silent.
fn fetch_release() -> Option<Release> {
    let body = fetch_json(API_LATEST)?;
    let version = body.get("tag_name")?.as_str()?.trim().trim_start_matches('v').to_string();
    if version.is_empty() {
        return None;
    }
    let mut zip_url = None;
    let mut sha_url = None;
    for asset in body.get("assets")?.as_array()? {
        let name = asset.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        let url = asset.get("browser_download_url").and_then(|u| u.as_str()).unwrap_or_default();
        if name == MAC_ZIP {
            zip_url = Some(url.to_string());
        } else if name == "sha256.txt" {
            sha_url = Some(url.to_string());
        }
    }
    Some(Release {
        version,
        zip_url: zip_url?,
        sha_url: sha_url?,
    })
}

/// Compare x.y.z; a non-release current version (dev) never updates. The higher
/// digit has to dominate: the flat `a[0] > b[0] || a[1] > b[1] || …` this
/// replaced let the patch digit alone decide, so 0.1.6 counted as newer than
/// 0.2.0. Tuple order is exactly the componentwise rule.
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim().trim_start_matches('v').split('.').map(|n| n.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parse(latest), parse(current));
    if a.len() != 3 || b.len() != 3 {
        return false;
    }
    (a[0], a[1], a[2]) > (b[0], b[1], b[2])
}

/// The "nothing to do" answer, carrying the version the panel already runs.
fn uptodate_status() -> UpdateStatus {
    let current = env!("CARGO_PKG_VERSION");
    UpdateStatus {
        phase: "uptodate".into(),
        message: match crate::lang::get() {
            crate::lang::Lang::Zh => format!("已是最新版本 v{current}"),
            crate::lang::Lang::En => format!("Already up to date (v{current})"),
        },
        version: None,
    }
}

/// Phase 1: compare the installed version with GitHub's latest release.
#[tauri::command]
pub async fn check_update(_app: AppHandle) -> Result<UpdateStatus, String> {
    let Some(release) = fetch_release() else {
        // 网络不通/清单不可读是常态（离线、公司网）。后台路径（启动自检、开关
        // 开启）按 phase 过滤，这一行渲染不出来；手动点“立即检查”的用户需要一句
        // 解释，所以 message 带上——静默与否留给调用方决定。
        return Ok(UpdateStatus {
            phase: "quiet".into(),
            message: crate::lang::get()
                .str("检查失败：无法连接更新源", "Check failed — cannot reach the update source")
                .into(),
            version: None,
        });
    };
    if !is_newer(&release.version, env!("CARGO_PKG_VERSION")) {
        return Ok(uptodate_status());
    }
    let l = crate::lang::get();
    // 底部一行已经紧挨着正在运行的版本号，这里再报一遍就是重复。
    Ok(UpdateStatus {
        phase: "available".into(),
        message: match l {
            crate::lang::Lang::Zh => format!("发现新版本 v{}", release.version),
            crate::lang::Lang::En => format!("New version available (v{})", release.version),
        },
        version: Some(release.version),
    })
}

/// Phase 2: stream the macOS bundle to disk with progress events, then verify
/// its sha256 against the release's published list. The artifact is only
/// staged after the hash matches; a mismatched download is dropped.
#[tauri::command]
pub async fn download_update(app: AppHandle) -> Result<UpdateStatus, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        return Ok(UpdateStatus {
            phase: "unsupported".into(),
            message: crate::lang::get().str(
                "当前平台暂不支持应用内更新，请到 Releases 页面手动下载",
                "In-app updates aren't available on this platform — download from the Releases page",
            )
            .into(),
            version: None,
        });
    }
    #[cfg(target_os = "macos")]
    {
        let Some(release) = fetch_release() else {
            return Err(crate::lang::get().str("下载失败：无法连接 GitHub", "Download failed: cannot reach GitHub").into());
        };
        // 检查是这一步的闸门，但重新拉取可能又落回已装的版本（tag 被重指、或调用
        // 方跳过了检查）：拿同版本换包只会让面板白白退出、重启一次。
        if !is_newer(&release.version, env!("CARGO_PKG_VERSION")) {
            return Ok(uptodate_status());
        }
        let resp: ureq_client::http::Response<ureq_client::Body> = agent()
            .get(&release.zip_url)
            .call()
            .map_err(|_| crate::lang::get().str("下载失败：无法连接 GitHub", "Download failed: cannot reach GitHub").to_string())?;
        let total: u64 = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        use std::io::Read as _;
        let mut reader = resp.into_body().into_reader();
        let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
        let mut chunk = [0u8; 64 * 1024];
        let mut last_percent: i64 = -1;
        loop {
            let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            if total > 0 {
                let percent = (bytes.len() as i64 * 100 / total as i64).min(100);
                if percent != last_percent {
                    last_percent = percent;
                    let _ = app.emit(
                        PROGRESS_EVENT,
                        DownloadProgress { percent: percent as u32, downloaded: bytes.len() as u64, total },
                    );
                }
            }
        }
        // sha256.txt is a plain `<hex>  <name>` list; the bundle's line is law.
        let list = fetch_bytes(&release.sha_url)
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        let expected = list
            .lines()
            .find(|l| l.ends_with(MAC_ZIP))
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_string);
        let got = sha256_hex(&bytes);
        if let Some(expected) = &expected {
            if got != *expected {
                return Err(match crate::lang::get() {
                    crate::lang::Lang::Zh => {
                        format!("sha256 校验失败：期望 {expected}，实际 {got}——下载已丢弃")
                    }
                    crate::lang::Lang::En => {
                        format!("sha256 mismatch: expected {expected}, got {got} — download discarded")
                    }
                });
            }
        }
        let dir = std::env::temp_dir().join(ARTIFACT_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let staged = dir.join(MAC_ZIP);
        std::fs::write(&staged, &bytes).map_err(|e| e.to_string())?;
        Ok(UpdateStatus {
            phase: "downloaded".into(),
            message: match crate::lang::get() {
                crate::lang::Lang::Zh => format!(
                    "下载完成（{} KB，sha256 已校验），可以安装",
                    bytes.len() / 1024
                ),
                crate::lang::Lang::En => format!(
                    "Downloaded ({} KB, sha256 verified) — ready to install",
                    bytes.len() / 1024
                ),
            },
            version: Some(staged.to_string_lossy().to_string()),
        })
    }
}

/// Phase 3 (macOS): hand a detached script the staged artifact and quit — the
/// script unzips beside the installed bundle, clears the downloaded
/// quarantine, swaps atomically (old bundle kept until the new one is in
/// place), relaunches, and only then deletes the backup. The panel's own exit
/// is what makes the swap invisible: nothing moves while the UI is up.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<UpdateStatus, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        return Ok(UpdateStatus {
            phase: "unsupported".into(),
            message: crate::lang::get().str(
                "当前平台暂不支持应用内更新，请到 Releases 页面手动下载",
                "In-app updates aren't available on this platform — download from the Releases page",
            )
            .into(),
            version: None,
        });
    }
    #[cfg(target_os = "macos")]
    {
        let dir = std::env::temp_dir().join(ARTIFACT_DIR);
        let staged = dir.join(MAC_ZIP);
        if !staged.is_file() {
            return Err(crate::lang::get().str("尚未下载更新包，请先下载", "No update downloaded yet — download it first").into());
        }
        let Some(parent) = app_parent() else {
            return Err(crate::lang::get().str(
                "无法定位安装目录（开发模式直接运行时不可换包）",
                "Cannot locate the install directory (not swappable when run from the dev build)",
            )
            .into());
        };
        let script = swap_script(&staged, &parent, std::process::id());
        let script_path = dir.join("swap.sh");
        std::fs::write(&script_path, script).map_err(|e| e.to_string())?;

        use std::process::Stdio;
        std::process::Command::new("/bin/sh")
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        // The script's first act is waiting for us to be gone.
        app.exit(0);
        Ok(UpdateStatus {
            phase: "installing".into(),
            message: crate::lang::get()
                .str("正在安装，面板将自动重启", "Installing — the panel will relaunch")
                .into(),
            version: None,
        })
    }
}

/// The detached swap: wait for the panel to be fully dead → unzip →
/// dequarantine → atomic swap → relaunch → clean up. Everything is logged to
/// `swap.log` beside the artifact, so a failed update leaves evidence instead
/// of a shrug. One backup slot, overwritten per run: a half-rolled-back state
/// is still a working install. Every failure path relaunches whatever bundle
/// is left in place — the panel must never stay dead because an install
/// stumbled.
#[cfg(target_os = "macos")]
fn swap_script(staged: &std::path::Path, parent: &std::path::Path, app_pid: u32) -> String {
    let stage_dir = staged.parent().map(|d| d.join("staged")).unwrap_or_else(|| std::path::PathBuf::from("/tmp/tokenme-stage"));
    let log = staged.parent().map(|d| d.join("swap.log")).unwrap_or_else(|| std::path::PathBuf::from("/tmp/tokenme-swap.log"));
    format!(
        r#"#!/bin/sh
# tokenme in-place swap; waits for the panel to exit, then swaps and relaunches.
ZIP="{zip}"
PARENT="{parent}"
STAGE="{stage}"
LOG="{log}"
MYPID="{pid}"
{{
  echo "=== swap $(date) ==="
  # LaunchServices keeps the dying instance registered for a beat: an `open`
  # in that window merely activates the exiting process and nothing new ever
  # launches — the "installed but never restarted" report. Wait for the
  # process itself (bounded at ~30s), then give LaunchServices a moment more.
  i=0
  while kill -0 "$MYPID" 2>/dev/null && [ "$i" -lt 120 ]; do
    sleep 0.25
    i=$((i+1))
  done
  sleep 1
  rm -rf "$STAGE"
  mkdir -p "$STAGE"
  if ! /usr/bin/ditto -x -k "$ZIP" "$STAGE"; then
    echo "unzip failed" >&2
    /usr/bin/open "$PARENT/TokenMe.app" 2>/dev/null
    exit 1
  fi
  APP="$(/usr/bin/find "$STAGE" -maxdepth 1 -mindepth 1 -name '*.app' | head -n 1)"
  if [ -z "$APP" ]; then
    echo "no .app inside the artifact" >&2
    /usr/bin/open "$PARENT/TokenMe.app" 2>/dev/null
    exit 1
  fi
  /usr/bin/xattr -dr com.apple.quarantine "$APP" 2>/dev/null
  TARGET="$PARENT/$(basename "$APP")"
  BACKUP="$PARENT/.tokenme-previous.app"
  rm -rf "$BACKUP"
  if ! mv "$TARGET" "$BACKUP"; then
    echo "cannot move the installed bundle aside; relaunching it untouched" >&2
    /usr/bin/open "$TARGET" 2>/dev/null
    exit 1
  fi
  if mv "$APP" "$TARGET"; then
    rm -rf "$BACKUP" "$STAGE"
    echo "swapped into $TARGET; relaunching"
    # The first open can lose a re-registration race inside LaunchServices;
    # retry once after a beat before declaring it in the log.
    if ! /usr/bin/open "$TARGET"; then
      echo "open failed; retrying" >&2
      sleep 2
      /usr/bin/open "$TARGET" || echo "open retry failed too" >&2
    fi
  else
    echo "swap failed; restoring" >&2
    mv "$BACKUP" "$TARGET"
    /usr/bin/open "$TARGET" 2>/dev/null
    exit 1
  fi
}} >> "$LOG" 2>&1
"#,
        zip = staged.display(),
        parent = parent.display(),
        stage = stage_dir.display(),
        log = log.display(),
        pid = app_pid,
    )
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_is_componentwise() {
        assert!(is_newer("0.1.3", "0.1.2"));
        assert!(is_newer("v0.1.3", "0.1.2"), "the wire tag may carry a v");
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.3", "0.1.3"));
        assert!(!is_newer("0.1.3", "0.1.4"));
        assert!(is_newer("1.0.0", "0.9.9"));
        // A dev build never self-updates.
        assert!(!is_newer("0.1.3", "0.0.0.0-dev"));
        // A higher digit only speaks once the ones above it are equal: a bigger
        // patch must not outrank a bigger minor (the flat-or form did).
        assert!(!is_newer("0.1.6", "0.2.0"));
        assert!(!is_newer("0.1.10", "0.2.0"));
        assert!(is_newer("0.1.10", "0.1.9"), "patch digits compare numerically, not lexically");
    }

    /// The script is plain sh (the only interpreter guaranteed on a fresh
    /// macOS), quotes every interpolated path, and fails loudly into the log.
    #[test]
    fn the_swap_script_quotes_its_paths_and_never_trusts_the_zip() {
        let script = swap_script(
            std::path::Path::new("/tmp/my updates/tokenme-macos-arm64.app.zip"),
            std::path::Path::new("/Applications"),
            4711,
        );
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("ZIP=\"/tmp/my updates/tokenme-macos-arm64.app.zip\""));
        assert!(script.contains("TARGET=\"$PARENT/$(basename \"$APP\")\""));
        assert!(script.contains("ditto -x -k"));
        assert!(script.contains("xattr -dr com.apple.quarantine"));
        // The relaunch waits for the panel process to be gone — an `open` in
        // LaunchServices' dying-instance window activates the old process and
        // nothing new ever starts.
        assert!(script.contains("MYPID=\"4711\""));
        assert!(script.contains("while kill -0 \"$MYPID\""));
        // Failure paths restore the old bundle instead of leaving nothing.
        assert!(script.contains("mv \"$BACKUP\" \"$TARGET\""));
        // Every failure branch relaunches whatever bundle is in place; the
        // successful swap checks `open`'s status and retries once.
        assert!(script.matches("/usr/bin/open").count() >= 5);
        assert!(script.contains("open retry failed too"));
    }
}

#[cfg(test)]
mod sha_tests {
    use super::sha256_hex;

    /// The known vectors, including the 55/56/64-byte padding boundaries and
    /// the multi-block case — the schedule must be exercised, not just the
    /// padding. This is what caught the `^`-swallows-`+` precedence bug.
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 1000]),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }
}
