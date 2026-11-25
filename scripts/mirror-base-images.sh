#!/usr/bin/env bash
# Copies the pinned base images that come through ECR Public into GHCR,
# digests preserved, and prints NAME=reference lines naming the copies
# (for $GITHUB_ENV). ECR Public throttles anonymous pulls from shared
# runner addresses (toomanyrequests: Data limit exceeded); GHCR, signed in
# with the workflow's token, does not. Each digest is copied once.
#
#   scripts/mirror-base-images.sh ghcr.io/OWNER/mirror
#
# Needs skopeo and a GHCR sign-in that may push to that namespace. An image
# that cannot be copied is left out, so its build pulls from ECR Public.
# With MIRROR_LOOKUP_ONLY=1 nothing is copied: only copies already there are
# named, so a job that may only read packages can still use them.
set -euo pipefail

mirror=${1:?usage: mirror-base-images.sh REGISTRY/NAMESPACE}
root=$(cd "$(dirname "$0")/.." && pwd)

# The pins stay in the files that use them; this only reads them.
# shellcheck disable=SC2016 # a literal ${POSTGRES_IMAGE:-…} in compose.yaml
declare -A sources=(
  [NODE_IMAGE]=$(sed -n 's/^ARG NODE_IMAGE=//p' "$root/docker/admin/Dockerfile")
  [POSTGRES_IMAGE]=$(sed -n 's/^ *image: \${POSTGRES_IMAGE:-\(.*\)}$/\1/p' "$root/compose.yaml")
)

for name in "${!sources[@]}"; do
  source=${sources[$name]}
  # repository:tag@sha256:digest
  [[ $source =~ ^([^@]+/([^/:@]+)):([^@]+)@(sha256:[0-9a-f]{64})$ ]] \
    || { echo "mirror-base-images: ${name} is not pinned by digest: ${source}" >&2; exit 1; }
  repository=${BASH_REMATCH[1]} image=${BASH_REMATCH[2]} tag=${BASH_REMATCH[3]}
  digest=${BASH_REMATCH[4]}
  target="${mirror}/${image}"
  if ! skopeo inspect --raw "docker://${target}@${digest}" >/dev/null 2>&1; then
    if [ "${MIRROR_LOOKUP_ONLY:-0}" = 1 ]; then
      echo "::warning::mirror-base-images: no GHCR copy of ${image}; it stays on ECR Public" >&2
      continue
    fi
    copied=false
    for attempt in 1 2 3; do
      # skopeo takes a tag or a digest, not both; the digest is the pin.
      if skopeo copy --quiet --all --preserve-digests \
        "docker://${repository}@${digest}" "docker://${target}:${tag}" >&2; then
        copied=true
        break
      fi
      echo "mirror-base-images: copying ${image} failed (attempt ${attempt})" >&2
      sleep $((attempt * 15))
    done
    if [ "$copied" != true ]; then
      # An annotation, so a fallback is seen rather than passing quietly.
      echo "::warning::mirror-base-images: ${image} could not be copied; it stays on ECR Public" >&2
      continue
    fi
  fi
  echo "${name}=${target}@${digest}"
done
