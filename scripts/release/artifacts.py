#!/usr/bin/env python3
"""Validate release identity and produce/verify deterministic Linux artifacts."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import uuid
from datetime import datetime, timezone

BINARY_CRATES = {
    "subhost": "crates/subhost-cli",
    "subhost-faucet": "crates/subhost-faucet",
    "subhost-explorer": "explorer",
    "subhost-bench": "crates/subhost-bench",
}
TARGET = "x86_64-unknown-linux-gnu"
RUST_TOOLCHAIN = "1.93.0"
BUILD_IMAGE = "rust:1.93.0-slim-bookworm@sha256:776861219cd851131c1cec3bbd7cbeb16b99a794048097eb69ad9682a8ed0d57"
CRATE_NAMES = {name: ("subhost-cli" if name == "subhost" else name) for name in BINARY_CRATES}
TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(alpha|beta|rc)\.(0|[1-9][0-9]*))?\Z")


def allow_publication(tag, stable_approved):
    match = TAG.fullmatch(tag)
    if not match:
        raise ValueError("invalid release tag")
    prerelease = match[1] == "0" or match[4] is not None
    if not prerelease and stable_approved != "true":
        raise ValueError("stable v1 requires the completed S12 review and maintainer approval")


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


def identity(root, tag):
    if not TAG.fullmatch(tag):
        raise ValueError("tag must be vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-rc.N/alpha.N/beta.N")
    with (root / "Cargo.toml").open("rb") as stream:
        version = tomllib.load(stream)["workspace"]["package"]["version"]
    if tag != "v" + version:
        raise ValueError("tag does not match the workspace package version")
    with (root / "rust-toolchain.toml").open("rb") as stream:
        toolchain = tomllib.load(stream)["toolchain"]["channel"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", toolchain):
        raise ValueError("release toolchain must be an exact Rust version")
    if toolchain != RUST_TOOLCHAIN:
        raise ValueError("toolchain changed without updating the immutable release build policy")
    if git(root, "status", "--porcelain", "--untracked-files=normal"):
        raise ValueError("release source checkout must be clean")
    commit = git(root, "rev-parse", "HEAD")
    epoch = int(git(root, "show", "-s", "--format=%ct", "HEAD"))
    return {
        "version": version,
        "tag": tag,
        "commit": commit,
        "source_date_epoch": epoch,
        "rust_toolchain": toolchain,
        "build_image": BUILD_IMAGE,
        "target": TARGET,
        "cargo_lock_sha256": hashlib.sha256((root / "Cargo.lock").read_bytes()).hexdigest(),
    }


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def archive(path, contents, epoch):
    with path.open("wb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as bundle:
                for name, payload, mode in sorted(contents):
                    info = tarfile.TarInfo(name)
                    info.size = len(payload)
                    info.mode = mode
                    info.mtime = epoch
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    bundle.addfile(info, io.BytesIO(payload))


def package(root, tag, binaries, output):
    metadata = identity(root, tag)
    if output.exists():
        raise ValueError("release destination already exists; refusing to replace artifacts")
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix=".subhost-release-", dir=output.parent))
    try:
        contents = [("LICENSE", (root / "LICENSE").read_bytes(), 0o644)]
        for name, crate in BINARY_CRATES.items():
            executable = binaries / name
            if executable.is_symlink() or not executable.is_file():
                raise ValueError("missing or symlinked release binary: " + name)
            payload = executable.read_bytes()
            if len(payload) < 20 or payload[:6] != b"\x7fELF\x02\x01" or payload[18:20] != b"\x3e\x00":
                raise ValueError("binary is not an x86_64 little-endian ELF: " + name)
            contents.append((name, payload, 0o755))
            bom_path = root / crate / "bom.json"
            if bom_path.is_symlink():
                raise ValueError("SBOM must be a regular file")
            bom = json.loads(bom_path.read_text())
            if (bom.get("bomFormat") != "CycloneDX"
                    or not isinstance(bom.get("components"), list)
                    or not bom["components"]
                    or bom.get("metadata", {}).get("component", {}).get("name") != CRATE_NAMES[name]):
                raise ValueError("missing CycloneDX dependency inventory for " + name)
            bom["serialNumber"] = "urn:uuid:" + str(uuid.uuid5(uuid.NAMESPACE_URL, metadata["commit"] + name + TARGET))
            bom.setdefault("metadata", {})["timestamp"] = datetime.fromtimestamp(
                metadata["source_date_epoch"], timezone.utc
            ).isoformat().replace("+00:00", "Z")
            (temporary / (name + ".cdx.json")).write_bytes(json_bytes(bom))
        manifest = json_bytes(metadata)
        contents.append(("RELEASE-METADATA.json", manifest, 0o644))
        (temporary / "RELEASE-METADATA.json").write_bytes(manifest)
        archive(temporary / f"subhost-{tag}-{TARGET}.tar.gz", contents, metadata["source_date_epoch"])
        checksums = []
        for artifact in sorted(temporary.iterdir()):
            checksums.append(hashlib.sha256(artifact.read_bytes()).hexdigest() + "  " + artifact.name + "\n")
        (temporary / "SHA256SUMS").write_text("".join(checksums))
        os.rename(temporary, output)
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def verify(output):
    checksums = output / "SHA256SUMS"
    if checksums.is_symlink():
        raise ValueError("checksum manifest must be a regular file")
    seen = set()
    lines = checksums.read_text().splitlines()
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
        if not match or match[2] in seen:
            raise ValueError("invalid, duplicate or unsafe checksum entry")
        digest, name = match.groups()
        seen.add(name)
        path = output / name
        if path.is_symlink() or not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError("artifact checksum mismatch: " + name)
    expected = {path.name for path in output.iterdir()} - {"SHA256SUMS", "SHA256SUMS.sigstore.json"}
    if not seen or seen != expected:
        raise ValueError("checksum manifest does not cover exactly the release artifacts")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    check = subcommands.add_parser("check")
    check.add_argument("--root", type=Path, default=Path.cwd())
    check.add_argument("--tag", required=True)
    build = subcommands.add_parser("package")
    build.add_argument("--root", type=Path, default=Path.cwd())
    build.add_argument("--tag", required=True)
    build.add_argument("--binaries", type=Path, required=True)
    build.add_argument("--output", type=Path, required=True)
    validate = subcommands.add_parser("verify")
    validate.add_argument("directory", type=Path)
    publish = subcommands.add_parser("allow-publication")
    publish.add_argument("--tag", required=True)
    publish.add_argument("--stable-approved", default="false")
    arguments = parser.parse_args()
    try:
        if arguments.command == "check":
            print(json.dumps(identity(arguments.root, arguments.tag), sort_keys=True))
        elif arguments.command == "package":
            package(arguments.root, arguments.tag, arguments.binaries, arguments.output)
        elif arguments.command == "verify":
            verify(arguments.directory)
        else:
            allow_publication(arguments.tag, arguments.stable_approved)
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"release validation failed: {error}\n")


if __name__ == "__main__":
    main()
