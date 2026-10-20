"""Local, executable alpha SDK workflow. Never installs into the real TokenMe profile."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import importlib.metadata
import io
import json
import math
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import time
import uuid
import zipfile

from jsonschema import Draft202012Validator

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "reference-plugin"
MAX_FILE = 32 * 1024 * 1024
MAX_PACKAGE = 64 * 1024 * 1024
MAX_FILES = 64
MAX_JSON = 1024 * 1024
COMPONENT_HEADER = b"\0asm\x0d\0\x01\0"
SANDBOX = ROOT / "sandbox"


class KitError(Exception):
    def __init__(self, code: str, detail: str = ""):
        self.code, self.detail = code, detail
        super().__init__(code + (": " + detail if detail else ""))


def require(condition, code, detail=""):
    if not condition:
        raise KitError(code, detail)


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode("utf-8")


def decoded(data):
    require(isinstance(data, (str, bytes, bytearray)) and len(data) <= MAX_JSON, "JSON.SIZE")
    def pairs(entries):
        result = {}
        for key, value in entries:
            require(key not in result, "JSON.DUPLICATE_KEY")
            result[key] = value
        return result
    def reject_constant(value):
        raise KitError("JSON.NONFINITE")
    try:
        if not isinstance(data,str): data = data.decode("utf-8")
        value = json.loads(data, object_pairs_hook=pairs, parse_constant=reject_constant)
        pending = [(value, 0)]
        while pending:
            item, depth = pending.pop()
            require(depth <= 64, "JSON.DEPTH")
            require(not isinstance(item,float) or math.isfinite(item), "JSON.NONFINITE")
            if isinstance(item,dict): pending.extend((child,depth+1) for child in item.values())
            elif isinstance(item,list): pending.extend((child,depth+1) for child in item)
        return value
    except (ValueError, UnicodeError, RecursionError) as error:
        raise KitError("JSON.INVALID") from error


def local_path(path, root, code="INSTALL.PATH"):
    """Reject existing symlinks, Windows reparse points and file hardlinks before I/O.

    This portable development tool does not defend against a hostile process of
    the same OS user changing directory handles concurrently. Production uses
    handle-relative opens in an independently locked application profile.
    """
    path, root = Path(os.path.abspath(path)), Path(os.path.abspath(root))
    require(path.is_relative_to(root), code)
    current = root
    for part in (None, *path.relative_to(root).parts):
        if part is not None: current = current / part
        try: info = current.lstat()
        except FileNotFoundError: continue
        require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400, code)
        require(not stat.S_ISREG(info.st_mode) or info.st_nlink == 1, code)
    require(path.resolve().is_relative_to(root.resolve()), code)
    return path


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex + ".tmp")
    with temporary.open("xb") as stream:
        stream.write(encoded(value)); stream.flush(); os.fsync(stream.fileno())
    os.replace(temporary, path)


@contextmanager
def installation_lock(plugin):
    local_path(plugin, SANDBOX)
    plugin.mkdir(parents=True, exist_ok=True)
    lock = plugin / ".install.lock"
    try:
        stream = lock.open("xb")
    except FileExistsError as error:
        raise KitError("INSTALL.BUSY") from error
    try:
        with stream: stream.write(encoded({"pid":os.getpid()}))
        yield
    finally:
        lock.unlink()


def path_name(name):
    require(isinstance(name, str) and len(name) <= 160, "PACKAGE.PATH")
    require("\\" not in name and ":" not in name and not name.startswith("/"), "PACKAGE.PATH")
    parts = name.split("/")
    require(all(part not in ("", ".", "..") and not part.endswith((".", " ")) for part in parts), "PACKAGE.PATH")
    require(all(re.fullmatch(r"[A-Za-z0-9_.-]+", part) for part in parts), "PACKAGE.PATH")
    reserved = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)), *(f"LPT{i}" for i in range(1, 10))}
    require(all(part.split(".")[0].upper() not in reserved for part in parts), "PACKAGE.PATH")
    return PurePosixPath(name)


def schema_check(name, value):
    schema = decoded((ROOT / "schema" / name).read_bytes())
    Draft202012Validator.check_schema(schema)
    errors = list(Draft202012Validator(schema).iter_errors(value))
    if errors:
        raise KitError("SCHEMA.INVALID", "/".join(map(str, errors[0].absolute_path)))


def validate_manifest(manifest, files):
    schema_check("manifest.schema.json", manifest)
    resources = [manifest["tool"]["icon"], *manifest["locales"].values()]
    for resource in resources:
        path_name(resource)
        require(resource in files, "RESOURCE.MISSING", resource)
    require(manifest["defaultLocale"] in manifest["locales"], "LOCALE.DEFAULT")
    dictionaries = {key: decoded(files[path]) for key, path in manifest["locales"].items()}
    default = dictionaries[manifest["defaultLocale"]]
    require(isinstance(default, dict) and all(isinstance(v, str) for v in default.values()), "LOCALE.TYPE")
    for dictionary in dictionaries.values():
        require(isinstance(dictionary, dict) and dictionary.keys() == default.keys(), "LOCALE.KEYS")
        for key in default:
            require(isinstance(dictionary[key], str), "LOCALE.TYPE")
            require(set(re.findall(r"\{(\w+)\}", dictionary[key])) == set(re.findall(r"\{(\w+)\}", default[key])), "LOCALE.PLACEHOLDERS", key)
    references = [manifest["tool"]["nameKey"], *(s["labelKey"] for s in manifest["settings"]), *(e["messageKey"] for e in manifest["diagnostics"]["events"])]
    require(set(references) == set(default), "LOCALE.REFERENCES")
    for event in manifest["diagnostics"]["events"]:
        require(set(re.findall(r"\{(\w+)\}", default[event["messageKey"]])) == set(event["fields"]), "LOG.TEMPLATE_FIELDS")
    ids = {}
    for collection, key in (("tasks", "id"), ("settings", "key"), ("endpoints", "id"), ("sources", "id"), ("diagnostics", "code")):
        entries = [v[key] for v in (manifest[collection]["events"] if collection == "diagnostics" else manifest[collection])]
        require(len(entries) == len(set(entries)), "MANIFEST.DUPLICATE_ID", collection)
        ids[collection] = set(entries)
    for category in ("endpoints", "sources"):
        require(set(manifest["permissions"][category]) <= ids[category], "PERMISSION.UNKNOWN_RESOURCE")
    for task in manifest["tasks"]:
        require(task["enabledSetting"] in ids["settings"], "TASK.SETTING")
        for category in ("endpoints", "sources"):
            require(set(task[category]) <= set(manifest["permissions"][category]), "TASK.PERMISSION")
        require((task["capability"] == "quota.read" and task["effect"] == "remote-read") or (task["capability"] == "usage.collect" and task["effect"] == "local-read"), "TASK.EFFECT")
    # The reference permits only simple, self-contained shape SVGs.
    import xml.etree.ElementTree as ET
    icon_bytes = files[manifest["tool"]["icon"]]
    require(len(icon_bytes) <= 128 * 1024, "ICON.LIMIT")
    try:
        icon_text = icon_bytes.decode("utf-8")
        require("<!doctype" not in icon_text.lower() and "<!entity" not in icon_text.lower(), "ICON.ACTIVE_CONTENT")
        icon = ET.fromstring(icon_text)
    except (ET.ParseError, UnicodeError) as error:
        raise KitError("ICON.INVALID") from error
    allowed = {"svg", "rect", "path", "circle", "g"}
    require(icon.tag in ("svg", "{http://www.w3.org/2000/svg}svg"), "ICON.ACTIVE_CONTENT")
    require(sum(1 for _ in icon.iter()) <= 2048, "ICON.LIMIT")
    attributes = {"viewBox", "width", "height", "x", "y", "rx", "ry", "cx", "cy", "r", "d", "fill", "stroke", "stroke-width", "stroke-linecap", "stroke-linejoin", "fill-rule", "clip-rule", "opacity", "fill-opacity", "stroke-opacity", "transform"}
    for element in icon.iter():
        require(element.tag in allowed or (element.tag.startswith("{http://www.w3.org/2000/svg}") and element.tag.rsplit("}", 1)[-1] in allowed), "ICON.ACTIVE_CONTENT")
        require(not (element.text or "").strip() and not (element.tail or "").strip(), "ICON.ACTIVE_CONTENT")
        for key, value in element.attrib.items():
            require(key in attributes and len(value) <= 8192 and "url(" not in value.lower(), "ICON.ACTIVE_CONTENT")
    return manifest


def project_files(project=PROJECT):
    require(project.is_dir(), "PROJECT.INVALID")
    local_path(project, project, "PROJECT.PATH")
    manifest = decoded(local_path(project / "manifest.json", project, "PROJECT.PATH").read_bytes())
    schema_check("manifest.schema.json", manifest)
    names = ["manifest.json", "LICENSE", manifest["tool"]["icon"], *manifest["locales"].values()]
    files = {}
    for name in names:
        relative = path_name(name)
        path = project.joinpath(*relative.parts)
        local_path(path, project, "PROJECT.PATH")
        require(path.is_file() and path.stat().st_size <= MAX_FILE, "RESOURCE.MISSING", name)
        files[name] = path.read_bytes()
    validate_manifest(manifest, files)
    return manifest, files


def process(arguments, *, capture=False, timeout=300, environment=None):
    flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
    try:
        child_environment = os.environ.copy()
        if environment: child_environment.update(environment)
        result = subprocess.run(arguments, cwd=ROOT, text=True, encoding="utf-8", errors="replace", capture_output=capture, timeout=timeout, creationflags=flags, env=child_environment)
    except subprocess.TimeoutExpired as error:
        raise KitError("PROCESS.TIMEOUT", Path(arguments[0]).name) from error
    if not capture:
        require(result.returncode == 0, "PROCESS.FAILED", str(arguments[0]))
    return result


def environment_check():
    lock = decoded((ROOT / "toolchain.json").read_bytes())
    require(sys.version_info >= (3, 11), "TOOLCHAIN.PYTHON_VERSION")
    dependencies = {}
    for line in (ROOT / "requirements-dev.txt").read_text().splitlines():
        if not line or line.startswith("#"): continue
        name, version = line.split("==")
        actual = importlib.metadata.version(name)
        require(actual == version, "TOOLCHAIN.PYTHON_DEPENDENCY", name)
        dependencies[name] = actual
    result = process(["rustc", "--version"], capture=True)
    require(result.returncode == 0 and result.stdout.split()[1] == lock["rustc"], "TOOLCHAIN.RUST_VERSION")
    targets = process(["rustup", "target", "list", "--installed"], capture=True)
    require(lock["target"] in targets.stdout.splitlines(), "TOOLCHAIN.WASM_TARGET")
    return {"rustc": result.stdout.strip(), "python": sys.version.split()[0], "pythonDependencies": dependencies}


def build():
    atomic_json(ROOT / "target/build-receipt.json", {"status":"building"})
    versions = environment_check()
    project_files()
    fingerprint = source_fingerprint()
    process(["cargo", "build", "--locked", "--target-dir", str(ROOT / "target"), "-p", "tokenme-plugin-reference-host"])
    cargo_home = Path(os.environ.get("CARGO_HOME",Path.home()/".cargo")).resolve()
    sysroot_result = process(["rustc","--print","sysroot"],capture=True)
    require(sysroot_result.returncode==0,"TOOLCHAIN.SYSROOT")
    paths = [(ROOT,"/plugin"),(cargo_home,"/rust-deps"),(Path(sysroot_result.stdout.strip()).resolve(),"/rust-sysroot")]
    flags = []
    for path, replacement in paths:
        for prefix in sorted({str(path),path.as_posix()}):
            flags.append(f"--remap-path-prefix={prefix}={replacement}")
    process(["cargo", "build", "--locked", "--target-dir", str(ROOT / "target"), "--release", "--target", "wasm32-wasip2", "-p", "tokenme-reference-plugin"],environment={"CARGO_ENCODED_RUSTFLAGS":"\x1f".join(flags)})
    require(fingerprint == source_fingerprint(), "BUILD.SOURCE_CHANGED")
    atomic_json(ROOT / "target/build-receipt.json", {"schema":"tokenme.build-receipt/0.1", "status":"passed", "sourceTreeSha256":fingerprint, "moduleSha256":hashlib.sha256(module_path().read_bytes()).hexdigest(), "hostSha256":hashlib.sha256(host_path().read_bytes()).hexdigest(), "versions":versions})


def module_path():
    return ROOT / "target/wasm32-wasip2/release/tokenme_reference_plugin.wasm"


def host_path():
    return ROOT / "target/debug" / ("tokenme-plugin-reference-host.exe" if os.name == "nt" else "tokenme-plugin-reference-host")


def build_receipt(*, current_source=False):
    receipt_path = ROOT / "target/build-receipt.json"
    require(receipt_path.is_file(), "BUILD.PROVENANCE_REQUIRED")
    receipt = decoded(receipt_path.read_bytes())
    require(isinstance(receipt, dict) and receipt.get("schema") == "tokenme.build-receipt/0.1" and receipt.get("status") == "passed", "BUILD.PROVENANCE_REQUIRED")
    require(host_path().is_file() and receipt.get("hostSha256") == hashlib.sha256(host_path().read_bytes()).hexdigest(), "BUILD.HOST_CHANGED")
    if current_source:
        require(receipt.get("sourceTreeSha256") == source_fingerprint(), "BUILD.STALE_SOURCE")
        require(module_path().is_file() and receipt.get("moduleSha256") == hashlib.sha256(module_path().read_bytes()).hexdigest(), "BUILD.MODULE_CHANGED")
    return receipt


def source_fingerprint():
    digest = hashlib.sha256()
    paths = [ROOT / name for name in ("Cargo.toml", "Cargo.lock", "toolchain.json", "requirements-dev.txt", "README.md", ".gitignore")]
    for directory in ("sdk", "reference-plugin", "reference-host", "wit", "schema", "tools"):
        paths += [p for p in (ROOT / directory).rglob("*") if p.is_file() and "__pycache__" not in p.parts]
    for path in sorted(paths):
        digest.update(path.relative_to(ROOT).as_posix().encode() + b"\0" + path.read_bytes() + b"\0")
    return digest.hexdigest()


def archive_bytes(files):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_STORED) as archive:
        for name, data in sorted(files.items()):
            path_name(name)
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | 0o644) << 16
            archive.writestr(info, data)
    return buffer.getvalue()


def pack():
    manifest, files = project_files()
    receipt = build_receipt(current_source=True)
    module = module_path()
    require(module.is_file(), "BUILD.REQUIRED")
    files[manifest["runtime"]["entrypoint"]] = module.read_bytes()
    require(files[manifest["runtime"]["entrypoint"]].startswith(COMPONENT_HEADER), "MODULE.NOT_COMPONENT")
    files["build-info.json"] = encoded({"schema":"tokenme.build/0.1","toolchain":decoded((ROOT/"toolchain.json").read_bytes()),"sourceTreeSha256":receipt["sourceTreeSha256"],"moduleSha256":receipt["moduleSha256"],"sdk":"tokenme-plugin-sdk-reference","sdkVersion":"0.1.0"})
    files["checksums.json"] = encoded({"schema":"tokenme.package-integrity/0.1","files":{name:{"sha256":hashlib.sha256(data).hexdigest(),"size":len(data)} for name,data in sorted(files.items())}})
    files["signature.json"] = encoded({"state":"unsigned-local","schema":"tokenme.package-signature/0.1"})
    data = archive_bytes(files)
    validate_package_bytes(data)
    output = ROOT / "dist" / f'{manifest["id"]}_{manifest["version"]}.tokenme-plugin'
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    output.with_suffix(output.suffix + ".sha256").write_text(hashlib.sha256(data).hexdigest()+"\n",encoding="ascii")
    return output


def validate_package_bytes(data):
    require(len(data) <= MAX_PACKAGE, "PACKAGE.SIZE")
    files, seen = {}, set()
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            require(len(archive.infolist()) <= MAX_FILES, "PACKAGE.FILE_COUNT")
            total = 0
            for info in archive.infolist():
                path_name(info.filename)
                folded = info.filename.casefold()
                require(folded not in seen, "PACKAGE.DUPLICATE_PATH")
                seen.add(folded)
                require(not info.is_dir() and not stat.S_ISLNK(info.external_attr >> 16), "PACKAGE.FILE_TYPE")
                require(info.compress_type == zipfile.ZIP_STORED, "PACKAGE.NONCANONICAL")
                total += info.file_size
                require(info.file_size <= MAX_FILE and total <= MAX_PACKAGE and not info.flag_bits & 1, "PACKAGE.SIZE")
                files[info.filename] = archive.read(info)
    except (zipfile.BadZipFile, RuntimeError, OSError) as error:
        raise KitError("PACKAGE.INVALID_ZIP") from error
    require({"manifest.json","checksums.json","signature.json","build-info.json","LICENSE"} <= files.keys(), "PACKAGE.MISSING_FILE")
    integrity = decoded(files["checksums.json"])
    require(isinstance(integrity,dict) and set(integrity) == {"schema","files"} and integrity.get("schema") == "tokenme.package-integrity/0.1" and isinstance(integrity.get("files"),dict), "PACKAGE.INTEGRITY_SCHEMA")
    require(set(integrity["files"]) == set(files)-{"checksums.json","signature.json"}, "PACKAGE.INTEGRITY_COVERAGE")
    for name, expected in integrity["files"].items():
        require(isinstance(expected,dict) and type(expected.get("size")) is int and expected == {"sha256":hashlib.sha256(files[name]).hexdigest(),"size":len(files[name])}, "PACKAGE.HASH_MISMATCH")
    manifest = validate_manifest(decoded(files["manifest.json"]), files)
    module = files.get(manifest["runtime"]["entrypoint"], b"")
    require(module.startswith(COMPONENT_HEADER), "MODULE.NOT_COMPONENT")
    signature = decoded(files["signature.json"])
    require(signature == {"schema":"tokenme.package-signature/0.1","state":"unsigned-local"}, "SIGNATURE.UNSUPPORTED")
    allowed = {"manifest.json","checksums.json","signature.json","build-info.json","LICENSE",manifest["runtime"]["entrypoint"],manifest["tool"]["icon"],*manifest["locales"].values()}
    require(set(files) == allowed, "PACKAGE.UNEXPECTED_FILE")
    info = decoded(files["build-info.json"])
    schema_check("build-info.schema.json", info)
    require(info["moduleSha256"] == hashlib.sha256(module).hexdigest(), "BUILD.MODULE_CHANGED")
    require(data == archive_bytes(files), "PACKAGE.NONCANONICAL")
    return manifest, files


def read_package(path):
    require(path.is_file() and path.stat().st_size <= MAX_PACKAGE, "PACKAGE.SIZE")
    return validate_package_bytes(path.read_bytes())


def install(path, allow_unsigned=False):
    require(path.is_file() and path.stat().st_size <= MAX_PACKAGE, "PACKAGE.SIZE")
    data = path.read_bytes()
    manifest, files = validate_package_bytes(data)
    require(allow_unsigned, "SIGNATURE.LOCAL_CONSENT_REQUIRED")
    local_path(SANDBOX, SANDBOX)
    SANDBOX.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256(data).hexdigest()
    plugin = SANDBOX / "plugins" / manifest["id"]
    destination = plugin / manifest["version"] / digest
    with installation_lock(plugin):
        local_path(destination, SANDBOX)
        if destination.exists():
            existing = installed_files(destination)
            require(existing == files, "INSTALL.IMMUTABLE_VERSION")
        else:
            stage = SANDBOX / (".staging-"+uuid.uuid4().hex)
            stage.mkdir()
            for name, content in files.items():
                target = local_path(stage.joinpath(*path_name(name).parts), SANDBOX)
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open("xb") as stream: stream.write(content)
            destination.parent.mkdir(parents=True, exist_ok=True)
            stage.rename(destination)
        active = local_path(plugin / "active.json", SANDBOX)
        atomic_json(active, {"version":manifest["version"],"packageSha256":digest})
    return destination


def installed_files(installed):
    files, total, visited = {}, 0, 0
    for directory, directories, names in os.walk(local_path(installed, SANDBOX), followlinks=False):
        visited += len(directories) + len(names)
        require(visited <= MAX_FILES * 2, "PACKAGE.FILE_COUNT")
        for name in directories: local_path(Path(directory) / name, SANDBOX)
        for name in names:
            path = local_path(Path(directory) / name, SANDBOX)
            size = path.stat().st_size
            total += size
            require(path.is_file() and size <= MAX_FILE and total <= MAX_PACKAGE, "PACKAGE.SIZE")
            require(len(files) < MAX_FILES, "PACKAGE.FILE_COUNT")
            relative = path.relative_to(installed).as_posix()
            path_name(relative)
            files[relative] = path.read_bytes()
    return files


def run_installed(plugin_id, task, *, fixture=None, repeat=1, extra=(), expected=0, case="manual", artifacts=None):
    require(len(plugin_id) <= 96 and re.fullmatch(r"[a-z][a-z0-9]*(?:\.[a-z][a-z0-9-]*){2,}",plugin_id), "PLUGIN.ID")
    base = SANDBOX / "plugins" / plugin_id
    active = decoded(local_path(base / "active.json", SANDBOX).read_bytes())
    require(isinstance(active,dict) and set(active) == {"version","packageSha256"}, "PLUGIN.ACTIVE_SCHEMA")
    require(isinstance(active["version"],str) and len(active["version"]) <= 48 and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?",active["version"]), "PLUGIN.VERSION")
    require(isinstance(active["packageSha256"],str) and re.fullmatch(r"[0-9a-f]{64}",active["packageSha256"]), "PLUGIN.DIGEST")
    installed = base/active["version"]/active["packageSha256"]
    require(local_path(installed, SANDBOX).is_dir(), "INSTALL.MISSING")
    files = installed_files(installed)
    data = archive_bytes(files)
    manifest,_ = validate_package_bytes(data)
    require(hashlib.sha256(data).hexdigest()==active["packageSha256"], "INSTALL.CHANGED")
    require(manifest["id"] == plugin_id and manifest["version"] == active["version"], "INSTALL.IDENTITY")
    host = host_path()
    require(host.is_file(), "BUILD.REQUIRED")
    build_receipt()
    require(re.fullmatch(r"[a-z0-9-]{1,48}",case) and re.fullmatch(r"[a-z][a-z0-9_-]{0,63}",task), "RUN.CASE")
    task_info = next((value for value in manifest["tasks"] if value["id"] == task), None)
    require(task_info is not None and 1 <= repeat <= 10, "TASK.NOT_REGISTERED")
    artifacts = artifacts or ROOT / "artifacts/manual" / uuid.uuid4().hex
    artifacts.mkdir(parents=True, exist_ok=True)
    log = artifacts/(task+"-"+case+".jsonl")
    output = artifacts/(task+"-"+case+".json")
    result = process([str(host),"--module",str(installed/manifest["runtime"]["entrypoint"]),"--manifest",str(installed/"manifest.json"),"--fixtures",str(fixture or PROJECT/"fixtures/happy.json"),"--task",task,"--repeat",str(repeat),"--log",str(log),"--output",str(output),*extra],capture=True,timeout=30)
    require(result.returncode == expected, "RUN.EXIT")
    report = decoded(result.stdout)
    for value in report.get("results",[]):
        schema_check("result.schema.json",value)
    return report


def init_project(destination, plugin_id):
    require(not destination.exists(), "INIT.EXISTS")
    require(re.fullmatch(r"[a-z][a-z0-9]*(?:\.[a-z][a-z0-9-]*){2,}",plugin_id) and len(plugin_id)<=96, "PLUGIN.ID")
    roots = ["sdk","reference-plugin","reference-host","wit","schema","tools"]
    originals = [ROOT/name for name in ("Cargo.toml","Cargo.lock","toolchain.json","requirements-dev.txt","README.md",".gitignore")]
    for name in roots:
        originals += [p for p in (ROOT/name).rglob("*") if p.is_file() and "__pycache__" not in p.parts]
    destination.mkdir(parents=True)
    for source in originals:
        require(not source.is_symlink(), "INIT.SYMLINK")
        target = destination/source.relative_to(ROOT)
        target.parent.mkdir(parents=True,exist_ok=True)
        shutil.copyfile(source,target)
    manifest_path = destination/"reference-plugin/manifest.json"
    manifest = decoded(manifest_path.read_bytes()); manifest["id"] = plugin_id
    manifest_path.write_bytes(encoded(manifest))
    return destination


def scenarios():
    value = decoded(local_path(PROJECT / "fixtures/scenarios.json", PROJECT, "PROJECT.PATH").read_bytes())
    schema_check("scenarios.schema.json", value)
    cases = value["cases"]
    require(len({case["id"] for case in cases}) == len(cases), "VERIFY.DUPLICATE_CASE")
    manifest, _ = project_files()
    tasks = {task["id"] for task in manifest["tasks"]}
    require({case["task"] for case in cases} <= tasks, "VERIFY.UNKNOWN_TASK")
    positive = {case["task"] for case in cases if case["exitCode"] == 0 and case["assertions"].get("/status") == "ok"}
    require(positive == tasks, "VERIFY.POSITIVE_CASE_REQUIRED")
    for case in cases:
        path = local_path(PROJECT / case["fixture"], PROJECT, "PROJECT.PATH")
        require(path.is_file() and path.stat().st_size <= MAX_JSON, "FIXTURE.SIZE")
        decoded(path.read_bytes())
    return cases


def pointer_value(value, pointer):
    try:
        for part in pointer[1:].split("/"):
            require(re.search(r"~(?![01])", part) is None, "VERIFY.POINTER")
            part = part.replace("~1", "/").replace("~0", "~")
            if isinstance(value, list):
                require(re.fullmatch(r"0|[1-9][0-9]*", part), "VERIFY.POINTER")
                value = value[int(part)]
            else: value = value[part]
        return value
    except (KeyError, IndexError, TypeError, ValueError) as error:
        raise KitError("VERIFY.POINTER") from error


def scenario_run(plugin_id, case, artifacts, *, identity_case=None):
    extra = []
    if case.get("disabled"): extra.append("--disabled")
    if case.get("denyHttp"): extra.append("--deny-http")
    if "fuel" in case: extra += ["--fuel", str(case["fuel"])]
    case_id = identity_case or case["id"]
    report = run_installed(plugin_id, case["task"], fixture=PROJECT / case["fixture"], repeat=case.get("repeat",1), extra=extra, expected=case["exitCode"], case=case_id, artifacts=artifacts)
    for pointer, expected in case["assertions"].items():
        actual = pointer_value(report, pointer)
        require(json_equal(actual, expected), "VERIFY.ASSERTION", case_id + pointer)
    log = artifacts / (case["task"] + "-" + case_id + ".jsonl")
    log_bytes = log.read_bytes()
    require(len(log_bytes) <= 8 * 1024 * 1024, "VERIFY.LOG_LIMIT")
    events = [decoded(line) for line in log_bytes.splitlines() if line]
    require(bool(events) and all(event.get("trace_id") == report.get("trace_id") and event.get("plugin_id") == plugin_id and event.get("task_id") == case["task"] and event.get("sequence") == number for number,event in enumerate(events,1)), "VERIFY.TRACE")
    for marker in case.get("forbiddenLogText",[]):
        require(marker.encode() not in log_bytes, "VERIFY.SECRET")
    for event in case.get("absentEvents",[]):
        require(all(entry.get("event") != event for entry in events), "VERIFY.GATE")
    return report


def json_equal(actual, expected):
    if isinstance(actual, dict) and isinstance(expected, dict):
        return actual.keys() == expected.keys() and all(json_equal(actual[key], expected[key]) for key in actual)
    if isinstance(actual,list) and isinstance(expected,list):
        return len(actual) == len(expected) and all(json_equal(left,right) for left,right in zip(actual,expected))
    if isinstance(actual,bool) or isinstance(expected,bool): return type(actual) is type(expected) and actual == expected
    return actual == expected


def verify_body(artifacts):
    versions = environment_check()
    manifest, _ = project_files()
    cases = scenarios()
    initial_source = source_fingerprint()
    process([sys.executable,"-m","unittest","discover","-s","tools/tests","-v"])
    process(["cargo","test","--workspace","--locked"])
    build()
    package = pack(); original = package.read_bytes()
    require(pack().read_bytes() == original, "BUILD.NONDETERMINISTIC_PACKAGE")
    install(package, allow_unsigned=True)
    results = {}
    for case in cases:
        results[case["id"]] = scenario_run(manifest["id"], case, artifacts)
    variant, files = read_package(package)
    variant["id"] = "org.example.audit.unseen-" + uuid.uuid4().hex[:8]
    variant["tool"]["id"] = "unseen_ide"
    files["manifest.json"] = encoded(variant)
    build_info = decoded(files["build-info.json"])
    build_info.update({"metadataOnlyVariant":True,"derivedFromPackageSha256":hashlib.sha256(original).hexdigest()})
    files["build-info.json"] = encoded(build_info)
    payload = {key:value for key,value in files.items() if key not in ("checksums.json","signature.json")}
    files["checksums.json"] = encoded({"schema":"tokenme.package-integrity/0.1","files":{key:{"sha256":hashlib.sha256(value).hexdigest(),"size":len(value)} for key,value in sorted(payload.items())}})
    variant_package = artifacts / "unseen-ide.tokenme-plugin"
    variant_package.write_bytes(archive_bytes(files))
    host_hash = hashlib.sha256(host_path().read_bytes()).hexdigest()
    install(variant_package, allow_unsigned=True)
    positive_case = next(case for case in cases if case["assertions"].get("/status") == "ok")
    scenario_run(variant["id"], positive_case, artifacts, identity_case="unseen-id")
    require(hashlib.sha256(host_path().read_bytes()).hexdigest() == host_hash, "VERIFY.UNKNOWN_ID")
    module = files[variant["runtime"]["entrypoint"]]
    for build_path in (ROOT,Path(os.environ.get("CARGO_HOME",Path.home()/".cargo")).resolve()):
        require(not any(prefix.encode().lower() in module.lower() for prefix in {str(build_path),build_path.as_posix()}), "VERIFY.LOCAL_BUILD_PATH")
    require(initial_source == source_fingerprint(), "VERIFY.SOURCE_CHANGED")
    return {"versions":versions,"pluginId":manifest["id"],"package":package.name,"packageSha256":hashlib.sha256(original).hexdigest(),"packageBytes":len(original),"sourceTreeSha256":initial_source,"hostSha256":host_hash,"moduleSha256":hashlib.sha256(module).hexdigest(),"wasmExecution":True,"cases":{key:{"status":"passed","summary":value.get("summary")} for key,value in results.items()},"unknownPluginWithoutHostRebuild":True,"localBuildPathsRemapped":True,"mainAppIntegrated":False,"signature":"unsigned-local"}


def verify():
    started = time.monotonic()
    run_id = uuid.uuid4().hex
    artifacts = ROOT / "artifacts/runs" / run_id
    artifacts.mkdir(parents=True)
    report = {"schema":"tokenme.reference-verification/0.2","status":"running","runId":run_id,"artifactDirectory":artifacts.relative_to(ROOT).as_posix(),"mainAppIntegrated":False}
    latest = ROOT / "artifacts/verification.json"
    atomic_json(latest, report)
    atomic_json(artifacts / "verification.json", report)
    try:
        report.update(verify_body(artifacts))
        report["status"] = "passed"
        return report
    except BaseException as error:
        report["status"] = "interrupted" if isinstance(error, (KeyboardInterrupt, SystemExit)) else "failed"
        report["errorCode"] = error.code if isinstance(error, KitError) else "VERIFY.INTERNAL"
        raise
    finally:
        report["elapsedSeconds"] = round(time.monotonic() - started, 3)
        atomic_json(artifacts / "verification.json", report)
        atomic_json(latest, report)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    commands=parser.add_subparsers(dest="command",required=True)
    for name in ("check","test","build","pack","verify"):
        commands.add_parser(name)
    inspect=commands.add_parser("inspect"); inspect.add_argument("package",type=Path)
    installation=commands.add_parser("install"); installation.add_argument("package",type=Path); installation.add_argument("--allow-unsigned-local",action="store_true")
    running=commands.add_parser("run"); running.add_argument("plugin"); running.add_argument("--task",required=True); running.add_argument("--repeat",type=int,default=1)
    initial=commands.add_parser("init"); initial.add_argument("destination",type=Path); initial.add_argument("--id",required=True)
    args=parser.parse_args()
    if args.command=="check":
        manifest,_=project_files(); cases=scenarios(); result={"status":"passed","plugin":manifest["id"],"cases":len(cases)}
    elif args.command=="test":
        process([sys.executable,"-m","unittest","discover","-s","tools/tests","-v"])
        process(["cargo","test","--workspace","--locked"]); result={"status":"passed","wasmExecution":False}
    elif args.command=="build": build(); result={"status":"built"}
    elif args.command=="pack": result={"package":str(pack())}
    elif args.command=="inspect":
        manifest,files=read_package(args.package); result={"id":manifest["id"],"version":manifest["version"],"files":sorted(files),"signature":"unsigned-local","executed":False}
    elif args.command=="install": result={"installed":str(install(args.package,args.allow_unsigned_local)),"profile":"reference-sandbox"}
    elif args.command=="run": result=run_installed(args.plugin,args.task,repeat=args.repeat)
    elif args.command=="init": result={"created":str(init_project(args.destination.resolve(),args.id))}
    else: result=verify()
    print(json.dumps(result,ensure_ascii=False,sort_keys=True))


if __name__=="__main__":
    try:
        main()
    except (KitError,OSError,KeyError) as error:
        print(json.dumps({"status":"failed","error":str(error)},ensure_ascii=False),file=sys.stderr)
        sys.exit(1)
