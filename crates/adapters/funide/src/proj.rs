//! One event per FunIDE turn, from the session file's own per-turn ledger.
//!
//! `cacheUsage` is the session's LIFETIME running total the UI itself displays,
//! and `items[]` carries the same money broken out per turn: `usagePrompt`,
//! `usageCached`, `usageCompletion`. Measured against a live session,
//! `usageCached` is a subset of `usagePrompt` (the UI prints 缓存 as a percent
//! *of* 输入), so it maps to `cache_read` and never doubles the input, and the
//! per-turn sums equal `cacheUsage` exactly. Each assistant item's `turnId` is
//! `turn_<the turn's start, in ms>_<seq>`, which makes the turns day-true.
//!
//! Stamping one cumulative event with the session's `updatedAt` is what this
//! file did until 2026-10-21, and it cannot age correctly: a session continued
//! across a day boundary drags its whole lifetime total into the newest day.
//! Measured here (session `smvf8pvrm7qrg`): 477,578,323 prompt tokens all
//! landed on 10-21 while 10-19 and 10-20 — the days its 13 turns actually ran —
//! went to zero. The same shape the dsh projection was retired for.
//!
//! `activeModelId` (`cloud:GLM-plan-flash`) is the session's model as the UI
//! shows it; the `cloud:` prefix is the FunIDE-hosted family, stripped for the
//! display id, and the turns of a session share it. `chargedPoints` is FunIDE's
//! points pricing, drawn from the cloud account — that balance IS reachable
//! (cloud API `billing/balance` with the IDE's own stored token), which the
//! `usage-quota` FunIdeQuota probe reads; this adapter stays usage-only.
//!
//! ## Where the header lives is layout-dependent
//!
//! Up to 0.0.4.3 one file held both the header (`id`, `activeModelId`,
//! `workspaceKey`, `updatedAt`) and the per-turn `items`. From 0.0.4.4 the app
//! splits them: `items` is the whole of `<sid>.main.json` and the header is
//! `<sid>.meta.json` beside it (see [`crate::paths`]). Parsing therefore takes
//! the body plus an optional header, reads a field from the body first and
//! falls back to the header — which is also what makes one function cover both
//! layouts: a legacy body answers every lookup itself, and a modern body only
//! has `items`.
//!
//! ## `workspaceKey` stopped being a path
//!
//! In the old layout it was the project's own folder (`d:\w\proj`), and the last
//! segment named the project. From 0.0.4.4 it is the opaque directory name the
//! session sits in (`10blr3b1m7plhc`) — a hash that no local file maps back to
//! the folder, so the session list would read as sessions from projects named
//! after hashes. The real paths are still on disk though: `fileObservations`
//! keys are `file://` URIs of every file the agent touched, and the folder
//! common to all of them is the workspace (measured on five live sessions:
//! `tokenme`, `FunIDE`, `iRouter` — the actual open folders). [`project_of`]
//! uses that and falls back to the old last-segment rule when it is missing.

use serde_json::Value;
use usage_core::{Meter, UsageEvent};

