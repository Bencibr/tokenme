"""Executable design oracles only. Does not run a production plugin or claim F01 completion."""
from __future__ import annotations

import argparse
import copy
from decimal import Decimal, ROUND_HALF_EVEN, localcontext
import hashlib
import json
import math
from pathlib import Path
import re
import sys
import unittest
import uuid

from jsonschema import Draft202012Validator

HERE = Path(__file__).resolve().parent
DOCS = HERE.parent
SAFE = 9007199254740991
REF = "1" * 32


class ContractError(Exception):
    pass


def require(condition, code):
    if not condition:
        raise ContractError(code)


def validate_schema(name, value):
    schema = json.loads((HERE / name).read_text(encoding="utf-8"))
    Draft202012Validator.check_schema(schema)
    require(not list(Draft202012Validator(schema).iter_errors(value)), "SCHEMA.INVALID")


def scope_for(capability, scope):
    validate_schema("scope.schema.json", scope)
    kinds = {
        "source.detect":{"tool"}, "auth.discover":{"tool"}, "pricing.resolve":{"tool"},
        "quota.read":{"account"}, "checkin.run":{"account"}, "auth.refresh":{"account"},
        "usage.collect":{"source"}, "telemetry.observe":{"source"},
    }
    require(scope["kind"] in kinds[capability], "CAPABILITY.SCOPE")
    return True


def resolve_policy(record):
    validate_schema("notification.schema.json", record)
    effective = copy.deepcopy(record["plugin"])
    origins = {key:"plugin" for key in effective}
    for label, values in (("global",record["global"]),("tool",record["override"]["values"])):
        for key, value in values.items():
            effective[key] = copy.deepcopy(value)
            origins[key] = label
    if record["kind"] == "quota_percent_low":
        levels = effective["thresholds_remaining_percent"]
        require(all(math.isfinite(level) for level in levels), "CONFIG.NONFINITE")
        require(levels == sorted(levels,reverse=True), "CONFIG.ORDER")
    if record["kind"] == "usage_threshold":
        levels = effective["thresholds_tokens"]
        require(levels == sorted(levels), "CONFIG.ORDER")
    return effective, origins


def validate_fact(value, *, observed_at=2000000):
    validate_schema("usage-fact.schema.json", value)
    for key, limit in (("occurrence_id",256),("model",128),("replaces",256)):
        text = value[key]
        if text is not None:
            require(len(text.encode("utf-8")) <= limit and not any(ord(ch) < 32 or ord(ch) == 127 for ch in text), "DATA.TEXT")
    require(any(value[key] is not None for key in ("counts","native_credits","vendor_cost")), "DATA.MEASUREMENT_REQUIRED")
    require(value["ts_ms"] is not None or value["completeness"] == "partial", "DATA.TIME_REQUIRED")
    require(value["ts_ms"] is None or value["ts_ms"] <= observed_at + 300000, "CLOCK.SKEW")
    require(value["identity_domain"] != "session_call" or value["session_ref"] is not None, "DATA.SESSION_REQUIRED")
    counts = value["counts"]
    if counts is not None:
        require(sum(counts[key] for key in ("input","cache_creation","cache_read","output")) <= SAFE, "DATA.COUNT_RANGE")
        require(counts["reasoning"] <= counts["output"], "DATA.REASONING_RANGE")
    cumulative = value["cumulative"]
    require((cumulative is not None) == (value["usage_form"] == "cumulative"), "DATA.CUMULATIVE_SHAPE")
    if cumulative is not None and (cumulative["starts_at_zero"] or cumulative["reset_seq"] > 0):
        require(cumulative["reset_evidence_ref"] is not None, "DATA.RESET_EVIDENCE")
    return True


def contribution_decision(old, new):
    if new["revision"] < old["revision"]:
        return "stale"
    if new["revision"] == old["revision"]:
        require(old == new, "DATA.REVISION_CONFLICT")
        return "duplicate"
    require(not (old["completeness"] == "final" and new["completeness"] == "partial"), "DATA.FINAL_REGRESSION")
    return "corrected"


