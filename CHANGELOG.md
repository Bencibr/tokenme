# Changelog

All notable changes to TokenMe are documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.5] — 2026-10-05

### Added

- **Kimi Code** — usage from the CLI's `wire.jsonl` (v1 and v2) and from the desktop app's embedded runtime home, plus live plan windows (5-hour / weekly / monthly) from the vendor's own `/usages`. Credentials are read-only: the probe never refreshes or stands in for your login.
- **MiniMax Code** — usage from the dated session streams under `~/.minimax`, plus the plan's 5-hour / weekly windows and both credit wallets (purchased and check-in). The ~1-hour access token is renewed in place from the app's own stored refresh token and written back atomically in the app's own format, so the app and the panel stay signed in together.
- **Qoder desktop quota** — the probe reads the 0.4 desktop app's own auth record (`auth.v1.dat` beside its Chromium profile, keychain-paired on macOS, DPAPI on Windows) when the IDE's encrypted snapshot is not available.
- **Homebrew delivery** — `brew install --cask tokenme` for the panel and `brew install tokenme-cli` for the CLI, via the `Bencibr/homebrew-tokenme` tap that self-bumps from each release's own `sha256.txt`.
- **Supervised engine and the "Updates stopped" badge** — the panel restarts its engine loop after a panic or an unexpected exit (capped backoff, reason in `panel.log`), and the header states the report's age once it goes stale, so frozen numbers no longer look identical to quiet ones.

### Fixed

- **antigravity retry billing** — only retry boxes carrying their own `responseId` count as their own call. Copies of the parent's stream (81,260 of the 81,727 boxes on the machine measured) and id-less boxes no longer bill twice, recovering the 17 genuinely distinct attempts they were hiding.
- **OpenCode v2 sessions** — the newer `session_message` table (role in the `type` column) is read alongside `message`, and rows updated in place are no longer skipped by the resume cursor.
- **Qoder app lookup off macOS** — the auth-record directory search gained the Windows `%APPDATA%` fallback instead of assuming macOS paths.

### Changed

- The accuracy gate (`scripts/verify-totals.py`) learns Kimi Code's and MiniMax Code's stages — including MiniMax's own vendor-side usage table as a third opinion — plus an identity check for antigravity's retry boxes.
- READMEs list the two new tools (22 built-in adapters); the user guide gained the Kimi / MiniMax log paths, the in-place token-renewal note, and the frozen-badge troubleshooting row.
