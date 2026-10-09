//! Trae adapter (ByteDance's AI IDE, international and CN builds): usage read
//! out of the agent's **encrypted** SQLite store,
//! `<data-dir>/Trae/ModularData/ai-agent/database.db`.
//!
//! The store is a SQLCipher 4 variant with a **constant** key (see
//! [`crate::crypto`]); this adapter decrypts it in memory — main pages plus the
//! newest WAL frames — and reads one usage record per completed chat turn.
//!
//! ## What a turn meters
//!
//! Two sources, in priority order, both the IDE's own bookkeeping:
//!
//! 1. `chat_turn.context` JSON → `token_usage` — the per-turn breakdown
//!    (`prompt_tokens`, `completion_tokens`, `reasoning_tokens`,
//!    `cache_read_input_tokens`, `cache_creation_input_tokens`) the CN
//!    community's schema guide documents. Written when a turn completes; this
//!    machine's build (3.5.104) leaves it empty on a free account, so:
//! 2. fallback — `history_v2.token_usage` joined by message id: the turn's
//!    user message (`reply_to_message_id`, content source `user_input`) counts
//!    the prompt sent, the assistant message (`response_message_id`,
//!    content source `llm_default`) the reply. Measured live: one turn on this
//!    machine reads 1852 in / 29 out, and the assistant row's number matches
//!    `server_history_info.item_token_usage` for the same message exactly.
//!
//! Model comes from the turn's `persist_user_message_context.model_info`
//! (`config_name`, falling back to `model_name` minus its `__dollar__…`
//! suffix). The project is the session's `project.absolute_path` basename.
//!
//! Turns only ever count once: `turn_status = 'completed'` is the terminal
//! state the status column itself declares, `deleted_at = 0` excludes removed
//! ones, and all-zero usage is silence, not a session. The dedupe key
//! `trae#<turn-id>` makes the full rescan each pass idempotent.

pub mod crypto;

use std::path::PathBuf;

use serde_json::Value;
use usage_core::{
    DateFilter, DetectedSource, Error, FileKind, Meter, ModelAttr, ReadCursor, ReadOutcome,
    Semantics, SourceAdapter, SourceFile, TokenCounts, UsageEvent, UsageForm,
};

pub const TOOL_ID: &str = "trae";
/// The CN builds are a separate host with a separate fleet, separate quotas
/// and a separate account system — the panel carries them as their own tool.
pub const TOOL_ID_CN: &str = "trae_cn";

/// Path to a `database.db` (or a directory holding one), replacing every
/// discovered install — the fixture and relocated-install escape hatch.
pub const ENV_TRAE_AGENT_DB: &str = "TRAE_AGENT_DB";
pub const ENV_TRAE_CN_AGENT_DB: &str = "TRAE_CN_AGENT_DB";

/// Which installs an instance reads. The international build stores under
/// `Trae`; the CN builds document theirs as `Trae CN` and `TRAE SOLO CN` —
/// same store, same constant key, but different fleets and different quotas.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum Edition {
    #[default]
    International,
    China,
}

