"""Failure-path and reproducibility checks for release packaging, with no compiler/registry access."""

import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

import artifacts


class ArtifactsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "source"
        self.root.mkdir()
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.0"\n')
        (self.root / "Cargo.lock").write_text("reviewed lockfile\n")
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.93.0"\n')
        (self.root / "LICENSE").write_text("Apache-2.0\n")
        (self.root / ".gitignore").write_text("**/bom.json\n/bin/\n")
        self.binaries = self.root / "bin"
        self.binaries.mkdir()
        for name, crate in artifacts.BINARY_CRATES.items():
            (self.root / crate).mkdir(parents=True, exist_ok=True)
            header = bytearray(64)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = b"\x3e\x00"
            (self.binaries / name).write_bytes(header + name.encode())
            bom = {
                "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
                "serialNumber": "urn:uuid:random-generator-output",
                "metadata": {"timestamp": "2026-01-01T00:00:00Z", "component": {"name": artifacts.CRATE_NAMES[name]}},
                "components": [{"name": "dependency", "version": "1.2.3"}],
            }
            (self.root / crate / "bom.json").write_text(json.dumps(bom))
        self.git("init", "-q")
        self.git("config", "user.name", "Release fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("add", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "LICENSE", ".gitignore")
        self.git("commit", "-qm", "fixture")

    def git(self, *args):
        subprocess.run(["git", "-C", str(self.root), *args], check=True, capture_output=True)

    def output(self, suffix="release"):
        return Path(self.temporary.name) / suffix

    def package(self, suffix="release"):
        result = self.output(suffix)
        artifacts.package(self.root, "v0.1.0", self.binaries, result)
        return result

    def test_repeated_packaging_is_identical_and_archive_metadata_is_normalized(self):
        first, second = self.package("first"), self.package("second")
        artifacts.verify(first)
        self.assertEqual({p.name: p.read_bytes() for p in first.iterdir()}, {p.name: p.read_bytes() for p in second.iterdir()})
        archive = next(first.glob("*.tar.gz"))
        with tarfile.open(archive) as bundle:
            self.assertEqual(bundle.getnames(), sorted(bundle.getnames()))
            for entry in bundle.getmembers():
                self.assertEqual((entry.uid, entry.gid), (0, 0))
                self.assertFalse(entry.name.startswith("/"))

    def test_existing_destination_is_preserved(self):
        destination = self.output()
        destination.mkdir()
        marker = destination / "keep"
        marker.write_bytes(b"existing release")
        with self.assertRaisesRegex(ValueError, "destination already exists"):
            self.package()
        self.assertEqual(marker.read_bytes(), b"existing release")

    def test_missing_binary_or_sbom_never_publishes_partial_destination(self):
        (self.binaries / "subhost").unlink()
        with self.assertRaises(ValueError):
            self.package()
        self.assertFalse(self.output().exists())
        self.assertFalse(list(self.output().parent.glob(".subhost-release-*")))

    def test_wrong_target_and_symlinked_binary_are_refused(self):
        binary = self.binaries / "subhost"
        content = bytearray(binary.read_bytes())
        content[18:20] = b"\xb7\x00"
        binary.write_bytes(content)
        with self.assertRaisesRegex(ValueError, "x86_64"):
            self.package()
        binary.unlink()
        binary.symlink_to(self.binaries / "subhost-faucet")
        with self.assertRaisesRegex(ValueError, "symlinked"):
            self.package()

    def test_wrong_sbom_component_is_refused(self):
        path = self.root / "crates/subhost-cli/bom.json"
        bom = json.loads(path.read_text())
        bom["metadata"]["component"]["name"] = "different-crate"
        path.write_text(json.dumps(bom))
        with self.assertRaisesRegex(ValueError, "dependency inventory"):
            self.package()
        self.assertFalse(self.output().exists())

    def test_dirty_sources_mismatched_versions_and_unpinned_toolchains_are_refused(self):
        for tag in ["v0.2.0", "v0.1.00", "v0.1.0;command", "0.1.0"]:
            with self.assertRaises(ValueError):
                artifacts.identity(self.root, tag)
        (self.root / "Cargo.lock").write_text("different dependency graph")
        with self.assertRaisesRegex(ValueError, "clean"):
            artifacts.identity(self.root, "v0.1.0")
        self.git("restore", "Cargo.lock")
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "stable"\n')
        with self.assertRaisesRegex(ValueError, "exact Rust"):
            artifacts.identity(self.root, "v0.1.0")
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.94.0"\n')
        with self.assertRaisesRegex(ValueError, "immutable release build policy"):
            artifacts.identity(self.root, "v0.1.0")

    def test_tampering_and_missing_inventory_are_detected(self):
        release = self.package()
        artifact = release / "RELEASE-METADATA.json"
        artifact.write_bytes(artifact.read_bytes() + b"tampered")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            artifacts.verify(release)

    def test_checksum_paths_duplicates_and_extra_files_are_refused(self):
        release = self.package()
        manifest = release / "SHA256SUMS"
        original = manifest.read_text()
        manifest.write_text("0" * 64 + "  ../outside\n")
        with self.assertRaisesRegex(ValueError, "unsafe"):
            artifacts.verify(release)
        manifest.write_text(original + original.splitlines()[0] + "\n")
        with self.assertRaisesRegex(ValueError, "duplicate"):
            artifacts.verify(release)
        manifest.write_text(original)
        (release / "uncovered").write_bytes(b"not in inventory")
        with self.assertRaisesRegex(ValueError, "exactly"):
            artifacts.verify(release)


if __name__ == "__main__":
    unittest.main()
