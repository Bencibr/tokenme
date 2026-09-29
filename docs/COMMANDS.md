# TokenMe CLI Reference

> Version: 0.1.0 · Updated: 2026-09-29 · Audience: CLI users · Source of truth: `crates/usage-cli/src/args.rs` (verified against `tokenme --help`)

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

## Storage locations

| Data | macOS | Linux | Windows |
| :--- | :--- | :--- | :--- |
| SQLite index | `~/Library/Application Support/tokenme/index.db` | `~/.local/share/tokenme/index.db` | `%LOCALAPPDATA%\tokenme\index.db` |
| Settings | `~/Library/Application Support/tokenme/settings.json` | `~/.config/tokenme/settings.json` | `%APPDATA%\tokenme\settings.json` |
| Quota cache | `<data dir>/tokenme/quota/*.json` | same pattern | same pattern |
