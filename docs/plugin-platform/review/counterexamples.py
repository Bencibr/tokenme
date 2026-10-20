"""Explain v0.4 document counterexamples against a saved source snapshot, not a runtime exploit."""
import argparse
import hashlib
import json
from pathlib import Path


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline",type=Path,required=True)
    parser.add_argument("--output",type=Path,required=True)
    args=parser.parse_args()
    def evidence(file,needle):
        path=args.baseline/file
        data=path.read_bytes()
        matches=[{"line":number,"text":line} for number,line in enumerate(data.decode("utf-8").splitlines(),1) if needle in line]
        if not matches: raise ValueError("baseline quote not found: "+file+" / "+needle)
        return {"file":file,"sha256":hashlib.sha256(data).hexdigest(),"quotes":matches}
    safe=9007199254740991
    cases=[
        {"id":"DA01","evidence":[evidence("PRODUCTION_CONTRACTS.md","| Scope |"),evidence("PRODUCTION_CONTRACTS.md","| BatchHeader |")],"input":{"stage":"source.detect before accounts exist","known_refs":["tool","device"]},"counterexample":{"missing_required_scope_field":"account_ref","batch_ref_issuer_service":"not specified"},"required_resolution":"discovery scope and host-issued reference path"},
        {"id":"DA02","evidence":[evidence("PRODUCTION_CONTRACTS.md","| revision |"),evidence("PRODUCTION_CONTRACTS.md","重试相同 batch_id 返回原 receipt")],"input":{"old":{"revision":1,"tokens":100},"new":{"revision":1,"tokens":200},"batch_id":"same"},"counterexample":{"last_writer_policy":200,"first_writer_policy":100,"specified_conflict_policy":None},"required_resolution":"same revision different payload must conflict; receipt binds scope and payload hash"},
        {"id":"DA03","evidence":[evidence("PRODUCTION_CONTRACTS.md","| counts |"),evidence("PRODUCTION_CONTRACTS.md","| ts_ms |"),evidence("PRODUCTION_CONTRACTS.md","cumulative 按")],"input":{"credits":"3.5","observed_tokens":None,"ts_ms":None,"same_millisecond_snapshots":[150,100]},"counterexample":{"required_counts_forces_fake_zero":True,"pending_wire_representation":None,"snapshot_order_without_sample_index":"ambiguous"},"required_resolution":"nullable counts/time with pending; explicit cumulative sequence and reset evidence"},
        {"id":"DA04","evidence":[evidence("PRODUCTION_CONTRACTS.md","所有设置/grants/generation 改变使旧结果失效"),evidence("PRODUCTION_CONTRACTS.md","不强制刷新配额")],"input":{"collection_started_at_settings_revision":1,"notification_saved_at_settings_revision":2},"counterexample":{"accept_valid_collection":False,"reason":"unrelated notification change invalidates it"},"required_resolution":"separate execution, policy and presentation revisions"},
        {"id":"DA05","evidence":[evidence("PRODUCTION_CONTRACTS.md","`quota.read` 输入"),evidence("PRODUCTION_CONTRACTS.md","有效期过后标 stale")],"input":{"last_good":{"daily":25},"login_result":{"status":"login_required","windows":[]},"new_percent":{"used":"50","limit":"200","used_percent":75}},"counterexample":{"empty_success_would_erase_last_good":True,"reported_vs_derived_percent":[75,25],"host_maximum_ttl":"not specified"},"required_resolution":"health separated from last_good; partial set; consistency check and TTL clamp"},
        {"id":"DA06","evidence":[evidence("PRODUCTION_CONTRACTS.md","冷却默认 30 分钟")],"input":{"remaining_percent":[25,15,3],"elapsed_between_crossings_ms":300000,"os_call_succeeded_before_local_receipt":True},"counterexample":{"naive_30_minute_cooldown_notifications":[20],"crash_replay_can_duplicate_os_notification":True},"required_resolution":"severity escalation bypass and explicit delivery_unknown recovery"},
        {"id":"DA07","evidence":[evidence("PRODUCTION_CONTRACTS.md","channels 初始 [system]，只可选择平台已实现且授权")],"input":{"default_channels":["system"],"os_permission":False},"counterexample":{"default_config_legal":False,"reason":"configuration schema and present delivery availability are conflated"},"required_resolution":"stable channel schema, availability gate, typed defaults for each public policy"},
        {"id":"DA08","evidence":[evidence("PRODUCTION_CONTRACTS.md","state.read/state.write"),evidence("PRODUCTION_CONTRACTS.md","v1 普通第三方 profile 不支持")],"input":{"operation":"credential refresh before quota read","planned_migration_services":["state.read","state.write"],"defined_state_services":["state.get","state.cas"]},"counterexample":{"undefined_migration_services":["state.read","state.write"],"credential_maintenance_profile_contract":None},"required_resolution":"explicit auth.refresh/effect/profile and real migration service names"},
        {"id":"DA09","evidence":[evidence("PRODUCTION_CONTRACTS.md","| pricing.resolve |"),evidence("PRODUCTION_CONTRACTS.md","Decimal |")],"input":{"per_units":"0","amount":"1","sum_inputs":[safe,2]},"counterexample":{"zero_divisor_allowed_by_decimal":True,"legacy_float_sum_as_integer":int(float(safe)+2),"exact_sum":safe+2},"required_resolution":"positive denominator/rate and exact aggregate overflow representation"},
        {"id":"DA10","evidence":[evidence("IMPLEMENTATION_SPEC.md","| F02 包与信任 |"),evidence("IMPLEMENTATION_SPEC.md","| F03 运行器 |")],"input":{"F02_requires_real_migration_runtime":True,"F02_declared_dependencies":["F01"]},"counterexample":{"runtime_dependency_present":False},"required_resolution":"F03 precedes real F02 migration; explicit DAG"},
        {"id":"DA11","evidence":[evidence("IMPLEMENTATION_SPEC.md","| F11 代表性迁移 |"),evidence("PLAN.md","P4：全面迁移")],"input":{"representative_tools":["codex","qoder_cn"],"quota_only_examples":["copilot","gemini"]},"counterexample":{"full_remaining_provider_migration_owner":"not explicitly assigned in F01-F12"},"required_resolution":"F11.2 union inventory, strategies and F12 legacy-removal gate"},
        {"id":"DA12","evidence":[evidence("LANGUAGE_AND_SDK.md","| 注册/初始化/卸载 |"),evidence("PRODUCTION_CONTRACTS.md","清单沿用当前 camelCase")],"input":{"manifest_meta":"defaultConfig","business_config_key":"thresholds_remaining_percent","requested_guest_hook":"unload"},"counterexample":{"recursive_casing_would_corrupt_business_key":True,"wire_export_for_guest_unload":"not defined"},"required_resolution":"generated field mapping only; no undeclared guest lifecycle hook"},
    ]
    output={"schema":"tokenme.document-counterexamples/0.5","evidence_class":"design_counterexamples_not_runtime_poc","production_runtime_validated":False,"cases":cases}
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(output,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    print(json.dumps({"counterexamples":len(cases),"baseline_quotes_checked":True,"runtime_exploits_claimed":False}))


if __name__=="__main__": main()
