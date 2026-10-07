//! Cross-machine sync: export this index's events as a gzipped JSONL bundle
//! plus a manifest, and merge such a bundle back idempotently.
//!
//! One Linux machine collects (`tokenme index`) and exports a window of its
//! local index (`tokenme export`) to `~/tokenme-sync`; every display machine —
//! macOS panel, Windows panel — merges it with `tokenme import` (the panel
//! engine runs the same merge automatically). The transport is any
//! authenticated file channel (scp/rsync/Syncthing); the bundle contract is
//! transport-independent.
//!
//! Correctness rules (frozen; see docs/internal/LINUX_SYNC_PLAN.md §5):
//! - every exported row carries a `key`: the row's own `dedupe_key`, or a
//!   synthesized `mix:<origin>:sha1(canonical fields)` for keyless rows
//!   (Codex), with `#2`, `#3`, … suffixes for same-content duplicates in one
//!   batch — the first occurrence keeps the bare key, exactly like the log
//!   replay would have written it;
//! - keyed rows follow the persist "growth" predicate verbatim, including its
//!   asymmetry: new side is `TokenCounts::total()`, old side adds credits;
//! - calls are unioned with `NOT EXISTS` (never the upstream's plain append,
//!   which accumulates duplicates on repeated growth);
//! - event writes, call union, rollup repair and the `meta` sync record share
//!   one IMMEDIATE transaction — a failed batch rolls back whole; and
//! - before COMMIT the batch reconciles **by key** against `event`: every
//!   manifest key must exist, and any count divergence must be explained by
//!   the growth predicate refusing the update. A window-sum comparison would
//!   permanently wedge once the Linux side purges rows the display side keeps
//!   (import never deletes), so stale rows are reported, not judged.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use usage_core::{origin_ok, Error, Result, SyncRecord};

use crate::{call_kind_of, call_kind_str, now_ms, sql_err, Index};

/// The bundle format version. A reader that does not know this exact string
/// refuses the file — a mismatched JSONL shape must fail loud, never half-merge.
pub const SYNC_FORMAT: &str = "tokenme-sync/1";

/// Slack on top of a manifest's own row count before the reader decides the
/// payload is longer than its manifest claims. No honest bundle exceeds its
/// manifest; the cap only stops a corrupt one from exhausting memory.
const ROW_CAP_SLACK: u64 = 1_000_000;

/// `~/tokenme-sync`: where `export` writes by default and where the panel
/// engine looks for bundles. A missing directory is normal (no sync set up).
pub fn default_sync_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("tokenme-sync"))
}

/// This machine's name, used as the default `origin`. `HOSTNAME` is set by
/// most shells but not by launchd/systemd, so unix goes through
/// `gethostname(2)` and Windows through `%COMPUTERNAME%` (always set there).
/// The fallback must stay stable — an origin that flips identities would
/// strand the previous origin's rows as permanently stale.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end]) {
                if !name.trim().is_empty() {
                    return name.trim().to_string();
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            if !name.trim().is_empty() {
                return name.trim().to_string();
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .filter(|n| !n.trim().is_empty())
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// sha256 of a file's raw bytes, streamed.
pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(hasher.finalize().as_slice()))
}

/// A `Write` sink that hashes everything passing through it, so the export
/// hashes the gz bytes on the way out instead of re-reading the file.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---- bundle shapes -----------------------------------------------------------

/// Six summed columns shared by the manifest totals, its per-tool table, and
/// the batch reconciliation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncSums {
    pub in_tok: f64,
    pub cc_tok: f64,
    pub cr_tok: f64,
    pub out_tok: f64,
    pub reason_tok: f64,
    pub credits: f64,
}

impl SyncSums {
    fn add(&mut self, o: &SyncSums) {
        self.in_tok += o.in_tok;
        self.cc_tok += o.cc_tok;
        self.cr_tok += o.cr_tok;
        self.out_tok += o.out_tok;
        self.reason_tok += o.reason_tok;
        self.credits += o.credits;
    }
}

/// `tokenme-<host>.manifest.json` — small on purpose (a 400-day full export
/// has ~390k keys; a per-key list would balloon it to megabytes). The row
/// stream is the authority for keys; the manifest is the authority for what
/// the stream *should* contain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncManifest {
    pub format: String,
    pub origin: String,
    pub schema_version: String,
    pub window_lo_ms: i64,
    pub window_hi_ms: i64,
    pub rows: u64,
    pub sums: SyncSums,
    /// Per-tool sums, informational (the badge tooltip and future audits read it).
    pub tools: BTreeMap<String, SyncSums>,
    /// sha256 of the `.jsonl.gz` file's raw bytes.
    pub sha256: String,
    pub generated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportCall {
    kind: String,
    name: String,
}

/// Custom deserializer: the writer emits non-finite floats as JSON `null`
/// (serde_json's own rule), so a null number must read back as 0.0 instead of
/// failing the whole file.
fn de_num<'de, D>(d: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<f64>::deserialize(d)?.unwrap_or(0.0))
}

/// One JSONL line: an event's full content plus its calls. Key names are the
/// frozen bundle contract — renaming one is a format-version change.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportRow {
    key: String,
    tool: String,
    ts_ms: i64,
    session: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    model: Option<String>,
    meter: String,
    source: String,
    #[serde(rename = "in", default, deserialize_with = "de_num")]
    in_tok: f64,
    #[serde(rename = "cc", default, deserialize_with = "de_num")]
    cc_tok: f64,
    #[serde(rename = "cr", default, deserialize_with = "de_num")]
    cr_tok: f64,
    #[serde(rename = "out", default, deserialize_with = "de_num")]
    out_tok: f64,
    #[serde(rename = "reason", default, deserialize_with = "de_num")]
    reason_tok: f64,
    #[serde(default, deserialize_with = "de_num")]
    credits: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    calls: Vec<ExportCall>,
}

