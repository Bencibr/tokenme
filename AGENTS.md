# AGENTS.md — how to build and release tokenme

Working notes for coding agents. Facts here were verified on 2026-09-27 while
shipping v0.1.0; re-check anything time-sensitive before relying on it.

## Repo map

- Panel (Tauri 2): `apps/tokenme-bar` — frontend in `src/`, Rust in `src-tauri/`.
- Usage engine: `crates/usage-quota` (one file per provider), `crates/usage-cli` (the `tokenme` CLI).
- Version lives in **five** homes and must move together: root `Cargo.toml`
  (`[workspace.package] version`), `apps/tokenme-bar/src-tauri/Cargo.toml`,
  `apps/tokenme-bar/src-tauri/tauri.conf.json`, `latest.json`, and
  `apps/tokenme-bar/package.json`. `scripts/release.sh x.y.z` bumps the first
  four (and commits + tags them); package.json is manual. Every crate in the
  workspace says `version.workspace = true`, so the root bump reaches all of them
  — `crates/adapters/all/src/lib.rs` gates the four source homes against the
  compiled version, and gates `latest.json` for self-consistency only: that file
  is a *published* manifest, so it may lag the tree (0.1.5 was never run through
  `release.sh`, and a lagging manifest is honest) but may never advertise an
  installer whose name or hash belongs to another release.

## Windows build (this machine)

```sh
cd apps/tokenme-bar && pnpm tauri build --bundles nsis
```

`beforeBuildCommand` runs `tsc --noEmit && vite build` first. The artifact is
`apps/tokenme-bar/src-tauri/target/release/bundle/nsis/TokenMe_<v>_x64-setup.exe`.
`mainBinaryName: "TokenMe"` makes the installed process `TokenMe.exe` (installed
to `%LOCALAPPDATA%\TokenMe`; Windows folds the casing onto the same directory the
lowercase era used, and the NSIS uninstaller-first flow keys on `productName`,
which never changed). The NSIS bundle carries the app icon
(`installerIcon`/`uninstallerIcon`), speaks SimpChinese + English with a
language selector, and `installMode: "both"` asks 所有用户 (Program Files,
elevated) vs 只为我 (`%LOCALAPPDATA%`) on every interactive run — MultiUser's
`highestAvailable` means admins see one UAC prompt at launch even when they
pick the per-user mode. Quiet upgrade: kill `TokenMe.exe` — and the legacy
`tokenme.exe` on installs from 2026-09-28 to 2026-10-07, or `tokenme-bar.exe` on
installs older than 2026-09-28 — then run the installer with
`/S /currentuser` (the mode flag is required in `both` mode; expect the UAC
consent click) — the NSIS uninstaller-first flow
removes the old binary, including a renamed one. Switching a machine between
只为我 and 所有用户 needs the old install uninstalled first: the two modes
register under different hives (HKCU vs HKLM) and cannot see each other.

The panel bundles the two static Linux collectors it uploads to servers
(`resources/collector/tokenme-{x86_64,aarch64}`); since the machines/sync feature
the bundle step fails without them. Stage with `./scripts/bundle-collector.sh` —
needs `cargo-zigbuild` + zig (both installed here: zig lives in
`E:\tokenme-tools\zig\zig-windows-x86_64-0.13.0`, add it to PATH; the musl
rustup targets are already added). SSH uses russh's `ring` backend on purpose —
the default `aws-lc-rs` wants NASM, which this machine does not ship.

## Gitea release (gitea.sp.dev/sp/tokenme — private repo)

1. Land the work as conventional commits and push. The remote moves often:
   pull and merge before pushing, and **rebuild the artifact after pulling** —
   a release asset must come from the exact commit the tag points at.
2. The API token comes from the Windows credential store — never echo it:

   ```sh
   CRED=$(printf "protocol=https\nhost=gitea.sp.dev\n\n" | git credential fill)
   USER=$(echo "$CRED" | sed -n 's/^username=//p')
   PASS=$(echo "$CRED" | sed -n 's/^password=//p')
   ```

