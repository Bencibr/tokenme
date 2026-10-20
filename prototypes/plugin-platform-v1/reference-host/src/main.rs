//! Offline contract host. Restricted WASI: no preopened directories, inherited environment, stdio or networking.
use std::{collections::BTreeMap, env, fs, path::Path, sync::{Arc, atomic::{AtomicBool, Ordering}}, thread, time::{Duration, SystemTime, UNIX_EPOCH}};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokenme_plugin_sdk_reference::{TaskResult, UsageFact, HOST_API};
use wasmtime::{component::{Component, Linker}, Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({ path: "../wit", world: "adapter" });

struct EpochClock(Arc<AtomicBool>);
impl EpochClock {
    fn start(engine: &Engine) -> Self {
        let stop=Arc::new(AtomicBool::new(false));
        let running=stop.clone(); let engine=engine.clone();
        thread::spawn(move || { while !running.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(10)); engine.increment_epoch(); } });
        Self(stop)
    }
}
impl Drop for EpochClock { fn drop(&mut self) { self.0.store(true,Ordering::Relaxed); } }

struct HostState {
    manifest: Value,
    fixture: Value,
    task: Value,
    logs: Vec<Value>,
    trace: String,
    sequence: u64,
    run: u64,
    module_hash: String,
    deny_http: bool,
    task_running: bool,
    limits: StoreLimits,
    wasi: WasiCtx,
    table: wasmtime::component::ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> { WasiCtxView { ctx:&mut self.wasi, table:&mut self.table } }
}

impl HostState {
    fn event(&mut self, event: &str, stage: &str, code: Option<&str>, attributes: Value) {
        if self.logs.len() >= 1000 { return; }
        self.sequence += 1;
        self.logs.push(json!({
            "schema_version":"tokenme.plugin-log/v1alpha1", "sequence":self.sequence,
            "plugin_id":self.manifest["id"], "plugin_version":self.manifest["version"],
            "module_sha256":self.module_hash, "task_id":self.task["id"],
            "task_run_id":format!("reference-{}",self.run), "trace_id":self.trace,
            "span_id":format!("{:016x}",self.sequence), "stage":stage, "event":event,
            "error_code":code, "attributes":attributes,
        }));
    }
    fn permitted(&self, kind: &str, id: &str) -> bool {
        self.task_running && self.manifest["permissions"][kind].as_array().is_some_and(|v| v.iter().any(|e| e.as_str() == Some(id)))
            && self.task[kind].as_array().is_some_and(|v| v.iter().any(|e| e.as_str() == Some(id)))
    }
}