impl Edition {
    fn dirs(self) -> &'static [&'static str] {
        match self {
            Edition::International => &["Trae"],
            Edition::China => &["Trae CN", "TRAE SOLO CN"],
        }
    }

    fn tool_id(self) -> &'static str {
        match self {
            Edition::International => TOOL_ID,
            Edition::China => TOOL_ID_CN,
        }
    }

    fn env_db(self) -> &'static str {
        match self {
            Edition::International => ENV_TRAE_AGENT_DB,
            Edition::China => ENV_TRAE_CN_AGENT_DB,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TraeAdapter {
    pub edition: Edition,
}

impl TraeAdapter {
    /// The international edition, under `Trae/`.
    pub fn international() -> Self {
        Self { edition: Edition::International }
    }

    /// The CN editions, under `Trae CN/` and `TRAE SOLO CN/`.
    pub fn china() -> Self {
        Self { edition: Edition::China }
    }
}

/// The agent databases of one edition. `TRAE_AGENT_DB` / `TRAE_CN_AGENT_DB`
/// replace the list with one path (a file, or a directory that holds
/// `database.db`).
fn agent_dbs(edition: Edition) -> Vec<PathBuf> {
    if let Some(over) = std::env::var_os(edition.env_db()) {
        let path = PathBuf::from(&over);
        if over.is_empty() {
            return Vec::new();
        }
        return if path.is_dir() { vec![path.join("database.db")] } else { vec![path] }
            .into_iter()
            .filter(|p| p.is_file())
            .collect();
    }
    let Some(base) = dirs::data_dir() else { return Vec::new() };
    edition
        .dirs()
        .iter()
        .map(|dir| base.join(dir).join("ModularData").join("ai-agent").join("database.db"))
        .filter(|p| p.is_file())
        .collect()
}

fn wal_of(db: &PathBuf) -> Option<PathBuf> {
    let mut name = db.file_name()?.to_os_string();
    name.push("-wal");
    let wal = db.with_file_name(name);
    wal.is_file().then_some(wal)
}

impl SourceAdapter for TraeAdapter {
    fn id(&self) -> &'static str {
        self.edition.tool_id()
    }

    fn display_name(&self) -> &'static str {
        self.edition.dirs()[0]
    }

    fn semantics(&self) -> Semantics {
        Semantics {
            usage_form: UsageForm::PerCall,
            meter: Meter::Tokens,
            model_attr: ModelAttr::Inline,
            dedupes_by_id: true,
            reports_quota: false,
        }
    }

    fn probe(&self) -> Option<DetectedSource> {
        let dbs = agent_dbs(self.edition);
        dbs.first()?;
        Some(DetectedSource {
            id: self.edition.tool_id().to_string(),
            display: self.display_name().to_string(),
            roots: dbs,
            hint: Some("agent database".into()),
        })
    }

    fn discover(&self, _filter: &DateFilter) -> Vec<SourceFile> {
        agent_dbs(self.edition)
            .into_iter()
            .filter_map(|path| {
                let meta = std::fs::metadata(&path).ok()?;
                let mtime_ms = meta
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as i64;
                // SQLite kind: the WAL fold is what makes fresh turns visible
                // between the agent's checkpoints.
                Some(SourceFile { path, kind: FileKind::Sqlite, size: meta.len(), mtime_ms }.with_wal_activity())
            })
            .collect()
    }

    fn read(&self, file: &SourceFile, _cursor: ReadCursor) -> Result<ReadOutcome, Error> {
        // Every pass decrypts the whole store, so the cursor is decorative;
        // dedupe keys make the rescan idempotent.
        let done = ReadCursor(file.size as u64);
        let wal = wal_of(&file.path);
        let name = file.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        if wal.is_none() {
            usage_core::read_note(format!(
                "{name}: no -wal sidecar — turns finished since the last checkpoint are invisible"
            ));
        }
        // Every failure below is silence, not an error — the engine would
        // record a failed read and retry, which is right for a transient
        // torn read and useless for a rotated key. The note is what lets a
        // scan.log tell "nothing new" from "could not read the store".
        let image = match crypto::decrypt_report(&file.path, wal.as_deref()) {
            Ok(image) => image,
            Err(reason) => {
                usage_core::read_note(format!("{name}: decrypt failed — {reason}"));
                return Ok(ReadOutcome { events: Vec::new(), cursor: done });
            }
        };
        let (conn, temp) = match crypto::open_image_checked(&image) {
            Ok(opened) => opened,
            Err(reason) => {
                usage_core::read_note(format!("{name}: {reason}"));
                return Ok(ReadOutcome { events: Vec::new(), cursor: done });
            }
        };
        let outcome = read_turns(&conn, self.edition.tool_id());
        drop(conn);
        let _ = std::fs::remove_file(temp);
        match outcome {
            Ok((events, stats)) => {
                usage_core::read_note(format!(
                    "{name}: {} pages rebuilt, {} turns read → {} events \
                     ({} zero-dropped, {} no-id, {} row-errors)",
                    image.len() / crypto::PAGE,
                    stats.rows,
                    events.len(),
                    stats.zero_dropped,
                    stats.bad_id,
                    stats.row_errors,
                ));
                Ok(ReadOutcome { events, cursor: done })
            }
            Err(reason) => {
                usage_core::read_note(format!("{name}: {reason}"));
                Ok(ReadOutcome { events: Vec::new(), cursor: done })
            }
        }
    }
}

