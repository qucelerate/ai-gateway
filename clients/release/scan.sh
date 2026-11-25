#!/usr/bin/env bash
# Scans the client sources and every release artifact in <dist> for secrets,
# with gitleaks (on PATH, or GITLEAKS) and gitleaks.toml. Artifacts are
# unpacked first, since gitleaks does not look inside archives.
#
#   clients/release/scan.sh <dist>
set -euo pipefail

dist="$(cd "${1:?usage: scan.sh <dist>}" && pwd)"
release="$(cd "$(dirname "$0")" && pwd)"
clients="$(dirname "$release")"
gitleaks="${GITLEAKS:-gitleaks}"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

# The committed client sources, not build output.
git -C "$clients" ls-files -z -- . | (cd "$clients" && tar --null --files-from=- -cf -) | tar -xf - -C "$work" --one-top-level=sources

for artifact in "$dist"/*; do
  out="$work/artifacts/$(basename "$artifact")"
  mkdir -p "$out"
  case "$artifact" in
    *.tar.gz | *.crate) tar -xzf "$artifact" -C "$out" ;;
    *.whl | *.jar) python3 -I -m zipfile -e "$artifact" "$out" ;;
    *) cp "$artifact" "$out/" ;;
  esac
done

"$gitleaks" dir "$work" --config "$release/gitleaks.toml" --no-banner --redact --exit-code 1