impl tokenme::plugin::host::Host for HostState {
    fn now_ms(&mut self) -> u64 { self.fixture["nowMs"].as_u64().unwrap_or(0) }
    fn http(&mut self, endpoint_id: String) -> Result<tokenme::plugin::host::HttpResponse, String> {
        if self.deny_http || !self.permitted("endpoints", &endpoint_id) {
            self.event("host.call.denied", "http", Some("PERMISSION.DENIED"), json!({"service":"http"}));
            return Err("PERMISSION.DENIED".into());
        }
        self.event("http.begin", "http", None, json!({"endpoint_id":endpoint_id}));
        let response = &self.fixture["endpoints"][&endpoint_id];
        let status = response["status"].as_u64().filter(|n| *n <= 599 && *n >= 100).ok_or("FIXTURE.ENDPOINT_MISSING")? as u16;
        let body = serde_json::to_string(&response["body"]).map_err(|_| "FIXTURE.INVALID")?;
        if body.len() > 1024 * 1024 { return Err("IO.RESPONSE_LIMIT".into()); }
        self.event("http.end", "http", None, json!({"endpoint_id":endpoint_id,"status":status}));
        Ok(tokenme::plugin::host::HttpResponse { status, body })
    }
    fn read_source(&mut self, source_id: String, cursor: Option<String>) -> Result<String, String> {
        if !self.permitted("sources", &source_id) {
            self.event("host.call.denied", "read", Some("PERMISSION.DENIED"), json!({"service":"source"}));
            return Err("PERMISSION.DENIED".into());
        }
        let records = self.fixture["sources"][&source_id]["records"].as_array().ok_or("FIXTURE.SOURCE_MISSING")?;
        let offset = cursor.as_deref().unwrap_or("0").parse::<usize>().map_err(|_| "CURSOR.INVALID")?;
        if offset > records.len() || records.len() > 1000 { return Err("CURSOR.INVALID".into()); }
        let body = json!({"records":records[offset..].to_vec(),"next_cursor":records.len().to_string()}).to_string();
        if body.len() > 1024 * 1024 { return Err("IO.RESPONSE_LIMIT".into()); }
        self.event("source.read", "read", None, json!({"source_id":source_id,"records":records.len()-offset}));
        Ok(body)
    }
    fn log(&mut self, event: String, fields_json: String) -> Result<(), String> {
        if !self.task_running { return Err("LOG.NO_CONTEXT".into()); }
        if fields_json.len() > 8192 { return Err("LOG.LIMIT".into()); }
        let declared = self.manifest["diagnostics"]["events"].as_array().and_then(|events| events.iter().find(|e| e["code"].as_str() == Some(&event))).cloned();
        let fields: Value = serde_json::from_str(&fields_json).map_err(|_| "LOG.INVALID")?;
        let schema = declared.as_ref().and_then(|v| v["fields"].as_object()).ok_or("LOG.INVALID")?;
        let values = fields.as_object().ok_or("LOG.INVALID")?;
        if schema.len() != values.len() || !schema.iter().all(|(key,kind)| match kind.as_str() {
            Some("integer") => values.get(key).and_then(Value::as_u64).is_some_and(|n| n <= 1000),
            Some("boolean") => values.get(key).is_some_and(Value::is_boolean),
            _ => false,
        }) { return Err("LOG.INVALID".into()); }
        self.event(&format!("plugin.{}",event), "parse", None, fields);
        Ok(())
    }
}

#[derive(Default)]
struct Ledger { facts: BTreeMap<String, UsageFact>, accepted: u64, duplicates: u64, updated: u64 }
impl Ledger {
    fn commit(&mut self, result: &TaskResult) -> Result<(), String> {
        result.validate()?;
        if let TaskResult::Usage { events, .. } = result {
            let mut staged = self.facts.clone();
            let (mut accepted, mut duplicates, mut updated) = (0, 0, 0);
            for event in events {
                match staged.get(&event.occurrence_id) {
                    None => accepted += 1,
                    Some(old) if old == event => duplicates += 1,
                    Some(_) => updated += 1,
                }
                staged.insert(event.occurrence_id.clone(), event.clone());
            }
            staged.values().try_fold(0u64, |n,e| n.checked_add(e.total()?).filter(|v| *v <= tokenme_plugin_sdk_reference::MAX_SAFE_INTEGER).ok_or_else(|| "DATA.NUMBER_RANGE".to_string()))?;
            self.facts = staged;
            self.accepted += accepted; self.duplicates += duplicates; self.updated += updated;
        }
        Ok(())
    }
    fn report(&self) -> Value {
        let mut models = BTreeMap::<String,u64>::new();
        for fact in self.facts.values() { *models.entry(fact.model.clone()).or_default() += fact.total().expect("validated fact"); }
        json!({"observed_tokens":models.values().sum::<u64>(),"by_model":models,"occurrences":self.facts.len(),"accepted":self.accepted,"duplicates":self.duplicates,"updated":self.updated})
    }
}

