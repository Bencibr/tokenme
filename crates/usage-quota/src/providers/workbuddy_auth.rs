//! WorkBuddy's two login records, read without consuming refresh grants.
//! Encrypted fields are opened by the corresponding installed Electron runtime.
//! Protocol reference: https://github.com/88lin/workbuddy-auto-signin (MIT).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub(super) fn record(cn: bool) -> Option<Value> {
    let env = if cn { "WORKBUDDY_CN_AUTH_FILE" } else { "WORKBUDDY_AUTH_FILE" };
    let name = if cn { "workbuddy-desktop.info" } else { "workbuddy-desktop-ai.info" };
    let paths = if let Some(path) = std::env::var_os(env).filter(|v| !v.is_empty()) {
        vec![PathBuf::from(path)]
    } else {
        let mut paths = Vec::new();
        // Windows writes this record under Local, not the roaming data dir.
        for base in [dirs::data_local_dir(), dirs::data_dir()].into_iter().flatten() {
            paths.push(base.join("CodeBuddyExtension/Data/Public/auth").join(name));
        }
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(if cn { ".workbuddy" } else { ".workbuddy-ai" }).join("auth").join(name));
        }
        paths
    };
    paths.into_iter().find_map(|p| {
        let mut info: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        if info.get("auth")?.get("domain").is_none() {
            info["auth"]["domain"] = Value::String(if cn { "www.workbuddy.cn" } else { "www.workbuddy.ai" }.into());
        }
        matches_edition(&info, cn).then_some(info)
    })
}

pub(super) fn matches_edition(info: &Value, cn: bool) -> bool {
    match info.pointer("/auth/domain").and_then(Value::as_str) {
        Some("workbuddy.cn" | "www.workbuddy.cn" | "codebuddy.cn" | "www.codebuddy.cn" | "copilot.tencent.com") => cn,
        Some("workbuddy.ai" | "www.workbuddy.ai") => !cn,
        _ => false,
    }
}

pub(super) fn credentials(cn: bool) -> Option<Value> {
    let mut info = record(cn)?;
    if info.pointer("/auth/expiresAt").and_then(Value::as_i64)
        .is_some_and(|ms| ms <= chrono::Utc::now().timestamp_millis()) { return None; }
    let field = info.pointer("/auth/accessToken")?;
    let token = if let Some(token) = field.as_str().filter(|t| !t.is_empty()) {
        token.to_string()
    } else {
        let mut cmd = Command::new(runtime(cn)?);
        cmd.args(["-e", DECRYPT_JS]);
        for (key, _) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_uppercase();
            if upper.starts_with("NODE_") || upper.starts_with("ELECTRON_") || upper.starts_with("WORKBUDDY_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("ELECTRON_RUN_AS_NODE", "1");
        let reply: Value = serde_json::from_slice(&run(cmd, field.to_string().into_bytes(), Duration::from_secs(10))?).ok()?;
        reply.get("accessToken")?.as_str().filter(|t| !t.is_empty() && t.len() <= 32768)?.to_string()
    };
    info["auth"]["accessToken"] = Value::String(token);
    Some(info)
}

fn runtime(cn: bool) -> Option<PathBuf> {
    let env = if cn { "WORKBUDDY_CN_EXE" } else { "WORKBUDDY_EXE" };
    if let Some(path) = std::env::var_os(env).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let product = if cn { "WorkBuddy" } else { "WorkBuddyAI" };
    #[cfg(windows)]
    {
        let mut cmd = Command::new("powershell.exe");
        // Product is a fixed string, never input interpolated into shell code.
        let pattern = if cn { "^WorkBuddy(?:\\s+\\d|$)" } else { "^WorkBuddy\\s?AI(?:\\s+\\d|$)" };
        let script = format!("$n='{product}.exe'; Get-CimInstance Win32_Process -Filter \"Name='$n'\" | Select-Object -First 1 -ExpandProperty ExecutablePath; Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*','HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*','HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' -ErrorAction SilentlyContinue | Where-Object {{$_.DisplayName -match '{pattern}'}} | ForEach-Object {{if($_.InstallLocation){{Join-Path $_.InstallLocation $n}}; if($_.DisplayIcon){{($_.DisplayIcon -replace ',[-0-9]+$','').Trim([char]34)}}}}" );
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        if let Some(out) = run(cmd, Vec::new(), Duration::from_secs(5)) {
            if let Ok(text) = String::from_utf8(out) {
                if let Some(path) = text.lines().map(str::trim).map(PathBuf::from)
                    .find(|p| p.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&format!("{product}.exe")))) {
                    return Some(path);
                }
            }
        }
        for base in [dirs::data_local_dir().map(|p| p.join("Programs")), std::env::var_os("ProgramFiles").map(PathBuf::from)].into_iter().flatten() {
            let path = base.join(product).join(format!("{product}.exe"));
            if path.is_file() { return Some(path); }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let product = if cn { "WorkBuddy" } else { "WorkBuddy AI" };
        for base in [Some(PathBuf::from("/Applications")), dirs::home_dir().map(|h| h.join("Applications"))].into_iter().flatten() {
            let contents = base.join(format!("{product}.app/Contents"));
            let mut cmd = Command::new("/usr/libexec/PlistBuddy");
            cmd.args(["-c", "Print :CFBundleExecutable"]).arg(contents.join("Info.plist"));
            if let Some(out) = run(cmd, Vec::new(), Duration::from_secs(2)) {
                if let Ok(name) = String::from_utf8(out) {
                    let name = name.trim();
                    if !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']) {
                        let path = contents.join("MacOS").join(name);
                        if path.is_file() { return Some(path); }
                    }
                }
            }
        }
    }
    let _ = product;
    None
}

