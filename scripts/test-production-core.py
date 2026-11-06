#!/usr/bin/env python3
"""Fail-closed source-contract tests. These do not run Nix or a Flatpak app."""
import importlib.util
import json
from pathlib import Path
import tempfile
import sys
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("production_core", ROOT / "scripts/check-production-core.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class SourceContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="anvil-core-contract-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for name in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml", "flake.nix", "packaging/flatpak/cargo-sources.json", "packaging/jterm-core-production.json", "packaging/jterm-core-origin.json", "packaging/jterm-core-revision", "packaging/jterm-core-src.tar.gz", "packaging/jterm-core-src.tar.gz.sha256", "vendor/host-adapter-bundle.json", "vendor/process-adapter-bundle.json", "scripts/link-recovery/Cargo.toml", "scripts/link-recovery/Cargo.lock"]:
            target = self.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((ROOT / name).read_bytes())

    def test_production_sources_agree(self):
        self.assertEqual(len(checker.check(self.root)), 40)

    def test_global_sibling_override_is_rejected(self):
        (self.root / ".cargo/config.toml").write_text('[patch."https://github.com/beamiter/jterm_core.git"]\njterm_core = { path = "../jterm_core" }\n')
        with self.assertRaisesRegex(ValueError, "overrides"):
            checker.check(self.root)

    def test_unqualified_path_lock_is_rejected(self):
        lock = self.root / "Cargo.lock"
        lock.write_text("\n".join(line for line in lock.read_text().splitlines() if not line.startswith('source = "git+https://github.com/beamiter/jterm_core.git')))
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            checker.check(self.root)

    def test_stale_flatpak_pin_is_rejected(self):
        path = self.root / "packaging/flatpak/cargo-sources.json"
        sources = json.loads(path.read_text())
        for source in sources:
            if source.get("type") == "git" and "jterm_core" in source.get("url", ""):
                source["commit"] = "0" * 40
        path.write_text(json.dumps(sources))
        with self.assertRaisesRegex(ValueError, "Flatpak"):
            checker.check(self.root)

    def test_historical_bundle_is_not_a_production_input(self):
        path = self.root / "flake.nix"
        path.write_text(path.read_text() + "\n# jterm-core-src.tar.gz\n")
        with self.assertRaisesRegex(ValueError, "historical"):
            checker.check(self.root)

    def test_legacy_config_override_is_rejected(self):
        (self.root / ".cargo/config").write_text('[patch."https://github.com/beamiter/jterm_core.git"]\njterm_core = { path = "../jterm_core" }\n')
        with self.assertRaisesRegex(ValueError, "overrides"):
            checker.check(self.root)

    def test_config_directory_symlink_is_rejected_without_reading_target(self):
        (self.root / ".cargo/config.toml").unlink()
        (self.root / ".cargo").rmdir()
        (self.root / ".cargo").symlink_to("missing-external-config", target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlink"):
            checker.check(self.root)

    def test_source_replacement_is_rejected(self):
        (self.root / ".cargo/config.toml").write_text('[source.crates-io]\nreplace-with = "local"\n[source.local]\ndirectory = "vendor"\n')
        with self.assertRaisesRegex(ValueError, "replaces"):
            checker.check(self.root)

    def test_receipt_revision_is_checked(self):
        path = self.root / "packaging/jterm-core-production.json"
        receipt = json.loads(path.read_text())
        receipt["revision"] = "0" * 40
        path.write_text(json.dumps(receipt))
        with self.assertRaisesRegex(ValueError, "receipt"):
            checker.check(self.root)

    def test_nix_hash_is_checked(self):
        path = self.root / "packaging/jterm-core-production.json"
        receipt = json.loads(path.read_text())
        receipt["source_nar_sha256"] = "sha256-" + "A" * 43 + "="
        path.write_text(json.dumps(receipt))
        with self.assertRaisesRegex(ValueError, "Nix hash"):
            checker.check(self.root)

    def test_adapter_receipts_are_checked(self):
        for name in ("host-adapter-bundle", "process-adapter-bundle"):
            path = self.root / f"vendor/{name}.json"
            original = path.read_text()
            receipt = json.loads(original)
            receipt["public_core_base"] = "0" * 40
            path.write_text(json.dumps(receipt))
            with self.assertRaisesRegex(ValueError, name):
                checker.check(self.root)
            path.write_text(original)

    def test_nested_fixture_manifest_is_checked(self):
        path = self.root / "scripts/link-recovery/Cargo.toml"
        revision = checker.check(self.root)
        path.write_text(path.read_text().replace(revision, "0" * 40))
        with self.assertRaisesRegex(ValueError, "fixture manifest"):
            checker.check(self.root)

    def test_nested_fixture_lock_is_checked(self):
        path = self.root / "scripts/link-recovery/Cargo.lock"
        revision = checker.check(self.root)
        path.write_text(path.read_text().replace(revision, "0" * 40))
        with self.assertRaisesRegex(ValueError, "fixture lock"):
            checker.check(self.root)

    def test_stale_audit_bundle_revision_is_rejected(self):
        (self.root / "packaging/jterm-core-revision").write_text("0" * 40 + "\n")
        with self.assertRaisesRegex(ValueError, "Audit bundle revision"):
            checker.check(self.root)

    def test_altered_audit_bundle_is_rejected(self):
        with (self.root / "packaging/jterm-core-src.tar.gz").open("ab") as stream:
            stream.write(b"changed")
        with self.assertRaisesRegex(ValueError, "Audit bundle checksum"):
            checker.check(self.root)

    def test_audit_bundle_tree_identity_is_checked(self):
        path = self.root / "packaging/jterm-core-origin.json"
        data = json.loads(path.read_text())
        data["published_core_tree"] = "0" * 40
        path.write_text(json.dumps(data))
        with self.assertRaisesRegex(ValueError, "Audit bundle source identity"):
            checker.check(self.root)


if __name__ == "__main__":
    unittest.main()