fn safe_error(code: &str) -> &str {
    match code {
        "PERMISSION.DENIED" | "AUTH.LOGIN_REQUIRED" | "NET.HTTP_ERROR" | "PARSE.SCHEMA_MISMATCH" | "INPUT.INVALID"
        | "TASK.NOT_REGISTERED" | "CURSOR.INVALID" | "DATA.NUMBER_RANGE" | "DATA.QUOTA_RANGE" | "DATA.IDENTITY"
        | "DATA.BATCH_LIMIT" | "LOG.INVALID" | "LOG.LIMIT" | "IO.RESPONSE_LIMIT" => code,
        _ => "PLUGIN.ERROR",
    }
}

fn load(path: &str) -> Result<Value,String> {
    let bytes = fs::read(path).map_err(|_| "FILE.READ_FAILED")?;
    if bytes.len() > 2 * 1024 * 1024 { return Err("FILE.LIMIT".into()); }
    serde_json::from_slice(&bytes).map_err(|_| "FILE.INVALID_JSON".into())
}
fn write_json(path: &str, value: &Value) -> Result<(),String> {
    if let Some(parent) = Path::new(path).parent() { fs::create_dir_all(parent).map_err(|_| "FILE.WRITE_FAILED")?; }
    fs::write(path, serde_json::to_vec_pretty(value).map_err(|_| "FILE.ENCODE_FAILED")?).map_err(|_| "FILE.WRITE_FAILED".into())
}

fn finish(state: &HostState, report: Value, args: &BTreeMap<String,String>) -> Result<Value,String> {
    if let Some(path) = args.get("--log") {
        if let Some(parent) = Path::new(path).parent() { fs::create_dir_all(parent).map_err(|_| "FILE.WRITE_FAILED")?; }
        let text = state.logs.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")+"\n";
        fs::write(path,text).map_err(|_| "FILE.WRITE_FAILED")?;
    }
    if let Some(path) = args.get("--output") { write_json(path,&report)?; }
    Ok(report)
}