/// The frozen canonical encoding of a keyless row's content: the same field
/// set, order and number formatting on every machine, so re-exports hash to
/// the same key.
fn canonical_of(row: &ExportRow) -> String {
    fn num(v: f64) -> serde_json::Value {
        serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
    let arr = serde_json::Value::Array(vec![
        serde_json::Value::String(row.tool.clone()),
        serde_json::Value::Number(serde_json::Number::from(row.ts_ms)),
        serde_json::Value::String(row.session.clone()),
        row.project.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null),
        row.model.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null),
        serde_json::Value::String(row.meter.clone()),
        num(row.in_tok),
        num(row.cc_tok),
        num(row.cr_tok),
        num(row.out_tok),
        num(row.reason_tok),
        num(row.credits),
    ]);
    arr.to_string()
}

// ---- export ------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Window length in days; the window is `[now - days, now + 1)`.
    pub days: i64,
    /// Directory the bundle lands in (created if missing).
    pub out_dir: PathBuf,
    /// Origin name recorded in the manifest and used for synthesized keys.
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportReport {
    pub origin: String,
    pub gz_path: PathBuf,
    pub manifest_path: PathBuf,
    pub window_lo_ms: i64,
    pub window_hi_ms: i64,
    pub rows: u64,
    pub sha256: String,
    pub took_ms: i64,
}

impl Index {
    /// Writes `tokenme-<origin>.jsonl.gz` + its manifest into `out_dir`. The
    /// whole read runs in one deferred (snapshot) transaction, so every row's
    /// calls come from the same consistent state. The `.gz` lands via
    /// `*.tmp` + rename and the manifest is written last: a transport can
    /// never observe a half-written bundle.
    pub fn export_sync(&self, opts: &ExportOptions) -> Result<ExportReport> {
        let started = Instant::now();
        let origin = opts.origin.trim();
        if origin.is_empty() {
            return Err(Error::Sync("export origin must not be empty".into()));
        }
        if !origin_ok(origin) {
            return Err(Error::Sync(
                "export origin must not contain ':', '/' or '\\', and no control characters \
                 (they would break source keys and bundle file names)"
                    .into(),
            ));
        }
        let days = opts.days.max(1);
        let hi = now_ms().saturating_add(1);
        let lo = hi.saturating_sub(days.saturating_mul(86_400_000));
        std::fs::create_dir_all(&opts.out_dir).map_err(|e| Error::io(&opts.out_dir, e))?;
        let gz_path = opts.out_dir.join(format!("tokenme-{origin}.jsonl.gz"));
        let manifest_path = opts.out_dir.join(format!("tokenme-{origin}.manifest.json"));

        // Snapshot read: the ingest writer may hold the WAL write lock, and a
        // deferred transaction only takes a read lock.
        let tx = self.conn.unchecked_transaction().map_err(sql_err)?;

        let mut calls: BTreeMap<i64, Vec<ExportCall>> = BTreeMap::new();
        {
            let mut stmt = tx
                .prepare(
                    "SELECT c.event_id, c.kind, c.name FROM call c \
                     JOIN event e ON e.id = c.event_id \
                     WHERE e.ts_ms >= ?1 AND e.ts_ms < ?2 ORDER BY c.event_id, c.rowid",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![lo, hi], |r| {
                    Ok((r.get::<_, i64>(0)?, ExportCall { kind: r.get(1)?, name: r.get(2)? }))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (id, call) = row.map_err(sql_err)?;
                calls.entry(id).or_default().push(call);
            }
        }

        let tmp_gz = opts.out_dir.join(format!("tokenme-{origin}.jsonl.gz.tmp"));
        let file = File::create(&tmp_gz).map_err(|e| Error::io(&tmp_gz, e))?;
        // A bundle names sessions, projects and local log paths — private by
        // default; the transport reads it as the same user anyway.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Error::io(&tmp_gz, e))?;
        }
        // Hash the *compressed* bytes: the encoder sits on top, the hashing
        // sink below it, the buffered file at the bottom.
        let mut writer = GzEncoder::new(
            HashingWriter { inner: BufWriter::new(file), hasher: Sha256::new() },
            Compression::default(),
        );
        let mut rows_out: u64 = 0;
        let mut sums = SyncSums::default();
        let mut tools: BTreeMap<String, SyncSums> = BTreeMap::new();
        // Same-content keyless duplicates: the first occurrence keeps the bare
        // synthesized key, later ones count up (`#2`, `#3`, …), so a real
        // duplicate row is never folded away on either side.
        let mut seen_canonical: BTreeMap<String, u64> = BTreeMap::new();

        {
            let mut stmt = tx
                .prepare(
                    "SELECT id, tool, ts_ms, session, project, model, meter, \
                            in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, dedupe_key, source \
                     FROM event WHERE ts_ms >= ?1 AND ts_ms < ?2 ORDER BY id",
                )
                .map_err(sql_err)?;
            let evs = stmt
                .query_map(params![lo, hi], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                        r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                        r.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                        r.get::<_, Option<f64>>(10)?.unwrap_or(0.0),
                        r.get::<_, Option<f64>>(11)?.unwrap_or(0.0),
                        r.get::<_, Option<f64>>(12)?.unwrap_or(0.0),
                        r.get::<_, Option<String>>(13)?,
                        r.get::<_, String>(14)?,
                    ))
                })
                .map_err(sql_err)?;
            for ev in evs {
                let (id, tool, ts_ms, session, project, model, meter, in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, dedupe_key, source) =
                    ev.map_err(sql_err)?;
                let mut row = ExportRow {
                    key: String::new(),
                    tool,
                    ts_ms,
                    session,
                    project,
                    model,
                    meter,
                    source,
                    in_tok,
                    cc_tok,
                    cr_tok,
                    out_tok,
                    reason_tok,
                    credits,
                    calls: calls.remove(&id).unwrap_or_default(),
                };
                row.key = match dedupe_key {
                    Some(k) => k,
                    None => {
                        let canonical = canonical_of(&row);
                        let n = seen_canonical.entry(canonical.clone()).or_insert(0);
                        *n += 1;
                        let base =
                            format!("mix:{origin}:{}", hex(Sha1::digest(canonical.as_bytes()).as_slice()));
                        if *n == 1 {
                            base
                        } else {
                            format!("{base}#{n}")
                        }
                    }
                };
                serde_json::to_writer(&mut writer, &row)
                    .map_err(|e| Error::Sync(format!("cannot serialize row: {e}")))?;
                writer
                    .write_all(b"\n")
                    .map_err(|e| Error::Sync(format!("cannot write bundle: {e}")))?;
                let s = SyncSums {
                    in_tok: row.in_tok,
                    cc_tok: row.cc_tok,
                    cr_tok: row.cr_tok,
                    out_tok: row.out_tok,
                    reason_tok: row.reason_tok,
                    credits: row.credits,
                };
                sums.add(&s);
                tools.entry(row.tool.clone()).or_default().add(&s);
                rows_out += 1;
            }
        }
        drop(tx);

        // finish() flushes the encoder's tail into the hashing sink, so the
        // digest covers every compressed byte including the gzip trailer.
        let hashing = writer
            .finish()
            .map_err(|e| Error::Sync(format!("cannot finish bundle: {e}")))?;
        let mut buf_writer = hashing.inner;
        buf_writer.flush().map_err(|e| Error::Sync(format!("cannot flush bundle: {e}")))?;
        let sha256 = hex(hashing.hasher.finalize().as_slice());
        drop(buf_writer);
        usage_core::replace_file(&tmp_gz, &gz_path).map_err(|e| Error::io(&gz_path, e))?;

        let manifest = SyncManifest {
            format: SYNC_FORMAT.to_string(),
            origin: origin.to_string(),
            schema_version: crate::SCHEMA_VERSION.to_string(),
            window_lo_ms: lo,
            window_hi_ms: hi,
            rows: rows_out,
            sums,
            tools,
            sha256: sha256.clone(),
            generated_at_ms: now_ms(),
        };
        let tmp_manifest = manifest_path.with_extension("json.tmp");
        {
            let f = File::create(&tmp_manifest).map_err(|e| Error::io(&tmp_manifest, e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| Error::io(&tmp_manifest, e))?;
            }
            serde_json::to_writer_pretty(BufWriter::new(f), &manifest)
                .map_err(|e| Error::Sync(format!("cannot write manifest: {e}")))?;
        }
        usage_core::replace_file(&tmp_manifest, &manifest_path)
            .map_err(|e| Error::io(&manifest_path, e))?;

        Ok(ExportReport {
            origin: origin.to_string(),
            gz_path,
            manifest_path,
            window_lo_ms: lo,
            window_hi_ms: hi,
            rows: rows_out,
            sha256,
            took_ms: started.elapsed().as_millis() as i64,
        })
    }
}

