#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 ]]; then
  echo "usage: $0 vVERSION NEW_OUTPUT_DIRECTORY" >&2
  exit 2
fi
release_root=$(git rev-parse --show-toplevel)
release_tag=$1
release_output=$2
release_target=x86_64-unknown-linux-gnu
release_image='rust:1.93.0-slim-bookworm@sha256:776861219cd851131c1cec3bbd7cbeb16b99a794048097eb69ad9682a8ed0d57'
python3 "$release_root/scripts/release/artifacts.py" check --root "$release_root" --tag "$release_tag"
if [[ -e "$release_output" ]]; then
  echo "release destination already exists" >&2
  exit 1
fi
if ! cargo cyclonedx --version | tail -n 1 | grep -Eq '(^| )0\.5\.10$'; then
  echo "install cargo-cyclonedx 0.5.10 with cargo install --locked" >&2
  exit 1
fi
release_epoch=$(git show -s --format=%ct HEAD)
release_lock_before=$(sha256sum "$release_root/Cargo.lock")
release_cache=${CARGO_HOME:-$HOME/.cargo}
mkdir -p "$release_cache"
# The immutable compiler image contains the target linker; no host compiler is used.
docker run --rm --platform linux/amd64 \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,source=$release_root,target=/workspace" \
  --mount "type=bind,source=$release_cache,target=/cargo-cache" \
  --workdir /workspace \
  --env CARGO_HOME=/cargo-cache \
  --env "SOURCE_DATE_EPOCH=$release_epoch" \
  --env 'RUSTFLAGS=--remap-path-prefix=/workspace=/workspace' \
  "$release_image" \
  cargo build --locked --release --target "$release_target" \
    -p subhost-cli -p subhost-faucet -p subhost-explorer -p subhost-bench
# CycloneDX has no --locked flag. Offline metadata plus this digest check prevents
# publishing an SBOM against a dependency graph that changed the reviewed lockfile.
CARGO_NET_OFFLINE=true cargo cyclonedx --format json --all --target "$release_target" --override-filename bom
if [[ "$release_lock_before" != "$(sha256sum "$release_root/Cargo.lock")" ]]; then
  echo "SBOM generation changed Cargo.lock; refusing publication" >&2
  exit 1
fi
python3 "$release_root/scripts/release/artifacts.py" package \
  --root "$release_root" --tag "$release_tag" \
  --binaries "$release_root/target/$release_target/release" --output "$release_output"
python3 "$release_root/scripts/release/artifacts.py" verify "$release_output"
