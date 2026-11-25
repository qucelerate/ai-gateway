#!/usr/bin/env bash
# Installs a client's release artifacts from <dist> into a clean project in a
# temporary directory and runs smoke/<client>, which makes one call through
# the installed package. It proves the artifact itself is usable: its files,
# its dependency metadata and its native linking, not the source tree's.
#
#   clients/release/smoke.sh <rust|python|go|java|c> <dist>
set -euo pipefail

client="${1:?usage: smoke.sh <client> <dist>}"
dist="$(cd "${2:?usage: smoke.sh <client> <dist>}" && pwd)"
release="$(cd "$(dirname "$0")" && pwd)"
smoke="$release/smoke/$client"
version="$(python3 -I "$release/version.py" "$client")"
work="$(mktemp -d)"
trap 'chmod -R u+w "$work"; rm -rf -- "$work"' EXIT

case "$client" in
  rust)
    mkdir -p "$work/package" "$work/app/src"
    tar -xzf "$dist/qucelerate-ai-gateway-$version.crate" -C "$work/package"
    cat > "$work/app/Cargo.toml" <<EOF
[package]
name = "gateway-smoke"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
qucelerate-ai-gateway = { path = "$work/package/qucelerate-ai-gateway-$version" }
tokio = { version = "1", features = ["macros", "rt"] }
EOF
    cp "$smoke/main.rs" "$work/app/src/main.rs"
    cargo run --quiet --manifest-path "$work/app/Cargo.toml"
    ;;
  python)
    python3 -m venv "$work/venv"
    # Only the wheel, never the source tree: a clean environment and the
    # wheel's declared dependencies.
    "$work/venv/bin/python" -m pip install --quiet --disable-pip-version-check \
      "$dist/qucelerate_ai_gateway-$version-py3-none-any.whl"
    (cd "$work" && "$work/venv/bin/python" -I "$smoke/smoke.py")
    ;;
  go)
    mkdir -p "$work/app"
    tar -xzf "$dist/qucelerate-ai-gateway-go-$version.tar.gz" -C "$work"
    cp "$smoke/main.go" "$work/app/"
    (
      cd "$work/app"
      export GOFLAGS=-mod=mod GOWORK=off
      go mod init example.com/smoke >/dev/null 2>&1
      go mod edit -require="github.com/qucelerate/ai-gateway/clients/go@v$version" \
        -replace="github.com/qucelerate/ai-gateway/clients/go=$work/qucelerate-ai-gateway-go-$version"
      go run .
    )
    ;;
  java)
    # A fresh local repository holding the released jar and pom, so the
    # dependency resolves from the artifacts and Maven Central alone.
    repo=(-Dmaven.repo.local="$work/m2")
    mvn -B -ntp -q "${repo[@]}" install:install-file -Dfile="$dist/qucelerate-ai-gateway-$version.jar" \
      -DpomFile="$dist/qucelerate-ai-gateway-$version.pom" -Dsources="$dist/qucelerate-ai-gateway-$version-sources.jar"
    mkdir -p "$work/app/src/main/java"
    cp "$smoke/Smoke.java" "$work/app/src/main/java/"
    cat > "$work/app/pom.xml" <<EOF
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>example</groupId>
  <artifactId>smoke</artifactId>
  <version>0</version>
  <properties>
    <maven.compiler.release>17</maven.compiler.release>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
  </properties>
  <build>
    <plugins>
      <!-- The client's own plugin version: an older Maven's default ignores
           maven.compiler.release and compiles for Java 5. -->
      <plugin>
        <groupId>org.apache.maven.plugins</groupId>
        <artifactId>maven-compiler-plugin</artifactId>
        <version>3.16.0</version>
      </plugin>
    </plugins>
  </build>
  <dependencies>
    <dependency>
      <groupId>com.qucelerate</groupId>
      <artifactId>qucelerate-ai-gateway</artifactId>
      <version>$version</version>
    </dependency>
  </dependencies>
</project>
EOF
    (
      cd "$work/app"
      mvn -B -ntp -q "${repo[@]}" compile dependency:build-classpath -Dmdep.outputFile=classpath.txt \
        -Dmdep.includeScope=runtime
      java -cp "target/classes:$(cat classpath.txt)" Smoke
    )
    ;;
  c)
    tar -xzf "$dist/qucelerate-ai-gateway-c-$version.tar.gz" -C "$work"
    cmake -S "$work/qucelerate-ai-gateway-c-$version" -B "$work/build" -DCMAKE_BUILD_TYPE=Release \
      -DGATEWAY_BUILD_TESTS=OFF ${CMAKE_ARGS_EXTRA:-} >/dev/null
    cmake --build "$work/build" --parallel >/dev/null
    cmake --install "$work/build" --prefix "$work/prefix" >/dev/null
    lib="$(find "$work/prefix" -name 'libqucelerate-ai-gateway.so.0' -printf '%h\n' | head -1)"
    # Through the CMake package...
    cmake -S "$smoke" -B "$work/smoke" -DCMAKE_PREFIX_PATH="$work/prefix" >/dev/null
    cmake --build "$work/smoke" >/dev/null
    LD_LIBRARY_PATH="$lib" "$work/smoke/smoke_c"
    LD_LIBRARY_PATH="$lib" "$work/smoke/smoke_cpp"
    # ...and through pkg-config.
    export PKG_CONFIG_PATH="$lib/pkgconfig"
    cc -std=c11 -o "$work/smoke_pc" "$smoke/smoke.c" $(pkg-config --cflags --libs qucelerate-ai-gateway)
    LD_LIBRARY_PATH="$lib" "$work/smoke_pc"
    ;;
  *)
    echo "unknown client: $client" >&2
    exit 2
    ;;
esac
