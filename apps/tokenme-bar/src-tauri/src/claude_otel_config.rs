//! Reversible, opt-in Claude Code OTLP configuration.
//!
//! Only the user's settings file is touched. Existing values are never
//! overwritten, and disable removes only keys this feature added when their
//! values still match the values TokenMe wrote.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

const OWNED_ENV: &[(&str, &str)] = &[
    ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
    ("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1"),
    ("OTEL_TRACES_EXPORTER", "otlp"),
    ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json"),
    ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://127.0.0.1:4319/v1/traces"),
    ("OTEL_TRACES_EXPORT_INTERVAL", "1000"),
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Ownership {
    settings_path: PathBuf,
    #[serde(default)]
    added_keys: Vec<String>,
    #[serde(default)]
    created_env: bool,
}

pub fn settings_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("settings.json"));
    }
    dirs::home_dir().map(|home| home.join(".claude").join("settings.json"))
        .ok_or_else(|| "Could not locate the user home directory for Claude Code settings.".into())
}

pub fn enable(owner_path: &Path) -> Result<(), String> {
    enable_at(owner_path, &settings_path()?)
}

pub fn disable(owner_path: &Path) -> Result<(), String> {
    disable_owned(owner_path)
}

fn enable_at(owner_path: &Path, config_path: &Path) -> Result<(), String> {
    let mut old_owner_bytes = read_optional(owner_path).map_err(io_error)?;
    let mut owner = match old_owner_bytes.as_deref() {
        Some(bytes) => serde_json::from_slice::<Ownership>(bytes)
            .map_err(|_| "TokenMe's Claude configuration ownership record is invalid; no settings were changed.".to_string())?,
        None => Ownership::default(),
    };
    if !owner.settings_path.as_os_str().is_empty() && owner.settings_path != config_path {
        disable_owned(owner_path)?;
        old_owner_bytes = None;
        owner = Ownership::default();
    }

    let original = read_optional(config_path).map_err(io_error)?;
    let mut root = match original.as_deref() {
        Some(bytes) => serde_json::from_slice::<Value>(bytes)
            .map_err(|_| "Claude settings.json is not valid JSON; it was left unchanged.".to_string())?,
        None => Value::Object(Map::new()),
    };
    let root_object = root.as_object_mut()
        .ok_or_else(|| "Claude settings.json must contain a JSON object; it was left unchanged.".to_string())?;
    let env_was_present = root_object.contains_key("env");
    if env_was_present && !root_object.get("env").is_some_and(Value::is_object) {
        return Err("Claude settings.json has a non-object `env` value; it was left unchanged.".into());
    }
    let env = root_object.entry("env").or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut().expect("env object validated or created");
    owner.settings_path = config_path.to_path_buf();
    owner.created_env |= !env_was_present;
    let mut changed = false;
    for (key, desired) in OWNED_ENV {
        match env.get(*key) {
            Some(Value::String(value)) if value == desired => {}
            Some(_) => return Err(format!("Claude setting `{key}` already has a different value; TokenMe left it unchanged.")),
            None => {
                env.insert((*key).into(), Value::String((*desired).into()));
                if !owner.added_keys.iter().any(|owned| owned == key) { owner.added_keys.push((*key).into()); }
                changed = true;
            }
        }
    }

    // Record ownership first so a process exit after changing settings can
    // still be safely rolled back on the next disable operation.
    let new_owner_bytes = serde_json::to_vec(&owner).map_err(|error| error.to_string())?;
    write_bytes_atomic(owner_path, old_owner_bytes.as_deref(), &new_owner_bytes).map_err(io_error)?;
    if changed {
        let bytes = serde_json::to_vec_pretty(&root).map_err(|error| error.to_string())?;
        if let Err(error) = write_bytes_atomic(config_path, original.as_deref(), &bytes) {
            if let Some(old_bytes) = old_owner_bytes.as_deref() {
                let _ = write_bytes_atomic(owner_path, Some(&new_owner_bytes), old_bytes);
            } else {
                let _ = remove_if_unchanged(owner_path, Some(&new_owner_bytes));
            }
            return Err(io_error(error));
        }
    }
    Ok(())
}

