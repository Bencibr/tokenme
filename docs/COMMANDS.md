# TokenMe CLI Reference

> Version: 0.1.5 · Updated: 2026-10-05 · Audience: CLI users · Source of truth: `crates/usage-cli/src/args.rs` (verified against `tokenme --help`)

All commands share these global options (place them anywhere on the line):

| Option | Effect |
| :--- | :--- |
| `--tool <ID>` | Restrict to specific sources; repeatable (`claude`, `codex`, `cline`, …) |
| `--json` | Machine-readable output on stdout |
| `--offline` | Never touch the network; use the cached/bundled price table |
| `--pricing-override <MODEL=IN/OUT/CC/CR>` | Force a price, USD per 1M tokens |
| `--since <YYYY-MM-DD>` / `--until <YYYY-MM-DD>` | Clip the window |
| `--db <PATH>` | Use a specific index database |
| `--no-ingest` | Report from the existing index without re-scanning |
| `-q` / `-v` | Quiet / verbose |

## detect

List every supported AI tool found on this machine, its log roots and index state.

```bash
tokenme detect
```

## daily / weekly / monthly

Per-period totals with session counts, token stages, cost and cache rate.

```bash
tokenme daily --days 30
tokenme weekly --weeks 8
tokenme monthly --months 6
```

| Flag | Default | Notes |
| :--- | :--- | :--- |
| `--days/--weeks/--months <N>` | 14 / 12 / 12 | Capped at 90 periods |

## report

One window broken down by tool, model, project, MCP server or skill, with the delta against the previous slice.

```bash
tokenme report --window week --group model
```

| Flag | Values |
| :--- | :--- |
| `--window` | `day` (default), `week`, `month` |
| `--group` | `tool` (default), `model`, `project`, `mcp`, `skill` |

## sessions

Recent sessions with per-session totals.

```bash
tokenme sessions --limit 20
```

## quota

Newest live quota sample each source reported: rolling 5-hour pools, weekly windows, credit plans, reset countdowns.

```bash
tokenme quota
```

Probes are read-only and cached per vendor TTL. Cline's probe rotates its OAuth token in place on refresh.

## pricing

Price provenance: models.dev sells most models through several providers, so a cost number is only meaningful together with the listing that won.

```bash
tokenme pricing explain deepseek-v4-flash   # which listing priced it, and the losers
tokenme pricing contested --limit 25        # multi-provider models and the money at stake
```

## budget

Spend caps measured against tokenme's own cost, for tools whose vendor publishes no limit.

```bash
tokenme budget list
tokenme budget set zcode --daily 5 --monthly 50
tokenme budget rm zcode
```

Units are USD. `0` clears just that window.

## index

Drive the index explicitly.

```bash
tokenme index --status              # show state without ingesting
tokenme index --rebuild             # drop and re-read the retention window
tokenme index --prune               # drop events older than retention
tokenme index --force               # steal the ingest lease from a dead process
```

## icons

The macOS application icon each tool row would show, as data URLs (what the panel requests over IPC). Written to `icons.json` for review.

## export

Export a window of this machine's events as a sync bundle (gzipped JSONL + a manifest carrying the sha256 and per-tool sums) for another machine to import. Lands in `~/tokenme-sync` by default — the directory the menu-bar engine merges automatically — so a Linux collector's whole writer is one timer line. Recent events are ingested first (same as every reporting command; `--no-ingest` skips that). A quiet or logless machine still gets a valid bundle with `rows 0` — the collector's timer never fails on a slow day.

```bash
tokenme export                          # last 30 days → ~/tokenme-sync/tokenme-<host>.jsonl.gz
tokenme export --days 400               # first backfill: the whole retention window
tokenme export --out /tmp/share         # somewhere else (a Syncthing folder, a USB stick)
tokenme export --origin workstation-01  # pin the identity (default: hostname)
```

The `.gz` is written via `*.tmp` + rename and the manifest last, so a transport never sees a half-written bundle. Re-exporting the same window is safe: imports are idempotent. Both files land `0600` on unix — a bundle names sessions, projects and local log paths, so it is private by default.

## import

Merge one bundle exported by another machine into the local index. Validates the sha256 and every batch's totals against the manifest first; any failure rolls the whole merge back and the previous numbers stay. Re-importing the same file changes nothing.

```bash
tokenme import ~/tokenme-sync/tokenme-build-01.jsonl.gz
tokenme import bundle.jsonl.gz --dry-run   # parse + reconcile, then roll back
```

On the menu-bar side you rarely need this: the panel engine auto-imports `~/tokenme-sync` every pass and shows a **Linux 同步 / Linux sync** badge with the newest merge per machine (warning-hued past 24 h).

Limits and trust: payload rows are capped (the manifest's count plus slack) so a corrupt file can't exhaust memory; a bundle can only move a row forward — the growth predicate refuses lower counts — and it never deletes (rows the collector purged later are reported as stale). A bundle is validated, not signed: anyone who can write into the sync folder can add rows, so treat it like the index itself. Task guide: [USER_GUIDE.md](USER_GUIDE.md) §4.

## Storage locations

| Data | macOS | Linux | Windows |
| :--- | :--- | :--- | :--- |
| SQLite index | `~/Library/Application Support/tokenme/index.db` | `~/.local/share/tokenme/index.db` | `%LOCALAPPDATA%\tokenme\index.db` |
| Settings | `~/Library/Application Support/tokenme/settings.json` | `~/.config/tokenme/settings.json` | `%APPDATA%\tokenme\settings.json` |
| Quota cache | `<data dir>/tokenme/quota/*.json` | same pattern | same pattern |
