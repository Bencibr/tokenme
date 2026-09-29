# AGENTS.md — how to build and release tokenme

Working notes for coding agents. Facts here were verified on 2026-09-27 while
shipping v0.1.0; re-check anything time-sensitive before relying on it.

## Repo map

- Panel (Tauri 2): `apps/tokenme-bar` — frontend in `src/`, Rust in `src-tauri/`.
- Usage engine: `crates/usage-quota` (one file per provider), `crates/usage-cli` (the `tokenme` CLI).
- Version lives in **three** homes and must move together:
  `apps/tokenme-bar/package.json`, `apps/tokenme-bar/src-tauri/tauri.conf.json`,
  `apps/tokenme-bar/src-tauri/Cargo.toml`. `scripts/release.sh x.y.z` bumps the
  latter two plus `latest.json`; package.json is manual.

## Windows build (this machine)

```sh
cd apps/tokenme-bar && pnpm tauri build --bundles nsis
```

`beforeBuildCommand` runs `tsc --noEmit && vite build` first. The artifact is
`apps/tokenme-bar/src-tauri/target/release/bundle/nsis/TokenMe_<v>_x64-setup.exe`.
`mainBinaryName: "tokenme"` makes the installed process `tokenme.exe` (installed
to `%LOCALAPPDATA%\tokenme`; the brand name stays `TokenMe` for the installer and
display). Quiet upgrade: kill `tokenme.exe` — and the legacy `tokenme-bar.exe` /
`TokenMe.exe` on installs older than 2026-09-28 — then run the installer with
`/S` — the NSIS uninstaller-first flow
removes the old binary, including a renamed one.

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
  No developer certificate by design — Gatekeeper clears with one right-click open.
- CI: `.github/workflows/release.yml` triggers on tags `v*` (macos-14), runs the
  same script and attaches `dist/tokenme-macos-arm64.app.zip`, the DMG,
  `sha256.txt` and `latest.json`. It fires when the GitHub repo lands
  (`src/lib/about.ts` centralizes the repo constants — edit there when it does).
- Gitea Actions is enabled on gitea.sp.dev but had **zero registered runners**
  as of 2026-09-27, so nothing can execute the workflow there; the macOS job
  also needs a runner labelled `macos-14`, which means real Apple hardware.
- Until then: build on a Mac, then attach the DMG to the Gitea release with the
  API call above and add its sha256 to the body.

## Conventions

- Conventional commits, one concern per commit; match the style of `git log`.
- `.bugx/` stays untracked (local tool state); never commit agent scratch files.
- Every published artifact gets a sha256 roundtrip before its link is reported.
