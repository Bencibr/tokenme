#!/usr/bin/env python3
"""Verify that the prepared assets still match their source snapshot."""
import hashlib
import json
from pathlib import Path

root = Path(__file__).resolve().parents[2]
manifest = json.loads((root / "PREPARED-INPUTS.json").read_text())
for name, expected in manifest["sha256"].items():
    path = root / name
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
        raise SystemExit(f"Prepared source input changed or missing: {name}")
for name in ("apps/tokenme-bar/dist/index.html",
             "apps/tokenme-bar/src-tauri/resources/collector/tokenme-x86_64",
             "apps/tokenme-bar/src-tauri/resources/collector/tokenme-aarch64"):
    if name not in manifest["sha256"]:
        raise SystemExit(f"Required prepared input is not recorded: {name}")
print(f"Verified {len(manifest['sha256'])} prepared inputs.")