/// The completed, alive turns joined to their session's project and their two
/// history rows (prompt sent / reply produced).
///
/// The history sides join a per-message aggregate, not the raw rows: the CN
/// builds append a row per exchange for the same message id (measured 3–79
/// rows on one session), and a plain join fans one turn out into that many
/// events — identical after dedupe, so the totals survived while every pass
/// burned the decrypt and inflated `deduped`. `MAX` is the same value
/// grow-on-write would keep of the copies.
const TURN_QUERY: &str = "\
SELECT t.turn_id, t.created_at, t.context,
       hu.token_usage, ho.token_usage,
       p.absolute_path, p.name
FROM chat_turn t
JOIN chat_session s ON s.session_id = t.session_id AND s.deleted_at = 0
LEFT JOIN project p ON p.project_id = s.project_id
LEFT JOIN (SELECT session_id, message_id, MAX(token_usage) AS token_usage \
           FROM history_v2 WHERE deleted_at = 0 GROUP BY session_id, message_id) hu
       ON hu.session_id = t.session_id AND hu.message_id = t.reply_to_message_id
LEFT JOIN (SELECT session_id, message_id, MAX(token_usage) AS token_usage \
           FROM history_v2 WHERE deleted_at = 0 GROUP BY session_id, message_id) ho
       ON ho.session_id = t.session_id AND ho.message_id = t.response_message_id
WHERE t.deleted_at = 0 AND t.turn_status = 'completed'";

/// What one pass over `chat_turn` saw, so the read can say "I looked at N
/// turns" instead of a bare event count — a store whose N is zero reads
/// nothing like a store whose turns all failed the zero gate.
#[derive(Default)]
struct TurnStats {
    rows: usize,
    zero_dropped: usize,
    bad_id: usize,
    row_errors: usize,
}

fn read_turns(
    conn: &rusqlite::Connection,
    tool_id: &'static str,
) -> Result<(Vec<UsageEvent>, TurnStats), String> {
    let mut stmt = conn.prepare(TURN_QUERY).map_err(|e| format!("usage query failed: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TurnRow {
                turn_id: row.get::<_, String>(0).unwrap_or_default(),
                created_at: row.get::<_, i64>(1).unwrap_or(0),
                context: row.get::<_, String>(2).unwrap_or_default(),
                prompt_history: row.get::<_, Option<i64>>(3).ok().flatten(),
                reply_history: row.get::<_, Option<i64>>(4).ok().flatten(),
                absolute_path: row.get::<_, Option<String>>(5).ok().flatten(),
                project_name: row.get::<_, Option<String>>(6).ok().flatten(),
            })
        })
        .map_err(|e| format!("usage query failed: {e}"))?;
    let mut events = Vec::new();
    let mut stats = TurnStats::default();
    for row in rows {
        let row = match row {
            Ok(row) => row,
            Err(_) => {
                stats.row_errors += 1;
                continue;
            }
        };
        stats.rows += 1;
        if row.turn_id.is_empty() {
            stats.bad_id += 1;
            continue;
        }
        match row.event(tool_id) {
            Some(event) => events.push(event),
            None => stats.zero_dropped += 1,
        }
    }
    Ok((events, stats))
}

struct TurnRow {
    turn_id: String,
    created_at: i64,
    context: String,
    /// `history_v2.token_usage` of the turn's user message — the prompt sent.
    prompt_history: Option<i64>,
    /// …of the turn's assistant message — the reply produced.
    reply_history: Option<i64>,
    absolute_path: Option<String>,
    project_name: Option<String>,
}

