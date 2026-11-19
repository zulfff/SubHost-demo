#!/usr/bin/env bash
# PREP ONLY — fresh public testnet key material (Q-012 / Q-037).
#
# Generates the four distinct encrypted producer keystores and a public-key
# manifest for a fresh chain-31338 genesis. It does NOT deploy anything, change
# any route, or write an activation-ready genesis. The final genesis assembly
# (PoP regeneration + fee policy + calldata schedule bound into
# consensus_context) is GATED and documented below; this script stops at the key
# material so that work can be done now without pre-empting the gate.
#
# Activation gate (do not press the button until all are merged + verified):
#   * Q-037 calldata-pricing ladder step L4 (flat 16 gas/byte, v3 headers).
#   * S9.6 encrypted producer bootstrap loader (PR #79, Track A).
#   * The fee-policy / calldata-schedule genesis CLI flags (pending flag PR).
# See docs/operations/public-testnet-activation.md.
#
# SECURITY: this script creates signing keys and credential files. They are the
# right to produce blocks. Keep them owner-only (0600), back them up offline,
# and NEVER commit them or the credentials to the repository. Run it on the
# secure host that will hold the validator identity, not in CI.
set -euo pipefail

CHAIN_ID=31338
VALIDATORS=4
DATA_DIR=""
SUBHOST="${SUBHOST:-subhost}"

usage() {
  cat <<'USAGE'
Usage: prepare-public-genesis.sh --data-dir <new-dir> [--chain-id 31338]
                                 [--validators 4] [--subhost <binary>]

PREP ONLY: generates encrypted producer keystores + a pubkey manifest for a
fresh public genesis. Does not deploy, change routes, or write a final genesis.
The genesis assembly (PoP + fee policy + calldata schedule) is gated on
Q-037 L4 + S9.6 bootstrap (#79) + the fee/calldata CLI flags.
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --data-dir) DATA_DIR="$2"; shift 2 ;;
    --chain-id) CHAIN_ID="$2"; shift 2 ;;
    --validators) VALIDATORS="$2"; shift 2 ;;
    --subhost) SUBHOST="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[ -n "$DATA_DIR" ] || { echo "error: --data-dir is required" >&2; usage >&2; exit 2; }
command -v "$SUBHOST" >/dev/null 2>&1 || {
  echo "error: subhost binary '$SUBHOST' not found; build it (cargo build -p subhost-cli) or pass --subhost" >&2
  exit 1
}
command -v openssl >/dev/null 2>&1 || { echo "error: openssl is required to generate credentials" >&2; exit 1; }

# Refuse a conflicting or incomplete existing setup; never clobber keys.
if [ -e "$DATA_DIR" ]; then
  echo "error: $DATA_DIR already exists; use a fresh directory (keys are never replaced)" >&2
  exit 1
fi
case "$VALIDATORS" in
  4) ;;
  *) echo "error: the public testnet is four validators (3-of-4 quorum); got $VALIDATORS" >&2; exit 1 ;;
esac

mkdir -p "$DATA_DIR"
chmod 700 "$DATA_DIR"
MANIFEST="$DATA_DIR/validator-manifest.txt"
: > "$MANIFEST"
chmod 600 "$MANIFEST"

echo "Preparing $VALIDATORS encrypted producer keystores for chain $CHAIN_ID under $DATA_DIR"
echo "PREP ONLY — no deployment, no route changes, no final genesis."
echo

for n in $(seq 1 "$VALIDATORS"); do
  vdir="$DATA_DIR/validator-$n"
  mkdir -p "$vdir"
  chmod 700 "$vdir"
  cred="$vdir/producer.credential"
  # Fresh random credential, owner-only. This is a secret; keep it offline.
  openssl rand -hex 32 > "$cred"
  chmod 600 "$cred"
  "$SUBHOST" producer create --data-dir "$vdir" --credential-file "$cred"
  keystore="$vdir/producer.keystore.json"
  pubkey="$("$SUBHOST" producer inspect "$keystore" | sed -n 's/^public key: //p')"
  [ -n "$pubkey" ] || { echo "error: could not read public key from $keystore" >&2; exit 1; }
  printf 'validator-%s %s\n' "$n" "$pubkey" >> "$MANIFEST"
  echo "  validator-$n public key: $pubkey"
done

echo
echo "Key material ready. Public-key manifest: $MANIFEST"
echo
cat <<GATED
NEXT STEPS — GATED, not performed by this script:
  The shared chain-$CHAIN_ID genesis still needs, once the gate clears:
    1. Regenerated proofs of possession for all $VALIDATORS validators under the
       corrected RFC 9380 suite (subhost init --validator ADDRESS:POWER with the
       producer credential, or the genesis CLI helper once it lands).
    2. The fee policy bound into the genesis (fee-policy CLI flag, pending).
    3. The calldata schedule bound into consensus_context — Q-037 L4 (flat
       16 gas/byte, v3 headers). A genesis without it is NOT activation-ready.
    4. Encrypted node bootstrap (S9.6, PR #79) so validators start from these
       keystores, not plaintext keys.
  Do not re-enable public routes or advertise activation until Q-037 L4 and the
  S9.6 bootstrap are merged and verified on the deployed revision, and the
  running-proofs in docs/operations/public-testnet-activation.md pass.

SECURITY REMINDER: the keystores and *.credential files under $DATA_DIR are
signing secrets. Keep them 0600, back them up offline, and never commit them.
GATED