# Releases & upgrades

The URL version identifies the API contract. Before changing a client or deployment:

1. Read the release notes or change description for the target version.
2. Compare the versioned OpenAPI artifact with the one your client uses.
3. Run bounded chat, stream, embedding, and error-path checks in a non-production environment.
4. Confirm headers, terminal SSE handling, and problem parsing.
5. Roll out gradually and watch status, latency, and request IDs.

Operators upgrading a deployment should also read [migrations](/operations/index.md#migrations) and [rollback and incidents](/operations/incidents.md#a-release-misbehaves). After an upgrade, `gateway doctor` reports whether the database's migrations match the new binary (see the [troubleshooting guide](/support/troubleshooting.md)).

The inference contract is stable; see [stability and compatibility](/reference/compatibility.md).

## What is released, and where

Each gateway release is a version tag, `v<version>`, of the public `qucelerate/ai-gateway` repository on GitHub, with its release notes. From each tag:

- **Container images** are published to GitHub Container Registry, tagged with the version: `ghcr.io/qucelerate/ai-gateway` (the gateway, including the `gwctl` client CLI), `ghcr.io/qucelerate/ai-gateway-admin` (the admin console) and `ghcr.io/qucelerate/ai-gateway-docs` (this site, as a static image). Pin a deployment to a version or a digest, never to a moving tag.
- **The Helm chart** is published as `oci://ghcr.io/qucelerate/charts/ai-gateway`.
- **This documentation** is built from the same tag and published at this site, so it describes the latest release. The contract version is shown in the navigation bar.
- **Client libraries** for every language, at the same version, published to each language's registry (the Go module as `v0.YYYYMM.PATCH`); see [client libraries](/clients/index.md#versions).

Versions are calendar versions, `YYYY.M.PATCH` (2026.11.0 is the first release of November 2026), with `-rc.N` for a pre-release. A full release also moves the `YYYY.M` and `latest` image tags; a pre-release is tagged only with its version. Changes that need you to act are listed under **Breaking** in the release notes.

Moving to a newer release means running its migrations first; see [deployment](/operations/index.md#migrations).

> **Scoped:** there is no maintained release branch; fixes land in the next release.
