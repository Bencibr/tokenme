use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use usage_core::origin_ok;

use crate::store::{SyncError, SyncResult};

pub(crate) mod signature_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &[u8; 64], serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        serializer.serialize_bytes(value)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 64], D::Error>
    where D: Deserializer<'de> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        bytes.try_into().map_err(|_| serde::de::Error::custom("signature must be 64 bytes"))
    }
}

pub const PROTOCOL_ID: &str = "tokenme-device-sync/1";
pub const SYNC_FORMAT: &str = "tokenme-sync/1";

pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_COMPRESSED_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_DECOMPRESSED_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_ROWS: usize = 500_000;
pub const MAX_STRING_BYTES: usize = 16 * 1024;
const MAX_CALLS_PER_ROW: usize = 4096;
const IDENTITY_DOMAIN: &[u8] = b"tokenme-device-sync/identity/v1";
const UPLOAD_DOMAIN: &[u8] = b"tokenme-device-sync/upload-signature/v1";
const PAIRING_DOMAIN: &[u8] = b"tokenme-device-sync/pairing/v1";
const STATUS_DOMAIN: &[u8] = b"tokenme-device-sync/status/v1";
const DISPLAY_NAME_DOMAIN: &[u8] = b"tokenme-device-sync/display-name/v1";
const DOWNLOAD_DOMAIN: &[u8] = b"tokenme-device-sync/download/v1";
// A status request and a disconnect notice share one struct shape. The
// purpose is signed so a captured status request cannot be replayed as a
// disconnect (which would revoke the device).
pub(crate) const STATUS_PURPOSE: &str = "status";
pub(crate) const DISCONNECT_PURPOSE: &str = "disconnect";
pub(crate) const MAX_AUTH_REQUEST_AGE_MS: i64 = 10 * 60 * 1000;
pub(crate) const MAX_AUTH_REQUEST_FUTURE_SKEW_MS: i64 = 60 * 1000;

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Fixed domain + u64 big-endian length prefixes. Every identity and
/// signature call goes through this function; ambiguous delimiter joins are
/// intentionally not available to callers.
pub fn identity_bytes(domain: &[u8], fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(domain.len() + fields.iter().map(|f| f.len() + 8).sum::<usize>() + 8);
    append_part(&mut out, domain);
    for field in fields {
        append_part(&mut out, field);
    }
    out
}

fn append_part(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn random_bytes<const N: usize>() -> SyncResult<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| SyncError::Crypto(format!("OS randomness unavailable: {e}")))?;
    Ok(bytes)
}

pub(crate) fn valid_text(label: &str, value: &str, allow_empty: bool) -> SyncResult<()> {
    if !allow_empty && value.is_empty() {
        return Err(SyncError::Invalid(format!("{label} must not be empty")));
    }
    if value.len() > MAX_STRING_BYTES {
        return Err(SyncError::Limit(format!("{label} exceeds {MAX_STRING_BYTES} bytes")));
    }
    if value.as_bytes().contains(&0) || value.chars().any(char::is_control) {
        return Err(SyncError::Invalid(format!("{label} contains a control character")));
    }
    Ok(())
}

pub(crate) fn validate_certificate_pin(pin: &str) -> SyncResult<()> {
    if pin.len() != 64 || !pin.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SyncError::Invalid("certificate pin must be exactly 64 hexadecimal characters".into()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IdentityFile {
    version: u8,
    device_id: String,
    origin: String,
    display_name: String,
    public_key_b64: String,
    secret_key_b64: String,
}

#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub origin: String,
    pub display_name: String,
    pub signing_key: SigningKey,
}

impl DeviceIdentity {
    pub fn create(origin: impl Into<String>, display_name: impl Into<String>) -> SyncResult<Self> {
        let secret = random_bytes::<32>()?;
        let signing_key = SigningKey::from_bytes(&secret);
        let device_id = format!("dev-{}", hex(&hash_bytes(&signing_key.verifying_key().to_bytes())));
        let origin = origin.into();
        let display_name = display_name.into();
        valid_text("origin", &origin, false)?;
        if !origin_ok(&origin) {
            return Err(SyncError::Invalid("origin contains a path separator or ':'".into()));
        }
        valid_text("display_name", &display_name, false)?;
        Ok(Self { device_id, origin, display_name, signing_key })
    }

    pub fn load_or_create(
        path: impl Into<PathBuf>,
        origin: impl Into<String>,
        display_name: impl Into<String>,
    ) -> SyncResult<Self> {
        let path = path.into();
        ensure_private_dir(path.parent().unwrap_or(Path::new(".")))?;
        if fs::symlink_metadata(&path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(SyncError::Security(format!("identity path is a symlink: {}", path.display())));
        }
        if path.exists() {
            verify_private_file(&path)?;
            let raw = fs::read(&path).map_err(|e| SyncError::Io(path.clone(), e))?;
            if raw.len() > MAX_MANIFEST_BYTES {
                return Err(SyncError::Limit("identity file is too large".into()));
            }
            let file: IdentityFile = serde_json::from_slice(&raw)
                .map_err(|e| SyncError::Invalid(format!("invalid identity file: {e}")))?;
            if file.version != 1 {
                return Err(SyncError::Unsupported(format!("identity version {}", file.version)));
            }
            let secret = base64::engine::general_purpose::STANDARD
                .decode(file.secret_key_b64)
                .map_err(|e| SyncError::Invalid(format!("invalid identity secret: {e}")))?;
            let public = base64::engine::general_purpose::STANDARD
                .decode(file.public_key_b64)
                .map_err(|e| SyncError::Invalid(format!("invalid identity public key: {e}")))?;
            let secret: [u8; 32] = secret
                .try_into()
                .map_err(|_| SyncError::Invalid("identity secret must be 32 bytes".into()))?;
            let public: [u8; 32] = public
                .try_into()
                .map_err(|_| SyncError::Invalid("identity public key must be 32 bytes".into()))?;
            let signing_key = SigningKey::from_bytes(&secret);
            if signing_key.verifying_key().to_bytes() != public {
                return Err(SyncError::Security("identity public key does not match its secret".into()));
            }
            let expected = format!("dev-{}", hex(&hash_bytes(&public)));
            if file.device_id != expected {
                return Err(SyncError::Security("identity device_id does not match its public key".into()));
            }
            valid_text("origin", &file.origin, false)?;
            valid_text("display_name", &file.display_name, false)?;
            if !origin_ok(&file.origin) {
                return Err(SyncError::Invalid("stored origin is not valid".into()));
            }
            return Ok(Self { device_id: file.device_id, origin: file.origin, display_name: file.display_name, signing_key });
        }
        let identity = Self::create(origin, display_name)?;
        identity.save(&path)?;
        Ok(identity)
    }

    pub fn save(&self, path: &Path) -> SyncResult<()> {
        ensure_private_dir(path.parent().unwrap_or(Path::new(".")))?;
        if fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(SyncError::Security(format!("identity path is a symlink: {}", path.display())));
        }
        let file = IdentityFile {
            version: 1,
            device_id: self.device_id.clone(),
            origin: self.origin.clone(),
            display_name: self.display_name.clone(),
            public_key_b64: base64::engine::general_purpose::STANDARD.encode(self.signing_key.verifying_key().as_bytes()),
            secret_key_b64: base64::engine::general_purpose::STANDARD.encode(self.signing_key.to_bytes()),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| SyncError::Invalid(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        if let Ok(meta) = fs::symlink_metadata(&tmp) {
            if meta.file_type().is_symlink() {
                return Err(SyncError::Security(format!("identity temporary path is a symlink: {}", tmp.display())));
            }
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut out = options.open(&tmp).map_err(|e| SyncError::Io(tmp.clone(), e))?;
        let write_result = (|| {
            set_private_file(&out, &tmp)?;
            out.write_all(&bytes).map_err(|e| SyncError::Io(tmp.clone(), e))?;
            out.sync_all().map_err(|e| SyncError::Io(tmp.clone(), e))
        })();
        drop(out);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        // Hard-link installation is deliberately exclusive: a concurrent
        // creator may win, but it can never be silently replaced.
        if let Err(error) = fs::hard_link(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(SyncError::Io(path.to_path_buf(), error));
        }
        fs::remove_file(&tmp).map_err(|e| SyncError::Io(tmp, e))?;
        Ok(())
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }
}

pub(crate) fn ensure_private_dir(path: &Path) -> SyncResult<()> {
    let _created = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(SyncError::Security(format!("private directory is not a real directory: {}", path.display())));
            }
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))?;
            true
        }
        Err(error) => return Err(SyncError::Io(path.to_path_buf(), error)),
    };
    let meta = fs::symlink_metadata(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(SyncError::Security(format!("private directory is not a real directory: {}", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(SyncError::Security(format!("private directory is accessible by group or other: {}", path.display())));
        }
    }
    #[cfg(windows)]
    if _created {
        set_current_sid_dacl(path)?;
    }
    Ok(())
}

pub(crate) fn set_private_file(file: &std::fs::File, path: &Path) -> SyncResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| SyncError::Io(path.to_path_buf(), e))?;
    }
    #[cfg(windows)]
    {
        let _ = file;
        set_current_sid_dacl(path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn set_current_sid_dacl(path: &Path) -> SyncResult<()> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER};
    use windows_sys::Win32::Security::Authorization::{
        BuildTrusteeWithSidW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
        GRANT_ACCESS, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, TOKEN_USER, TOKEN_QUERY, TokenUser, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::Security::ACL;

    let mut token = null_mut();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(SyncError::Security(format!("open current Windows token failed: {}", GetLastError())));
        }
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
        if GetLastError() != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
            CloseHandle(token);
            return Err(SyncError::Security(format!("query current Windows SID size failed: {}", GetLastError())));
        }
        let mut buffer = vec![0u8; needed as usize];
        if GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            let error = GetLastError();
            CloseHandle(token);
            return Err(SyncError::Security(format!("query current Windows SID failed: {error}")));
        }
        let token_user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
        let sid_len = GetLengthSid(token_user.User.Sid);
        if sid_len == 0 {
            CloseHandle(token);
            return Err(SyncError::Security("current Windows SID is invalid".into()));
        }
        let sid = std::slice::from_raw_parts(token_user.User.Sid.cast::<u8>(), sid_len as usize);
        let mut sid_copy = sid.to_vec();
        let mut entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: windows_sys::Win32::Security::NO_INHERITANCE,
            Trustee: Default::default(),
        };
        BuildTrusteeWithSidW(&mut entry.Trustee, sid_copy.as_mut_ptr().cast());
        let mut acl: *mut ACL = null_mut();
        let acl_error = SetEntriesInAclW(1, &entry, null(), &mut acl);
        if acl_error != 0 || acl.is_null() {
            CloseHandle(token);
            return Err(SyncError::Security(format!("build private Windows DACL failed: {acl_error}")));
        }
        let mut wide = path.as_os_str().encode_wide().chain(once(0)).collect::<Vec<_>>();
        let set_error = SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null_mut(),
        );
        LocalFree(acl.cast());
        CloseHandle(token);
        if set_error != 0 {
            return Err(SyncError::Security(format!("set private Windows DACL failed: {set_error}")));
        }
    }
    Ok(())
}

