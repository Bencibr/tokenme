import copy
import importlib.util
import io
from pathlib import Path
import stat
import tempfile
import unittest
import zipfile

MODULE = Path(__file__).resolve().parents[1] / "plugin.py"
SPEC = importlib.util.spec_from_file_location("reference_kit", MODULE)
kit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(kit)


def example_files():
    _, files = kit.project_files()
    files["dist/plugin.wasm"] = kit.COMPONENT_HEADER + b"synthetic-not-executable"
    files["build-info.json"] = kit.encoded({"schema":"tokenme.build/0.1", "toolchain":kit.decoded((kit.ROOT/"toolchain.json").read_bytes()), "sourceTreeSha256":"a"*64, "moduleSha256":kit.hashlib.sha256(files["dist/plugin.wasm"]).hexdigest(), "sdk":"tokenme-plugin-sdk-reference", "sdkVersion":"0.1.0"})
    return seal(files)


def seal(files):
    files = {key:value for key,value in files.items() if key not in ("checksums.json","signature.json")}
    files["checksums.json"] = kit.encoded({"schema":"tokenme.package-integrity/0.1","files":{key:{"sha256":kit.hashlib.sha256(value).hexdigest(),"size":len(value)} for key,value in files.items()}})
    files["signature.json"] = kit.encoded({"schema":"tokenme.package-signature/0.1","state":"unsigned-local"})
    return files


class PackageTests(unittest.TestCase):
    def assert_code(self, code, action):
        with self.assertRaises(kit.KitError) as result:
            action()
        self.assertEqual(result.exception.code, code)

    def test_roundtrip_and_deterministic_archive(self):
        files = example_files()
        first = kit.archive_bytes(files)
        second = kit.archive_bytes(dict(reversed(list(files.items()))))
        self.assertEqual(first, second)
        manifest, unpacked = kit.validate_package_bytes(first)
        self.assertEqual(manifest["id"], kit.project_files()[0]["id"])
        self.assertEqual(unpacked, files)

    def test_module_tampering_is_detected(self):
        files = example_files()
        files["dist/plugin.wasm"] += b"tampered"
        self.assert_code("PACKAGE.HASH_MISMATCH", lambda:kit.validate_package_bytes(kit.archive_bytes(files)))

    def test_undeclared_package_files_are_rejected(self):
        files = example_files()
        files["user-secrets.txt"] = b"not-allowed"
        self.assert_code("PACKAGE.UNEXPECTED_FILE", lambda:kit.validate_package_bytes(kit.archive_bytes(seal(files))))

    def test_case_collision_and_zip_slip(self):
        for names, code in ((["manifest.json","MANIFEST.JSON"],"PACKAGE.DUPLICATE_PATH"),(["../escape.txt"],"PACKAGE.PATH")):
            buffer = io.BytesIO()
            with zipfile.ZipFile(buffer,"w") as archive:
                for name in names:
                    archive.writestr(name,b"{}")
            self.assert_code(code, lambda:kit.validate_package_bytes(buffer.getvalue()))

    def test_symlink_entry_is_rejected_before_extracting(self):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer,"w") as archive:
            info = zipfile.ZipInfo("link")
            info.create_system = 3
            info.external_attr = (stat.S_IFLNK | 0o777) << 16
            archive.writestr(info,b"outside")
        self.assert_code("PACKAGE.FILE_TYPE", lambda:kit.validate_package_bytes(buffer.getvalue()))

    def test_windows_reserved_and_ambiguous_names(self):
        for name in ("assets/CON.svg","assets/a.","C:/x","assets\\escape","assets//x","assets/../x"):
            self.assert_code("PACKAGE.PATH", lambda:kit.path_name(name))

    def test_duplicate_json_key_is_rejected(self):
        self.assert_code("JSON.DUPLICATE_KEY", lambda:kit.decoded(b'{"id":"first","id":"second"}'))

    def test_unknown_capability_cannot_silently_register(self):
        manifest, files = kit.project_files()
        manifest = copy.deepcopy(manifest)
        manifest["tasks"][0]["capability"] = "checkin.run"
        self.assert_code("SCHEMA.INVALID", lambda:kit.validate_manifest(manifest,files))

    def test_locale_placeholder_mismatch_is_rejected(self):
        manifest, files = kit.project_files()
        changed = dict(files)
        locale = kit.decoded(changed["i18n/zh-CN.json"])
        locale["log.quotaParsed"] = "Wrong {token}."
        changed["i18n/zh-CN.json"] = kit.encoded(locale)
        self.assert_code("LOCALE.PLACEHOLDERS", lambda:kit.validate_manifest(manifest,changed))

    def test_unsigned_install_requires_explicit_local_flag(self):
        with tempfile.TemporaryDirectory(prefix="tokenme-kit-") as temporary:
            package = Path(temporary)/"fixture.tokenme-plugin"
            package.write_bytes(kit.archive_bytes(example_files()))
            self.assert_code("SIGNATURE.LOCAL_CONSENT_REQUIRED", lambda:kit.install(package))

    def test_new_plugin_identity_needs_no_host_tool_table(self):
        manifest, files = kit.project_files()
        manifest = copy.deepcopy(manifest)
        manifest["id"] = "org.example.thirdparty.new-ide"
        manifest["tool"]["id"] = "brand_new_ide"
        self.assertEqual(kit.validate_manifest(manifest,files)["tool"]["id"],"brand_new_ide")


if __name__ == "__main__":
    unittest.main()