// ---- import ------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// The `tokenme-*.jsonl.gz` to merge; its manifest must sit next to it
    /// under the same stem (`tokenme-<host>.manifest.json`).
    pub file: PathBuf,
    /// Do everything except commit: parse, merge, reconcile, then roll back.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportReport {
    pub origin: String,
    pub file: String,
    pub sha256: String,
    pub window_lo_ms: i64,
    pub window_hi_ms: i64,
    pub rows: u64,
    pub inserted: u64,
    pub updated: u64,
    pub deduped: u64,
    pub calls_added: u64,
    /// Rows of this origin already on this machine that the bundle did not
    /// carry (a Linux-side purge after they were imported). Kept, reported.
    pub stale_rows: u64,
    /// The bundle's origin equals this machine's hostname — a self-import.
    pub self_import: bool,
    pub dry_run: bool,
    pub took_ms: i64,
}

fn manifest_path_for(gz_path: &Path) -> Result<PathBuf> {
    let name = gz_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Sync(format!("bad bundle path {}", gz_path.display())))?;
    let stem = name
        .strip_suffix(".jsonl.gz")
        .ok_or_else(|| Error::Sync(format!("expected a *.jsonl.gz bundle, got {name}")))?;
    Ok(gz_path.with_file_name(format!("{stem}.manifest.json")))
}

