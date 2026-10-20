use serde::Deserialize;
use tokenme_plugin_sdk_reference::{HostServices, QuotaWindow, TaskInput, TaskResult, UsageFact};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VendorQuota { window_id: String, used_percent: f64, resets_at_ms: u64 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePage { records: Vec<UsageFact>, next_cursor: Option<String> }

pub fn execute(task_id: &str, input: &str, host: &mut dyn HostServices) -> Result<String, String> {
    let input: TaskInput = serde_json::from_str(input).map_err(|_| "INPUT.INVALID".to_string())?;
    let result = match task_id {
        "read-quota" => {
            let (status, body) = host.http("quota")?;
            if status != 200 { return Err(if status == 401 { "AUTH.LOGIN_REQUIRED" } else { "NET.HTTP_ERROR" }.into()); }
            let quota: VendorQuota = serde_json::from_str(&body).map_err(|_| "PARSE.SCHEMA_MISMATCH".to_string())?;
            host.log("quota.parsed", serde_json::json!({"windows": 1}))?;
            TaskResult::Quota { windows: vec![QuotaWindow { id: quota.window_id, used_percent: quota.used_percent, resets_at_ms: quota.resets_at_ms, sampled_at_ms: host.now_ms() }] }
        }
        "collect-usage" => {
            let body = host.read_source("sessions", input.cursor)?;
            let page: SourcePage = serde_json::from_str(&body).map_err(|_| "PARSE.SCHEMA_MISMATCH".to_string())?;
            TaskResult::Usage { events: page.records, next_cursor: page.next_cursor }
        }
        _ => return Err("TASK.NOT_REGISTERED".into()),
    };
    result.validate()?;
    serde_json::to_string(&result).map_err(|_| "RESULT.ENCODE_FAILED".into())
}

#[cfg(target_arch = "wasm32")]
mod component {
    wit_bindgen::generate!({ path: "../wit", world: "adapter" });
    struct AdapterPlugin;
    struct Services;
    impl tokenme_plugin_sdk_reference::HostServices for Services {
        fn now_ms(&mut self) -> u64 { tokenme::plugin::host::now_ms() }
        fn http(&mut self, endpoint: &str) -> Result<(u16, String), String> {
            tokenme::plugin::host::http(endpoint).map(|r| (r.status, r.body))
        }
        fn read_source(&mut self, source: &str, cursor: Option<String>) -> Result<String, String> {
            tokenme::plugin::host::read_source(source, cursor.as_deref())
        }
        fn log(&mut self, event: &str, fields: serde_json::Value) -> Result<(), String> {
            tokenme::plugin::host::log(event, &fields.to_string())
        }
    }
    impl Guest for AdapterPlugin {
        fn execute(task_id: String, input_json: String) -> Result<String, String> { super::execute(&task_id, &input_json, &mut Services) }
    }
    export!(AdapterPlugin);
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Mock { body: String, allowed: bool }
    impl HostServices for Mock {
        fn now_ms(&mut self) -> u64 { 1792404000000 }
        fn http(&mut self, _: &str) -> Result<(u16, String), String> { if self.allowed { Ok((200, self.body.clone())) } else { Err("PERMISSION.DENIED".into()) } }
        fn read_source(&mut self, _: &str, _: Option<String>) -> Result<String, String> { Ok(self.body.clone()) }
        fn log(&mut self, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
    }
    #[test]
    fn parses_quota_without_manufacturing_tokens() {
        let mut host = Mock { body: r#"{"windowId":"daily","usedPercent":25,"resetsAtMs":1792440000000}"#.into(), allowed: true };
        let result: TaskResult = serde_json::from_str(&execute("read-quota", "{}", &mut host).unwrap()).unwrap();
        assert!(matches!(result, TaskResult::Quota { windows } if windows.len() == 1 && windows[0].used_percent == 25.0));
    }
    #[test]
    fn retains_permission_and_parse_error_categories() {
        let mut host = Mock { body: "secret-not-json".into(), allowed: false };
        assert_eq!(execute("read-quota", "{}", &mut host).unwrap_err(), "PERMISSION.DENIED");
        host.allowed = true;
        assert_eq!(execute("read-quota", "{}", &mut host).unwrap_err(), "PARSE.SCHEMA_MISMATCH");
    }
    #[test]
    fn validates_usage_and_cursor_at_the_boundary() {
        let mut host = Mock { body: r#"{"records":[{"occurrence_id":"r1","model":"model-a","ts_ms":1,"input":120,"output":80,"cache_creation":0,"cache_read":0}],"next_cursor":"1"}"#.into(), allowed: true };
        let result: TaskResult = serde_json::from_str(&execute("collect-usage", "{}", &mut host).unwrap()).unwrap();
        assert!(matches!(result, TaskResult::Usage { events, next_cursor } if events[0].total().unwrap() == 200 && next_cursor.as_deref() == Some("1")));
        assert_eq!(execute("collect-usage", r#"{"unexpected":true}"#, &mut host).unwrap_err(), "INPUT.INVALID");
    }
}