def batch_receipt(existing, scope, payload):
    digest = hashlib.sha256(canonical(payload)).hexdigest()
    require(existing["scope"] == scope and existing["digest"] == digest, "DATA.BATCH_ID_REUSED")
    return existing["receipt"]


def canonical(value):
    return json.dumps(value,ensure_ascii=False,sort_keys=True,separators=(",",":"),allow_nan=False).encode("utf-8")


def accepts_context(old, current):
    keys = ("generation","execution_revision","grant_revision","lease_token","cursor_revision")
    return all(old[key] == current[key] for key in keys)


def decimal(value):
    require(isinstance(value,str) and re.fullmatch(r"(0|[1-9][0-9]{0,17})(\.[0-9]{1,12})?",value) is not None, "DATA.DECIMAL")
    return Decimal(value)


def price_amount(count, amount, per_units):
    with localcontext() as ctx:
        ctx.prec = 100
        denominator = decimal(per_units)
        require(denominator > 0, "DATA.PRICE_DIVISOR")
        result = Decimal(count) * decimal(amount) / denominator
        require(result < Decimal(10) ** 18, "DATA.AMOUNT_RANGE")
        return result.quantize(Decimal("0.000000000001"),rounding=ROUND_HALF_EVEN)


def aggregate(values, *, known_empty=False):
    if not values and not known_empty:
        return {"status":"unavailable","exact_value":None,"numeric_value":None,"reason":"NO_TOKEN_MEASUREMENT"}
    total = sum(values)
    require(0 <= total < 10 ** 38, "DATA.COUNT_RANGE")
    return {"status":"ok" if total <= SAFE else "out_of_number_range", "exact_value":str(total), "numeric_value":total if total <= SAFE else None}


def cumulative_total(samples):
    total, previous = 0, None
    for sample in sorted(samples,key=lambda entry:entry["index"]):
        if previous is None:
            if sample.get("zero"):
                require(sample.get("evidence"), "DATA.RESET_EVIDENCE")
                total += sample["value"]
        else:
            require(sample["value"] >= previous, "DATA.CUMULATIVE_RESET_UNKNOWN")
            total += sample["value"] - previous
        previous = sample["value"]
    return total


def normalized_percent(used, limit, remaining, reported):
    with localcontext() as ctx:
        ctx.prec = 100
        computed = None
        if limit is not None and decimal(limit) > 0:
            if used is not None:
                used_amount = decimal(used)
            elif remaining is not None:
                used_amount = decimal(limit) - decimal(remaining)
                require(used_amount >= 0,"DATA.QUOTA_INCONSISTENT")
            else:
                used_amount = None
            if used_amount is not None:
                computed = used_amount * 100 / decimal(limit)
                if remaining is not None:
                    require(abs(decimal(remaining)-max(decimal(limit)-used_amount,Decimal(0))) <= Decimal("0.000000000001"), "DATA.QUOTA_INCONSISTENT")
        if reported is not None:
            require(type(reported) in (int,float) and math.isfinite(reported) and 0 <= reported <= 10000,"DATA.QUOTA_RANGE")
            require(computed is None or abs(Decimal(str(reported))-computed) <= Decimal("0.01"),"DATA.QUOTA_INCONSISTENT")
        chosen = Decimal(str(reported)) if reported is not None else computed
        require(chosen is None or 0 <= chosen <= 10000,"DATA.QUOTA_RANGE")
        return None if chosen is None else float(chosen)


def quota_cache(previous, sample, observed_at):
    require(sample["sampled_at"] <= observed_at+300000,"CLOCK.SKEW")
    require(sample["valid_until"] >= sample["sampled_at"],"DATA.TIME_ORDER")
    if sample["status"] in ("login_required","unavailable"):
        require(not sample["windows"],"DATA.STATUS_WINDOWS")
        return {**copy.deepcopy(previous),"health":sample["status"]}
    windows = {} if sample["window_set"] == "complete" else copy.deepcopy(previous["windows"])
    windows.update(sample["windows"])
    return {"health":sample["status"],"windows":windows,"effective_valid_until":min(sample["valid_until"],sample["sampled_at"]+300000,observed_at+300000)}


