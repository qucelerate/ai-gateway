#!/usr/bin/env bash
# Builds each client's example (one chat call, one streamed call) into <out>
# and writes <out>/examples.json, the CLIENT_EXAMPLES file the functional
# suite's clients spec runs (tests/functional/specs/clients.spec.ts).
#
#   clients/release/build-examples.sh <out> [client...]   # default: all five
set -euo pipefail

mkdir -p "${1:?usage: build-examples.sh <out> [client...]}"
out="$(cd "$1" && pwd)"
shift
release="$(cd "$(dirname "$0")" && pwd)"
clients="$(dirname "$release")"
[ $# -gt 0 ] || set -- rust python go java c

entries=()
for client in "$@"; do
  version="$(python3 -I "$release/version.py" "$client")"
  cd "$clients/$client"
  case "$client" in
    rust)
      cargo build --quiet --release --locked --example chat
      command=("$clients/rust/target/release/examples/chat")
      ;;
    python)
      python3 -m venv "$out/python"
      "$out/python/bin/python" -m pip install --quiet --disable-pip-version-check .
      command=("$out/python/bin/python" -I "$clients/python/examples/chat.py")
      ;;
    go)
      go build -o "$out/go-chat" ./examples/chat
      command=("$out/go-chat")
      ;;
    java)
      mvn -B -ntp -q ${MAVEN_ARGS_EXTRA:-} -DskipTests package dependency:build-classpath \
        -Dmdep.includeScope=runtime -Dmdep.outputFile="$out/java-classpath.txt"
      mkdir -p "$out/java"
      javac -d "$out/java" -cp "target/classes:$(cat "$out/java-classpath.txt")" examples/Chat.java
      command=(java -cp "$out/java:$clients/java/target/classes:$(cat "$out/java-classpath.txt")" Chat)
      ;;
    c)
      cmake -S . -B "$out/c" -DCMAKE_BUILD_TYPE=Release -DGATEWAY_BUILD_TESTS=OFF ${CMAKE_ARGS_EXTRA:-} >/dev/null
      cmake --build "$out/c" --parallel --target gateway_chat >/dev/null
      command=("$out/c/gateway_chat")
      ;;
    *)
      echo "unknown client: $client" >&2
      exit 2
      ;;
  esac
  entries+=("$(python3 -I -c 'import json, sys; print(json.dumps({sys.argv[1]: {"version": sys.argv[2], "command": sys.argv[3:]}}))' \
    "$client" "$version" "${command[@]}")")
done
printf '%s\n' "${entries[@]}" | python3 -I -c '
import json, sys
merged = {}
for line in sys.stdin:
    merged.update(json.loads(line))
json.dump(merged, sys.stdout, indent=2)
print()' > "$out/examples.json"
cat "$out/examples.json"
