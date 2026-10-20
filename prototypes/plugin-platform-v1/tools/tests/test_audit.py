"""Adversarial regressions for the local authoring/install/evidence boundary."""
import io
import json
from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from test_package import kit, example_files, seal


class AuditTests(unittest.TestCase):
    def assert_code(self, code, action):
        with self.assertRaises(kit.KitError) as result:
            action()
        self.assertEqual(result.exception.code, code)

    def test_scaffold_package_checks_accept_its_new_identity(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-scaffold-") as directory:
            destination = Path(directory) / "new-plugin"
            kit.init_project(destination, "org.example.audit.new-ide")
            result = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tools/tests", "-p", "test_package.py"], cwd=destination, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_noncanonical_compressed_package_is_rejected(self):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, content in example_files().items():
                archive.writestr(name, content)
        self.assert_code("PACKAGE.NONCANONICAL", lambda: kit.validate_package_bytes(buffer.getvalue()))

    def test_pack_requires_provenance_for_existing_component(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-build-") as directory:
            root = Path(directory)
            module = root / "target/wasm32-wasip2/release/tokenme_reference_plugin.wasm"
            module.parent.mkdir(parents=True)
            module.write_bytes(kit.COMPONENT_HEADER + b"stale-component")
            (root / "toolchain.json").write_bytes((kit.ROOT / "toolchain.json").read_bytes())
            shutil.copytree(kit.ROOT / "schema", root / "schema")
            manifest, files = kit.project_files()
            with patch.object(kit, "ROOT", root), patch.object(kit, "project_files", return_value=(manifest, files)), patch.object(kit, "source_fingerprint", return_value="a" * 64):
                self.assert_code("BUILD.PROVENANCE_REQUIRED", kit.pack)

    def test_failed_verification_does_not_leave_previous_pass_current(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-evidence-") as directory:
            root = Path(directory)
            (root / "artifacts").mkdir()
            latest = root / "artifacts/verification.json"
            latest.write_bytes(kit.encoded({"status": "passed", "runId": "old-run"}))
            with patch.object(kit, "ROOT", root), patch.object(kit, "source_fingerprint", return_value="b" * 64), patch.object(kit, "environment_check", side_effect=kit.KitError("TEST.INJECTED_FAILURE")):
                self.assert_code("TEST.INJECTED_FAILURE", kit.verify)
            self.assertEqual(json.loads(latest.read_bytes())["status"], "failed")

    def test_install_does_not_follow_predictable_temporary_hardlink(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-install-") as directory:
            root = Path(directory)
            package = root / "fixture.tokenme-plugin"
            files = example_files()
            package.write_bytes(kit.archive_bytes(files))
            manifest = kit.decoded(files["manifest.json"])
            plugin = root / "sandbox/plugins" / manifest["id"]
            plugin.mkdir(parents=True)
            sentinel = root / "sentinel.txt"
            sentinel.write_bytes(b"preserve-this-file")
            os.link(sentinel, plugin / "active.tmp")
            with patch.object(kit, "SANDBOX", root / "sandbox"):
                kit.install(package, allow_unsigned=True)
            self.assertEqual(sentinel.read_bytes(), b"preserve-this-file")

    def test_active_pointer_must_match_packaged_identity(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-identity-") as directory:
            root = Path(directory)
            shutil.copytree(kit.ROOT / "schema", root / "schema")
            files = example_files()
            manifest = kit.decoded(files["manifest.json"])
            digest = kit.hashlib.sha256(kit.archive_bytes(files)).hexdigest()
            base = root / "sandbox/plugins/org.example.audit.impersonated"
            installed = base / manifest["version"] / digest
            for name, content in files.items():
                target = installed / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(content)
            (base / "active.json").write_bytes(kit.encoded({"version": manifest["version"], "packageSha256": digest}))
            with patch.object(kit, "ROOT", root), patch.object(kit, "SANDBOX", root / "sandbox"):
                self.assert_code("INSTALL.IDENTITY", lambda: kit.run_installed("org.example.audit.impersonated", "collect-usage"))

    def test_stale_sources_and_changed_module_cannot_be_relabelled(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-provenance-") as directory:
            root = Path(directory)
            with patch.object(kit, "ROOT", root), patch.object(kit, "source_fingerprint", return_value="b" * 64):
                kit.host_path().parent.mkdir(parents=True)
                kit.host_path().write_bytes(b"host")
                kit.module_path().parent.mkdir(parents=True)
                kit.module_path().write_bytes(kit.COMPONENT_HEADER)
                receipt = {"schema":"tokenme.build-receipt/0.1", "status":"passed", "sourceTreeSha256":"a"*64, "hostSha256":kit.hashlib.sha256(b"host").hexdigest(), "moduleSha256":kit.hashlib.sha256(kit.COMPONENT_HEADER).hexdigest()}
                kit.atomic_json(root / "target/build-receipt.json", receipt)
                self.assert_code("BUILD.STALE_SOURCE", lambda: kit.build_receipt(current_source=True))
                receipt["sourceTreeSha256"] = "b" * 64
                kit.atomic_json(root / "target/build-receipt.json", receipt)
                kit.module_path().write_bytes(kit.COMPONENT_HEADER + b"changed")
                self.assert_code("BUILD.MODULE_CHANGED", lambda: kit.build_receipt(current_source=True))

    def test_process_budget_allows_compilation_before_the_task_deadline(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-deadline-") as directory:
            root = Path(directory)
            package = root / "fixture.tokenme-plugin"
            files = example_files()
            manifest = kit.decoded(files["manifest.json"])
            manifest["tasks"][0]["timeoutMs"] = 1700
            files["manifest.json"] = kit.encoded(manifest)
            package.write_bytes(kit.archive_bytes(seal(files)))
            fake_host = root / "host.exe"; fake_host.write_bytes(b"not-run")
            result = subprocess.CompletedProcess([], 0, stdout=kit.encoded({"status":"ok","results":[]}).decode())
            with patch.object(kit,"SANDBOX",root/"sandbox"), patch.object(kit,"host_path",return_value=fake_host), patch.object(kit,"build_receipt",return_value={}), patch.object(kit,"process",return_value=result) as execute:
                kit.install(package,allow_unsigned=True)
                kit.run_installed(manifest["id"], manifest["tasks"][0]["id"], artifacts=root/"reports")
                self.assertEqual(execute.call_args.kwargs["timeout"],30)
                installed = kit.SANDBOX/"plugins"/manifest["id"]
                active = kit.decoded((installed/"active.json").read_bytes())
                loaded = kit.decoded((installed/active["version"]/active["packageSha256"]/"manifest.json").read_bytes())
                self.assertEqual(loaded["tasks"][0]["timeoutMs"],1700)

    def test_duplicate_diagnostic_id_is_rejected(self):
        manifest, files = kit.project_files()
        manifest["diagnostics"]["events"].append(manifest["diagnostics"]["events"][0])
        self.assert_code("MANIFEST.DUPLICATE_ID", lambda:kit.validate_manifest(manifest,files))

    def test_svg_entities_and_foreign_namespaces_are_rejected(self):
        manifest, files = kit.project_files()
        for icon in (b'<!DOCTYPE svg [<!ENTITY x "expanded">]><svg>&x;</svg>', b'<svg xmlns="urn:foreign"><path d="M0 0"/></svg>', b'<path d="M0 0"/>'):
            changed = dict(files); changed[manifest["tool"]["icon"]] = icon
            self.assert_code("ICON.ACTIVE_CONTENT", lambda:kit.validate_manifest(manifest,changed))

    def test_malformed_manifest_fails_schema_before_dereferencing(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-manifest-") as directory:
            root = Path(directory)
            (root/"manifest.json").write_bytes(b'[]')
            self.assert_code("SCHEMA.INVALID", lambda:kit.project_files(root))

    def test_overflowing_json_exponent_is_not_a_finite_number(self):
        self.assert_code("JSON.NONFINITE", lambda:kit.decoded(b'{"count":1e999}'))

    def test_installer_lock_rejects_a_competing_activation(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-lock-") as directory:
            root = Path(directory)
            files = example_files(); manifest = kit.decoded(files["manifest.json"])
            package = root/"fixture.tokenme-plugin"; package.write_bytes(kit.archive_bytes(files))
            with patch.object(kit,"SANDBOX",root/"sandbox"):
                with kit.installation_lock(root/"sandbox/plugins"/manifest["id"]):
                    self.assert_code("INSTALL.BUSY", lambda:kit.install(package,allow_unsigned=True))
                self.assertFalse((root/"sandbox/plugins"/manifest["id"]/"active.json").exists())

    def test_boolean_expectation_cannot_pass_as_numeric_zero(self):
        self.assertFalse(kit.json_equal({"count":False}, {"count":0}))
        self.assertTrue(kit.json_equal(25.0, 25))

    def test_utf16_dtd_cannot_bypass_the_static_icon_profile(self):
        manifest, files = kit.project_files()
        files[manifest["tool"]["icon"]] = '<!DOCTYPE svg [<!ENTITY x "expanded">]><svg>&x;</svg>'.encode("utf-16")
        self.assert_code("ICON.INVALID", lambda:kit.validate_manifest(manifest,files))

    def test_json_wire_format_requires_utf8(self):
        self.assert_code("JSON.INVALID", lambda:kit.decoded('{"accepted":true}'.encode("utf-16")))


if __name__ == "__main__":
    unittest.main()