def threshold_step(state, remaining, *, now, enabled=True, master=True, os_allowed=True, channels=("system",), cooldown=1800000, thresholds=(20,5)):
    old = state["previous"]
    state["previous"] = remaining
    if old is None:
        state["seen"].update(level for level in thresholds if remaining <= level)
        return None
    crossed = [level for level in thresholds if old > level >= remaining and level not in state["seen"]]
    state["seen"].update(crossed)
    if not crossed or not (enabled and master and os_allowed and channels):
        return None
    chosen = min(crossed)
    more_severe = state["delivered_threshold"] is not None and chosen < state["delivered_threshold"]
    if state["delivered_at"] is not None and now-state["delivered_at"] < cooldown and not more_severe:
        return None
    state["delivered_at"], state["delivered_threshold"] = now, chosen
    return chosen


def delivery_recovery(state, os_stable_id_supported=False):
    if state == "delivering":
        return "reconcile_same_id" if os_stable_id_supported else "delivery_unknown_no_retry"
    return "unchanged"


def dispatch_service(capability, effect, service, profile="standard-v1", *, migration_snapshot=False):
    if capability == "host:migrate":
        return migration_snapshot and service in ("state.get","state.cas")
    if service == "credential.exchange":
        return capability == "auth.refresh" and effect == "credential_write" and profile == "credential-maintenance-v1"
    return service in ("state.get","state.cas","clock.now","identity.resolve")


def acyclic(dependencies):
    remaining = {name:set(values) for name,values in dependencies.items()}
    order = []
    while remaining:
        ready = sorted(name for name,values in remaining.items() if not values)
        require(bool(ready),"PLAN.DEPENDENCY_CYCLE")
        for name in ready:
            order.append(name); del remaining[name]
        for values in remaining.values(): values.difference_update(ready)
    return order


def example_fact():
    return {"identity_domain":"vendor_request","occurrence_id":"req-1","session_ref":None,"ts_ms":1000,"model":"model-a","usage_form":"per_call","counts":{"input":120,"cache_creation":0,"cache_read":0,"output":80,"reasoning":40},"cumulative":None,"native_credits":None,"vendor_cost":None,"completeness":"final","revision":1,"replaces":None}


def example_policy():
    return {"kind":"quota_percent_low","plugin":{"enabled":True,"thresholds_remaining_percent":[20,5],"cooldown_ms":1800000,"channels":["system"]},"global":{},"override":{"mode":"inherit","values":{}}}


def notification_state():
    return {"previous":25,"seen":set(),"delivered_at":None,"delivered_threshold":None}