/// Parse one session body into the per-turn events its items record. Empty for
/// sessions with no billable usage yet (opened, never answered).
///
/// `header` is the sibling `.meta.json` of the 0.0.4.4+ layout, `None` for the
/// single-file layout whose body is also its header.
pub fn parse(body: &str, header: Option<&str>, mtime_ms: i64, source_key: &str) -> Vec<UsageEvent> {
    let Ok(body) = serde_json::from_str::<Value>(body) else { return Vec::new() };
    let header = header.and_then(|text| serde_json::from_str::<Value>(text).ok());
    // The body answers first, so a legacy session needs nothing from the header;
    // a split session answers only `items` and takes every attribute from it.
    let field = |key: &str| body.get(key).or_else(|| header.as_ref().and_then(|h| h.get(key)));
    let Some(session) = field("id").and_then(Value::as_str) else { return Vec::new() };
    let model = field("activeModelId")
        .and_then(Value::as_str)
        .map(|m| m.strip_prefix("cloud:").unwrap_or(m).to_string())
        .filter(|m| !m.is_empty());
    let project = field("workspaceKey")
        .and_then(Value::as_str)
        .map(|w| project_of(w, &body, header.as_ref()));
    // A turn with no `turnId` still has to be billed somewhere, and the
    // session's own edit time is the only timestamp the files offer for it.
    let fallback_ts = field("updatedAt")
        .and_then(Value::as_i64)
        .filter(|ts| *ts > 0)
        .unwrap_or(mtime_ms.max(0));
    let items: &[Value] = body
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut events = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if item.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let num = |k: &str| item.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        let prompt = num("usagePrompt");
        let cached = num("usageCached").min(prompt);
        let output = num("usageCompletion");
        if prompt + output <= 0.0 {
            continue;
        }
        let turn = item.get("turnId").and_then(Value::as_str).filter(|t| !t.is_empty());
        let ts_ms = turn.and_then(turn_start_ms).unwrap_or(fallback_ts);
        let mut event = UsageEvent::new(crate::TOOL_ID, ts_ms, session);
        event.counts.input = prompt - cached;
        event.counts.cache_read = cached;
        event.counts.output = output;
        event.meter = Meter::Tokens;
        event.model = model.clone();
        event.project = project.clone();
        // The indexer's dedupe index is global and the file is re-read whole on
        // every pass, so this key is the only thing keeping the replay
        // idempotent. A turn with no id falls back to its position — item order
        // is stable across rewrites.
        event.dedupe_key = Some(match turn {
            Some(t) => format!("{}{}#{}", crate::DEDUPE_PREFIX, session, t),
            None => format!("{}{}#idx{}", crate::DEDUPE_PREFIX, session, idx),
        });
        event.source = source_key.to_string();
        events.push(event);
    }
    events
}

/// The project a session belongs to.
///
/// `workspaceKey` is the project folder in the layout up to 0.0.4.3, and an
/// opaque hash from 0.0.4.4 (see the module notes). For the latter the real
/// folder is recovered from `fileObservations`: those keys are the `file://`
/// URIs of every file the turn touched, and the deepest folder they all share is
/// the workspace. A session that touched exactly one file (a fresh conversation
/// reading a single path) still lands on its containing folder, which is the same
/// rule one level in.
///
/// `fileObservations` is part of the body, not the header, so it is looked for in
/// both — the split moved `items` out and left the observations where they were,
/// while a legacy body carries them too.
///
/// Falls back to the key's own last segment — the old answer — so a session that
/// never touched a file keeps something human-readable.
fn project_of(workspace_key: &str, body: &Value, header: Option<&Value>) -> String {
    let fallback = || {
        let key = workspace_key.trim_end_matches(['/', '\\']);
        key.rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(key)
            .to_string()
    };
    let Some((name, depth)) = [Some(body), header].into_iter().flatten().find_map(common_folder_name)
    else {
        return fallback();
    };
    // A session whose files only meet at the home folder (`/Users/demo`) has no
    // project in common — naming it after the user's own account is worse than
    // the key's opaque id, because it reads like a real project on every row it
    // touches. Below three segments there is no `/Users/<user>/<something>`.
    if depth <= 2 {
        return fallback();
    }
    name
}