3. Create the release, then attach the asset (multipart field is `attachment`):

   ```sh
   curl -u "$USER:$PASS" -H "Content-Type: application/json" \
     -d "{\"tag_name\":\"v$V\",\"target_commitish\":\"$SHA\",\"name\":\"TokenMe $V\",\"body\":\"...\"}" \
     https://gitea.sp.dev/api/v1/repos/sp/tokenme/releases
   curl -u "$USER:$PASS" \
     -F "attachment=@.../TokenMe_${V}_x64-setup.exe" \
     "https://gitea.sp.dev/api/v1/repos/sp/tokenme/releases/<id>/assets?name=TokenMe_${V}_x64-setup.exe"
   ```

4. Verify by roundtrip: download `browser_download_url` with the same
   credentials and compare sha256. An anonymous GET returning the login page is
   expected (private repo); say so when handing out links.
5. Put the SHA256 in the release body.

## macOS DMG

**A DMG cannot be produced on Windows.** `codesign`, `hdiutil` and the .app
layout are macOS-only; there is no viable cross-compile for a Tauri bundle.

- On a Mac: `./scripts/build-macos.sh` → ad-hoc-signed `TokenMe.app` and
  `bundle/dmg/tokenme_<v>_<arch>.dmg` (hdiutil UDZO, drag-install layout).
  Iteration builds: `./scripts/build-macos.sh --fast` skips the test gate and
  the DMG; `scripts/check-theme-parity.py` runs either way. The default path
  skips `cargo test` only when no crate source,
  fixture or manifest changed since the last green run (`.cargo-test-stamp`,
  gitignored) — delete the stamp to force the gate. Release profile is thin
  LTO on purpose: fat LTO re-optimized the whole program at every link, so
  even a frontend-only change paid a full re-link.
  No developer certificate by design — Gatekeeper clears with one right-click open.
- **The gate stamps its own build number.** `build-macos.sh` computes
  `BUILD_ID = file + 1`, writes it back and embeds that number, so the id a
  build carries is never the value the tree had *before* it ran — commit
  BUILD_ID *after* the gate, not before. Hand-writing the anticipated number
  first only skips it: the 0.1.5 batch's delivered build is 105, from a
  hand-edit to 104 that the script then incremented — no binary was ever 104,
  and a commit message quoting the pre-build value will disagree with the
  app's own `build N starting` log line. The test stamp excludes the file for
  exactly this reason: it churns on every build while reaching only the app
  crate. Log lines land in `~/Library/Logs/tokenme/panel.log` and are the
  quickest way to confirm which build is actually installed.
- **An in-place `.app` upgrade cannot rename the executable.** `mainBinaryName` is
  `TokenMe`, and a fresh bundle really does contain `Contents/MacOS/TokenMe` — but on
  APFS's default case-insensitive volume, `ditto`/Finder copying the new bundle over an
  old install writes *into* the existing `tokenme` (same name to the filesystem), so
  Activity Monitor keeps reading `tokenme` after a drag-over upgrade while the shipped
  zip and a clean install say `TokenMe`. Verified by hashing both binaries: identical
  bytes, different spelling. Anything that looks for the executable by name must list
  the directory instead — `scripts/verify-cold-start.py` does exactly that.
- CI: `.github/workflows/release.yml` triggers on tags `v*` (macos-14), runs the
  same script and attaches `dist/tokenme-macos-arm64.app.zip`, the DMG,
  `sha256.txt` and `latest.json`. It fires when the GitHub repo lands
  (`src/lib/about.ts` centralizes the repo constants — edit there when it does).
- Gitea Actions is enabled on gitea.sp.dev but had **zero registered runners**
  as of 2026-09-27, so nothing can execute the workflow there; the macOS job
  also needs a runner labelled `macos-14`, which means real Apple hardware.