impl TurnRow {
    fn event(self, tool_id: &'static str) -> Option<UsageEvent> {
        if self.turn_id.is_empty() {
            return None;
        }
        let ctx: Value = serde_json::from_str(&self.context).unwrap_or(Value::Null);
        let counts = counts_from(&ctx, self.prompt_history, self.reply_history);
        if counts.is_zero() {
            return None;
        }
        let mut event = UsageEvent::new(tool_id, self.created_at.saturating_mul(1000), &self.turn_id);
        event.counts = counts;
        event.meter = Meter::Tokens;
        event.model = model_from(&ctx);
        event.project = self
            .absolute_path
            .as_deref()
            .and_then(basename)
            .or(self.project_name)
            .filter(|p| !p.is_empty());
        // `trae#<turn-id>` / `trae_cn#<turn-id>`: the full rescan each pass
        // stays idempotent, and the CN turns never collide with the
        // international ones.
        event.dedupe_key = Some(format!("{tool_id}#{}", self.turn_id));
        Some(event)
    }
}

/// Priority to the documented breakdown; the history join is the fallback this
/// build actually answers with.
fn counts_from(ctx: &Value, prompt_history: Option<i64>, reply_history: Option<i64>) -> TokenCounts {
    let num = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
    let usage = ctx.get("token_usage").filter(|u| u.is_object());
    if let Some(usage) = usage {
        return TokenCounts {
            input: num(usage.get("prompt_tokens")),
            output: num(usage.get("completion_tokens")),
            cache_read: num(usage.get("cache_read_input_tokens")),
            cache_creation: num(usage.get("cache_creation_input_tokens")),
            reasoning: num(usage.get("reasoning_tokens")),
            credits: 0.0,
        };
    }
    TokenCounts {
        input: prompt_history.map(|n| n.max(0) as f64).unwrap_or(0.0),
        output: reply_history.map(|n| n.max(0) as f64).unwrap_or(0.0),
        cache_read: 0.0,
        cache_creation: 0.0,
        reasoning: 0.0,
        credits: 0.0,
    }
}