/// Last segment of the folder the session's files actually live in, with how
/// deep that folder is (segments from the root), so the caller can reject an
/// answer that only reaches the home folder.
///
/// One correction over a plain common prefix, measured on live sessions: the
/// IDE's own files are not project files. A session that read its own extension
/// store alongside the repo (`~/.funide/extensions/...`) has a common prefix of
/// `~`, which would name the project after the home folder. Those paths are
/// dropped first — see [`is_tool_owned`].
///
/// A session that genuinely touched two repos keeps their shared parent as its
/// project (`iWorkspace`). Picking one of the two would be a guess, and the
/// folder name is only a label on a row — the tokens are already attributed
/// correctly by session either way.
fn common_folder_name(header: &Value) -> Option<(String, usize)> {
    let observations = header.get("fileObservations")?.as_object()?;
    let mut dirs: Vec<Vec<String>> = Vec::new();
    for uri in observations.keys() {
        let Some(path) = uri.strip_prefix("file://") else { continue };
        if is_tool_owned(path) {
            continue;
        }
        // Windows URIs are `file:///C:/dir/file`, POSIX `file:///dir/file`; both
        // split into segments the same way once the empty leading one is dropped.
        let mut segments: Vec<String> = path
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect();
        // The last segment is the file itself — the folder is what precedes it.
        segments.pop();
        if !segments.is_empty() {
            dirs.push(segments);
        }
    }
    let prefix = common_prefix(&dirs)?;
    Some((prefix.last()?.clone(), prefix.len()))
}

/// A path inside the IDE's own installation or extension store rather than in a
/// folder the user opened. Hidden dot-folders under the data dir are where both
/// live (`~/.funide/extensions`, `~/.vscode/extensions`), and a `.app` bundle is
/// the application itself.
fn is_tool_owned(path: &str) -> bool {
    path.contains("/.funide/")
        || path.contains("/.vscode/")
        || path.contains("/.cursor/")
        || path.contains(".app/Contents/")
}

/// Segments shared by every directory, longest first. `None` when they share
/// none (files on different volumes) or there is nothing to compare.
fn common_prefix(dirs: &[Vec<String>]) -> Option<Vec<String>> {
    let mut iter = dirs.iter();
    let mut prefix = iter.next()?.clone();
    for dir in iter {
        let shared = prefix.iter().zip(dir).take_while(|(a, b)| a == b).count();
        prefix.truncate(shared);
        if prefix.is_empty() {
            return None;
        }
    }
    Some(prefix)
}