fn run(args: &BTreeMap<String,String>) -> Result<Value,String> {
    let get = |key: &str| args.get(key).map(String::as_str).ok_or_else(|| "ARG.MISSING".to_string());
    let manifest = load(get("--manifest")?)?;
    if manifest["hostApi"].as_str() != Some(HOST_API) { return Err("PLUGIN.API_INCOMPATIBLE".into()); }
    let task_id = get("--task")?;
    let task = manifest["tasks"].as_array().and_then(|v| v.iter().find(|t| t["id"].as_str() == Some(task_id))).cloned().ok_or("TASK.NOT_REGISTERED")?;
    let fixture = load(get("--fixtures")?)?;
    let bytes = fs::read(get("--module")?).map_err(|_| "MODULE.READ_FAILED")?;
    if bytes.len() > 32 * 1024 * 1024 || bytes.get(..8) != Some(b"\0asm\x0d\0\x01\0") { return Err("MODULE.NOT_COMPONENT".into()); }
    let repeat = args.get("--repeat").map(|v| v.parse::<usize>()).transpose().map_err(|_| "ARG.INVALID")?.unwrap_or(1);
    if !(1..=10).contains(&repeat) { return Err("ARG.INVALID".into()); }
    let fuel = args.get("--fuel").map(|v| v.parse::<u64>()).transpose().map_err(|_| "ARG.INVALID")?.unwrap_or(10_000_000);
    let deadline_ms=task["timeoutMs"].as_u64().filter(|n| (1..=30000).contains(n)).ok_or("TASK.DEADLINE")?;
    let epoch_ticks=deadline_ms.div_ceil(10);
    let mut config = Config::new(); config.wasm_component_model(true); config.consume_fuel(true); config.epoch_interruption(true);
    let engine = Engine::new(&config).map_err(|_| "RUNTIME.ENGINE")?;
    let _epoch_clock=EpochClock::start(&engine);
    let component = Component::from_binary(&engine, &bytes).map_err(|_| "MODULE.INVALID")?;
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| "CLOCK.INVALID")?.as_nanos();
    let mut wasi = WasiCtxBuilder::new();
    wasi.allow_tcp(false).allow_udp(false).allow_ip_name_lookup(false);
    let state = HostState { manifest, fixture, task, logs: Vec::new(), trace:format!("{:032x}",seed), sequence:0, run:0,
        module_hash:format!("{:x}",Sha256::digest(&bytes)), deny_http:args.contains_key("--deny-http"),task_running:false,
        limits:StoreLimitsBuilder::new().memory_size(64*1024*1024).build(),wasi:wasi.build(),table:wasmtime::component::ResourceTable::new() };
    let mut store = Store::new(&engine,state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(fuel).map_err(|_| "RUNTIME.FUEL")?;
    store.set_epoch_deadline(epoch_ticks);
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|_| "RUNTIME.WASI_LINK")?;
    Adapter::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |state| state).map_err(|_| "RUNTIME.LINK")?;
    let settings_key = store.data().task["enabledSetting"].as_str().ok_or("TASK.SETTING")?;
    let default_enabled = store.data().manifest["settings"].as_array().and_then(|v|v.iter().find(|s|s["key"].as_str()==Some(settings_key))).and_then(|s|s["default"].as_bool()).ok_or("TASK.SETTING")?;
    if args.contains_key("--disabled") || !default_enabled {
        store.data_mut().event("task.gated","gate",Some("TASK.GATED"),json!({"reason":"reference-setting-disabled"}));
        let report = json!({"status":"gated","error_code":null,"summary":Ledger::default().report(),"results":[],"trace_id":store.data().trace});
        return finish(store.data(),report,args);
    }
    let binding = match Adapter::instantiate(&mut store,&component,&linker) {
        Ok(binding) => binding,
        Err(error) => {
            let code = match error.downcast_ref::<Trap>() { Some(Trap::OutOfFuel) => "RUNTIME.BUDGET_EXCEEDED", Some(Trap::Interrupt) => "RUNTIME.DEADLINE", _ => "MODULE.IMPORTS_UNSUPPORTED" };
            store.data_mut().event("plugin.load.failed","load",Some(code),json!({}));
            let report = json!({"status":"failed","error_code":code,"summary":Ledger::default().report(),"results":[],"trace_id":store.data().trace});
            return finish(store.data(),report,args);
        }
    };
    let mut ledger = Ledger::default();
    let mut outputs = Vec::new();
    let mut error = None;
    for number in 1..=repeat {
        store.data_mut().run = number as u64;
        store.set_fuel(fuel).map_err(|_| "RUNTIME.FUEL")?;
        store.set_epoch_deadline(epoch_ticks);
        store.data_mut().event("task.begin","task",None,json!({}));
        store.data_mut().task_running=true;
        let called = binding.call_execute(&mut store,task_id,"{}");
        store.data_mut().task_running=false;
        let parsed = match called {
            Ok(Ok(body)) if body.len() <= 1024 * 1024 => serde_json::from_str::<TaskResult>(&body).map_err(|_| "RESULT.INVALID".to_string()),
            Ok(Ok(_)) => Err("RESULT.LIMIT".into()),
            Ok(Err(code)) => Err(safe_error(&code).into()),
            Err(e) => Err(match e.downcast_ref::<Trap>() { Some(Trap::OutOfFuel) => "RUNTIME.BUDGET_EXCEEDED", Some(Trap::Interrupt) => "RUNTIME.DEADLINE", _ => "RUNTIME.TRAP" }.into()),
        };
        match parsed {
            Ok(result) => {
                let expected = matches!((&result,store.data().task["capability"].as_str()),(TaskResult::Quota { .. },Some("quota.read")) | (TaskResult::Usage { .. },Some("usage.collect")));
                let committed = if expected { ledger.commit(&result) } else { Err("DATA.CAPABILITY_MISMATCH".into()) };
                if let Err(code) = committed { error=Some(code); break; }
                store.data_mut().event("batch.committed","ingest",None,json!({"occurrences":ledger.facts.len()}));
                outputs.push(serde_json::to_value(result).map_err(|_| "RESULT.INVALID")?);
                store.data_mut().event("task.end","task",None,json!({"outcome":"succeeded"}));
            }
            Err(code) => { store.data_mut().event("task.end","task",Some(&code),json!({"outcome":"failed"})); error=Some(code); break; }
        }
    }
    store.data_mut().event("report.published","report",None,json!({"observed_tokens":ledger.report()["observed_tokens"]}));
    let report = json!({"status":if error.is_some(){"failed"}else if args.contains_key("--disabled"){"gated"}else{"ok"},"error_code":error,"summary":ledger.report(),"results":outputs,"trace_id":store.data().trace});
    finish(store.data(),report,args)
}