impl Index {
    /// Merges one bundle into this index. Idempotent: re-importing the same
    /// bundle inserts nothing, updates nothing, and still reconciles clean.
    /// Any failure — bad hash, format/schema mismatch, manifest that
    /// disagrees with its payload, or a batch that does not reconcile —
    /// rolls the whole merge back and leaves the index exactly as it was.
    pub fn import_sync(&mut self, opts: &ImportOptions) -> Result<ImportReport> {
        let started = Instant::now();
        let gz_path = &opts.file;
        let manifest_path = manifest_path_for(gz_path)?;
        let file_name =
            gz_path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let raw = std::fs::read(&manifest_path).map_err(|e| Error::io(&manifest_path, e))?;
        let manifest: SyncManifest = serde_json::from_slice(&raw)
            .map_err(|e| Error::Sync(format!("bad manifest {}: {e}", manifest_path.display())))?;
        if manifest.format != SYNC_FORMAT {
            return Err(Error::Sync(format!(
                "bundle format {:?} is not {SYNC_FORMAT:?}; upgrade the exporting machine's tokenme",
                manifest.format
            )));
        }
        if manifest.schema_version != crate::SCHEMA_VERSION {
            return Err(Error::Sync(format!(
                "bundle was exported from index schema {} but this machine reads {}; upgrade this side first",
                manifest.schema_version,
                crate::SCHEMA_VERSION
            )));
        }
        if manifest.origin.trim().is_empty() {
            return Err(Error::Sync("manifest origin is empty".into()));
        }
        let origin = manifest.origin.trim().to_string();
        // The same character set as export: a bundle that slipped past a
        // modified exporter must not stamp sources this index cannot parse
        // back into an origin (`linux:<origin>:<key>`) or a report row.
        if !origin_ok(&origin) {
            return Err(Error::Sync(format!(
                "manifest origin {origin:?} contains ':', '/' or '\\' or a control character; refusing to import"
            )));
        }
        let sha256 = sha256_file(gz_path)?;
        if !sha256.eq_ignore_ascii_case(manifest.sha256.trim()) {
            return Err(Error::Sync(format!(
                "{file_name}: sha256 mismatch (manifest {}, file {sha256}) — corrupted or truncated bundle, refusing to import",
                manifest.sha256
            )));
        }

        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        tx.execute("DROP TABLE IF EXISTS temp.sync_batch", []).map_err(sql_err)?;
        tx.execute(
            "CREATE TEMP TABLE sync_batch(\
                 key TEXT PRIMARY KEY, tool TEXT NOT NULL, ts_ms INTEGER NOT NULL, \
                 session TEXT NOT NULL, project TEXT, model TEXT, meter TEXT NOT NULL, \
                 source TEXT NOT NULL, \
                 in_tok REAL NOT NULL, cc_tok REAL NOT NULL, cr_tok REAL NOT NULL, \
                 out_tok REAL NOT NULL, reason_tok REAL NOT NULL, credits REAL NOT NULL, \
                 calls TEXT NOT NULL, is_new INTEGER NOT NULL DEFAULT 0, applied INTEGER NOT NULL DEFAULT 0)",
            [],
        )
        .map_err(sql_err)?;

        // Parse the payload straight into the temp table: reconciliation runs
        // in SQL and nothing here needs the rows in Rust memory afterwards.
        let mut rows_out: u64 = 0;
        let mut sums = SyncSums::default();
        {
            let f = File::open(gz_path).map_err(|e| Error::io(gz_path, e))?;
            let mut reader = BufReader::new(GzDecoder::new(BufReader::new(f)));
            let mut line = String::new();
            let mut ins = tx
                .prepare(
                    "INSERT INTO sync_batch(key, tool, ts_ms, session, project, model, meter, source, \
                        in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, calls) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                )
                .map_err(sql_err)?;
            loop {
                line.clear();
                let n = reader
                    .read_line(&mut line)
                    .map_err(|e| Error::Sync(format!("cannot read {file_name}: {e}")))?;
                if n == 0 {
                    break;
                }
                if line.trim().is_empty() {
                    continue;
                }
                let row: ExportRow = serde_json::from_str(&line)
                    .map_err(|e| Error::Sync(format!("{file_name}: bad row at line {}: {e}", rows_out + 1)))?;
                rows_out += 1;
                if rows_out > manifest.rows.saturating_add(ROW_CAP_SLACK) {
                    return Err(Error::Sync(format!(
                        "{file_name}: payload has more rows than its manifest claims ({} + slack) — refusing",
                        manifest.rows
                    )));
                }
                if row.meter != "tokens" && row.meter != "credits" {
                    return Err(Error::Sync(format!(
                        "{file_name}: row {} has meter {:?} (want \"tokens\"/\"credits\")",
                        rows_out, row.meter
                    )));
                }
                if row.ts_ms < manifest.window_lo_ms || row.ts_ms >= manifest.window_hi_ms {
                    return Err(Error::Sync(format!(
                        "{file_name}: row {} (ts {}) falls outside the manifest window [{}, {})",
                        rows_out, row.ts_ms, manifest.window_lo_ms, manifest.window_hi_ms
                    )));
                }
                for c in &row.calls {
                    if c.kind != "mcp" && c.kind != "skill" {
                        return Err(Error::Sync(format!(
                            "{file_name}: row {} has call kind {:?} (want \"mcp\"/\"skill\")",
                            rows_out, c.kind
                        )));
                    }
                }
                let calls_json = serde_json::to_string(&row.calls)
                    .map_err(|e| Error::Sync(format!("cannot encode calls: {e}")))?;
                ins.execute(params![
                    row.key,
                    row.tool,
                    row.ts_ms,
                    row.session,
                    row.project,
                    row.model,
                    row.meter,
                    row.source,
                    row.in_tok,
                    row.cc_tok,
                    row.cr_tok,
                    row.out_tok,
                    row.reason_tok,
                    row.credits,
                    calls_json,
                ])
                .map_err(|e| Error::Sync(format!("{file_name}: duplicate or bad key {:?}: {e}", row.key)))?;
                sums.add(&SyncSums {
                    in_tok: row.in_tok,
                    cc_tok: row.cc_tok,
                    cr_tok: row.cr_tok,
                    out_tok: row.out_tok,
                    reason_tok: row.reason_tok,
                    credits: row.credits,
                });
            }
        }
        if rows_out != manifest.rows {
            return Err(Error::Sync(format!(
                "{file_name}: manifest claims {} rows but the payload has {rows_out} — refusing",
                manifest.rows
            )));
        }
        if sums != manifest.sums {
            return Err(Error::Sync(format!(
                "{file_name}: payload sums disagree with the manifest (payload {sums:?}, manifest {:?}) — refusing",
                manifest.sums
            )));
        }

        // Classify every batch row against the current index, before touching
        // it: `is_new` = no row with this key; `applied` = the merge will
        // write it (new, or the growth predicate accepts). Rows the predicate
        // refuses keep the stored (larger) counts and count as deduped,
        // exactly like a log replay.
        tx.execute(
            "UPDATE sync_batch SET is_new = 1 \
             WHERE NOT EXISTS(SELECT 1 FROM event e WHERE e.dedupe_key = sync_batch.key)",
            [],
        )
        .map_err(sql_err)?;
        tx.execute(
            "UPDATE sync_batch SET applied = 1 WHERE is_new = 1 OR \
                 (in_tok + cc_tok + cr_tok + out_tok) > \
                 COALESCE((SELECT COALESCE(e.in_tok,0)+COALESCE(e.cc_tok,0)+COALESCE(e.cr_tok,0) \
                           +COALESCE(e.out_tok,0)+COALESCE(e.credits,0) \
                           FROM event e WHERE e.dedupe_key = sync_batch.key), 0)",
            [],
        )
        .map_err(sql_err)?;

        // Old timestamps of the rows the UPDATE is about to move — captured
        // before the write so the rollup is repaired on both sides of a move.
        let mut affected_ts: Vec<i64> = {
            let mut stmt = tx
                .prepare(
                    "SELECT e.ts_ms FROM sync_batch b JOIN event e ON e.dedupe_key = b.key \
                     WHERE b.applied = 1 AND b.is_new = 0",
                )
                .map_err(sql_err)?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).map_err(sql_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?
        };

        let inserted = tx
            .execute(
                "INSERT INTO event(tool, ts_ms, session, project, model, meter, \
                     in_tok, cc_tok, cr_tok, out_tok, reason_tok, credits, dedupe_key, source) \
                 SELECT b.tool, b.ts_ms, b.session, b.project, b.model, b.meter, \
                     b.in_tok, b.cc_tok, b.cr_tok, b.out_tok, b.reason_tok, b.credits, \
                     b.key, 'linux:' || ?1 || ':' || b.source \
                 FROM sync_batch b WHERE b.applied = 1 AND b.is_new = 1",
                params![origin],
            )
            .map_err(sql_err)? as u64;

        let updated = tx
            .execute(
                "UPDATE event SET ts_ms = b.ts_ms, session = b.session, project = b.project, \
                     model = b.model, meter = b.meter, in_tok = b.in_tok, cc_tok = b.cc_tok, \
                     cr_tok = b.cr_tok, out_tok = b.out_tok, reason_tok = b.reason_tok, \
                     credits = b.credits, source = 'linux:' || ?1 || ':' || b.source \
                 FROM sync_batch b \
                 WHERE event.dedupe_key = b.key AND b.applied = 1 AND b.is_new = 0",
                params![origin],
            )
            .map_err(sql_err)? as u64;

        affected_ts.extend({
            let mut stmt = tx
                .prepare("SELECT ts_ms FROM sync_batch WHERE applied = 1")
                .map_err(sql_err)?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).map_err(sql_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(sql_err)?
        });
        crate::ingest::recompute_days_tx(&tx, &affected_ts)?;

        // Call union: only for rows this merge touched, and only when that
        // exact (event, kind, name) is absent. Never a delete.
        let mut calls_added: u64 = 0;
        {
            let mut stmt = tx
                .prepare(
                    "SELECT e.id, b.calls FROM sync_batch b JOIN event e ON e.dedupe_key = b.key \
                     WHERE b.applied = 1",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map_err(sql_err)?;
            let mut ins = tx
                .prepare(
                    "INSERT INTO call(event_id, kind, name) \
                     SELECT ?1, ?2, ?3 WHERE NOT EXISTS(\
                         SELECT 1 FROM call WHERE event_id = ?1 AND kind = ?2 AND name = ?3)",
                )
                .map_err(sql_err)?;
            for row in rows {
                let (id, calls_json) = row.map_err(sql_err)?;
                let calls: Vec<ExportCall> = serde_json::from_str(&calls_json)
                    .map_err(|e| Error::Sync(format!("bad calls payload: {e}")))?;
                for c in calls {
                    calls_added += ins
                        .execute(params![id, call_kind_str(call_kind_of(&c.kind)), c.name])
                        .map_err(sql_err)? as u64;
                }
            }
        }

        // ---- reconciliation, by key (window sums would wedge on stale rows) --
        let missing: i64 = tx
            .query_row(
                "SELECT count(*) FROM sync_batch b \
                 WHERE NOT EXISTS(SELECT 1 FROM event e WHERE e.dedupe_key = b.key)",
                [],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if missing != 0 {
            return Err(Error::Sync(format!(
                "{file_name}: {missing} of {rows_out} keys are missing after the merge — rolling back"
            )));
        }
        // A stored row may differ from the batch only when the growth
        // predicate legitimately refused it: the batch's prompt-side total
        // must be <= the stored row's total *including* credits.
        let unexplained: i64 = tx
            .query_row(
                "SELECT count(*) FROM sync_batch b JOIN event e ON e.dedupe_key = b.key \
                 WHERE NOT (\
                     COALESCE(e.in_tok,0) = b.in_tok AND COALESCE(e.cc_tok,0) = b.cc_tok \
                     AND COALESCE(e.cr_tok,0) = b.cr_tok AND COALESCE(e.out_tok,0) = b.out_tok \
                     AND COALESCE(e.reason_tok,0) = b.reason_tok AND COALESCE(e.credits,0) = b.credits) \
                 AND NOT ((b.in_tok + b.cc_tok + b.cr_tok + b.out_tok) <= \
                     COALESCE(e.in_tok,0)+COALESCE(e.cc_tok,0)+COALESCE(e.cr_tok,0) \
                     +COALESCE(e.out_tok,0)+COALESCE(e.credits,0))",
                [],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if unexplained != 0 {
            return Err(Error::Sync(format!(
                "{file_name}: {unexplained} merged rows cannot be explained by the growth predicate — rolling back"
            )));
        }
        // Stale = this origin's rows on this machine that the bundle did not
        // carry. They are the standing "import never deletes" semantics, so
        // they are reported (the badge tooltip shows them) rather than judged.
        let prefix = format!("linux:{origin}:");
        let stale_rows: u64 = tx
            .query_row(
                "SELECT count(*) FROM event \
                 WHERE substr(source, 1, ?1) = ?2 AND ts_ms >= ?3 AND ts_ms < ?4 \
                   AND NOT EXISTS(SELECT 1 FROM sync_batch b WHERE b.key = event.dedupe_key)",
                params![prefix.len() as i64, prefix, manifest.window_lo_ms, manifest.window_hi_ms],
                |r| r.get::<_, i64>(0),
            )
            .map_err(sql_err)? as u64;

        let deduped = rows_out.saturating_sub(inserted).saturating_sub(updated);
        let record = SyncRecord {
            origin: origin.clone(),
            imported_at_ms: now_ms(),
            window_lo_ms: manifest.window_lo_ms,
            window_hi_ms: manifest.window_hi_ms,
            rows: rows_out,
            file: file_name.clone(),
            sha256: Some(sha256.clone()),
        };
        let record_json = serde_json::to_string(&record)
            .map_err(|e| Error::Sync(format!("cannot encode sync record: {e}")))?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            params![format!("sync:linux:{origin}"), record_json],
        )
        .map_err(sql_err)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            params![format!("sync:file:{file_name}"), sha256],
        )
        .map_err(sql_err)?;

        let self_import = origin == hostname();
        if opts.dry_run {
            tx.rollback().map_err(sql_err)?;
        } else {
            tx.commit().map_err(sql_err)?;
        }

        Ok(ImportReport {
            origin,
            file: file_name,
            sha256,
            window_lo_ms: manifest.window_lo_ms,
            window_hi_ms: manifest.window_hi_ms,
            rows: rows_out,
            inserted,
            updated,
            deduped,
            calls_added,
            stale_rows,
            self_import,
            dry_run: opts.dry_run,
            took_ms: started.elapsed().as_millis() as i64,
        })
    }

    /// The sha256 recorded for a bundle file name by a previous `import`, if
    /// any — the panel engine's skip test before re-parsing a bundle.
    pub fn sync_file_memo(&self, file_name: &str) -> Result<Option<String>> {
        self.meta_value(&format!("sync:file:{file_name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Index;
    use usage_core::MachineScope;

    fn ts(hours_ago: i64) -> i64 {
        now_ms() - hours_ago * 3_600_000
    }

    #[allow(clippy::too_many_arguments)]
    fn seed(
        idx: &Index,
        tool: &str,
        session: &str,
        at: i64,
        key: Option<&str>,
        in_tok: f64,
        out_tok: f64,
        credits: f64,
        calls: &[(&str, &str)],
    ) {
        idx.conn()
            .execute(
                "INSERT INTO event(tool, ts_ms, session, project, model, meter, in_tok, out_tok, \
                     credits, dedupe_key, source) \
                 VALUES(?1, ?2, ?3, 'proj', 'm', 'tokens', ?4, ?5, ?6, ?7, '/logs/x.jsonl')",
                params![tool, at, session, in_tok, out_tok, credits, key],
            )
            .unwrap();
        if !calls.is_empty() {
            let id = idx.conn().last_insert_rowid();
            for (kind, name) in calls {
                idx.conn()
                    .execute(
                        "INSERT INTO call(event_id, kind, name) VALUES(?1, ?2, ?3)",
                        params![id, kind, name],
                    )
                    .unwrap();
            }
        }
    }

    fn export_to(idx: &Index, dir: &Path, origin: &str) -> ExportReport {
        idx.export_sync(&ExportOptions {
            days: 30,
            out_dir: dir.to_path_buf(),
            origin: origin.to_string(),
        })
        .unwrap()
    }

    fn import(tgt: &mut Index, rep: &ExportReport) -> ImportReport {
        tgt.import_sync(&ImportOptions { file: rep.gz_path.clone(), dry_run: false }).unwrap()
    }

    fn one_i64(idx: &Index, sql: &str) -> i64 {
        idx.conn()
            .query_row(sql, [], |r| {
                Ok(match r.get_ref(0)? {
                    rusqlite::types::ValueRef::Integer(i) => i,
                    rusqlite::types::ValueRef::Real(f) => f.round() as i64,
                    other => panic!("unexpected column type {other:?}"),
                })
            })
            .unwrap()
    }

    fn base_source() -> Index {
        let src = Index::open_in_memory().unwrap();
        seed(&src, "claude", "s1", ts(5), Some("claude#k1"), 100.0, 10.0, 0.0, &[("mcp", "srv")]);
        seed(&src, "claude", "s1", ts(4), Some("claude#k2"), 200.0, 20.0, 0.0, &[]);
        seed(&src, "claude", "s2", ts(3), Some("claude#k3"), 300.0, 30.0, 5.0, &[]);
        // Two identical keyless (Codex-style) rows: neither may be folded.
        // One timestamp read for both — two `ts(2)` calls can straddle a
        // millisecond, and a 1 ms difference is a different canonical row.
        let dup_ts = ts(2);
        seed(&src, "codex", "c1", dup_ts, None, 7.0, 1.0, 0.0, &[]);
        seed(&src, "codex", "c1", dup_ts, None, 7.0, 1.0, 0.0, &[]);
        src
    }

    #[test]
    fn hostname_is_stable_and_nonempty() {
        let first = hostname();
        assert!(!first.trim().is_empty());
        assert_eq!(first, hostname());
    }

    #[test]
    fn round_trip_is_idempotent_and_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let rep = export_to(&src, dir.path(), "test-origin-xyz");
        assert_eq!(rep.rows, 5);
        // The bundle names sessions, projects and local paths: 0600 on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&rep.gz_path, &rep.manifest_path] {
                let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{p:?} must be private");
            }
        }
        let mut tgt = Index::open_in_memory().unwrap();
        let first = import(&mut tgt, &rep);
        assert_eq!((first.inserted, first.updated, first.deduped), (5, 0, 0));
        assert_eq!(first.calls_added, 1);
        assert!(!first.self_import);
        assert_eq!(tgt.event_count().unwrap(), 5);
        assert_eq!(one_i64(&tgt, "SELECT count(*) FROM call"), 1);
        // The sync record and the file memo both landed.
        let record = tgt.meta_value("sync:linux:test-origin-xyz").unwrap().unwrap();
        let record: SyncRecord = serde_json::from_str(&record).unwrap();
        assert_eq!(record.rows, 5);
        assert_eq!(record.origin, "test-origin-xyz");
        assert_eq!(
            tgt.sync_file_memo(&rep.gz_path.file_name().unwrap().to_string_lossy()).unwrap().as_deref(),
            Some(rep.sha256.as_str())
        );
        // Second import: pure no-op, still reconciles.
        let second = import(&mut tgt, &rep);
        assert_eq!((second.inserted, second.updated, second.deduped), (0, 0, 5));
        assert_eq!(second.calls_added, 0);
        assert_eq!(tgt.event_count().unwrap(), 5);
        // Rollup repaired in step with event, day for day. The Claude rows sit
        // 3–5 h in the past, so their days differ whenever the suite runs
        // between 03:00 and 05:00 local — compare per day, not one fixed day.
        let mut ev_by_day: BTreeMap<String, i64> = BTreeMap::new();
        {
            let mut stmt = tgt
                .conn()
                .prepare("SELECT ts_ms, in_tok FROM event WHERE tool='claude'")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))
                .unwrap();
            for row in rows {
                let (ms, v) = row.unwrap();
                let day = usage_core::local_day_of(ms).unwrap().format("%Y-%m-%d").to_string();
                *ev_by_day.entry(day).or_insert(0) += v.round() as i64;
            }
        }
        let mut roll_by_day: BTreeMap<String, i64> = BTreeMap::new();
        {
            let mut stmt = tgt
                .conn()
                .prepare("SELECT day, sum(in_tok) FROM event_rollup WHERE tool='claude' GROUP BY day")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))
                .unwrap();
            for row in rows {
                let (day, v) = row.unwrap();
                roll_by_day.insert(day, v.round() as i64);
            }
        }
        assert_eq!(ev_by_day, roll_by_day, "rollup disagrees with event day by day");
        // Keyless duplicates kept distinct via #2.
        assert_eq!(
            one_i64(&tgt, "SELECT count(*) FROM event WHERE dedupe_key LIKE 'mix:test-origin-xyz:%#2'"),
            1
        );
    }

    #[test]
    fn growth_updates_shrink_and_credits_asymmetry_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let mut tgt = Index::open_in_memory().unwrap();
        import(&mut tgt, &export_to(&src, dir.path(), "o1"));

        // Growth: 100 -> 200 tokens on k1 => updated.
        src.conn().execute("UPDATE event SET in_tok = 200 WHERE dedupe_key = 'claude#k1'", []).unwrap();
        let rep = export_to(&src, dir.path(), "o1");
        let r = import(&mut tgt, &rep);
        assert_eq!((r.inserted, r.updated), (0, 1));
        assert_eq!(one_i64(&tgt, "SELECT in_tok FROM event WHERE dedupe_key='claude#k1'"), 200);

        // Shrink: 200 -> 50 => predicate refuses, stored stays 200.
        src.conn().execute("UPDATE event SET in_tok = 50 WHERE dedupe_key = 'claude#k1'", []).unwrap();
        let rep = export_to(&src, dir.path(), "o1");
        let r = import(&mut tgt, &rep);
        assert_eq!((r.inserted, r.updated, r.deduped), (0, 0, 5));
        assert_eq!(one_i64(&tgt, "SELECT in_tok FROM event WHERE dedupe_key='claude#k1'"), 200);

        // Credits asymmetry on k3 (300 tokens + 5 credits stored): 302 tokens
        // alone is <= 300+5, so it must NOT grow.
        src.conn().execute("UPDATE event SET in_tok = 302 WHERE dedupe_key = 'claude#k3'", []).unwrap();
        let rep = export_to(&src, dir.path(), "o1");
        let r = import(&mut tgt, &rep);
        assert_eq!(r.updated, 0);
        assert_eq!(one_i64(&tgt, "SELECT in_tok FROM event WHERE dedupe_key='claude#k3'"), 300);
        // 306 tokens > 300+5 => grows now.
        src.conn().execute("UPDATE event SET in_tok = 306 WHERE dedupe_key = 'claude#k3'", []).unwrap();
        let rep = export_to(&src, dir.path(), "o1");
        let r = import(&mut tgt, &rep);
        assert_eq!(r.updated, 1);
        assert_eq!(one_i64(&tgt, "SELECT in_tok FROM event WHERE dedupe_key='claude#k3'"), 306);
    }

    /// The F1 regression: a Linux-side purge leaves stale rows on the display
    /// side (import never deletes), and that must not wedge future imports.
    #[test]
    fn stale_rows_are_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let mut tgt = Index::open_in_memory().unwrap();
        import(&mut tgt, &export_to(&src, dir.path(), "o1"));
        assert_eq!(tgt.event_count().unwrap(), 5);

        // Linux purges k2 (e.g. its log was rewritten), so the next bundle
        // carries 4 rows; the display side keeps all 5 by design.
        src.conn().execute("DELETE FROM call WHERE event_id IN (SELECT id FROM event WHERE dedupe_key='claude#k2')", []).unwrap();
        src.conn().execute("DELETE FROM event WHERE dedupe_key = 'claude#k2'", []).unwrap();
        let rep = export_to(&src, dir.path(), "o1");
        assert_eq!(rep.rows, 4);
        let r = import(&mut tgt, &rep);
        assert_eq!(r.rows, 4);
        assert_eq!(r.stale_rows, 1);
        assert_eq!(tgt.event_count().unwrap(), 5, "stale row kept, not deleted");

        // And the import after that still reconciles clean (no permanent wedge).
        let r = import(&mut tgt, &rep);
        assert_eq!((r.inserted, r.updated, r.deduped), (0, 0, 4));
        assert_eq!(r.stale_rows, 1);
    }

    #[test]
    fn call_union_never_duplicates_on_repeated_growth() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let mut tgt = Index::open_in_memory().unwrap();
        import(&mut tgt, &export_to(&src, dir.path(), "o1"));
        assert_eq!(one_i64(&tgt, "SELECT count(*) FROM call"), 1);
        src.conn().execute("UPDATE event SET in_tok = 200 WHERE dedupe_key = 'claude#k1'", []).unwrap();
        let rep = export_to(&src, dir.path(), "o1");
        let r = import(&mut tgt, &rep);
        assert_eq!(r.updated, 1);
        assert_eq!(r.calls_added, 0, "the same call must not be appended twice");
        assert_eq!(one_i64(&tgt, "SELECT count(*) FROM call"), 1);
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let rep = export_to(&src, dir.path(), "o1");
        let mut tgt = Index::open_in_memory().unwrap();
        let r = tgt
            .import_sync(&ImportOptions { file: rep.gz_path.clone(), dry_run: true })
            .unwrap();
        assert!(r.dry_run);
        assert_eq!(r.inserted, 5);
        assert_eq!(tgt.event_count().unwrap(), 0);
        assert!(tgt.meta_value("sync:linux:o1").unwrap().is_none());
        // A real import after the dry run still works (no leftover state).
        let r = import(&mut tgt, &rep);
        assert_eq!(r.inserted, 5);
        assert_eq!(tgt.event_count().unwrap(), 5);
    }

    #[test]
    fn sha_mismatch_is_rejected_and_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let rep = export_to(&src, dir.path(), "o1");
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&rep.gz_path).unwrap();
            let len = f.metadata().unwrap().len();
            f.seek(SeekFrom::Start(len / 2)).unwrap();
            let mut b = [0u8; 1];
            f.read_exact(&mut b).unwrap();
            b[0] ^= 0xFF;
            f.seek(SeekFrom::Start(len / 2)).unwrap();
            f.write_all(&b).unwrap();
        }
        let mut tgt = Index::open_in_memory().unwrap();
        let err = tgt
            .import_sync(&ImportOptions { file: rep.gz_path.clone(), dry_run: false })
            .unwrap_err()
            .to_string();
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert_eq!(tgt.event_count().unwrap(), 0);
    }

    #[test]
    fn manifest_payload_disagreement_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let rep = export_to(&src, dir.path(), "o1");
        let raw = std::fs::read(&rep.manifest_path).unwrap();
        let mut manifest: SyncManifest = serde_json::from_slice(&raw).unwrap();
        manifest.rows += 1;
        std::fs::write(&rep.manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let mut tgt = Index::open_in_memory().unwrap();
        let err = tgt
            .import_sync(&ImportOptions { file: rep.gz_path.clone(), dry_run: false })
            .unwrap_err()
            .to_string();
        assert!(err.contains("manifest claims"), "{err}");
        assert_eq!(tgt.event_count().unwrap(), 0);
    }

    #[test]
    fn format_and_schema_are_gated() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let rep = export_to(&src, dir.path(), "o1");
        let raw = std::fs::read(&rep.manifest_path).unwrap();
        for (field, bogus, needle) in [
            ("schema_version", "3", "schema"),
            ("format", "tokenme-sync/0", "format"),
        ] {
            let mut manifest: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            manifest[field] = serde_json::Value::String(bogus.into());
            std::fs::write(&rep.manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            let mut tgt = Index::open_in_memory().unwrap();
            let err = tgt
                .import_sync(&ImportOptions { file: rep.gz_path.clone(), dry_run: false })
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{field}: {err}");
            assert_eq!(tgt.event_count().unwrap(), 0);
        }
        // Restore for tidiness is unnecessary (tempdir), but the original
        // manifest must still import clean.
        std::fs::write(&rep.manifest_path, &raw).unwrap();
        let mut tgt = Index::open_in_memory().unwrap();
        import(&mut tgt, &rep);
        assert_eq!(tgt.event_count().unwrap(), 5);
    }

    #[test]
    fn self_import_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let rep = export_to(&src, dir.path(), &hostname());
        let mut tgt = Index::open_in_memory().unwrap();
        let r = import(&mut tgt, &rep);
        assert!(r.self_import);
    }

    #[test]
    fn imported_record_surfaces_in_report_facts() {
        let dir = tempfile::tempdir().unwrap();
        let src = base_source();
        let rep = export_to(&src, dir.path(), "linux-origin-1");
        let mut tgt = Index::open_in_memory().unwrap();
        assert_eq!(import(&mut tgt, &rep).inserted, 5);

        // A hand-edited/broken record must be skipped, not kill the badge.
        tgt.conn()
            .execute("INSERT INTO meta(key, value) VALUES('sync:linux:broken', 'not json')", [])
            .unwrap();

        let plan = usage_core::AggregatePlan::build(usage_core::report::now_ms(), 5);
        let facts = tgt.report_facts(&plan, &MachineScope::All).unwrap();
        assert_eq!(facts.syncs.len(), 1, "{:?}", facts.syncs);
        let rec = &facts.syncs[0];
        assert_eq!(rec.origin, "linux-origin-1");
        assert_eq!(rec.rows, rep.rows);
        assert_eq!(rec.window_lo_ms, rep.window_lo_ms);
        assert_eq!(rec.window_hi_ms, rep.window_hi_ms);
        assert_eq!(rec.file, "tokenme-linux-origin-1.jsonl.gz");
        assert_eq!(rec.sha256.as_deref(), Some(rep.sha256.as_str()));
    }
}
