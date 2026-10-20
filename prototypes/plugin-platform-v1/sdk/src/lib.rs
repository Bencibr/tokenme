//! Executable alpha contract. This crate is a reference, not the published SDK.
use serde::{Deserialize, Serialize};

pub const HOST_API: &str = "tokenme.plugin/0.1";
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const MAX_BATCH: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindow {
    pub id: String,
    pub used_percent: f64,
    pub resets_at_ms: u64,
    pub sampled_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageFact {
    pub occurrence_id: String,
    pub model: String,
    pub ts_ms: u64,
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl UsageFact {
    pub fn total(&self) -> Result<u64, String> {
        [self.input, self.cache_creation, self.cache_read, self.output].iter().try_fold(0u64, |sum, value| {
            if *value > MAX_SAFE_INTEGER { return Err("DATA.NUMBER_RANGE".into()); }
            sum.checked_add(*value).filter(|n| *n <= MAX_SAFE_INTEGER).ok_or_else(|| "DATA.NUMBER_RANGE".into())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskResult {
    Quota { windows: Vec<QuotaWindow> },
    Usage { events: Vec<UsageFact>, next_cursor: Option<String> },
}

impl TaskResult {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Quota { windows } => {
                if windows.len() > MAX_BATCH { return Err("DATA.BATCH_LIMIT".into()); }
                for window in windows {
                    valid_id(&window.id)?;
                    if !window.used_percent.is_finite() || !(0.0..=10000.0).contains(&window.used_percent)
                        || window.resets_at_ms > MAX_SAFE_INTEGER || window.sampled_at_ms > MAX_SAFE_INTEGER {
                        return Err("DATA.QUOTA_RANGE".into());
                    }
                }
            }
            Self::Usage { events, next_cursor } => {
                if events.len() > MAX_BATCH || next_cursor.as_ref().is_some_and(|v| v.len() > 1024) { return Err("DATA.BATCH_LIMIT".into()); }
                for event in events {
                    valid_id(&event.occurrence_id)?;
                    valid_id(&event.model)?;
                    if event.ts_ms > MAX_SAFE_INTEGER { return Err("DATA.IDENTITY".into()); }
                    event.total()?;
                }
            }
        }
        Ok(())
    }
}

fn valid_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) { Err("DATA.IDENTITY".into()) } else { Ok(()) }
}

pub trait HostServices {
    fn now_ms(&mut self) -> u64;
    fn http(&mut self, endpoint: &str) -> Result<(u16, String), String>;
    fn read_source(&mut self, source: &str, cursor: Option<String>) -> Result<String, String>;
    fn log(&mut self, event: &str, fields: serde_json::Value) -> Result<(), String>;
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_nonfinite_quota_and_unsafe_counts() {
        let bad = TaskResult::Quota { windows: vec![QuotaWindow { id: "daily".into(), used_percent: f64::NAN, resets_at_ms: 1, sampled_at_ms: 1 }] };
        assert_eq!(bad.validate().unwrap_err(), "DATA.QUOTA_RANGE");
        let fact = UsageFact { occurrence_id: "r1".into(), model: "m".into(), ts_ms: 1, input: MAX_SAFE_INTEGER, output: 1, cache_creation: 0, cache_read: 0 };
        assert_eq!(fact.total().unwrap_err(), "DATA.NUMBER_RANGE");
    }
    #[test]
    fn rejects_undeclared_result_fields() {
        assert!(serde_json::from_str::<TaskResult>(r#"{"kind":"usage","events":[],"next_cursor":null,"credits_as_tokens":200}"#).is_err());
    }
}