class DesignContracts(unittest.TestCase):
    def rejects(self, code, action):
        with self.assertRaises(ContractError) as caught:
            action()
        self.assertEqual(str(caught.exception),code)

    def test_discovery_without_a_fake_account(self):
        self.assertTrue(scope_for("source.detect",{"kind":"tool","tool_ref":REF,"origin_device_ref":REF}))

    def test_account_task_cannot_use_discovery_scope(self):
        self.rejects("CAPABILITY.SCOPE",lambda:scope_for("checkin.run",{"kind":"tool","tool_ref":REF,"origin_device_ref":REF}))

    def test_source_scope_must_include_account(self):
        self.rejects("SCHEMA.INVALID",lambda:scope_for("usage.collect",{"kind":"source","tool_ref":REF,"source_ref":REF,"origin_device_ref":REF}))

    def test_scope_rejects_foreign_untyped_fields(self):
        self.rejects("SCHEMA.INVALID",lambda:scope_for("source.detect",{"kind":"tool","tool_ref":REF,"origin_device_ref":REF,"account_ref":REF}))

    def test_partial_override_keeps_global_and_field_sources(self):
        record=example_policy(); record["global"]={"thresholds_remaining_percent":[30,5],"cooldown_ms":60000}; record["override"]={"mode":"override","values":{"thresholds_remaining_percent":[10]}}
        value,origins=resolve_policy(record)
        self.assertEqual(value["thresholds_remaining_percent"],[10]); self.assertEqual(value["cooldown_ms"],60000)
        self.assertEqual(origins["thresholds_remaining_percent"],"tool");self.assertEqual(origins["cooldown_ms"],"global")

    def test_false_zero_empty_are_real_overrides(self):
        record=example_policy();record["override"]={"mode":"override","values":{"enabled":False,"cooldown_ms":0,"thresholds_remaining_percent":[]}}
        value,_=resolve_policy(record)
        self.assertEqual((value["enabled"],value["cooldown_ms"],value["thresholds_remaining_percent"]),(False,0,[]))

    def test_inherit_cannot_retain_hidden_override(self):
        record=example_policy();record["override"]["values"]={"enabled":False}
        self.rejects("SCHEMA.INVALID",lambda:resolve_policy(record))

    def test_unknown_channel_is_not_an_arbitrary_webhook(self):
        record=example_policy();record["override"]={"mode":"override","values":{"channels":["https://outside.invalid"]}}
        self.rejects("SCHEMA.INVALID",lambda:resolve_policy(record))

    def test_valid_system_setting_can_be_unavailable_for_delivery(self):
        value,_=resolve_policy(example_policy());state=notification_state()
        self.assertEqual(value["channels"],["system"])
        self.assertIsNone(threshold_step(state,15,now=0,os_allowed=False))
        self.assertIn(20,state["seen"])

    def test_empty_channel_override_observes_without_delivering(self):
        state=notification_state();self.assertIsNone(threshold_step(state,15,now=0,channels=()))
        self.assertIn(20,state["seen"])

    def test_first_sample_is_a_baseline_not_a_historical_alert(self):
        state=notification_state();state["previous"]=None
        self.assertIsNone(threshold_step(state,3,now=0));self.assertEqual(state["seen"],{20,5})

    def test_different_windows_have_independent_threshold_state(self):
        daily,weekly=notification_state(),notification_state()
        self.assertEqual(threshold_step(daily,15,now=0),20)
        self.assertEqual(threshold_step(weekly,15,now=1000),20)

    def test_boolean_cannot_be_a_threshold_number(self):
        record=example_policy();record["global"]={"thresholds_remaining_percent":[True]}
        self.rejects("SCHEMA.INVALID",lambda:resolve_policy(record))

    def test_global_change_does_not_affect_overridden_threshold(self):
        record=example_policy();record["override"]={"mode":"override","values":{"thresholds_remaining_percent":[10]}}
        before,_=resolve_policy(record);record["global"]={"thresholds_remaining_percent":[30,5]};after,_=resolve_policy(record)
        self.assertEqual(before,after)

    def test_higher_severity_bypasses_lower_tier_cooldown(self):
        state=notification_state();self.assertEqual(threshold_step(state,15,now=0),20)
        self.assertEqual(threshold_step(state,3,now=300000),5)

    def test_multiple_crossings_make_one_most_severe_notification(self):
        self.assertEqual(threshold_step(notification_state(),3,now=0),5)

    def test_same_tier_oscillation_does_not_repeat(self):
        state=notification_state();self.assertEqual(threshold_step(state,15,now=0),20)
        self.assertIsNone(threshold_step(state,25,now=2000000));self.assertIsNone(threshold_step(state,15,now=4000000))

    def test_muted_observation_is_not_replayed_on_unmute(self):
        state=notification_state();self.assertIsNone(threshold_step(state,15,now=0,master=False))
        self.assertIsNone(threshold_step(state,14,now=1000));self.assertEqual(threshold_step(state,3,now=2000),5)

    def test_os_receipt_loss_is_unknown_not_blind_retry(self):
        self.assertEqual(delivery_recovery("delivering"),"delivery_unknown_no_retry")
        self.assertEqual(delivery_recovery("delivering",True),"reconcile_same_id")

    def test_notification_setting_change_does_not_invalidate_collection(self):
        old={"generation":1,"execution_revision":1,"grant_revision":1,"lease_token":1,"cursor_revision":1,"settings_snapshot_revision":1,"notification_policy_revision":1}
        current={**old,"settings_snapshot_revision":2,"notification_policy_revision":2}
        self.assertTrue(accepts_context(old,current))

    def test_execution_or_grant_change_invalidates_collection(self):
        old={"generation":1,"execution_revision":1,"grant_revision":1,"lease_token":1,"cursor_revision":1}
        for key in old:
            self.assertFalse(accepts_context(old,{**old,key:2}))

    def test_same_revision_different_contribution_is_conflict(self):
        old=example_fact();new=copy.deepcopy(old);new["counts"]["input"]=999
        self.rejects("DATA.REVISION_CONFLICT",lambda:contribution_decision(old,new))

    def test_replay_stale_and_corrected_revisions(self):
        old=example_fact();self.assertEqual(contribution_decision(old,copy.deepcopy(old)),"duplicate")
        self.assertEqual(contribution_decision(old,{**old,"revision":0}),"stale")
        self.assertEqual(contribution_decision(old,{**old,"revision":2}),"corrected")

    def test_final_contribution_cannot_regress_to_partial(self):
        old=example_fact();self.rejects("DATA.FINAL_REGRESSION",lambda:contribution_decision(old,{**old,"revision":2,"completeness":"partial"}))

    def test_batch_ref_binds_scope_and_payload(self):
        payload={"facts":[1]};existing={"scope":"account-source-a","digest":hashlib.sha256(canonical(payload)).hexdigest(),"receipt":"receipt-1"}
        self.assertEqual(batch_receipt(existing,"account-source-a",payload),"receipt-1")
        self.rejects("DATA.BATCH_ID_REUSED",lambda:batch_receipt(existing,"account-source-b",payload))
        self.rejects("DATA.BATCH_ID_REUSED",lambda:batch_receipt(existing,"account-source-a",{"facts":[2]}))

    def test_canonical_golden_bytes_are_utf8_and_key_sorted(self):
        self.assertEqual(canonical({"z":None,"a":"中"}).hex(),"7b2261223a22e4b8ad222c227a223a6e756c6c7d")

    def test_canonical_sort_is_unicode_scalar_not_utf16_order(self):
        self.assertEqual(canonical({chr(0x10000):1,chr(0xE000):2}).hex(),"7b22ee8080223a322c22f0908080223a317d")

    def test_credits_only_fact_does_not_manufacture_zero_tokens(self):
        fact=example_fact();fact["counts"]=None;fact["native_credits"]={"unit":"credits","amount":"3.5"}
        self.assertTrue(validate_fact(fact));self.assertIsNone(fact["counts"])

    def test_explicit_zero_token_fact_is_valid(self):
        fact=example_fact();fact["counts"]={key:0 for key in fact["counts"]}
        self.assertTrue(validate_fact(fact))

    def test_missing_timestamp_is_pending_and_not_final(self):
        fact=example_fact();fact["ts_ms"]=None
        self.rejects("DATA.TIME_REQUIRED",lambda:validate_fact(fact));fact["completeness"]="partial";self.assertTrue(validate_fact(fact))

    def test_future_timestamp_and_multibyte_length_are_rejected(self):
        fact=example_fact();fact["ts_ms"]=2300001
        self.rejects("CLOCK.SKEW",lambda:validate_fact(fact))
        fact=example_fact();fact["occurrence_id"]="汉"*86
        self.rejects("DATA.TEXT",lambda:validate_fact(fact))

    def test_unsafe_stage_sum_and_reasoning_are_rejected(self):
        fact=example_fact();fact["counts"]["input"]=SAFE
        self.rejects("DATA.COUNT_RANGE",lambda:validate_fact(fact))
        fact=example_fact();fact["counts"]["reasoning"]=81
        self.rejects("DATA.REASONING_RANGE",lambda:validate_fact(fact))

    def test_cumulative_first_sample_needs_real_zero_evidence(self):
        fact=example_fact();fact["usage_form"]="cumulative";fact["cumulative"]={"series_ref":REF,"sample_index":0,"reset_seq":0,"starts_at_zero":True,"reset_evidence_ref":None}
        self.rejects("DATA.RESET_EVIDENCE",lambda:validate_fact(fact))
        fact["cumulative"]["reset_evidence_ref"]=REF;self.assertTrue(validate_fact(fact))

    def test_cumulative_same_millisecond_orders_by_sample_index(self):
        self.assertEqual(cumulative_total([{"index":2,"value":150},{"index":1,"value":100}]),50)
        self.assertEqual(cumulative_total([{"index":1,"value":100,"zero":True,"evidence":REF},{"index":2,"value":150}]),150)
        self.rejects("DATA.CUMULATIVE_RESET_UNKNOWN",lambda:cumulative_total([{"index":1,"value":150},{"index":2,"value":100}]))

    def test_quota_native_unit_percent_is_derived_and_consistent(self):
        self.assertEqual(normalized_percent("50","200","150",None),25)
        self.rejects("DATA.QUOTA_INCONSISTENT",lambda:normalized_percent("50","200","150",75))
        self.rejects("DATA.QUOTA_INCONSISTENT",lambda:normalized_percent("50","200","140",25))
        self.assertIsNone(normalized_percent("50",None,None,None))

    def test_failed_health_preserves_last_good_quota(self):
        previous={"health":"ok","windows":{"daily":25},"effective_valid_until":10000}
        sample={"status":"login_required","window_set":"complete","sampled_at":1000,"valid_until":10000,"windows":{}}
        result=quota_cache(previous,sample,1000)
        self.assertEqual(result["windows"],{"daily":25});self.assertEqual(result["health"],"login_required")

    def test_partial_quota_set_does_not_delete_other_windows(self):
        previous={"health":"ok","windows":{"daily":25,"weekly":10},"effective_valid_until":10000}
        sample={"status":"ok","window_set":"partial","sampled_at":1000,"valid_until":999999999,"windows":{"daily":30}}
        result=quota_cache(previous,sample,1000)
        self.assertEqual(result["windows"],{"daily":30,"weekly":10});self.assertEqual(result["effective_valid_until"],301000)
        sample["window_set"]="complete";self.assertEqual(quota_cache(previous,sample,1000)["windows"],{"daily":30})

    def test_future_vendor_clock_cannot_extend_host_cache_ttl(self):
        previous={"health":"ok","windows":{},"effective_valid_until":0}
        sample={"status":"ok","window_set":"complete","sampled_at":300000,"valid_until":999999999,"windows":{"daily":25}}
        self.assertEqual(quota_cache(previous,sample,0)["effective_valid_until"],300000)

    def test_zero_divisor_and_decimal_exponent_are_rejected(self):
        self.assertEqual(price_amount(1000,"2","1000"),Decimal("2"))
        self.rejects("DATA.PRICE_DIVISOR",lambda:price_amount(1000,"2","0"))
        self.rejects("DATA.DECIMAL",lambda:decimal("1e9"))

    def test_repeating_price_fraction_has_explicit_storage_rounding(self):
        self.assertEqual(price_amount(1,"1","3"),Decimal("0.333333333333"))

    def test_large_aggregate_has_exact_string_not_rounded_number(self):
        self.assertEqual(aggregate([SAFE,2]),{"status":"out_of_number_range","exact_value":"9007199254740993","numeric_value":None})
        self.assertEqual(aggregate([120,80])["numeric_value"],200)

    def test_no_token_measurement_is_not_a_measured_zero(self):
        self.assertEqual(aggregate([]),{"status":"unavailable","exact_value":None,"numeric_value":None,"reason":"NO_TOKEN_MEASUREMENT"})
        self.assertEqual(aggregate([],known_empty=True),{"status":"ok","exact_value":"0","numeric_value":0})

    def test_read_task_cannot_hide_credential_refresh(self):
        self.assertFalse(dispatch_service("quota.read","remote_read","credential.exchange","credential-maintenance-v1"))
        self.assertFalse(dispatch_service("auth.refresh","credential_write","credential.exchange"))
        self.assertTrue(dispatch_service("auth.refresh","credential_write","credential.exchange","credential-maintenance-v1"))
        self.assertFalse(dispatch_service("host:migrate","local_read","state.write"))
        self.assertFalse(dispatch_service("host:migrate","local_read","state.cas"))
        self.assertTrue(dispatch_service("host:migrate","local_read","state.cas",migration_snapshot=True))
        self.assertFalse(dispatch_service("quota.read","remote_read","shell.run"))

    def test_runtime_is_a_dependency_of_real_migration(self):
        order=acyclic({"F01":set(),"F03":{"F01"},"F02":{"F01","F03"},"F04":{"F02","F03"},"F05":{"F04"}})
        self.assertLess(order.index("F03"),order.index("F02"))
        self.rejects("PLAN.DEPENDENCY_CYCLE",lambda:acyclic({"F02":{"F03"},"F03":{"F02"}}))

    def test_written_implementation_dag_includes_runtime_and_full_migration(self):
        dependencies={}
        for line in (DOCS/"IMPLEMENTATION_SPEC.md").read_text(encoding="utf-8").splitlines():
            cells=line.split("|")
            if len(cells)<6: continue
            match=re.match(r"\s*(F[0-9]{2}(?:\.[0-9]+)?)\s+",cells[1])
            if match:
                dependencies[match[1]]=set(re.findall(r"F[0-9]{2}(?:\.[0-9]+)?",cells[3]))
        order=acyclic(dependencies)
        self.assertIn("F03",dependencies["F02"])
        self.assertIn("F04",dependencies["F05"])
        self.assertIn("F11.2",dependencies["F12"])
        self.assertLess(order.index("F11.1"),order.index("F11.2"))

    def test_migration_inventory_includes_adapters_and_quota_only_products(self):
        repository=DOCS.parents[1]
        adapter=(repository/"crates/adapters/all/src/lib.rs").read_text(encoding="utf-8")
        adapters=set(re.findall(r'"([a-z][a-z0-9_]*)"',adapter.split("pub const TOOL_IDS:",1)[1].split("];",1)[0]))
        inventory=(DOCS/"MIGRATION_INVENTORY.md").read_text(encoding="utf-8")
        recorded=set(re.findall(r"^\| ([a-z][a-z0-9_]*) \|",inventory,re.M))
        self.assertTrue(adapters<=recorded)
        self.assertTrue({"copilot","gemini"}<=recorded-adapters)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output",type=Path,required=True)
    args=parser.parse_args()
    report={"schema":"tokenme.design-review/0.5","status":"running","run_id":uuid.uuid4().hex,"production_runtime_validated":False,"scope":"document_semantics_only"}
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(report,sort_keys=True)+"\n",encoding="utf-8")
    result=unittest.TextTestRunner(verbosity=2,stream=sys.stdout).run(unittest.defaultTestLoader.loadTestsFromTestCase(DesignContracts))
    report.update({"status":"passed" if result.wasSuccessful() else "failed","tests_run":result.testsRun,"failures":len(result.failures),"errors":len(result.errors),"documents":{path.name:hashlib.sha256(path.read_bytes()).hexdigest() for path in DOCS.glob("*.md")},"review_files":{path.name:hashlib.sha256(path.read_bytes()).hexdigest() for path in HERE.iterdir() if path.is_file()}})
    history=args.output.parent/"runs"/(report["run_id"]+".json")
    history.parent.mkdir(exist_ok=True)
    history.write_text(json.dumps(report,indent=2,sort_keys=True)+"\n",encoding="utf-8")
    args.output.write_text(json.dumps(report,indent=2,sort_keys=True)+"\n",encoding="utf-8")
    print(json.dumps({key:report[key] for key in ("status","tests_run","failures","errors","production_runtime_validated")},sort_keys=True))
    raise SystemExit(0 if result.wasSuccessful() else 1)


if __name__ == "__main__":
    main()