pub(crate) fn verify_private_file(path: &Path) -> SyncResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SyncError::Security(format!("private file is not a regular file: {}", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(SyncError::Security(format!("identity file is accessible by group or other: {}", path.display())));
        }
    }
    #[cfg(windows)]
    {
        // Re-apply the private DACL on every load so an existing identity
        // cannot silently become readable after an ACL change.
        set_current_sid_dacl(path)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkCounts {
    #[serde(rename = "in", default)]
    pub in_tok: Option<f64>,
    #[serde(default)]
    pub cc_tok: Option<f64>,
    #[serde(default)]
    pub cr_tok: Option<f64>,
    #[serde(rename = "out", default)]
    pub out_tok: Option<f64>,
    #[serde(default)]
    pub reason_tok: Option<f64>,
    #[serde(default)]
    pub credits: Option<f64>,
}

impl NetworkCounts {
    pub fn all_zero() -> Self {
        Self { in_tok: Some(0.0), cc_tok: Some(0.0), cr_tok: Some(0.0), out_tok: Some(0.0), reason_tok: Some(0.0), credits: Some(0.0) }
    }

    pub(crate) fn validate(&self) -> SyncResult<()> {
        let present = [&self.in_tok, &self.cc_tok, &self.cr_tok, &self.out_tok, &self.reason_tok]
            .into_iter().filter(|v| v.is_some()).count();
        if present != 0 && present != 5 {
            return Err(SyncError::Protocol("tokens vector is partially present; missing is not zero".into()));
        }
        for value in [&self.in_tok, &self.cc_tok, &self.cr_tok, &self.out_tok, &self.reason_tok, &self.credits]
            .into_iter().flatten()
        {
            if !value.is_finite() || *value < 0.0 {
                return Err(SyncError::Invalid("counts must be finite and non-negative".into()));
            }
        }
        let mut total = 0.0;
        for value in [&self.in_tok, &self.cc_tok, &self.cr_tok, &self.out_tok, &self.reason_tok]
            .into_iter().flatten()
        {
            total += *value;
            if !total.is_finite() {
                return Err(SyncError::Limit("row token total is not finite".into()));
            }
        }
        Ok(())
    }

}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkCall {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkRow {
    /// Original stable key. Keyless rows may carry an adapter-persisted
    /// occurrence_id; the strict exporter refuses rows with neither.
    pub key: Option<String>,
    #[serde(default)]
    pub occurrence_id: Option<String>,
    pub tool: String,
    pub ts_ms: i64,
    pub session: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub meter: String,
    pub source: String,
    pub counts: NetworkCounts,
    #[serde(default)]
    pub calls: Vec<NetworkCall>,
}

impl NetworkRow {
    pub(crate) fn identity_material(&self) -> SyncResult<&str> {
        match (&self.key, &self.occurrence_id) {
            (Some(key), _) if !key.is_empty() => Ok(key),
            (None, Some(occurrence)) if !occurrence.is_empty() => Ok(occurrence),
            _ => Err(SyncError::UnstableOccurrence),
        }
    }

    fn validate(&self, manifest: &NetworkManifest) -> SyncResult<()> {
        let material = self.identity_material()?;
        valid_text("row identity", material, false)?;
        for (label, value) in [
            ("tool", self.tool.as_str()), ("session", self.session.as_str()),
            ("source", self.source.as_str()), ("meter", self.meter.as_str()),
        ] { valid_text(label, value, false)?; }
        for (label, value) in [("project", self.project.as_deref()), ("model", self.model.as_deref())] {
            if let Some(value) = value { valid_text(label, value, false)?; }
        }
        if self.meter != "tokens" && self.meter != "credits" {
            return Err(SyncError::Protocol(format!("unknown meter {:?}", self.meter)));
        }
        if self.ts_ms < manifest.window_lo_ms || self.ts_ms >= manifest.window_hi_ms {
            return Err(SyncError::Protocol(format!("event timestamp {} is outside the package window", self.ts_ms)));
        }
        self.counts.validate()?;
        if self.calls.len() > MAX_CALLS_PER_ROW {
            return Err(SyncError::Limit(format!("calls exceed {MAX_CALLS_PER_ROW} per row")));
        }
        for call in &self.calls {
            valid_text("call kind", &call.kind, false)?;
            valid_text("call name", &call.name, false)?;
            if call.kind != "mcp" && call.kind != "skill" {
                return Err(SyncError::Protocol(format!("unknown call kind {:?}", call.kind)));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSums {
    #[serde(rename = "in")]
    pub in_tok: f64,
    pub cc_tok: f64,
    pub cr_tok: f64,
    #[serde(rename = "out")]
    pub out_tok: f64,
    pub reason_tok: f64,
    pub credits: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkManifest {
    pub format: String,
    pub schema_version: String,
    pub origin: String,
    pub window_lo_ms: i64,
    pub window_hi_ms: i64,
    pub rows: u64,
    pub sums: NetworkSums,
    pub generated_at_ms: i64,
    /// True only on the final package of a secondary's initial snapshot.
    /// `default` keeps manifests produced by older clients readable while
    /// making completion explicit for new clients.
    #[serde(default)]
    pub snapshot_complete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UploadEnvelope {
    pub protocol_id: String,
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub stream_id: String,
    pub data_epoch: u64,
    pub generation: u64,
    pub manifest_bytes: Vec<u8>,
    pub gzip_sha256: [u8; 32],
}

impl UploadEnvelope {
    pub fn signing_bytes(&self) -> Vec<u8> {
        identity_bytes(
            UPLOAD_DOMAIN,
            &[
                self.protocol_id.as_bytes(), self.primary_id.as_bytes(), self.device_id.as_bytes(),
                self.origin.as_bytes(), self.stream_id.as_bytes(), &self.data_epoch.to_be_bytes(),
                &self.generation.to_be_bytes(), &self.manifest_bytes, &self.gzip_sha256,
            ],
        )
    }

    pub(crate) fn validate(&self) -> SyncResult<()> {
        if self.protocol_id != PROTOCOL_ID {
            return Err(SyncError::Unsupported(format!("protocol {:?}", self.protocol_id)));
        }
        for (label, value) in [
            ("primary_id", self.primary_id.as_str()), ("device_id", self.device_id.as_str()),
            ("origin", self.origin.as_str()), ("stream_id", self.stream_id.as_str()),
        ] { valid_text(label, value, false)?; }
        if self.generation == 0 || self.data_epoch == 0 || self.generation > i64::MAX as u64 || self.data_epoch > i64::MAX as u64 {
            return Err(SyncError::Invalid("generation and data_epoch start at 1".into()));
        }
        if self.manifest_bytes.is_empty() || self.manifest_bytes.len() > MAX_MANIFEST_BYTES {
            return Err(SyncError::Limit(format!("manifest exceeds {MAX_MANIFEST_BYTES} bytes")));
        }
        Ok(())
    }

    pub(crate) fn verify_signature(&self, public_key: &VerifyingKey, signature: &[u8; 64]) -> SyncResult<()> {
        self.validate()?;
        public_key.verify(&self.signing_bytes(), &Signature::from_bytes(signature))
            .map_err(|_| SyncError::Auth("upload signature verification failed".into()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignedUpload {
    pub envelope: UploadEnvelope,
    pub signature: [u8; 64],
    pub gzip_bytes: Vec<u8>,
}

impl SignedUpload {
    pub fn sign(envelope: UploadEnvelope, signing_key: &SigningKey, gzip_bytes: Vec<u8>) -> SyncResult<Self> {
        envelope.validate()?;
        if gzip_bytes.len() > MAX_COMPRESSED_BYTES {
            return Err(SyncError::Limit(format!("compressed package exceeds {MAX_COMPRESSED_BYTES} bytes")));
        }
        let digest = hash_bytes(&gzip_bytes);
        if digest != envelope.gzip_sha256 {
            return Err(SyncError::Invalid("gzip hash does not match envelope".into()));
        }
        let signature = signing_key.sign(&envelope.signing_bytes()).to_bytes();
        Ok(Self { envelope, signature, gzip_bytes })
    }

    pub fn verify(&self, public_key: &VerifyingKey) -> SyncResult<()> {
        self.envelope.validate()?;
        if self.gzip_bytes.len() > MAX_COMPRESSED_BYTES {
            return Err(SyncError::Limit(format!("compressed package exceeds {MAX_COMPRESSED_BYTES} bytes")));
        }
        if hash_bytes(&self.gzip_bytes) != self.envelope.gzip_sha256 {
            return Err(SyncError::Auth("gzip hash mismatch".into()));
        }
        self.envelope.verify_signature(public_key, &self.signature)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetworkPackage {
    pub manifest_bytes: Vec<u8>,
    pub gzip_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PrimaryDownloadPage {
    pub request_nonce: [u8; 16],
    pub enabled: bool,
    pub preparing: bool,
    pub package: Option<NetworkPackage>,
    pub next_identity: Option<String>,
    pub next_dirty_revision: u64,
    pub complete: bool,
}

impl NetworkPackage {
    pub fn from_rows(
        origin: impl Into<String>, schema_version: impl Into<String>, window_lo_ms: i64,
        window_hi_ms: i64, rows: &[NetworkRow], generated_at_ms: i64,
    ) -> SyncResult<Self> {
        Self::from_rows_with_snapshot_complete(origin, schema_version, window_lo_ms, window_hi_ms, rows, generated_at_ms, false)
    }

    pub fn from_rows_with_snapshot_complete(
        origin: impl Into<String>, schema_version: impl Into<String>, window_lo_ms: i64,
        window_hi_ms: i64, rows: &[NetworkRow], generated_at_ms: i64, snapshot_complete: bool,
    ) -> SyncResult<Self> {
        if rows.len() > MAX_ROWS {
            return Err(SyncError::Limit(format!("rows exceed {MAX_ROWS}")));
        }
        let origin = origin.into();
        let schema_version = schema_version.into();
        valid_text("manifest origin", &origin, false)?;
        if !origin_ok(&origin) || window_lo_ms >= window_hi_ms {
            return Err(SyncError::Protocol("invalid package origin/window".into()));
        }
        let stub = NetworkManifest { format: SYNC_FORMAT.into(), schema_version: schema_version.clone(), origin: origin.clone(), window_lo_ms, window_hi_ms, rows: rows.len() as u64, sums: NetworkSums::default(), generated_at_ms, snapshot_complete };
        let mut plain = Vec::new();
        let mut sums = NetworkSums::default();
        let mut identities = BTreeSet::new();
        for row in rows {
            row.validate(&stub)?;
            if !identities.insert(row.identity_material()?.to_string()) {
                return Err(SyncError::Protocol("duplicate row identity in package".into()));
            }
            if row.source.starts_with("linux:") {
                return Err(SyncError::Protocol("strict export cannot re-export remote source rows".into()));
            }
            let line = serde_json::to_vec(row).map_err(|e| SyncError::Protocol(format!("encode row: {e}")))?;
            if line.len() + 1 > MAX_LINE_BYTES { return Err(SyncError::Limit("row line too large".into())); }
            plain.extend_from_slice(&line);
            plain.push(b'\n');
            add_sums(&mut sums, &row.counts)?;
        }
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&plain).map_err(|e| SyncError::Io(PathBuf::from("package"), e))?;
        let gzip_bytes = encoder.finish().map_err(|e| SyncError::Io(PathBuf::from("package"), e))?;
        if gzip_bytes.len() > MAX_COMPRESSED_BYTES { return Err(SyncError::Limit("compressed package too large".into())); }
        let manifest = NetworkManifest { format: SYNC_FORMAT.into(), schema_version, origin, window_lo_ms, window_hi_ms, rows: rows.len() as u64, sums, generated_at_ms, snapshot_complete };
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(|e| SyncError::Protocol(format!("encode manifest: {e}")))?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES { return Err(SyncError::Limit("manifest too large".into())); }
        Ok(Self { manifest_bytes, gzip_bytes })
    }

    pub fn validate(&self) -> SyncResult<(NetworkManifest, Vec<NetworkRow>)> {
        let mut rows = Vec::new();
        let manifest = validate_network_reader(
            &self.manifest_bytes,
            Cursor::new(self.gzip_bytes.as_slice()),
            self.gzip_bytes.len(),
            hash_bytes(&self.gzip_bytes),
            |row| { rows.push(row); Ok(()) },
        )?;
        Ok((manifest, rows))
    }
}

/// Validate a package without materialising its decompressed JSONL body. The
/// callback runs once per bounded row while the caller's transaction is open;
/// a bad compressed hash, manifest, row, or aggregate makes the whole caller
/// operation fail. This is shared by memory and staging-file imports.
pub(crate) fn validate_network_reader<R, F>(
    manifest_bytes: &[u8], compressed: R, compressed_len: usize, expected_hash: [u8; 32], mut on_row: F,
) -> SyncResult<NetworkManifest>
where
    R: Read,
    F: FnMut(NetworkRow) -> SyncResult<()>,
{
    if manifest_bytes.len() > MAX_MANIFEST_BYTES { return Err(SyncError::Limit("manifest too large".into())); }
    if compressed_len > MAX_COMPRESSED_BYTES { return Err(SyncError::Limit("compressed package too large".into())); }
    let manifest: NetworkManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|e| SyncError::Protocol(format!("invalid manifest: {e}")))?;
    if manifest.format != SYNC_FORMAT { return Err(SyncError::Unsupported(format!("inner format {:?}", manifest.format))); }
    valid_text("manifest origin", &manifest.origin, false)?;
    if !origin_ok(&manifest.origin) || manifest.window_lo_ms >= manifest.window_hi_ms {
        return Err(SyncError::Protocol("invalid manifest origin/window".into()));
    }
    if manifest.rows > MAX_ROWS as u64 { return Err(SyncError::Limit(format!("rows exceed {MAX_ROWS}"))); }

    let mut hashing = HashingReader::new(compressed, compressed_len);
    let mut decoder = GzDecoder::new(&mut hashing);
    let mut reader = BufReader::new(&mut decoder);
    let mut decompressed = 0usize;
    let mut rows = 0usize;
    let mut sums = NetworkSums::default();
    let mut identities = BTreeSet::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = read_line_bounded(&mut reader, &mut line)?;
        if n == 0 { break; }
        decompressed = decompressed.checked_add(n).ok_or_else(|| SyncError::Limit("decompressed package too large".into()))?;
        if decompressed > MAX_DECOMPRESSED_BYTES { return Err(SyncError::Limit("decompressed package too large".into())); }
        if line.len() > MAX_LINE_BYTES { return Err(SyncError::Limit(format!("line exceeds {MAX_LINE_BYTES} bytes"))); }
        if line.iter().all(|b| b.is_ascii_whitespace()) { continue; }
        let row: NetworkRow = serde_json::from_slice(&line).map_err(|e| SyncError::Protocol(format!("invalid row {}: {e}", rows + 1)))?;
        row.validate(&manifest)?;
        let identity = row.identity_material()?.to_string();
        if !identities.insert(identity) { return Err(SyncError::Protocol("duplicate row identity in package".into())); }
        rows = rows.checked_add(1).ok_or_else(|| SyncError::Limit(format!("rows exceed {MAX_ROWS}")))?;
        if rows > MAX_ROWS { return Err(SyncError::Limit(format!("rows exceed {MAX_ROWS}"))); }
        add_sums(&mut sums, &row.counts)?;
        on_row(row)?;
    }
    drop(reader);
    drop(decoder);
    if hashing.remaining != 0 { return Err(SyncError::Protocol("gzip body ended before content-length".into())); }
    if hashing.finish() != expected_hash { return Err(SyncError::Auth("gzip hash mismatch".into())); }
    if rows as u64 != manifest.rows { return Err(SyncError::Protocol("manifest row count mismatch".into())); }
    if sums != manifest.sums { return Err(SyncError::Protocol("manifest sums mismatch".into())); }
    Ok(manifest)
}

fn read_line_bounded<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> SyncResult<usize> {
    loop {
        let buffer = reader.fill_buf().map_err(|e| SyncError::Protocol(format!("read package: {e}")))?;
        if buffer.is_empty() { return Ok(line.len()); }
        let end = buffer.iter().position(|byte| *byte == b'\n').map(|index| index + 1).unwrap_or(buffer.len());
        if line.len().saturating_add(end) > MAX_LINE_BYTES {
            return Err(SyncError::Limit(format!("line exceeds {MAX_LINE_BYTES} bytes")));
        }
        line.extend_from_slice(&buffer[..end]);
        let ended = line.last() == Some(&b'\n');
        reader.consume(end);
        if ended { return Ok(line.len()); }
    }
}

struct HashingReader<R> {
    inner: R,
    remaining: usize,
    hasher: Sha256,
}

impl<R> HashingReader<R> {
    fn new(inner: R, remaining: usize) -> Self { Self { inner, remaining, hasher: Sha256::new() } }
    fn finish(self) -> [u8; 32] { self.hasher.finalize().into() }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 { return Ok(0); }
        let size = buffer.len().min(self.remaining);
        let read = self.inner.read(&mut buffer[..size])?;
        if read > 0 {
            self.remaining -= read;
            self.hasher.update(&buffer[..read]);
        }
        Ok(read)
    }
}

fn add_sums(sums: &mut NetworkSums, counts: &NetworkCounts) -> SyncResult<()> {
    fn add(label: &str, total: &mut f64, value: Option<f64>) -> SyncResult<()> {
        let value = value.unwrap_or(0.0);
        let next = *total + value;
        if !next.is_finite() {
            return Err(SyncError::Limit(format!("{label} sum is not finite")));
        }
        *total = next;
        Ok(())
    }
    add("input", &mut sums.in_tok, counts.in_tok)?;
    add("cache creation", &mut sums.cc_tok, counts.cc_tok)?;
    add("cache read", &mut sums.cr_tok, counts.cr_tok)?;
    add("output", &mut sums.out_tok, counts.out_tok)?;
    add("reasoning", &mut sums.reason_tok, counts.reason_tok)?;
    add("credits", &mut sums.credits, counts.credits)?;
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingInvite {
    pub invite_id: String,
    pub endpoint: String,
    pub certificate_pin: String,
    pub expires_at_ms: i64,
    pub secret: Vec<u8>,
}

impl std::fmt::Debug for PairingInvite {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PairingInvite")
            .field("invite_id", &self.invite_id)
            .field("endpoint", &self.endpoint)
            .field("certificate_pin", &self.certificate_pin)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingInviteDetails {
    pub invite: PairingInvite,
    pub primary_id: Option<String>,
    pub server_name: Option<String>,
    pub primary_display_name: Option<String>,
    pub certificate_der: Option<Vec<u8>>,
}

impl PairingInvite {
    pub fn new(endpoint: impl Into<String>, certificate_pin: impl Into<String>, expires_at_ms: i64) -> SyncResult<Self> {
        let endpoint = endpoint.into();
        let certificate_pin = certificate_pin.into();
        valid_text("endpoint", &endpoint, false)?;
        crate::transport::parse_endpoint(&endpoint)
            .map_err(|error| SyncError::Invalid(format!("invite endpoint is invalid: {error}")))?;
        valid_text("certificate_pin", &certificate_pin, false)?;
        validate_certificate_pin(&certificate_pin)?;
        let id = format!("inv-{}", hex(&random_bytes::<16>()?));
        Ok(Self { invite_id: id, endpoint, certificate_pin, expires_at_ms, secret: random_bytes::<32>()?.to_vec() })
    }

    pub fn url(&self) -> String {
        format!("tokenme://device-pair?invite={}&endpoint={}&pin={}&secret={}&expires={}", self.invite_id, self.endpoint, self.certificate_pin, hex(&self.secret), self.expires_at_ms)
    }

    /// Adds only public connection material to the user-facing invite. The
    /// invite secret remains the same and is still sent only after the TLS
    /// leaf pin is checked by the transport.
    pub fn url_with_certificate(&self, primary_id: &str, server_name: &str, certificate_der: &[u8]) -> SyncResult<String> {
        valid_text("primary_id", primary_id, false)?;
        valid_text("server_name", server_name, false)?;
        if certificate_der.is_empty() || certificate_der.len() > MAX_MANIFEST_BYTES {
            return Err(SyncError::Limit("invite certificate is missing or too large".into()));
        }
        Ok(format!("{}&primary={}&name={}&cert={}", self.url(), primary_id, server_name, hex(certificate_der)))
    }

    /// Includes the primary's human-readable host name without changing the
    /// TLS server name (`name`), which is often `localhost` for pinned certs.
    pub fn url_with_primary_name(
        &self,
        primary_id: &str,
        server_name: &str,
        certificate_der: &[u8],
        primary_display_name: &str,
    ) -> SyncResult<String> {
        valid_text("primary_display_name", primary_display_name, false)?;
        let mut url = self.url_with_certificate(primary_id, server_name, certificate_der)?;
        url.push_str("&primary_name=");
        url.push_str(&hex(primary_display_name.as_bytes()));
        Ok(url)
    }

    pub fn parse_url(value: &str) -> SyncResult<PairingInviteDetails> {
        let query = value.strip_prefix("tokenme://device-pair?")
            .ok_or_else(|| SyncError::Invalid("invite must use tokenme://device-pair".into()))?;
        let mut fields = std::collections::BTreeMap::new();
        for part in query.split('&') {
            let (key, value) = part.split_once('=').ok_or_else(|| SyncError::Invalid("invite query is malformed".into()))?;
            if fields.insert(key, value).is_some() {
                return Err(SyncError::Invalid(format!("invite query repeats {key}")));
            }
        }
        let required = |name: &str| fields.get(name).copied().ok_or_else(|| SyncError::Invalid(format!("invite lacks {name}")));
        let secret = decode_hex(required("secret")?, "invite secret")?;
        if secret.len() != 32 { return Err(SyncError::Invalid("invite secret must be 32 bytes".into())); }
        let expires_at_ms = fields.get("expires").copied().unwrap_or("9223372036854775807")
            .parse::<i64>().map_err(|_| SyncError::Invalid("invite expiry is invalid".into()))?;
        let invite = PairingInvite {
            invite_id: required("invite")?.to_string(),
            endpoint: required("endpoint")?.to_string(),
            certificate_pin: required("pin")?.to_string(),
            expires_at_ms,
            secret,
        };
        valid_text("invite_id", &invite.invite_id, false)?;
        valid_text("endpoint", &invite.endpoint, false)?;
        crate::transport::parse_endpoint(&invite.endpoint)
            .map_err(|error| SyncError::Invalid(format!("invite endpoint is invalid: {error}")))?;
        valid_text("certificate_pin", &invite.certificate_pin, false)?;
        validate_certificate_pin(&invite.certificate_pin)?;
        let primary_id = fields.get("primary").map(|value| value.to_string());
        if let Some(primary_id) = &primary_id { valid_text("primary_id", primary_id, false)?; }
        let server_name = fields.get("name").map(|value| value.to_string());
        if let Some(server_name) = &server_name { valid_text("server_name", server_name, false)?; }
        let primary_display_name = fields.get("primary_name").map(|value| {
            if value.len() > MAX_STRING_BYTES.saturating_mul(2) {
                return Err(SyncError::Limit("primary display name exceeds the invite limit".into()));
            }
            let bytes = decode_hex(value, "primary display name")?;
            let name = String::from_utf8(bytes)
                .map_err(|_| SyncError::Invalid("primary display name is not UTF-8".into()))?;
            valid_text("primary_display_name", &name, false)?;
            Ok(name)
        }).transpose()?;
        let certificate_der = fields.get("cert").map(|value| decode_hex(value, "invite certificate")).transpose()?
            .map(|der| { if der.len() > MAX_MANIFEST_BYTES { Err(SyncError::Limit("invite certificate is too large".into())) } else { Ok(der) } }).transpose()?;
        Ok(PairingInviteDetails { invite, primary_id, server_name, primary_display_name, certificate_der })
    }

    pub fn secret_hash(&self) -> [u8; 32] { hash_bytes(&self.secret) }
}

fn decode_hex(value: &str, label: &str) -> SyncResult<Vec<u8>> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SyncError::Invalid(format!("{label} is not hexadecimal")));
    }
    (0..value.len() / 2)
        .map(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| SyncError::Invalid(format!("{label} is invalid"))))
        .collect()
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequest {
    pub request_id: String,
    pub invite_id: String,
    pub invite_secret: Vec<u8>,
    pub device_id: String,
    pub origin: String,
    pub display_name: String,
    pub os: String,
    pub public_key: [u8; 32],
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

impl std::fmt::Debug for PairingRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PairingRequest")
            .field("request_id", &self.request_id)
            .field("invite_id", &self.invite_id)
            .field("device_id", &self.device_id)
            .field("origin", &self.origin)
            .field("display_name", &self.display_name)
            .field("os", &self.os)
            .field("invite_secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl PairingRequest {
    pub fn new(invite: &PairingInvite, identity: &DeviceIdentity, os: impl Into<String>) -> SyncResult<Self> {
        let os = os.into();
        valid_text("os", &os, false)?;
        let request_id = format!("req-{}", hex(&random_bytes::<16>()?));
        let mut request = Self { request_id, invite_id: invite.invite_id.clone(), invite_secret: invite.secret.clone(), device_id: identity.device_id.clone(), origin: identity.origin.clone(), display_name: identity.display_name.clone(), os, public_key: identity.public_key(), signature: [0; 64] };
        request.signature = identity.signing_key.sign(&request.signing_bytes()).to_bytes();
        Ok(request)
    }

    fn signing_bytes(&self) -> Vec<u8> {
        identity_bytes(PAIRING_DOMAIN, &[self.request_id.as_bytes(), self.invite_id.as_bytes(), &self.invite_secret, self.device_id.as_bytes(), self.origin.as_bytes(), self.display_name.as_bytes(), self.os.as_bytes(), &self.public_key])
    }

    pub fn verify(&self) -> SyncResult<()> {
        if self.invite_secret.len() != 32 {
            return Err(SyncError::Auth("pairing invite secret must be 32 bytes".into()));
        }
        for (label, value) in [
            ("request_id", self.request_id.as_str()),
            ("invite_id", self.invite_id.as_str()),
            ("device_id", self.device_id.as_str()),
            ("origin", self.origin.as_str()),
            ("display_name", self.display_name.as_str()),
            ("os", self.os.as_str()),
        ] {
            valid_text(label, value, false)?;
        }
        VerifyingKey::from_bytes(&self.public_key)
            .map_err(|_| SyncError::Auth("invalid pairing public key".into()))?
            .verify(&self.signing_bytes(), &Signature::from_bytes(&self.signature))
            .map_err(|_| SyncError::Auth("pairing signature verification failed".into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamOpenReceipt {
    pub primary_id: String,
    pub device_id: String,
    pub stream_id: String,
    pub data_epoch: u64,
    pub expected_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedStatusRequest {
    pub protocol_id: String,
    pub purpose: String,
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub nonce: [u8; 16],
    pub issued_at_ms: i64,
    pub want_recovery: bool,
    #[serde(default)]
    pub checkpoint_generation: Option<u64>,
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

impl AuthenticatedStatusRequest {
    pub fn new(identity: &DeviceIdentity, primary_id: impl Into<String>, want_recovery: bool) -> SyncResult<Self> {
        Self::new_at(identity, primary_id, want_recovery, unix_now_ms())
    }

    pub fn new_at(identity: &DeviceIdentity, primary_id: impl Into<String>, want_recovery: bool, issued_at_ms: i64) -> SyncResult<Self> {
        Self::new_at_with_checkpoint(identity, primary_id, want_recovery, None, issued_at_ms)
    }

    pub fn new_at_with_checkpoint(
        identity: &DeviceIdentity,
        primary_id: impl Into<String>,
        want_recovery: bool,
        checkpoint_generation: Option<u64>,
        issued_at_ms: i64,
    ) -> SyncResult<Self> {
        Self::new_with_purpose(identity, primary_id, want_recovery, checkpoint_generation, issued_at_ms, STATUS_PURPOSE)
    }

    /// A disconnect notice is signed under its own purpose, so a captured
    /// status request cannot be replayed to revoke the device.
    pub fn new_disconnect_at(
        identity: &DeviceIdentity,
        primary_id: impl Into<String>,
        issued_at_ms: i64,
    ) -> SyncResult<Self> {
        Self::new_with_purpose(identity, primary_id, false, None, issued_at_ms, DISCONNECT_PURPOSE)
    }

    fn new_with_purpose(
        identity: &DeviceIdentity,
        primary_id: impl Into<String>,
        want_recovery: bool,
        checkpoint_generation: Option<u64>,
        issued_at_ms: i64,
        purpose: &str,
    ) -> SyncResult<Self> {
        if checkpoint_generation.is_some_and(|generation| generation == 0 || generation > i64::MAX as u64) {
            return Err(SyncError::Invalid("checkpoint generation must start at 1".into()));
        }
        let mut request = Self {
            protocol_id: PROTOCOL_ID.into(),
            purpose: purpose.into(),
            primary_id: primary_id.into(),
            device_id: identity.device_id.clone(),
            origin: identity.origin.clone(),
            nonce: random_bytes::<16>()?,
            issued_at_ms,
            want_recovery,
            checkpoint_generation,
            signature: [0; 64],
        };
        request.signature = identity.signing_key.sign(&request.signing_bytes()).to_bytes();
        Ok(request)
    }

    pub fn verify(&self, public_key: &VerifyingKey) -> SyncResult<()> {
        if self.protocol_id != PROTOCOL_ID || !matches!(self.purpose.as_str(), STATUS_PURPOSE | DISCONNECT_PURPOSE)
            || self.primary_id.is_empty() || self.device_id.is_empty() || self.origin.is_empty()
            || self.checkpoint_generation.is_some_and(|generation| generation == 0 || generation > i64::MAX as u64) {
            return Err(SyncError::Auth("invalid authenticated status request".into()));
        }
        public_key.verify(&self.signing_bytes(), &Signature::from_bytes(&self.signature))
            .map_err(|_| SyncError::Auth("authenticated status signature verification failed".into()))
    }

    pub(crate) fn is_fresh(&self, now_ms: i64) -> bool {
        self.issued_at_ms > now_ms.saturating_sub(MAX_AUTH_REQUEST_AGE_MS)
            && self.issued_at_ms <= now_ms.saturating_add(MAX_AUTH_REQUEST_FUTURE_SKEW_MS)
    }

    fn signing_bytes(&self) -> Vec<u8> {
        let mut checkpoint = [0u8; 9];
        if let Some(generation) = self.checkpoint_generation {
            checkpoint[0] = 1;
            checkpoint[1..].copy_from_slice(&generation.to_be_bytes());
        }
        identity_bytes(STATUS_DOMAIN, &[
            self.protocol_id.as_bytes(), self.purpose.as_bytes(), self.primary_id.as_bytes(), self.device_id.as_bytes(),
            self.origin.as_bytes(), &self.nonce, &self.issued_at_ms.to_be_bytes(), if self.want_recovery { b"1" } else { b"0" }, &checkpoint,
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedDownloadRequest {
    pub protocol_id: String,
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub nonce: [u8; 16],
    pub issued_at_ms: i64,
    pub after_identity: Option<String>,
    pub after_dirty_revision: u64,
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

impl AuthenticatedDownloadRequest {
    pub fn new_at(
        identity: &DeviceIdentity,
        primary_id: impl Into<String>,
        after_identity: Option<String>,
        after_dirty_revision: u64,
        issued_at_ms: i64,
    ) -> SyncResult<Self> {
        let mut request = Self {
            protocol_id: PROTOCOL_ID.into(),
            primary_id: primary_id.into(),
            device_id: identity.device_id.clone(),
            origin: identity.origin.clone(),
            nonce: random_bytes::<16>()?,
            issued_at_ms,
            after_identity,
            after_dirty_revision,
            signature: [0; 64],
        };
        request.validate_fields()?;
        request.signature = identity.signing_key.sign(&request.signing_bytes()).to_bytes();
        Ok(request)
    }

    pub fn verify(&self, public_key: &VerifyingKey) -> SyncResult<()> {
        self.validate_fields()?;
        public_key.verify(&self.signing_bytes(), &Signature::from_bytes(&self.signature))
            .map_err(|_| SyncError::Auth("authenticated download signature verification failed".into()))
    }

    fn validate_fields(&self) -> SyncResult<()> {
        if self.protocol_id != PROTOCOL_ID || self.after_dirty_revision > i64::MAX as u64 {
            return Err(SyncError::Auth("invalid authenticated download request".into()));
        }
        valid_text("primary_id", &self.primary_id, false)?;
        valid_text("device_id", &self.device_id, false)?;
        valid_text("origin", &self.origin, false)?;
        if !origin_ok(&self.origin) { return Err(SyncError::Auth("invalid authenticated download origin".into())); }
        if let Some(identity) = self.after_identity.as_deref() {
            valid_text("download identity cursor", identity, false)?;
        }
        Ok(())
    }

    pub(crate) fn is_fresh(&self, now_ms: i64) -> bool {
        self.issued_at_ms > now_ms.saturating_sub(MAX_AUTH_REQUEST_AGE_MS)
            && self.issued_at_ms <= now_ms.saturating_add(MAX_AUTH_REQUEST_FUTURE_SKEW_MS)
    }

    fn signing_bytes(&self) -> Vec<u8> {
        let cursor_present = [u8::from(self.after_identity.is_some())];
        identity_bytes(DOWNLOAD_DOMAIN, &[
            self.protocol_id.as_bytes(), self.primary_id.as_bytes(), self.device_id.as_bytes(),
            self.origin.as_bytes(), &self.nonce, &self.issued_at_ms.to_be_bytes(),
            &cursor_present, self.after_identity.as_deref().unwrap_or("").as_bytes(),
            &self.after_dirty_revision.to_be_bytes(),
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedDisplayNameRequest {
    pub protocol_id: String,
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub display_name: String,
    pub nonce: [u8; 16],
    pub issued_at_ms: i64,
    #[serde(with = "signature_serde")]
    pub signature: [u8; 64],
}

impl AuthenticatedDisplayNameRequest {
    pub fn new_at(
        identity: &DeviceIdentity,
        primary_id: impl Into<String>,
        display_name: impl Into<String>,
        issued_at_ms: i64,
    ) -> SyncResult<Self> {
        let display_name = display_name.into();
        valid_text("display_name", &display_name, false)?;
        let mut request = Self {
            protocol_id: PROTOCOL_ID.into(),
            primary_id: primary_id.into(),
            device_id: identity.device_id.clone(),
            origin: identity.origin.clone(),
            display_name,
            nonce: random_bytes::<16>()?,
            issued_at_ms,
            signature: [0; 64],
        };
        request.signature = identity.signing_key.sign(&request.signing_bytes()).to_bytes();
        Ok(request)
    }

    pub fn verify(&self, public_key: &VerifyingKey) -> SyncResult<()> {
        if self.protocol_id != PROTOCOL_ID || self.primary_id.is_empty() || self.device_id.is_empty()
            || self.origin.is_empty() || !origin_ok(&self.origin)
        {
            return Err(SyncError::Auth("invalid authenticated display-name request".into()));
        }
        valid_text("display_name", &self.display_name, false)?;
        public_key.verify(&self.signing_bytes(), &Signature::from_bytes(&self.signature))
            .map_err(|_| SyncError::Auth("authenticated display-name signature verification failed".into()))
    }

    pub(crate) fn is_fresh(&self, now_ms: i64) -> bool {
        self.issued_at_ms > now_ms.saturating_sub(MAX_AUTH_REQUEST_AGE_MS)
            && self.issued_at_ms <= now_ms.saturating_add(MAX_AUTH_REQUEST_FUTURE_SKEW_MS)
    }

    fn signing_bytes(&self) -> Vec<u8> {
        identity_bytes(DISPLAY_NAME_DOMAIN, &[
            self.protocol_id.as_bytes(), self.primary_id.as_bytes(), self.device_id.as_bytes(),
            self.origin.as_bytes(), self.display_name.as_bytes(), &self.nonce, &self.issued_at_ms.to_be_bytes(),
        ])
    }
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticatedStatus {
    pub primary_id: String,
    pub device_id: String,
    pub origin: String,
    pub request_nonce: [u8; 16],
    pub status: String,
    pub data_epoch: u64,
    pub active_stream_id: Option<String>,
    pub expected_generation: Option<u64>,
    pub last_generation: Option<u64>,
    pub highwater_generation: Option<u64>,
    #[serde(with = "option_hash_serde")]
    pub highwater_receipt_hash: Option<[u8; 32]>,
    #[serde(default)]
    pub checkpoint_generation: Option<u64>,
    #[serde(default, with = "option_hash_serde")]
    pub checkpoint_receipt_hash: Option<[u8; 32]>,
    // Older primaries omit this; its presence gates the optional update endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_display_name: Option<String>,
    #[serde(default)]
    pub primary_data_sharing: bool,
    pub coverage_lo_ms: Option<i64>,
    pub coverage_hi_ms: Option<i64>,
    pub observed_at_ms: i64,
}

mod option_hash_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &Option<[u8; 32]>, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        value.as_ref().map(|bytes| bytes.as_slice()).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<[u8; 32]>, D::Error>
    where D: Deserializer<'de> {
        let value = Option::<Vec<u8>>::deserialize(deserializer)?;
        value.map(|bytes| bytes.try_into().map_err(|_| serde::de::Error::custom("receipt hash must be 32 bytes"))).transpose()
    }
}

pub(crate) fn network_event_key(device_id: &str, original: &str) -> String {
    format!("device-sync:v1:{}", hex(&hash_bytes(&identity_bytes(IDENTITY_DOMAIN, &[device_id.as_bytes(), original.as_bytes()]))))
}

pub(crate) fn public_key_from_bytes(bytes: &[u8]) -> SyncResult<VerifyingKey> {
    let key: [u8; 32] = bytes.try_into().map_err(|_| SyncError::Auth("registered public key has invalid length".into()))?;
    VerifyingKey::from_bytes(&key).map_err(|_| SyncError::Auth("registered public key is invalid".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn identity_save_creates_private_parent_and_file_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("identity");
        let path = parent.join("device.json");
        let first = DeviceIdentity::create("linux", "first").unwrap();
        first.save(&path).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(mode(&parent), 0o700);
            assert_eq!(mode(&path), 0o600);
        }
        let original = fs::read(&path).unwrap();
        let second = DeviceIdentity::create("linux", "second").unwrap();
        assert!(matches!(second.save(&path), Err(SyncError::Io(_, _))));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_shared_parent_without_chmodifying_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let before = mode(&parent);
        let identity = DeviceIdentity::create("linux", "Linux").unwrap();
        assert!(matches!(identity.save(&parent.join("device.json")), Err(SyncError::Security(_))));
        assert_eq!(mode(&parent), before);
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_final_and_temporary_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::create("linux", "Linux").unwrap();
        let target = dir.path().join("target.json");
        fs::write(&target, b"do not replace").unwrap();
        let final_path = dir.path().join("final.json");
        std::os::unix::fs::symlink(&target, &final_path).unwrap();
        assert!(matches!(identity.save(&final_path), Err(SyncError::Security(_))));

        let temp_path = dir.path().join("temp.json");
        let tmp = temp_path.with_extension("json.tmp");
        std::os::unix::fs::symlink(&target, &tmp).unwrap();
        assert!(matches!(identity.save(&temp_path), Err(SyncError::Security(_))));
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_existing_temporary_file_without_truncating_it() {
        let dir = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::create("linux", "Linux").unwrap();
        let path = dir.path().join("device.json");
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, b"sentinel").unwrap();
        assert!(identity.save(&path).is_err());
        assert_eq!(fs::read(&tmp).unwrap(), b"sentinel");
    }

    #[test]
    fn pairing_request_json_round_trip_preserves_signature() {
        let identity = DeviceIdentity::create("linux", "Linux").unwrap();
        let invite = PairingInvite::new("https://127.0.0.1:47821", &"ab".repeat(32), i64::MAX).unwrap();
        let request = PairingRequest::new(&invite, &identity, "linux").unwrap();
        let bytes = serde_json::to_vec(&request).unwrap();
        let restored: PairingRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, request);
        restored.verify().unwrap();
    }

    #[test]
    fn pairing_invites_reject_plain_http_endpoints() {
        assert!(PairingInvite::new("http://192.168.1.20:47821", &"ab".repeat(32), i64::MAX).is_err());
        let url = "tokenme://device-pair?invite=inv-1&endpoint=http://192.168.1.20:47821&pin=abababababababababababababababababababababababababababababababab&secret=abababababababababababababababababababababababababababababababab&expires=9223372036854775807";
        assert!(PairingInvite::parse_url(url).is_err());
    }

    fn row(key: Option<&str>) -> NetworkRow {
        NetworkRow { key: key.map(str::to_string), occurrence_id: None, tool: "codex".into(), ts_ms: 10, session: "s".into(), project: None, model: Some("gpt-5.6-luna".into()), meter: "tokens".into(), source: "local.jsonl".into(), counts: NetworkCounts { in_tok: Some(1.0), cc_tok: Some(2.0), cr_tok: Some(3.0), out_tok: Some(4.0), reason_tok: Some(0.0), credits: Some(0.0) }, calls: vec![NetworkCall { kind: "mcp".into(), name: "tools/list".into() }] }
    }

    #[test]
    fn signing_domain_binds_stream_epoch_and_raw_manifest() {
        let a = DeviceIdentity::create("a", "A").unwrap();
        let b = DeviceIdentity::create("b", "B").unwrap();
        let package = NetworkPackage::from_rows("b", "4", 0, 100, &[row(Some("k"))], 1).unwrap();
        let envelope = UploadEnvelope { protocol_id: PROTOCOL_ID.into(), primary_id: "p".into(), device_id: b.device_id.clone(), origin: "b".into(), stream_id: "s1".into(), data_epoch: 1, generation: 1, manifest_bytes: package.manifest_bytes.clone(), gzip_sha256: hash_bytes(&package.gzip_bytes) };
        let upload = SignedUpload::sign(envelope.clone(), &b.signing_key, package.gzip_bytes.clone()).unwrap();
        upload.verify(&b.signing_key.verifying_key()).unwrap();
        let mut changed = envelope;
        changed.stream_id = "s2".into();
        let forged = SignedUpload { envelope: changed, signature: upload.signature, gzip_bytes: upload.gzip_bytes };
        assert!(matches!(forged.verify(&b.signing_key.verifying_key()), Err(SyncError::Auth(_))));
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn authenticated_status_checkpoint_generation_is_signed() {
        let identity = DeviceIdentity::create("checkpoint-signature", "Linux").unwrap();
        let mut request = AuthenticatedStatusRequest::new_at_with_checkpoint(&identity, "primary-1", false, Some(7), 10).unwrap();
        let public_key = identity.signing_key.verifying_key();
        request.verify(&public_key).unwrap();
        request.checkpoint_generation = Some(8);
        assert!(matches!(request.verify(&public_key), Err(SyncError::Auth(_))));
        assert!(matches!(AuthenticatedStatusRequest::new_at_with_checkpoint(&identity, "primary-1", false, Some(0), 10), Err(SyncError::Invalid(_))));
    }

    #[test]
    fn the_signed_purpose_separates_a_status_request_from_a_disconnect_notice() {
        let identity = DeviceIdentity::create("purpose-signature", "Linux").unwrap();
        let public_key = identity.signing_key.verifying_key();
        let status = AuthenticatedStatusRequest::new_at(&identity, "primary-1", false, 10).unwrap();
        let disconnect = AuthenticatedStatusRequest::new_disconnect_at(&identity, "primary-1", 10).unwrap();
        assert_eq!(status.purpose, STATUS_PURPOSE);
        assert_eq!(disconnect.purpose, DISCONNECT_PURPOSE);
        status.verify(&public_key).unwrap();
        disconnect.verify(&public_key).unwrap();
        let mut replayed = status.clone();
        replayed.purpose = DISCONNECT_PURPOSE.into();
        assert!(matches!(replayed.verify(&public_key), Err(SyncError::Auth(_))), "relabelling a captured status request must break its signature");
    }

    #[test]
    fn authenticated_display_name_request_binds_the_name_to_the_identity() {
        let identity = DeviceIdentity::create("stable-origin", "Old name").unwrap();
        let public_key = identity.signing_key.verifying_key();
        let mut request = AuthenticatedDisplayNameRequest::new_at(&identity, "primary-1", "New name", 10).unwrap();
        request.verify(&public_key).unwrap();
        request.display_name = "forged name".into();
        assert!(matches!(request.verify(&public_key), Err(SyncError::Auth(_))));
    }

    #[test]
    fn authenticated_download_request_binds_both_cursors() {
        let identity = DeviceIdentity::create("secondary-origin", "Secondary").unwrap();
        let key = identity.signing_key.verifying_key();
        let mut request = AuthenticatedDownloadRequest::new_at(&identity, "primary-1", Some("key:a".into()), 7, 10).unwrap();
        request.verify(&key).unwrap();
        request.after_identity = Some("key:b".into());
        assert!(matches!(request.verify(&key), Err(SyncError::Auth(_))));

        let mut request = AuthenticatedDownloadRequest::new_at(&identity, "primary-1", Some("key:a".into()), 7, 10).unwrap();
        request.after_dirty_revision = 8;
        assert!(matches!(request.verify(&key), Err(SyncError::Auth(_))));
    }

    #[test]
    fn authenticated_status_accepts_older_primary_without_display_names() {
        let request_nonce = [0u8; 16];
        let status: AuthenticatedStatus = serde_json::from_value(serde_json::json!({
            "primary_id": "primary-1", "device_id": "device-1", "origin": "stable-origin",
            "request_nonce": request_nonce, "status": "paired", "data_epoch": 1,
            "active_stream_id": null, "expected_generation": null, "last_generation": null,
            "highwater_generation": null, "highwater_receipt_hash": null,
            "checkpoint_generation": null, "checkpoint_receipt_hash": null,
            "coverage_lo_ms": null, "coverage_hi_ms": null, "observed_at_ms": 10
        })).unwrap();
        assert_eq!(status.display_name, None);
        assert_eq!(status.primary_display_name, None);
    }

    #[test]
    fn bounded_package_rejects_keyless_without_occurrence() {
        let package = NetworkPackage::from_rows("a", "4", 0, 100, &[row(None)], 1);
        assert!(matches!(package, Err(SyncError::UnstableOccurrence)));
    }

    #[test]
    fn package_roundtrip_preserves_missing_counts_as_missing() {
        let mut r = row(Some("k"));
        r.counts = NetworkCounts { in_tok: None, cc_tok: None, cr_tok: None, out_tok: None, reason_tok: None, credits: Some(2.0) };
        let package = NetworkPackage::from_rows("a", "4", 0, 100, &[r], 1).unwrap();
        let (_, rows) = package.validate().unwrap();
        assert!(rows[0].counts.in_tok.is_none());
        assert_eq!(rows[0].counts.credits, Some(2.0));
    }

    #[test]
    fn package_roundtrip_preserves_duplicate_call_multiplicity() {
        let mut r = row(Some("call-multiplicity"));
        r.calls.push(NetworkCall { kind: "mcp".into(), name: "tools/list".into() });
        let package = NetworkPackage::from_rows("a", "4", 0, 100, &[r], 1).unwrap();
        let (_, rows) = package.validate().unwrap();
        assert_eq!(rows[0].calls.len(), 2);
        assert_eq!(rows[0].calls.iter().filter(|call| call.name == "tools/list").count(), 2);
    }

    #[test]
    fn package_roundtrip_carries_snapshot_completion_and_accepts_old_manifests() {
        let package = NetworkPackage::from_rows_with_snapshot_complete("a", "4", 0, 100, &[row(Some("complete"))], 1, true).unwrap();
        let (manifest, _) = package.validate().unwrap();
        assert!(manifest.snapshot_complete);

        let mut old_manifest = serde_json::to_value(&manifest).unwrap();
        old_manifest.as_object_mut().unwrap().remove("snapshot_complete");
        let parsed: NetworkManifest = serde_json::from_value(old_manifest).unwrap();
        assert!(!parsed.snapshot_complete);
    }

    #[test]
    fn package_rejects_a_long_unterminated_line_before_unbounded_allocation() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&vec![b'x'; MAX_LINE_BYTES + 1]).unwrap();
        let manifest = NetworkManifest {
            format: SYNC_FORMAT.into(), schema_version: "4".into(), origin: "a".into(),
            window_lo_ms: 0, window_hi_ms: 100, rows: 1, sums: NetworkSums::default(), generated_at_ms: 1, snapshot_complete: false,
        };
        let package = NetworkPackage {
            manifest_bytes: serde_json::to_vec(&manifest).unwrap(),
            gzip_bytes: encoder.finish().unwrap(),
        };
        assert!(matches!(package.validate(), Err(SyncError::Limit(message)) if message.contains("line exceeds")));
    }

    #[test]
    fn package_rejects_duplicate_row_identity() {
        let first = row(Some("same"));
        let second = row(Some("same"));
        assert!(matches!(NetworkPackage::from_rows("a", "4", 0, 100, &[first, second], 1), Err(SyncError::Protocol(_))));
    }

    #[test]
    fn strict_export_rejects_any_network_source_prefix() {
        let mut remote = row(Some("k"));
        remote.source = "linux:malformed-without-key".into();
        assert!(matches!(NetworkPackage::from_rows("a", "4", 0, 100, &[remote], 1), Err(SyncError::Protocol(message)) if message.contains("re-export")));
    }

    #[test]
    fn package_sums_survive_the_json_round_trip_bit_exactly() {
        // Credits meters carry 17-significant-digit decimals (Qoder). Without
        // serde_json's `float_roundtrip` the receiving parse lands 1 ulp off
        // and the recomputed sums disagree with the manifest — the sync-killing
        // "manifest sums mismatch" seen on a real 256-row page. This total is
        // the exact value that misparsed in the field.
        let values = [
            55.883975737371436f64, 0.16658642857142855, 0.16647642857142858, 0.14730298382857143,
            0.05871714285714285, 0.06676714285714286, 0.07775214285714284, 0.2392317705142857, 0.2906989285714286,
        ];
        for value in values {
            let text = serde_json::to_string(&value).unwrap();
            let parsed: f64 = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed.to_bits(), value.to_bits(), "serde_json parse of {text} must be bit-exact");
        }
        let rows = (0..256)
            .map(|index| {
                let mut r = row(Some(&format!("float-{index}")));
                r.counts.credits = Some(values[index % values.len()]);
                r
            })
            .collect::<Vec<_>>();
        let package = NetworkPackage::from_rows("a", "4", 0, 100, &rows, 1).unwrap();
        let (manifest, parsed) = package.validate().unwrap();
        assert_eq!(manifest.rows, 256);
        let total: f64 = parsed.iter().map(|r| r.counts.credits.unwrap()).sum();
        assert_eq!(total.to_bits(), manifest.sums.credits.to_bits(), "the manifest sum must stay bit-identical through the JSON round trip");
    }
}