/// Bound process lifetime and pipe output; neither tokens nor runtime errors
/// are printed, written to disk, or included in command-line arguments.
fn run(mut cmd: Command, input: Vec<u8>, timeout: Duration) -> Option<Vec<u8>> {
    if input.len() > 65536 { return None; }
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    std::thread::spawn(move || { let _ = stdin.write_all(&input); });
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(65537).read_to_end(&mut bytes);
        let _ = tx.send(result.ok().filter(|_| bytes.len() <= 65536).map(|_| bytes));
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then(|| rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok().flatten()).flatten(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => { let _ = child.kill(); let _ = child.wait(); return None; }
        }
    }
}

// Reconstructed from the installed runtime's storage envelope, using the
// native binding to keep all build key material inside the child process.
const DECRYPT_JS: &str = r#"
const crypto = require('crypto');
const fail = () => { throw Error('invalid credential'); };
const b64 = (s, n) => {
  if (typeof s !== 'string' || s.length > 65536) fail();
  const b = Buffer.from(s, 'base64');
  if (b.toString('base64') !== s || (n !== undefined && b.length !== n)) fail();
  return b;
};
let input = '', size = 0;
process.stdin.on('data', b => { size += b.length; if (size > 65536) process.exit(1); input += b; });
process.stdin.on('end', () => {
  let key, plain;
  try {
    const field = JSON.parse(input); input = '';
    if (field.$wbEncrypted !== 1) fail();
    const e = JSON.parse(b64(field.envelope).toString('utf8'));
    if (e.suite !== 1 || !/^[0-9a-f]{16}$/.test(e.keyId)) fail();
    const native = process._linkedBinding('electron_browser_workbuddy_storage');
    const payload = JSON.parse(native.loggerGet());
    if (payload.version !== 1) fail();
    const secret = b64(payload.atRestSecretKey, 32);
    const empty = secret.every(b => b === 0); secret.fill(0); if (empty) fail();
    key = crypto.createHash('sha256').update(payload.atRestSecretKey, 'utf8').digest();
    if (crypto.createHash('sha256').update(key).digest('hex').slice(0, 16) !== e.keyId) fail();
    const lp = s => { const b = Buffer.from(s); const n = Buffer.alloc(4); n.writeUInt32BE(b.length); return Buffer.concat([n,b]); };
    const aad = Buffer.concat([Buffer.from('WB-AAD\0'),Buffer.from([1]),lp('WBEV1'),lp('sym-v1'),Buffer.from([0,0,0,1]),lp(e.keyId),Buffer.from([2,0,0])]);
    const cipher = crypto.createDecipheriv('aes-256-gcm', key, b64(e.nonce,12));
    cipher.setAAD(aad); cipher.setAuthTag(b64(e.authTag,16));
    plain = Buffer.concat([cipher.update(b64(e.ciphertext)),cipher.final()]);
    const token = plain.toString('utf8');
    if (!Buffer.from(token).equals(plain) || !/^[A-Za-z0-9._~+\/-]+=*$/.test(token) || token.length > 32768) fail();
    process.stdout.write(JSON.stringify({accessToken:token}), () => process.exit(0));
  } catch (_) { process.exit(1); }
  finally { if (key) key.fill(0); if (plain) plain.fill(0); }
});
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_credential_never_crosses_editions_or_uses_an_untrusted_domain() {
        for domain in ["www.workbuddy.cn", "www.codebuddy.cn"] {
            let info = json!({"auth":{"domain":domain}});
            assert!(matches_edition(&info, true));
            assert!(!matches_edition(&info, false));
        }
        let global = json!({"auth":{"domain":"www.workbuddy.ai"}});
        assert!(matches_edition(&global, false));
        assert!(!matches_edition(&global, true));
        for domain in ["evil.workbuddy.cn", "www.workbuddy.cn.evil.invalid", "www.workbuddy.cn/path"] {
            assert!(!matches_edition(&json!({"auth":{"domain":domain}}), true));
        }
    }
}
