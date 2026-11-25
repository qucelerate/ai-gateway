#!/usr/bin/env bash
# Reads and sets tags of an image (or OCI Helm chart) in GHCR by digest,
# through the registry API: nothing is pulled, rebuilt or re-pushed, so a
# tag points at exactly the manifest that was built, tested and signed.
#
#   scripts/registry-tag.sh digest ghcr.io/<owner>/<name> <tag>
#       prints the digest <tag> points at, or nothing when it does not exist
#   scripts/registry-tag.sh tag ghcr.io/<owner>/<name> sha256:<digest> <tag>...
#       points each <tag> at <digest>
#
# Needs curl, jq and sha256sum, and GH_TOKEN (the workflow's token, with
# packages: write for `tag`); GITHUB_ACTOR names its user.
#
# The release workflow (.github/workflows/release.yml) uses it so a re-run
# finishes a partial release: a tag already at the digest is left alone. A
# version tag (X.Y.Z or X.Y.Z-pre) never moves once it exists: pointing it
# at a different digest fails. Other tags (YYYY.M, latest) move.
set -euo pipefail

accept="application/vnd.oci.image.index.v1+json,application/vnd.oci.image.manifest.v1+json"
accept="$accept,application/vnd.docker.distribution.manifest.list.v2+json"
accept="$accept,application/vnd.docker.distribution.manifest.v2+json"

usage() {
  echo "usage: registry-tag.sh digest ghcr.io/<owner>/<name> <tag>" >&2
  echo "       registry-tag.sh tag ghcr.io/<owner>/<name> sha256:<digest> <tag>..." >&2
  exit 2
}

[ $# -ge 3 ] || usage
command=$1 image=$2
shift 2
case "$image" in
  ghcr.io/*/*) repository=${image#ghcr.io/} ;;
  *) echo "registry-tag.sh: $image is not a ghcr.io image" >&2; exit 2 ;;
esac
: "${GH_TOKEN:?GH_TOKEN must hold a token that can read (and for tag, write) the package}"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

scope=pull
[ "$command" = tag ] && scope=pull,push
token=$(curl -fsS -u "${GITHUB_ACTOR:-token}:${GH_TOKEN}" \
  "https://ghcr.io/token?service=ghcr.io&scope=repository:${repository}:${scope}" | jq -r .token)
[ -n "$token" ] && [ "$token" != null ] || { echo "registry-tag.sh: no registry token for $repository" >&2; exit 1; }
api="https://ghcr.io/v2/${repository}/manifests"

# The digest a tag points at, or nothing (a missing tag, or a package that
# does not exist yet: GHCR answers 401 or 404 for one).
digest_of() {
  local status
  status=$(curl -sS -o /dev/null -D "$work/headers" -w '%{http_code}' -I \
    -H "Authorization: Bearer $token" -H "Accept: $accept" "$api/$1")
  case "$status" in
    200) tr -d '\r' <"$work/headers" | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: *//p' | head -1 ;;
    401 | 403 | 404) ;;
    *) echo "registry-tag.sh: HEAD $image:$1 answered $status" >&2; return 1 ;;
  esac
}

is_version() { [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; }

case "$command" in
  digest)
    [ $# -eq 1 ] || usage
    digest_of "$1"
    ;;
  tag)
    [ $# -ge 2 ] || usage
    digest=$1
    shift
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "registry-tag.sh: $digest is not a sha256 digest" >&2; exit 2; }
    # The manifest's exact bytes and media type, so the tag's digest is the same.
    curl -fsS -D "$work/headers" -o "$work/manifest" \
      -H "Authorization: Bearer $token" -H "Accept: $accept" "$api/$digest"
    type=$(tr -d '\r' <"$work/headers" | sed -n 's/^[Cc]ontent-[Tt]ype: *//p' | head -1)
    [ "sha256:$(sha256sum "$work/manifest" | cut -d' ' -f1)" = "$digest" ] ||
      { echo "registry-tag.sh: the manifest GHCR returned for $digest does not hash to it" >&2; exit 1; }
    for tag in "$@"; do
      current=$(digest_of "$tag")
      if [ "$current" = "$digest" ]; then
        echo "$image:$tag already points at $digest"
        continue
      fi
      if [ -n "$current" ] && is_version "$tag"; then
        echo "::error::$image:$tag already points at $current, not $digest; a version tag never moves" >&2
        exit 1
      fi
      curl -fsS -o /dev/null -X PUT -H "Authorization: Bearer $token" -H "Content-Type: $type" \
        --data-binary "@$work/manifest" "$api/$tag"
      [ "$(digest_of "$tag")" = "$digest" ] ||
        { echo "registry-tag.sh: $image:$tag does not point at $digest after the push" >&2; exit 1; }
      echo "$image:$tag -> $digest${current:+ (was $current)}"
    done
    ;;
  *) usage ;;
esac
