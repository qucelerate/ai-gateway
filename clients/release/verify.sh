#!/usr/bin/env bash
# Verifies a downloaded client release: the SHA256SUMS signature, every
# file's checksum, and each artifact's signed provenance.
#
#   clients/release/verify.sh <dir> [cosign.pub]   # default: the key beside this script
#
# Needs cosign 2.4 or later and sha256sum (GNU coreutils). The bundles carry no transparency-log entry
# (sign.sh uploads nothing), so the log check is skipped; the key is the
# trust root.
set -euo pipefail

dir="$(cd "${1:?usage: verify.sh <dir> [cosign.pub]}" && pwd)"
key="$(realpath -e "${2:-$(dirname "$0")/cosign.pub}")"
cosign="${COSIGN:-cosign}"
cd "$dir"

"$cosign" verify-blob --key "$key" --insecure-ignore-tlog=true \
  --bundle SHA256SUMS.sigstore.json SHA256SUMS 2>/dev/null ||
  { echo "SHA256SUMS: the signature does not verify with $key" >&2; exit 1; }
echo "SHA256SUMS: signature verified"
sha256sum --strict -c SHA256SUMS
# Every other file must be one SHA256SUMS lists, or a bundle for one: a file
# added beside a release (a forged compatibility result, say) is refused,
# not passed over.
listed="$(sed -E 's/^[0-9a-f]{64} [ *]//' SHA256SUMS)"
shopt -s nullglob
for file in *; do
  case "$file" in
    SHA256SUMS | SHA256SUMS.sigstore.json) continue ;;
    *.provenance.sigstore.json) subject="${file%.provenance.sigstore.json}" ;;
    *) subject="$file" ;;
  esac
  printf '%s\n' "$listed" | grep -Fxq -- "$subject" || { echo "$file: not covered by the signed SHA256SUMS" >&2; exit 1; }
done
checked=0
for file in *; do
  case "$file" in
    SHA256SUMS | compatibility-*.json | *.sigstore.json) continue ;;
  esac
  [ -f "$file.provenance.sigstore.json" ] || { echo "$file: no provenance bundle" >&2; exit 1; }
  "$cosign" verify-blob-attestation --key "$key" --insecure-ignore-tlog=true --type slsaprovenance1 \
    --bundle "$file.provenance.sigstore.json" "$file" 2>/dev/null ||
    { echo "$file: its provenance does not verify with $key" >&2; exit 1; }
  echo "$file: provenance verified"
  checked=$((checked + 1))
done
[ "$checked" -gt 0 ] || { echo "no artifacts to verify in $dir" >&2; exit 1; }