- Until then: build on a Mac, then attach the DMG to the Gitea release with the
  API call above and add its sha256 to the body.

## Linux CLI (collector side)

- `./scripts/build-linux.sh [x86_64|aarch64]` cross-compiles fully static musl
  `tokenme` binaries with cargo-zigbuild (needs `zig` + `cargo-zigbuild` +
  the rustup musl targets) into `dist/linux/tokenme-cli-<v>-<arch>-linux-musl.tar.gz`
  + `sha256.txt`. The tarball ships `install-linux.sh` next to the binary.
- `./scripts/install-linux.sh [--every 15] [--push user@host] [--uninstall]`:
  binary → `~/.local/bin/tokenme`; `--every` adds a systemd **--user** timer
  running `tokenme export --days 30` into `~/tokenme-sync` (plus an scp push
  with `--push`), with an automatic cron fallback when the user manager is not
  reachable (containers, bare ssh) — both paths run the same generated
  `tokenme-export-run.sh`, and `--uninstall` removes either. Display machines
  need nothing: the panel engine merges `~/tokenme-sync` every pass (see
  `engine.rs::import_sync_dir`).
- Sync protocol: `crates/usage-index/src/sync.rs` (`export_sync`/`import_sync`);
  format + merge semantics frozen in `docs/internal/LINUX_SYNC_PLAN.md` §5.
  `release.yml` has an additive `cli-linux` job; on Gitea (no runners) build
  locally and attach the tarball to the release like the Windows artifact.

## Conventions

- Conventional commits, one concern per commit; match the style of `git log`.
- **Never run `cargo fmt` here.** The Rust tree is hand-formatted on purpose (dense
  one-liners), so `cargo fmt` — even `cargo fmt -- apps/…/one_file.rs`, which formats
  the *whole workspace* — rewrote 133 files and ~11,000 lines of pure reflow on
  2026-10-07, none of it semantic and all of it poison for blame. Format new code by
  hand to match its neighbours; if a formatter has already run, `git checkout HEAD --`
  the churn-only files and re-apply the intended edits.
- `apps/tokenme-bar/src/styles/theme.css` declares the dark palette twice (the
  OS media query and the 深色 pin); the two blocks must stay mechanically
  identical. `scripts/check-theme-parity.py` gates drift — `build-macos.sh`
  runs it, and it takes an optional path argument to audit a candidate file. It
  also binds `index.html`'s `--boot-ink` / `--boot-accent` (the boot shell must
  paint before the bundle's CSS exists, so it carries a copy of `--ink-3` and
  `--accent`) to the real palette, and fails if the shell's marker comments go
  away — an unchecked copy is a colour flip on the panel's first real frame.
- `.bugx/` stays untracked (local tool state); never commit agent scratch files.
- Every published artifact gets a sha256 roundtrip before its link is reported.
- **`cargo test --workspace` never compiles the panel's Rust.** `apps/tokenme-bar`
  is excluded from the root workspace (root `Cargo.toml`, `exclude`) and its
  `src-tauri` carries its own `[workspace]`, so `cd apps/tokenme-bar/src-tauri && cargo test`
  too — the root run cannot count the panel's tests. `build-macos.sh` runs both
  sides as one gate; `.github/workflows/ci.yml` still runs only the workspace one
  (a runner would need WebKit's dev packages, and that has not been proven here —
  so the local build gate is the enforcement point, not CI). Don't restate how
  many tests there are; some are `#[ignore]`d on purpose (the server e2e needs a
  live sshd target) and only `cargo test --ignored` runs those.