fn main() {
    let mut args = BTreeMap::new();
    let mut input = env::args().skip(1);
    while let Some(key) = input.next() {
        if key == "--help" { println!("reference host: --module FILE --manifest FILE --fixtures FILE --task ID [--repeat N] [--output FILE] [--log FILE] [--deny-http] [--disabled] [--fuel N]"); return; }
        if ["--deny-http","--disabled"].contains(&key.as_str()) { args.insert(key,"true".into()); }
        else if ["--module","--manifest","--fixtures","--task","--repeat","--output","--log","--fuel"].contains(&key.as_str()) {
            if let Some(value)=input.next() { args.insert(key,value); } else { eprintln!("ARG.MISSING"); std::process::exit(2); }
        } else { eprintln!("ARG.INVALID"); std::process::exit(2); }
    }
    match run(&args) {
        Ok(report) => { println!("{report}"); if report["status"] == "failed" { std::process::exit(1); } }
        Err(code) => { println!("{}",json!({"status":"failed","error_code":code})); std::process::exit(1); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fact(output:u64) -> UsageFact { UsageFact { occurrence_id:"same-request".into(),model:"m".into(),ts_ms:1,input:120,cache_creation:0,cache_read:0,output } }
    #[test]
    fn replay_is_idempotent_and_corrections_replace() {
        let mut ledger=Ledger::default();
        for _ in 0..2 { ledger.commit(&TaskResult::Usage{events:vec![fact(80)],next_cursor:Some("1".into())}).unwrap(); }
        assert_eq!(ledger.report()["observed_tokens"],200);
        assert_eq!(ledger.report()["duplicates"],1);
        ledger.commit(&TaskResult::Usage{events:vec![fact(90)],next_cursor:Some("1".into())}).unwrap();
        assert_eq!(ledger.report()["observed_tokens"],210);
        assert_eq!(ledger.report()["updated"],1);
    }
    #[test]
    fn malformed_batch_does_not_partially_commit() {
        let mut ledger=Ledger::default();
        let mut bad=fact(1); bad.occurrence_id.clear();
        assert!(ledger.commit(&TaskResult::Usage{events:vec![fact(80),bad],next_cursor:Some("2".into())}).is_err());
        assert_eq!(ledger.report()["occurrences"],0);
    }
    #[test]
    fn epoch_clock_interrupts_a_guest_loop_without_fuel() {
        let mut config=Config::new(); config.epoch_interruption(true);
        let engine=Engine::new(&config).unwrap();
        let bytes=b"\0asm\x01\0\0\0\x01\x04\x01\x60\0\0\x03\x02\x01\0\x07\x07\x01\x03run\0\0\x0a\x09\x01\x07\0\x03\x40\x0c\0\x0b\x0b";
        let module=wasmtime::Module::new(&engine,bytes).unwrap();
        let mut store=Store::new(&engine,()); store.set_epoch_deadline(1);
        let instance=wasmtime::Instance::new(&mut store,&module,&[]).unwrap();
        let run=instance.get_typed_func::<(),()>(&mut store,"run").unwrap();
        let _clock=EpochClock::start(&engine);
        let error=run.call(&mut store,()).unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(),Some(&Trap::Interrupt));
    }
}
