# The gateway: one immutable image, run as `api`, `worker` or `all-in-one`
# (GATEWAY_PROFILE), or with the `migrate` command before a release. No
# secrets or configuration are baked in; everything comes from the
# environment at run time (see .env.example and the operations guide (https://gateway.qucelerate.com/operations/)).
#
#   docker build -t gateway .

# Base images come from their publishers' registries, not Docker Hub (whose
# anonymous pull limits stop CI), pinned by digest; Docker's built-in
# Dockerfile frontend is used, so nothing is fetched to parse this file. To
# update one: `docker buildx imagetools inspect <image:tag>`, then replace
# the tag and its index digest together. A build may override a whole
# reference (--build-arg RUST_IMAGE=...), never just its tag.
# CI builds from GHCR copies of the ECR Public images, made under the same
# digests when a pin changes (scripts/mirror-base-images.sh).
#
# The Rust project's own image (same digest as Docker Hub's rust).
ARG RUST_IMAGE=ghcr.io/rust-lang/rust:1.90-slim-bookworm@sha256:64232e656c058f4468e8d024e990acff04f0fd5a5c0a88a574dc37773d7325c9

FROM ${RUST_IMAGE} AS build
WORKDIR /src
# TLS is rustls with bundled roots, so no OpenSSL; ring needs only a C
# compiler, which the image has.
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY migrations ./migrations
# Curated model card files, compiled into the binary.
COPY model-cards ./model-cards
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin gateway --bin gwctl \
    && cp target/release/gateway /gateway \
    && cp target/release/gwctl /gwctl

# Debian 12 with glibc and nothing else: no shell, no package manager, and
# a non-root user (65532).
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /gateway /usr/local/bin/gateway
# The client CLI, for operators and agents: `kubectl run ... --command -- gwctl`.
COPY --from=build /gwctl /usr/local/bin/gwctl
# Listen on every interface inside the container, IPv4 and IPv6 alike (a
# host without IPv6 falls back to IPv4); publish or proxy it.
ENV BIND_ADDR=[::]:3000
EXPOSE 3000
# Numeric, not `nonroot`: Kubernetes verifies runAsNonRoot only for a
# numeric user. 65532 is distroless's nonroot.
USER 65532:65532
# OCI metadata; a CI build adds the revision, version and its repository.
LABEL org.opencontainers.image.title="QuCelerate AI Gateway" \
      org.opencontainers.image.description="LLM gateway: inference API, admin API and worker in one image" \
      org.opencontainers.image.vendor="QuCelerate" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.url="https://gateway.qucelerate.com" \
      org.opencontainers.image.documentation="https://gateway.qucelerate.com" \
      org.opencontainers.image.source="https://github.com/qucelerate/ai-gateway"
ENTRYPOINT ["/usr/local/bin/gateway"]