- The panel links the adapters as path dependencies through its own target dir, and
  those rlibs go stale across `build-macos.sh` runs (builds 65–67 shipped an old
  Cline adapter this way). Before a release build:
  `cargo clean --release -p usage-index -p usage-adapter-cline -p usage-adapter-all`
  in `apps/tokenme-bar/src-tauri`, then confirm the bundled asset hash matches `dist/`.
  The same applies to `src-tauri/resources/collector/*` — those musl binaries ship
  inside the .app, so rebuild them (`scripts/build-linux.sh` +
  `scripts/bundle-collector.sh`) after any change under `crates/`, and prove it by
  hashing the copy *inside* `dist/tokenme-macos-arm64.app.zip`.
- Cold start is measurable from the log, not from opinion: `panel.log`'s `boot:` line
  breaks engine startup into `restore / detect / pricing / index open / has-events /
  sources / report`, and the two page milestones — `the page booted N ms after launch`
  (bundle ran) and `the page reported content N ms after launch` (figures drawn) — are
  the webview's side. Keep them apart: the seconds between them are the wait the user sees.
  There are now **three frames**, and the first is not a computation: the previous run's
  own `Report` is mirrored to `<data dir>/tokenme/report.json` on every publish and is
  published at the top of `engine::run` (3–22 ms measured on this machine) before `detect_all`, before the price
  table, before any query. It carries `from_previous_run`, the footer labels it, its quota
  windows are dropped (a vendor sample from an hour ago is not this run's answer), and the
  frozen-badge warning is suppressed while it is on screen. `snapshot::accept` is the whole
  honesty gate — same scope, same local day (`Window::key`), under an hour, clock not moved
  — and a restored report is never written back, so a snapshot cannot chain. Interleaved
  A/B on this machine: figures-on-screen went 6,691 / 1,572 / 4,682 ms → 1,284 / 905 / 1,126 ms.
  Do not "fix" the blank first frame by holding the window back until the page
  answers — measured here, an ordered-out window loads its page in 3.5–4.3 s instead
  of 1.1–1.6 s, so the gate causes the latency it was waiting for.
  And know which half markup cannot reach: from the window's first frame (~+1.4 s)
  until the page paints (~+3 s) every pixel is the native backing, so that stretch is
  covered by `panel::show_boot_note` (AppKit), not by `index.html`. The shell in
  `index.html` does paint, and is measured doing it — but only from the page's own
  first paint onward. Retire the native note on the *content* milestone, never on
  `boot`: doing that left the sheet bare from +3.2 s to +8.1 s in a recording.
  The note's AppKit calls run inside tao's `did_finish_launching` — a frame that
  cannot unwind — and `objc`'s `msg_send` does not verify selectors, so one wrong
  selector is an ObjC exception that aborts the whole app, logged only as "panic
  in a function that cannot unwind" with no origin and no class name (build 98
  died on first open because `NSProgressIndicator` got `setTag:`; `NSView.tag` is
  read-only and the setter belongs to `NSControl`). Verify selectors against a
  live instance (`osascript -l JavaScript` + `respondsToSelector`) before a build,
  and after any change here launch the bundle and prove it survives its own first
  open — the process must stay up and log `the page reported content`.
- **What the screen actually showed is `scripts/verify-cold-start.py`, and only that.**
  Four traps it encodes, each of which produced a wrong conclusion once: a
  `screencapture` still and a `screencapture -v` frame are encoded differently, so
  diffing video against a still baseline makes the whole desktop "change" (one run
  reported a full-screen panel at t=-0.8 s); the largest changed blob is not the
  panel (a ChatGPT window at 394×650 read as a blank 400×660 panel, so the panel is
  now found by its configured size *and* by hanging off the menu bar); behind a lock
  or a sleeping display the recorder writes nothing at all, which the script exits 2
  on rather than reporting "the panel never appeared"; and the quit path has to kill
  the app by every name the bundle can produce (`Contents/MacOS/*` plus the
  historical spellings) and refuse to record when one survives — a surviving instance
  turns the run into a measurement of a *warm* open, and it reported "first content:
  never" for exactly that reason. Positions it prints are points, not the physical
  pixels `panel.log` logs.