/// `config_name` is the clean product name (`Dola-Seed-2.0-Code`); `model_name`
/// carries a wire suffix (`…__dollar__dev`) that reads fine once cut at `__`.
fn model_from(ctx: &Value) -> Option<String> {
    let info = ctx.pointer("/persist_user_message_context/model_info")?;
    let clean = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(|s| s.split("__").next().unwrap_or(s).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    clean(info.get("config_name")).or_else(|| clean(info.get("model_name")))
}

fn basename(path: &str) -> Option<String> {
    path.rsplit(['/', '\\']).find(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::testing::encrypt_image;
    use rusqlite::Connection;

    /// The schema subset the adapter reads, in one fixture db: one session in
    /// a project, one completed turn with both history rows, plus a streaming
    /// and a deleted turn that must stay out. `reply_rows` are extra token
    /// values appended for the reply message — the CN builds write several.
    fn fixture_plain(turn_context: &str) -> Vec<u8> {
        fixture_plain_with_reply_rows(turn_context, &[])
    }

    fn fixture_plain_with_reply_rows(turn_context: &str, reply_rows: &[i64]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("plain.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA page_size = 4096;
             CREATE TABLE chat_session (session_id TEXT, project_id TEXT, deleted_at BIGINT DEFAULT 0);
             CREATE TABLE project (project_id TEXT, absolute_path TEXT, name TEXT);
             CREATE TABLE chat_turn (turn_id TEXT, session_id TEXT, turn_status TEXT,
               context TEXT, created_at BIGINT, deleted_at BIGINT DEFAULT 0,
               reply_to_message_id TEXT, response_message_id TEXT);
             CREATE TABLE history_v2 (session_id TEXT, message_id TEXT, token_usage BIGINT, deleted_at BIGINT DEFAULT 0);
             INSERT INTO project VALUES ('p1', '/Users/me/workspace/bug-hunter', 'hunter');
             INSERT INTO chat_session VALUES ('s1', 'p1', 0);
             INSERT INTO chat_turn VALUES ('t1', 's1', 'completed', '{}', 1790609380, 0, 'm-user', 'm-bot');
             INSERT INTO chat_turn VALUES ('t-live', 's1', 'streaming', '{}', 1790609381, 0, '', '');
             INSERT INTO chat_turn VALUES ('t-gone', 's1', 'completed', '{}', 1790609382, 1790609390, '', '');
             INSERT INTO history_v2 VALUES ('s1', 'm-user', 1852, 0);
             INSERT INTO history_v2 VALUES ('s1', 'm-bot', 29, 0);",
        )
        .unwrap();
        for v in reply_rows {
            conn.execute("INSERT INTO history_v2 VALUES ('s1', 'm-bot', ?1, 0)", [v]).unwrap();
        }
        conn.execute("UPDATE chat_turn SET context = ?1 WHERE turn_id = 't1'", [turn_context])
            .unwrap();
        drop(conn);
        // Declare the vendor's 80-byte per-page reserve, then let SQLite
        // itself rewrite the file to it (cells move below 4016) — the true
        // plaintext `encrypt_image` expects.
        let mut plain = std::fs::read(&db).unwrap();
        plain[20] = 80;
        std::fs::write(&db, &plain).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("VACUUM;").unwrap();
        drop(conn);
        std::fs::read(&db).unwrap()
    }

    #[test]
    fn a_completed_turn_becomes_one_per_call_event() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let plain = fixture_plain("{}");
        // The fixture turn carries a model in its context, not tokens.
        let db = dir.path().join("database.db");
        std::fs::write(&db, encrypt_image(&plain)).unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let adapter = TraeAdapter::default();
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].kind, FileKind::Sqlite);
        let out = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1, "streaming and deleted turns stay out: {:?}", out.events);
        let e = &out.events[0];
        assert_eq!(e.tool, "trae");
        assert_eq!(e.session, "t1");
        assert_eq!(e.counts.input, 1852.0, "{:?}", e.counts);
        assert_eq!(e.counts.output, 29.0);
        assert_eq!(e.dedupe_key.as_deref(), Some("trae#t1"));
        assert_eq!(e.project.as_deref(), Some("bug-hunter"));
        assert_eq!(e.meter, Meter::Tokens);
        // The read said what it saw: scan.log can tell "one turn read" from a
        // bare count when a remote user's numbers look wrong.
        let notes = usage_core::drain_read_notes();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("1 turns read → 1 events") && n.contains("0 zero-dropped")),
            "{notes:?}"
        );
        // Re-read re-emits the same key: the indexer replaces, never doubles.
        let again = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(again.events[0].dedupe_key, e.dedupe_key);
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }

    /// The CN builds append one history row per exchange under the same
    /// message id. The read must answer one event per turn at the largest
    /// value, not one event per row — the fan-out a plain join produced.
    #[test]
    fn a_multi_row_history_message_yields_one_event_at_its_largest() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let plain = fixture_plain_with_reply_rows("{}", &[150, 7, 150]);
        let db = dir.path().join("database.db");
        std::fs::write(&db, encrypt_image(&plain)).unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let adapter = TraeAdapter::default();
        let sources = adapter.discover(&DateFilter::default());
        let out = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert_eq!(out.events.len(), 1, "one turn, not one history row: {:?}", out.events.len());
        assert_eq!(out.events[0].counts.output, 150.0, "{:?}", out.events[0].counts);
        let notes = usage_core::drain_read_notes();
        assert!(notes.iter().any(|n| n.contains("1 turns read → 1 events")), "{notes:?}");
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }

    /// The documented `token_usage` breakdown, when a build writes it, wins
    /// over the history fallback — and the model comes from the turn context.
    #[test]
    fn a_documented_breakdown_takes_precedence() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let plain = fixture_plain(
            r#"{"token_usage":{"prompt_tokens":900,"completion_tokens":80,
                 "reasoning_tokens":20,"cache_read_input_tokens":5000,
                 "cache_creation_input_tokens":120},
               "persist_user_message_context":{"model_info":{
                 "config_name":"Dola-Seed-2.0-Code",
                 "model_name":"Dola-Seed-2.0-Code__dollar__dev"}}}"#,
        );
        let db = dir.path().join("database.db");
        std::fs::write(&db, encrypt_image(&plain)).unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let out = TraeAdapter::default().read(
            &SourceFile {
                path: db.clone(),
                kind: FileKind::Sqlite,
                size: std::fs::metadata(&db).unwrap().len(),
                mtime_ms: 0,
            },
            ReadCursor(0),
        )
        .unwrap();
        assert_eq!(out.events.len(), 1);
        let e = &out.events[0];
        assert_eq!(e.counts.input, 900.0);
        assert_eq!(e.counts.output, 80.0);
        assert_eq!(e.counts.reasoning, 20.0);
        assert_eq!(e.counts.cache_read, 5000.0);
        assert_eq!(e.counts.cache_creation, 120.0);
        assert_eq!(e.model.as_deref(), Some("Dola-Seed-2.0-Code"));
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }

    /// `config_name` empty, `model_name` suffixed: the suffix is cut, not kept.
    #[test]
    fn the_wire_model_name_is_trimmed_of_its_routing_suffix() {
        let ctx: Value = serde_json::from_str(
            r#"{"persist_user_message_context":{"model_info":{"config_name":"",
                 "model_name":"kimi-k2.5__dollar__dev"}}}"#,
        )
        .unwrap();
        assert_eq!(model_from(&ctx).as_deref(), Some("kimi-k2.5"));
        assert_eq!(model_from(&Value::Null), None);
    }

    #[test]
    fn an_undecryptable_store_is_silence_not_an_error() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, b"not a database at all").unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let adapter = TraeAdapter::default();
        let sources = adapter.discover(&DateFilter::default());
        assert_eq!(sources.len(), 1);
        let out = adapter.read(&sources[0], ReadCursor(0)).unwrap();
        assert!(out.events.is_empty());
        // …and the silence names itself: this note is the only difference, in
        // a scan.log, between a quiet day and a store that cannot be read.
        let notes = usage_core::drain_read_notes();
        assert!(
            notes.iter().any(|n| n.contains("decrypt failed") && n.contains("4096-byte pages")),
            "{notes:?}"
        );
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }

    /// The wrong-key shape: pages decrypt to garbage, so the rebuilt header
    /// misses the gate. This is the reason a store whose edition rotated its
    /// constant reads as zero events forever — and now says so.
    #[test]
    fn a_wrong_key_fails_the_header_gate_by_name() {
        let _env = crate::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("database.db");
        std::fs::write(&db, vec![0u8; crypto::PAGE * 2]).unwrap();
        std::env::set_var(ENV_TRAE_AGENT_DB, &db);
        let out = TraeAdapter::default().read(
            &SourceFile {
                path: db.clone(),
                kind: FileKind::Sqlite,
                size: std::fs::metadata(&db).unwrap().len(),
                mtime_ms: 0,
            },
            ReadCursor(0),
        )
        .unwrap();
        assert!(out.events.is_empty());
        let notes = usage_core::drain_read_notes();
        assert!(notes.iter().any(|n| n.contains("header gate mismatch")), "{notes:?}");
        std::env::remove_var(ENV_TRAE_AGENT_DB);
    }
}

#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "reads the real Trae database on this machine"]
    fn the_live_store_round_trips_through_wal() {
        std::env::remove_var(ENV_TRAE_AGENT_DB);
        let adapter = TraeAdapter::default();
        let sources = adapter.discover(&DateFilter::default());
        let f = &sources[0];
        println!("source: {}", f.path.display());
        println!("stat: size={} mtime={}", f.size, f.mtime_ms);
        let out = adapter.read(f, ReadCursor(0)).unwrap();
        println!("events: {}", out.events.len());
        for e in &out.events {
            println!(
                "  {} ts={} in={} out={} reasoning={} model={:?}",
                e.session, e.ts_ms, e.counts.input, e.counts.output, e.counts.reasoning, e.model
            );
        }
        assert!(!out.events.is_empty(), "the live store yields events");
    }
}
