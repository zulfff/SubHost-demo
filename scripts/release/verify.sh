#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 2 ]]; then
  echo "usage: $0 RELEASE_DIRECTORY vVERSION" >&2
  exit 2
fi
release_directory=$1
release_tag=$2
release_repository=${SUBHOST_RELEASE_REPOSITORY:-zulfff/SubHost-demo}
if [[ ! "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-(alpha|beta|rc)\.[0-9]+)?$ ]] \
  || [[ ! "$release_repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "invalid release tag or repository identity" >&2
  exit 2
fi
release_scripts=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cosign verify-blob "$release_directory/SHA256SUMS" \
  --bundle "$release_directory/SHA256SUMS.sigstore.json" \
  --certificate-identity "https://github.com/$release_repository/.github/workflows/release.yml@refs/tags/$release_tag" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
python3 "$release_scripts/artifacts.py" verify "$release_directory"
