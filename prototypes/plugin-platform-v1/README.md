# TokenMe plugin reference kit 0.1

A runnable Rust → Wasm component → validated package → isolated local install →
offline host execution workflow. This is a reference implementation for the
plugin contract, separate from the desktop application and its data profile.

Implemented capabilities: `quota.read` and `usage.collect`. The host serves
synthetic fixtures, enforces the manifest's task/resource grants, validates
results, deduplicates usage, produces a model summary, and writes metadata-only
diagnostic events. It does not contact a vendor, load real credentials, install
into TokenMe, or implement the production scheduler/UI/notification bridge.

## Toolchain

`toolchain.json` and `Cargo.lock` fix the reference versions:

| Tool | Version / requirement |
| --- | --- |
| Rust | 1.98.1 |
| Rust target | wasm32-wasip2 |
| Wasmtime / WASI library | 49.0.0, embedded by the reference host |
| wit-bindgen | 0.63.0, used by the guest build |
| Python | 3.11 or newer |
| Python jsonschema | 4.25.1 |

A native compiler/linker for the host is also required. Wasmtime CLI is not
required. Users of a finished plugin will not need this development toolchain.
The reference is checked on Windows x64; other operating systems need their own
build and runtime validation.

If the active Rust toolchain is not 1.98.1, install/select that version for this
directory. Do not change the workspace's global toolchain to satisfy this kit.

```powershell
rustup toolchain install 1.98.1 --profile minimal
rustup override set 1.98.1
rustup target add wasm32-wasip2
python -m pip install -r requirements-dev.txt
```

## Project structure

```text
Cargo.toml                         standalone workspace; no root workspace change
Cargo.lock                         exact Rust dependency graph
toolchain.json                     compiler, runtime, SDK and host API profile
requirements-dev.txt               Python checker dependency
wit/plugin.wit                     executable guest/host interface
schema/manifest.schema.json        normative alpha manifest structure
schema/result.schema.json          normative result structure
sdk/
  Cargo.toml
  src/lib.rs                       HostServices, task input/results and validation
reference-plugin/
  Cargo.toml                       cdylib / rlib; Wasm guest
  manifest.json                    identity, resources, permissions, task/settings
  src/lib.rs                       vendor parsers, capability dispatch, WIT binding
  i18n/en.json                     English resource keys
  i18n/zh-CN.json                   Chinese keys with matching placeholders
  assets/icon.svg                  static icon
  fixtures/happy.json              synthetic quota + two requests totaling 550 tokens
  fixtures/malformed-quota.json    invalid response with a non-exportable marker
  fixtures/scenarios.json          independent expected values, tasks and negative cases
  LICENSE
reference-host/
  Cargo.toml
  src/main.rs                      Wasmtime linker, grants, test ledger and diagnostics
tools/
  plugin.py                        real init/check/test/build/pack/inspect/install/run/verify
  tests/test_package.py            package/locale/tamper/path regression checks
  tests/test_audit.py              scaffold, provenance, install and evidence attacks
target/                            generated native host and Wasm component, ignored
dist/                              package and SHA-256, ignored
sandbox/                           local development installs only, ignored
artifacts/                         reports and JSONL diagnostics, ignored
```

The plugin author edits `reference-plugin/`: manifest, parser/dispatch code,
resources, fixtures and its dependency declarations. The SDK and host implement
the contract. A new tool identity does not require a table entry in the host.

## One command to verify the reference

From this directory, after installing the prerequisites:

```powershell
python tools/plugin.py verify
```

This runs package/schema/resource tests, native Rust contract tests, final Wasm
build and actual component execution. It packs twice and compares bytes,
installs into `sandbox/`, replays usage, reads quota, and checks denied access,
malformed parsing, secret-marker exclusion, disabled execution and fuel limits.
It also installs a previously unseen plugin/tool ID without rebuilding the host,
and checks that local source/dependency paths are remapped out of the component.
Expected values come from `reference-plugin/fixtures/scenarios.json`, not from
the observed output. Each registered task needs a successful scenario. Cases
select fixtures and task IDs, assert JSON pointers, and can require the absence
of secret markers or host events. Update these independent expectations when
changing the parser, task names or fixture data. A passing author scenario suite
does not establish coverage of arbitrary hostile components or real vendors.

Expected evidence:

```text
artifacts/verification.json                  latest attempt, including failures/interruption
artifacts/runs/<runId>/verification.json     complete report for that attempt
artifacts/runs/<runId>/collect-usage-replay.json     550 tokens; 2 occurrences; 2 duplicates
artifacts/runs/<runId>/read-quota-happy.json         used_percent=25
artifacts/runs/<runId>/read-quota-denied.json        PERMISSION.DENIED
artifacts/runs/<runId>/read-quota-malformed.json     PARSE.SCHEMA_MISMATCH
artifacts/runs/<runId>/read-quota-disabled.json      gated; no http.begin in its JSONL
artifacts/runs/<runId>/read-quota-fuel.json          RUNTIME.BUDGET_EXCEEDED
artifacts/runs/<runId>/collect-usage-unseen-id.json  new ID; unchanged host
dist/org.example.tokenme.reference_0.1.0.tokenme-plugin
dist/org.example.tokenme.reference_0.1.0.tokenme-plugin.sha256
```