/// Decode the escapes a `file://` URI carries in its path. Only `%XX` is
/// meaningful here, and a malformed one is left as written rather than dropped.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// `turn_1790510647992_1111` — the middle field is the turn's start in ms.
fn turn_start_ms(turn_id: &str) -> Option<i64> {
    turn_id
        .split('_')
        .nth(1)?
        .parse::<i64>()
        .ok()
        // A seconds stamp would day-bucket 1970. The writer only ever emits ms,
        // so anything under this is not the field we think it is.
        .filter(|ms| *ms > 100_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two billed turns of a live session, 36 hours apart — the case the
    /// lifetime projection collapsed onto one day.
    const LIVE: &str = r#"{
      "version": 2,
      "id": "smumnmwb73amg",
      "title": "扫描这个项目",
      "workspaceKey": "C:\\Users\\demo\\figma-mcp-server",
      "activeModelId": "cloud:GLM-plan-flash",
      "updatedAt": 1790685147111,
      "cacheUsage": {"totalTokens": 2267706, "promptTokens": 2187464,
                     "cachedTokens": 1864256, "completionTokens": 80242,
                     "chargedPoints": 0.006123, "reported": true},
      "items": [
        {"role": "user", "turnId": "turn_1790510647992_1111", "text": "扫一下"},
        {"role": "assistant", "turnId": "turn_1790510647992_1111",
         "usagePrompt": 68403, "usageCached": 23296, "usageCompletion": 1415,
         "usage": "输入 68403 · 输出 1415 · 缓存 23296（34.1%）"},
        {"role": "user", "turnId": "turn_1790640247992_2222", "text": "再来"},
        {"role": "assistant", "turnId": "turn_1790640247992_2222",
         "usagePrompt": 2119061, "usageCached": 1840960, "usageCompletion": 78827}
      ]
    }"#;

    /// The 0.0.4.4+ split, measured on live session `smvtoqr65haav` of a 0.0.4.5
    /// install: `main.json` is nothing but `items`, and every attribute the
    /// event needs (id, model, project, edit time) lives in the sibling
    /// `meta.json`. The header's own `cacheUsage` is the UI's lifetime total.
    const SPLIT_BODY: &str = r#"{
      "items": [
        {"role": "user", "turnId": "turn_1793329978081_320", "sentAt": 1793329978082,
         "contextStart": 0, "text": "扫一下"},
        {"role": "assistant", "turnId": "turn_1793329978081_320",
         "usagePrompt": 233107, "usageCached": 173396, "usageCompletion": 2312},
        {"role": "user", "turnId": "turn_1793330391605_5476", "sentAt": 1793330391606,
         "contextStart": 18, "text": "再来"},
        {"role": "assistant", "turnId": "turn_1793330391605_5476",
         "usagePrompt": 613172, "usageCached": 588800, "usageCompletion": 5779}
      ],
      "contextMessages": [],
      "fileObservations": {}
    }"#;

    const SPLIT_HEADER: &str = r#"{
      "id": "smvtoqr65haav",
      "title": "上游当前最新版本是0.5.112",
      "workspaceKey": "1qzmeggs20nb4",
      "updatedAt": 1793330609706,
      "activeModelId": "cloud:deepseek-flash-free",
      "midTurn": true,
      "cacheUsage": {"totalTokens": 854370, "promptTokens": 846279,
                     "cachedTokens": 762196, "completionTokens": 8091}
    }"#;

    #[test]
    fn each_turn_is_its_own_event_stamped_with_that_turns_start() {
        let events = parse(LIVE, None, 1790685000000, "funide/s1.json");
        assert_eq!(events.len(), 2, "one event per billed assistant turn");
        let [a, b] = events.as_slice() else { panic!("two turns expected") };

        assert_eq!(a.session, "smumnmwb73amg");
        assert_eq!(a.model.as_deref(), Some("GLM-plan-flash"), "the cloud: prefix is display noise");
        assert_eq!(a.project.as_deref(), Some("figma-mcp-server"));
        // cached is a subset of prompt: input lands exclusive, cache as read.
        assert_eq!(a.counts.input, 68403.0 - 23296.0);
        assert_eq!(a.counts.cache_read, 23296.0);
        assert_eq!(a.counts.output, 1415.0);
        assert_eq!(a.ts_ms, 1790510647992, "the turn's own start, not the session's edit time");
        assert_eq!(
            a.dedupe_key.as_deref(),
            Some("funide#smumnmwb73amg#turn_1790510647992_1111")
        );

        assert_eq!(b.ts_ms, 1790640247992, "the later turn keeps its later day");
        assert_ne!(
            usage_core::local_day_of(a.ts_ms),
            usage_core::local_day_of(b.ts_ms),
            "the fixture spans a local day boundary — the case the projection lost"
        );
    }

    #[test]
    fn the_turn_events_sum_to_the_sessions_own_lifetime_total() {
        let events = parse(LIVE, None, 1790685000000, "funide/s1.json");
        let mut prompt = 0.0;
        let mut output = 0.0;
        for e in &events {
            prompt += e.counts.input + e.counts.cache_read;
            output += e.counts.output;
        }
        assert_eq!(prompt, 2187464.0, "promptTokens, split the way the UI reads it");
        assert_eq!(output, 80242.0, "completionTokens");
        assert_eq!(prompt + output, 2267706.0, "totalTokens");
    }

    #[test]
    fn an_unanswered_turn_and_an_itemless_file_yield_nothing() {
        let text = r#"{"id":"s2","updatedAt":5,
            "items":[{"role":"user","turnId":"turn_1790510647992_1","text":"hi"},
                     {"role":"assistant","turnId":"turn_1790510647992_1",
                      "usagePrompt":0,"usageCached":0,"usageCompletion":0}]}"#;
        assert!(parse(text, None, 9, "k").is_empty(), "no billable usage, no event");
        assert!(
            parse(r#"{"id":"s3","cacheUsage":{"promptTokens":10}}"#, None, 9, "k").is_empty(),
            "a file with no items has no turn to bill: the lifetime total is not a day"
        );
    }

    #[test]
    fn a_turn_without_an_id_falls_back_to_its_position_and_the_edit_time() {
        let text = r#"{"id":"s4","activeModelId":"cloud:GLM-plan-flash","updatedAt":1790510647992,
            "items":[{"role":"assistant","usagePrompt":100,"usageCached":40,"usageCompletion":7}]}"#;
        let events = parse(text, None, 1, "k");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ts_ms, 1790510647992, "no turn id: the session's stamp is all there is");
        assert_eq!(events[0].dedupe_key.as_deref(), Some("funide#s4#idx0"), "a replayed file still dedupes");
    }

    #[test]
    fn a_turn_id_without_a_usable_stamp_is_not_guessed_at() {
        assert_eq!(turn_start_ms("turn_1790510647992_1111"), Some(1790510647992));
        assert_eq!(turn_start_ms("turn_1790510647_1111"), None, "seconds is not ms");
        assert_eq!(turn_start_ms("turn_x_1111"), None);
        assert_eq!(turn_start_ms("turn"), None);
    }

    #[test]
    fn the_split_layout_bills_its_turns_from_the_body_and_its_attributes_from_the_header() {
        let events = parse(SPLIT_BODY, Some(SPLIT_HEADER), 1, "funide/s9.main.json");
        assert_eq!(events.len(), 2, "the body's items are the ledger; the header has none");
        let [a, b] = events.as_slice() else { panic!("two turns expected") };

        // Every attribute but `items` lives in the sibling .meta.json.
        assert_eq!(a.session, "smvtoqr65haav", "the id comes from the header");
        assert_eq!(a.model.as_deref(), Some("deepseek-flash-free"));
        assert_eq!(a.ts_ms, 1793329978081, "day-true from the turn id, as before");
        assert_eq!(b.ts_ms, 1793330391605);
        assert_eq!(a.counts.input, 233107.0 - 173396.0);
        assert_eq!(a.counts.cache_read, 173396.0);
        assert_eq!(a.counts.output, 2312.0);
        assert_eq!(a.dedupe_key.as_deref(), Some("funide#smvtoqr65haav#turn_1793329978081_320"));

        let prompt: f64 = events.iter().map(|e| e.counts.input + e.counts.cache_read).sum();
        let output: f64 = events.iter().map(|e| e.counts.output).sum();
        assert_eq!(prompt, 846279.0, "the header's promptTokens, split the way the UI reads it");
        assert_eq!(output, 8091.0, "completionTokens");
        assert_eq!(prompt + output, 854370.0, "totalTokens");
    }

    #[test]
    fn a_body_with_no_readable_header_bills_nothing_rather_than_a_nameless_session() {
        // `main.json` answers no `id`, so without the header there is no session
        // to key on. Emitting turns under a guessed id would double-bill the same
        // conversation on every pass that saw a different guess.
        assert!(parse(SPLIT_BODY, None, 1, "k").is_empty());
        assert!(parse(SPLIT_BODY, Some("not json"), 1, "k").is_empty());
    }

    #[test]
    fn the_project_comes_from_the_observed_files_when_the_key_is_an_opaque_id() {
        // `workspaceKey` is the session's directory name from 0.0.4.4, so the
        // last-segment rule would name the project `1qzmeggs20nb4`.
        let with_files = SPLIT_BODY.replace(
            r#""fileObservations": {}"#,
            r#""fileObservations": {
                "file:///Users/demo/iWorkspace/open-source/tokenme/README.md": {},
                "file:///Users/demo/iWorkspace/open-source/tokenme/crates/a.rs": {},
                "file:///Users/demo/iWorkspace/open-source/tokenme/apps/b/c.tsx": {}
            }"#,
        );
        let events = parse(&with_files, Some(SPLIT_HEADER), 1, "k");
        assert_eq!(events[0].project.as_deref(), Some("tokenme"));

        // One observation is still enough: its folder is the workspace.
        let one_file = SPLIT_BODY.replace(
            r#""fileObservations": {}"#,
            r#""fileObservations": {
                "file:///Users/demo/iWorkspace/idea/iRouter/docs/x.md": {}
            }"#,
        );
        let events = parse(&one_file, Some(SPLIT_HEADER), 1, "k");
        assert_eq!(events[0].project.as_deref(), Some("docs"), "one level in, the old rule's answer");

        // No observation at all: the opaque key is all there is, and it is
        // better than no project.
        let events = parse(SPLIT_BODY, Some(SPLIT_HEADER), 1, "k");
        assert_eq!(events[0].project.as_deref(), Some("1qzmeggs20nb4"));
    }

    /// Build a body whose only `fileObservations` are `uris`.
    fn body_observing(uris: &[&str]) -> String {
        let entries: Vec<String> = uris.iter().map(|u| format!("\"{u}\": {{}}")).collect();
        SPLIT_BODY.replace(
            r#""fileObservations": {}"#,
            &format!(r#""fileObservations": {{{}}}"#, entries.join(",")),
        )
    }

    #[test]
    fn the_ides_own_files_do_not_name_the_project() {
        // Measured on live session `smvqdbhprafem`: the repo files and its own
        // extension store share only `~`, which named the project after the home
        // folder. The extension paths are dropped before the prefix is taken.
        let body = body_observing(&[
            "file:///Users/demo/iWorkspace/idea/FunIDE/README.md",
            "file:///Users/demo/iWorkspace/idea/FunIDE/manifest/versions.json",
            "file:///Users/demo/.funide/extensions/extensions.json",
            "file:///Users/demo/.funide/extensions/.obsolete",
            "file:///Applications/IntelliJ%20IDEA.app/Contents/Resources/product-info.json",
        ]);
        let events = parse(&body, Some(SPLIT_HEADER), 1, "k");
        assert_eq!(events[0].project.as_deref(), Some("FunIDE"));
    }

    #[test]
    fn a_session_that_strayed_into_another_repo_keeps_its_common_folder() {
        // Measured on live session `smv60evckij74`: most of its files are in
        // `iRouter` and a few in `tokenme`. Picking the majority repo would be a
        // guess about what the conversation "meant", so the shared folder is the
        // honest label — the tokens are attributed per session regardless.
        let mut uris: Vec<String> = (0..20)
            .map(|i| format!("file:///Users/demo/iWorkspace/idea/iRouter/cli/src/f{i}.js"))
            .collect();
        uris.push("file:///Users/demo/iWorkspace/open-source/tokenme/README.md".to_string());
        let refs: Vec<&str> = uris.iter().map(String::as_str).collect();
        let events = parse(&body_observing(&refs), Some(SPLIT_HEADER), 1, "k");
        assert_eq!(events[0].project.as_deref(), Some("iWorkspace"));
    }

    #[test]
    fn a_session_whose_files_only_meet_at_home_is_not_named_after_the_user() {
        // Measured on live session `smv60evckij74` before the guard: `idea/iRouter`
        // and `open-source/tokenme` meet at `/Users/demo`, and every row of that
        // session was labelled after the account — a folder that is not a project
        // at all.
        let body = body_observing(&[
            "file:///Users/demo/iWorkspace/idea/iRouter/src/a.js",
            "file:///Users/demo/Documents/notes/todo.md",
        ]);
        let events = parse(&body, Some(SPLIT_HEADER), 1, "k");
        assert_eq!(events[0].project.as_deref(), Some("1qzmeggs20nb4"), "the key stands rather than a guess");
    }

    #[test]
    fn an_encoded_folder_name_reaches_the_ui_decoded() {
        let encoded = SPLIT_BODY.replace(
            r#""fileObservations": {}"#,
            r#""fileObservations": {
                "file:///Users/demo/iWorkspace/idea/Mac%E7%AB%AF/notes.md": {}
            }"#,
        );
        let events = parse(&encoded, Some(SPLIT_HEADER), 1, "k");
        assert_eq!(events[0].project.as_deref(), Some("Mac端"));
        assert_eq!(percent_decode("plain"), "plain", "no escapes, no change");
        assert_eq!(percent_decode("50%"), "50%", "a malformed escape is left as written");
    }
}