fn disable_owned(owner_path: &Path) -> Result<(), String> {
    let Some(owner_bytes) = read_optional(owner_path).map_err(io_error)? else { return Ok(()) };
    let owner = serde_json::from_slice::<Ownership>(&owner_bytes)
        .map_err(|_| "TokenMe's Claude configuration ownership record is invalid; no settings were changed.".to_string())?;
    let Some(original) = read_optional(&owner.settings_path).map_err(io_error)? else {
        remove_if_unchanged(owner_path, Some(&owner_bytes)).map_err(io_error)?;
        return Ok(());
    };
    let mut root = serde_json::from_slice::<Value>(&original)
        .map_err(|_| "Claude settings.json is not valid JSON; collection remains enabled so it is not modified.".to_string())?;
    let root_object = root.as_object_mut()
        .ok_or_else(|| "Claude settings.json must contain a JSON object; collection remains enabled.".to_string())?;
    let mut changed = false;
    if let Some(env) = root_object.get_mut("env").and_then(Value::as_object_mut) {
        for key in &owner.added_keys {
            if let Some((_, desired)) = OWNED_ENV.iter().find(|(owned, _)| *owned == key) {
                if env.get(key).and_then(Value::as_str) == Some(*desired) {
                    env.remove(key);
                    changed = true;
                }
            }
        }
        if owner.created_env && env.is_empty() {
            root_object.remove("env");
            changed = true;
        }
    }
    if changed {
        let bytes = serde_json::to_vec_pretty(&root).map_err(|error| error.to_string())?;
        write_bytes_atomic(&owner.settings_path, Some(&original), &bytes).map_err(io_error)?;
    }
    remove_if_unchanged(owner_path, Some(&owner_bytes)).map_err(io_error)?;
    Ok(())
}

fn read_optional(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_bytes_atomic(path: &Path, expected: Option<&[u8]>, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    if read_optional(path)?.as_deref() != expected {
        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "settings changed concurrently; retry"));
    }
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("settings.json");
    let tmp = path.with_file_name(format!(".{name}.tokenme-{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    if read_optional(path)?.as_deref() != expected {
        let _ = fs::remove_file(&tmp);
        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "settings changed concurrently; retry"));
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => { let _ = fs::remove_file(&tmp); Err(error) }
    }
}

fn remove_if_unchanged(path: &Path, expected: Option<&[u8]>) -> std::io::Result<()> {
    if read_optional(path)?.as_deref() != expected {
        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "file changed concurrently; retry"));
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn io_error(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        "Claude settings changed while TokenMe was updating them; retry the setting.".into()
    } else {
        format!("Claude OTLP settings could not be updated: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_merges_and_disable_removes_only_tokenme_owned_values() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let owner = dir.path().join("owner.json");
        fs::write(&settings, br#"{"theme":"dark","env":{"MY_EXISTING":"keep"}}"#).unwrap();
        enable_at(&owner, &settings).unwrap();
        let enabled: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(enabled["theme"], "dark");
        assert_eq!(enabled["env"]["MY_EXISTING"], "keep");
        assert_eq!(enabled["env"]["OTEL_TRACES_EXPORTER"], "otlp");
        assert_eq!(enabled["env"]["OTEL_TRACES_EXPORT_INTERVAL"], "1000");
        disable_owned(&owner).unwrap();
        let disabled: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(disabled["theme"], "dark");
        assert_eq!(disabled["env"]["MY_EXISTING"], "keep");
        assert!(disabled["env"].get("OTEL_TRACES_EXPORTER").is_none());
    }

    #[test]
    fn conflicting_user_values_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let owner = dir.path().join("owner.json");
        fs::write(&settings, br#"{"env":{"OTEL_EXPORTER_OTLP_TRACES_ENDPOINT":"http://collector.local:4318/v1/traces"}}"#).unwrap();
        assert!(enable_at(&owner, &settings).unwrap_err().contains("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"));
        let unchanged: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(unchanged["env"]["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"], "http://collector.local:4318/v1/traces");
        assert!(!owner.exists());
    }

    #[test]
    fn trace_specific_endpoint_preserves_existing_metrics_and_logs_routing() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let owner = dir.path().join("owner.json");
        fs::write(&settings, br#"{"env":{"OTEL_EXPORTER_OTLP_ENDPOINT":"http://collector.local:4318","OTEL_METRICS_EXPORTER":"otlp","OTEL_LOGS_EXPORTER":"otlp"}}"#).unwrap();
        enable_at(&owner, &settings).unwrap();
        let enabled: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(enabled["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://collector.local:4318");
        assert_eq!(enabled["env"]["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"], "http://127.0.0.1:4319/v1/traces");
        disable_owned(&owner).unwrap();
        let disabled: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(disabled["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://collector.local:4318");
        assert_eq!(disabled["env"]["OTEL_METRICS_EXPORTER"], "otlp");
        assert_eq!(disabled["env"]["OTEL_LOGS_EXPORTER"], "otlp");
    }

    #[test]
    fn user_edits_to_managed_values_survive_disable() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let owner = dir.path().join("owner.json");
        enable_at(&owner, &settings).unwrap();
        let mut current: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        current["env"]["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"] = Value::String("http://my-collector:4318/v1/traces".into());
        fs::write(&settings, serde_json::to_vec(&current).unwrap()).unwrap();
        disable_owned(&owner).unwrap();
        let after: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(after["env"]["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"], "http://my-collector:4318/v1/traces");
        assert!(after["env"].get("OTEL_TRACES_EXPORTER").is_none());
    }
}