Each scenario has a separate JSON report and JSONL trace. Native unit tests
alone do not establish that the Wasm artifact executes successfully.
The latest report is changed to `running` before checks start, and to `failed`
or `interrupted` when an attempt fails. A killed process can leave `running`;
that is never a pass. Previous run directories are retained. Evidence is valid
only for its recorded source/package/host/module hashes. Manual `run` output
uses a new directory under `artifacts/manual/` for each invocation.

## Individual commands

```powershell
python tools/plugin.py check
python tools/plugin.py test
python tools/plugin.py build
python tools/plugin.py pack
python tools/plugin.py inspect dist/org.example.tokenme.reference_0.1.0.tokenme-plugin
python tools/plugin.py install dist/org.example.tokenme.reference_0.1.0.tokenme-plugin --allow-unsigned-local
python tools/plugin.py run org.example.tokenme.reference --task collect-usage --repeat 2
python tools/plugin.py run org.example.tokenme.reference --task read-quota
```

`check` validates schema, links, permissions, task/settings references, locale
parity, scenarios and a conservative icon profile. `inspect` validates ZIP paths, file
allowlist, integrity coverage, module header and resources without running
plugin code. Full ABI linking and execution are established by `verify`/`run`.
`build` writes `target/build-receipt.json` binding the source tree to the native
host and guest hashes. `pack` rejects missing receipts, changed sources and
changed binaries. Run `build` again after editing source, resources, scenarios,
schemas, tooling, dependencies or this README. Repeated packing checks stable
archive bytes for one built component; it does not prove independent hermetic
builds. These local hashes are consistency evidence, not signed attestations.

The host's `http` and `read-source` services return fixture data. A URL in the
manifest is metadata here, not permission to perform real network access.

## Create a separate plugin project

```powershell
python tools/plugin.py init sandbox/new-ide --id org.example.community.new-ide
Set-Location sandbox/new-ide
python tools/plugin.py check
```

The destination must not exist. It receives this complete standalone workspace,
SDK, host, schemas, tools and a manifest with the new identity. Edit the vendor
parser and fixtures, then run `python tools/plugin.py verify` from that new
directory. The reference crate names may stay unchanged; the manifest defines
the installed plugin/tool identity. Change `tool.id` and the localized name when
creating a different tool.
Keep `fixtures/scenarios.json` consistent with any renamed tasks and new fixture
values. The tool has no fixed tool ID, task name or Token/quota expectation.

## Package format and install semantics

The `.tokenme-plugin` is a ZIP_STORED archive with sorted canonical paths, fixed
timestamps and regular files only. Its exact contents are:

```text
manifest.json
dist/plugin.wasm
assets/icon.svg
i18n/en.json
i18n/zh-CN.json
LICENSE
build-info.json
checksums.json
signature.json
```

`checksums.json` enumerates each payload file's size and SHA-256, excluding itself
and the signature envelope. It proves consistency, not publisher identity. This
kit writes `signature.json` with `state=unsigned-local`; installation therefore
requires the explicit local flag. Publisher signing and production trust policy
are outside this reference implementation.

Development installs are immutable by package hash:
`sandbox/plugins/<id>/<version>/<package-sha256>/`. `active.json` atomically
selects the version/hash. Different builds of a development version can coexist
without overwriting old files. The production registry should bind one released
version to one package digest. There is no command in this kit that deletes the
real application profile or existing history.
Only canonical ZIP_STORED bytes are accepted, so validation, installation and
run-time reconstruction use the same digest. Windows reparse points, symbolic
links and file hardlinks are rejected in the sandbox paths used for I/O. Each
plugin activation uses an exclusive lock and a unique temporary file. An
`INSTALL.BUSY` from an abandoned lock requires confirming no installer is active
before removing that plugin's `.install.lock`; no automatic lock stealing occurs.
The portable tool is for a private development sandbox. A hostile process with
the same OS user's access can still race path checks; production needs native
handle-based traversal and its own trust/transaction implementation.

## Contract boundaries

`execute(task-id, input-json)` returns a JSON result matching the result schema.
The SDK validates before returning; the host independently validates before
committing. Tokens use four disjoint stages; quota does not manufacture tokens.
The reference ledger is scoped to one synthetic tool/account. Production must
add account/source/device ownership and the existing index transaction rules.

The Rust standard library imports WASI support. The host supplies a restricted
context: no inherited environment, stdio or preopened directories, and TCP/UDP/
DNS are disabled. External data is supplied only through the custom WIT host
services. This avoids treating standard-library linkage as ambient permission.

Each guest linear memory is limited to 64 MiB and call fuel to 10 million units;
the selected task's `timeoutMs` (maximum 30 seconds) limits instantiation and
each guest call through Wasmtime epoch interruption with 10 ms resolution.
The supervising Python process has a separate 30-second limit for the entire
native process, including compilation. A process timeout kills the child and
marks verification failed; the terminated child may not have written a task log.
Epoch interruption does not cancel arbitrary blocking native I/O. These do not establish
production-wide JIT/host memory budgets, all native crash behavior, arbitrary
vendor I/O cancellation or cross-platform sandbox acceptance.
The reference does not implement an aggregate Store memory/table/instance
budget. Production must enforce totals across all guest memories and tables;
Wasmtime's per-memory limit alone does not provide that guarantee.

## Extending the reference

Add a capability by specifying its schema, WIT imports/exports, SDK type,
manifest registration, implementation, fixtures, negative tests and host result
acceptance. Do not silently register unsupported capabilities: this alpha schema
rejects them. Check-in, real authentication, SQLite, estimates, price strategies,
UI rendering, notifications and device sync need separate implementation units
before they can be declared supported.
