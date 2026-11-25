#!/usr/bin/env bash
# Builds one client's release artifacts into a directory.
#
#   clients/release/package.sh <rust|python|go|java|c> <dist>
#
# The artifacts, each named for the client and its version:
#   rust    qucelerate-ai-gateway-<v>.crate (cargo package, which also builds it)
#   python  qucelerate_ai_gateway-<v>.tar.gz and the wheel built from it,
#           qucelerate_ai_gateway-<v>-py3-none-any.whl
#   go      qucelerate-ai-gateway-go-<v>.tar.gz, the module's committed files
#   java    qucelerate-ai-gateway-<v>.jar, -sources.jar and .pom
#   c       qucelerate-ai-gateway-c-<v>.tar.gz, the library's committed source
#
# Timestamps come from the last commit (SOURCE_DATE_EPOCH), so a rebuild of
# the same commit gives the same bytes. Source archives hold only files git
# tracks: nothing untracked or ignored can leak into a release.
set -euo pipefail

client="${1:?usage: package.sh <client> <dist>}"
mkdir -p "${2:?usage: package.sh <client> <dist>}"
dist="$(cd "$2" && pwd)"
release="$(cd "$(dirname "$0")" && pwd)"
clients="$(dirname "$release")"
version="$(python3 -I "$release/version.py" "$client")"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$clients" log -1 --format=%ct)}"
cd "$clients/$client"
if [ -n "$(git status --porcelain -- .)" ] && [ "${ALLOW_DIRTY:-0}" != 1 ]; then
  echo "clients/$client has uncommitted changes; a release is built from commits (ALLOW_DIRTY=1 to try anyway)" >&2
  exit 1
fi

# The committed files of this client, as a reproducible tar.gz under <prefix>/.
source_archive() {
  local prefix="$1" out="$2"
  git ls-files -z -- . | tar --null --files-from=- --sort=name --mtime="@$SOURCE_DATE_EPOCH" \
    --owner=0 --group=0 --numeric-owner --mode='u+rw,go+r,go-w' --transform "s,^,$prefix/," -cf - |
    gzip -n -9 > "$out"
}

case "$client" in
  rust)
    cargo package --locked --quiet
    cp "target/package/qucelerate-ai-gateway-$version.crate" "$dist/"
    ;;
  python)
    # The sdist first, then the wheel built from it: a file the sdist misses
    # breaks the wheel, and so the smoke test.
    python3 -m build --outdir "$dist" . >/dev/null
    ;;
  go)
    source_archive "qucelerate-ai-gateway-go-$version" "$dist/qucelerate-ai-gateway-go-$version.tar.gz"
    ;;
  java)
    mvn -B -ntp -q ${MAVEN_ARGS_EXTRA:-} -DskipTests -Dproject.build.outputTimestamp="$SOURCE_DATE_EPOCH" package
    cp "target/qucelerate-ai-gateway-$version.jar" "target/qucelerate-ai-gateway-$version-sources.jar" "$dist/"
    cp pom.xml "$dist/qucelerate-ai-gateway-$version.pom"
    ;;
  c)
    source_archive "qucelerate-ai-gateway-c-$version" "$dist/qucelerate-ai-gateway-c-$version.tar.gz"
    ;;
  *)
    echo "unknown client: $client" >&2
    exit 2
    ;;
esac
ls -1 "$dist"
