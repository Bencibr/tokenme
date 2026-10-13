#!/usr/bin/env python3
"""Prepare an unsigned Debian source upload from an isolated public source tree.

Run on Linux. Network access is used here, before the offline Debian build.
The supplied tree must contain only public sources. It is copied into a new
output directory; the checkout and installed application are not modified.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tomllib


def run(args, cwd, **kwargs):
    return subprocess.run(args, cwd=cwd, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-tree", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--series", default="resolute", choices=("resolute", "noble"))
    parser.add_argument("--revision", default="1ppa1")
    parser.add_argument("--maintainer", required=True, help="Name <email>, used in Debian metadata")
    parser.add_argument("--source-label", required=True, help="Upstream commit or explicit draft snapshot label")
    parser.add_argument("--prepared-collectors", type=Path,
                        help="Optional collectors built from this exact source; hashes are recorded")
    args = parser.parse_args()
    source = args.source_tree.resolve()
    output = args.output.resolve()
    if not source.is_dir() or output == source or source in output.parents:
        parser.error("Use an existing source tree and a separate output directory.")
    if output.exists():
        parser.error("Output directory must not exist; previous source uploads are immutable.")
    if not re.fullmatch(r"[0-9][A-Za-z0-9.+~]*", args.revision):
        parser.error("Invalid Debian revision.")
    if not re.fullmatch(r"[^<>\r\n]+ <[^<>\s]+@[^<>\s]+>", args.maintainer):
        parser.error("Maintainer must be Name <email>.")
    upstream = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", upstream):
        parser.error("Expected a numeric upstream version.")
    suite_version = {"resolute": "26.04", "noble": "24.04"}[args.series]
    debversion = f"{upstream}-{args.revision}~ubuntu{suite_version}.1"
    output.mkdir(parents=True)
    tree = output / f"token-me-{upstream}"

    def ignored(directory, names):
        directory = Path(directory)
        excluded = {".git", ".bugx", ".workbuddy-ai", "node_modules", "target", "dist",
                    "__pycache__", ".debian-target", ".debian-cargo", "AGENTS.md"}
        if directory == source / "docs":
            excluded.add("internal")
        if directory == source / "apps/tokenme-bar/src-tauri/resources":
            excluded.update(("collector", "cli", "diagnostics"))
        return [name for name in names if name in excluded or name.endswith((".log", ".pyc"))
                or (directory == source and name.startswith("."))]

    shutil.copytree(source, tree, ignore=ignored)
    run(["python3", "scripts/check-theme-parity.py"], tree)
    run(["python3", "scripts/check-i18n-parity.py"], tree)
    panel = tree / "apps/tokenme-bar"
    run(["pnpm", "install", "--frozen-lockfile"], panel)
    run(["pnpm", "build"], panel)
    # Preserve licenses for packages used to produce the frontend, including
    # native build tools. Their code is not needed by the Debian Rust build.
    licenses = tree / "vendor/frontend-licenses"
    for package in (panel / "node_modules/.pnpm").glob("*/node_modules/**/package.json"):
        relative = package.relative_to(panel / "node_modules/.pnpm")
        for license_file in package.parent.iterdir():
            if license_file.is_file() and license_file.name.lower().startswith(("license", "licence", "copying", "notice")):
                destination = licenses / relative.parent / license_file.name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(license_file, destination)
    shutil.rmtree(panel / "node_modules")
    collectors = panel / "src-tauri/resources/collector"
    if args.prepared_collectors:
        collectors.mkdir(parents=True)
        for arch in ("x86_64", "aarch64"):
            origin = args.prepared_collectors.resolve() / f"tokenme-{arch}"
            if origin.read_bytes()[:4] != b"\x7fELF":
                raise SystemExit(f"Collector is not ELF: {origin.name}")
            shutil.copy2(origin, collectors / origin.name)
            (collectors / origin.name).chmod(0o755)
    else:
        run(["bash", "scripts/build-linux.sh"], tree)
        run(["bash", "scripts/bundle-collector.sh"], tree)
    (tree / ".cargo").mkdir(exist_ok=True)
    with (tree / ".cargo/config.toml").open("w") as config:
        run(["cargo", "vendor", "--locked", "--versioned-dirs", "--respect-source-config",
             "--sync", "apps/tokenme-bar/src-tauri/Cargo.toml", "vendor/rust"], tree, stdout=config)
    dependencies = []
    for manifest in sorted((tree / "vendor/rust").glob("*/Cargo.toml")):
        package = tomllib.loads(manifest.read_text())["package"]
        dependencies.append({key: package.get(key) for key in ("name", "version", "license", "license-file")})
    # Generated build trees are not source inputs. Both Cargo projects will be
    # compiled from the vendored source on the Debian/Launchpad builder.
    for generated in (tree / "target", panel / "src-tauri/target", tree / "dist"):
        if generated.exists():
            shutil.rmtree(generated)
    sha256 = {}
    for path in sorted(tree.rglob("*")):
        if path.is_file():
            sha256[path.relative_to(tree).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    manifest = {"source_label": args.source_label,
                "build_contract": "Frontend assets and both static collectors are prepared before upload; native desktop and CLI are compiled offline by Debian rules.",
                "rust_dependencies": dependencies, "sha256": sha256}
    (tree / "PREPARED-INPUTS.json").write_text(json.dumps(manifest, indent=2) + "\n")
    original = output / f"token-me_{upstream}.orig.tar.xz"
    with tarfile.open(original, "w:xz") as archive:
        archive.add(tree, arcname=tree.name)
    shutil.copytree(tree / "packaging/ppa/debian", tree / "debian")
    (tree / "debian/rules").chmod(0o755)
    control = tree / "debian/control"
    control.write_text(control.read_text().replace("TokenMe contributors <packaging@example.com>", args.maintainer))
    date = datetime.datetime.now(datetime.timezone.utc).strftime("%a, %d %b %Y %H:%M:%S +0000")
    (tree / "debian/changelog").write_text(
        f"token-me ({debversion}) {args.series}; urgency=medium\n\n"
        f"  * Package TokenMe {upstream} for Ubuntu {suite_version}.\n\n"
        f" -- {args.maintainer}  {date}\n")
    run(["dpkg-buildpackage", "-S", "-sa", "-us", "-uc", "-d"], tree)
    print(f"Unsigned source upload prepared in {output}. Sign the .changes and .dsc before uploading.")


if __name__ == "__main__":
    main()
