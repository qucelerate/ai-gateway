# Client release tooling

Scripts that build, check and smoke-test the release artifacts of every
client. CI runs them on each change (`.github/workflows/clients-release-gate.yml`),
and a release runs the same ones, so what is tested is what ships.

Development happens in the private repository (`qucelerate/gateway`);
each release is exported to the public one (`qucelerate/ai-gateway`), and
the same workflow files run in both. Only the public repository publishes
to package registries (*Registries*, below); in the private one a client
tag makes a GitHub release and nothing else.

| Client | Package | Registry |
| --- | --- | --- |
| rust | `qucelerate-ai-gateway` (crate `qucelerate_ai_gateway`) | crates.io |
| python | `qucelerate-ai-gateway` (import `qucelerate_ai_gateway`) | PyPI |
| java | `com.qucelerate:qucelerate-ai-gateway` (package `com.qucelerate.aigateway`) | Maven Central |
| go | `github.com/qucelerate/ai-gateway/clients/go` (package `gateway`) | the Go module proxy |
| c | `libqucelerate-ai-gateway`, pkg-config `qucelerate-ai-gateway`, CMake `qucelerate::ai-gateway` | GitHub releases (source archive) |

| Script | What it does |
| --- | --- |
| `version.py <client>` | Prints the client's version after checking every declaration agrees (manifest, User-Agent constant, header macros). |
| `package.sh <client> <dist>` | Builds the client's artifacts into `<dist>` from its committed files, with timestamps from the last commit, so a rebuild of a commit gives the same bytes. Refuses a client with uncommitted changes (`ALLOW_DIRTY=1` to try anyway). |
| `smoke.sh <client> <dist>` | Installs the artifacts into a clean project in a temporary directory and runs `smoke/<client>`, which calls a closed port and expects the client's transport error. |
| `build-examples.sh <out> [client...]` | Builds each client's example and writes `<out>/examples.json`, which the functional suite's clients spec runs against a gateway. |
| `changelog.py <client> [version]` | Prints the client's `CHANGELOG.md` section for the version (default: the declared one); fails if it is missing or empty. |
| `tag.py client-<client>-v<version>` | Checks a release tag against the client's declared version and changelog. |
| `compatibility.py ...` | Checks that a release's compatibility results show the client passing against every gateway in `gateways.txt`, and copies them into its artifacts. |
| `sign.sh <dist>` | Signs a release with the clients' cosign key: a SLSA v1 provenance statement per artifact and a signature on `SHA256SUMS` (needs `COSIGN_PRIVATE_KEY` and `COSIGN_PASSWORD`). |
| `verify.sh <dir> [cosign.pub]` | Verifies a downloaded release: the `SHA256SUMS` signature, every checksum, and each artifact's provenance. |
| `scan.sh <dist>` | Unpacks every artifact and runs gitleaks over them and the committed client sources, with `gitleaks.toml` (the default rules plus the gateway's `gw_` API keys). |

## Compatibility

`.github/workflows/clients-compatibility.yml` runs every client's example
(one chat call, one streamed call) against each gateway version in
`gateways.txt`: the published GHCR images on the functional suite's Compose
stack, with Bedrock faked (`tests/functional`, *The client libraries*). It
runs on client changes, daily, and on demand, and keeps each run's result,
naming every client and gateway version, as the `compatibility-<version>`
artifact.

## Releasing

A client is released on its own, by pushing a tag
`client-<client>-v<version>` (for example `client-python-v0.2.0`) on a commit
on main:

1. In a pull request, bump the version everywhere `version.py` reads it, and
   add a `## <version>` section (optionally `## <version> - YYYY-MM-DD`) at
   the top of the client's `CHANGELOG.md`. The release gate fails until both
   agree.
2. Once it is merged, tag the merge commit and push the tag:

   ```sh
   git tag -a client-python-v0.2.0 -m "Python client 0.2.0" <commit>
   git push origin client-python-v0.2.0
   ```

   Push release tags one at a time: GitHub starts no workflow for a push
   of more than three tags at once.

In the private repository, `.github/workflows/clients-release.yml` then
checks the tag (`tag.py`) and
that the commit is on main, runs the release gate and the compatibility run
for that client, and, only if both pass, creates a GitHub release named
for the tag. The release holds the gate's artifacts, one
`compatibility-<gateway>.json` per supported gateway, and `SHA256SUMS`, and
its notes are the changelog section. A version with a pre-release suffix
(`0.2.0-rc.1`) becomes a pre-release. Each client has its own versions, so
the workflow never marks a release as the repository's latest; GitHub still
shows the most recent full release as "Latest", whichever client it is.
Find a client's newest release by its tag prefix
(`gh release list --exclude-pre-releases --limit 1000 | grep -m1 client-python-`;
the list is newest first, and `--limit` lifts its default of 30).

### Signatures and provenance

Each artifact `X` in a release comes with `X.provenance.sigstore.json`, a
cosign bundle holding a signed [SLSA v1 provenance](https://slsa.dev/provenance/v1)
statement: the artifact's digest, the repository, the tagged commit and the
workflow run that built it. `SHA256SUMS`, which also covers the
compatibility results, is signed in `SHA256SUMS.sigstore.json`. To check a
download (cosign 2.4 or later):

```sh
gh release download client-python-v0.1.0 --dir /tmp/python-0.1.0
clients/release/verify.sh /tmp/python-0.1.0   # a directory holding only the release
```

The signing key is the project's own: its public half is
`clients/release/cosign.pub`. The private half and its password are the
`client-release` environment's secrets, which only the release job, run
from a `client-*-v*` tag, can read. Nothing is uploaded to a transparency
log (the signing config names no services), so verification skips that
check and the key is the trust root. The provenance is stated by the
release job itself, on GitHub's hosted runners, not by an isolated builder.

If the key must be replaced, generate a new pair (`cosign generate-key-pair`),
replace the two secrets and `cosign.pub` in one change, and keep the old
public key with the releases it signed.

A Go module proxy only reads `clients/go/v<version>` tags as versions of
the Go module, so a `client-go-v*` tag alone never publishes it. In the
public repository the release workflow adds that tag itself (*Registries*);
never push one in the private repository.

## Registries

`.github/workflows/release-clients.yml` runs on the same `client-*-v*` tags,
but every job is guarded with `github.repository == 'qucelerate/ai-gateway'`,
so it does nothing in the private repository (where `clients-release.yml`
runs instead, guarded the other way). In the public repository it checks the
tag (`tag.py`, and that the commit is on main), runs the release gate, makes
the signed GitHub release as above (without the compatibility results: the
private release ran those against the private images), and then publishes
the client:

| Client | How | Environment | Credentials |
| --- | --- | --- | --- |
| rust | `cargo publish --locked` | `crates-io` | crates.io trusted publishing (`rust-lang/crates-io-auth-action`, OIDC); the secret `CARGO_REGISTRY_TOKEN` instead, if set |
| python | the gate's wheel and sdist, `pypa/gh-action-pypi-publish` | `pypi` | PyPI trusted publishing (OIDC), no token |
| java | `mvn -P release deploy` (javadoc jar, GPG signatures, `central-publishing-maven-plugin`) | `maven-central` | secrets `MAVEN_CENTRAL_USERNAME`, `MAVEN_CENTRAL_PASSWORD` (a Central Portal user token), `MAVEN_GPG_PRIVATE_KEY` (armored), `MAVEN_GPG_PASSPHRASE` |
| go | creates the tag `clients/go/v<version>` on the tagged commit, then fetches `https://proxy.golang.org/github.com/qucelerate/ai-gateway/clients/go/@v/v<version>.info` so the proxy records it | none | the workflow's token (`contents: write`) |
| c | the GitHub release only | `client-release` | the cosign key, as above |

Registry publishing cannot be undone (a crates.io, PyPI or Maven Central
version can never be reused), so it runs only after the GitHub release
step, which verifies the signatures, succeeds. A failed publish can be
rerun from the workflow run (*Re-run failed jobs*).

**The first crates.io publish.** crates.io trusted publishing can only be
configured for a crate that exists. For the very first version, put a
crates.io API token (scope `publish-new`, for `qucelerate-ai-gateway`) in
the `crates-io` environment as `CARGO_REGISTRY_TOKEN`; the job uses it
instead of OIDC. Then add the trusted publisher on crates.io (owner
`qucelerate`, repository `ai-gateway`, workflow `release-clients.yml`,
environment `crates-io`), delete the secret, and revoke the token.

To publish the Go module by hand instead (the workflow does the same):

```sh
git tag clients/go/v0.2.0 client-go-v0.2.0^{commit}
git push origin clients/go/v0.2.0
curl -fsS https://proxy.golang.org/github.com/qucelerate/ai-gateway/clients/go/@v/v0.2.0.info
```

## Artifacts

| Client | Artifacts |
| --- | --- |
| rust | `qucelerate-ai-gateway-<v>.crate` |
| python | `qucelerate_ai_gateway-<v>-py3-none-any.whl`, `qucelerate_ai_gateway-<v>.tar.gz` |
| go | `qucelerate-ai-gateway-go-<v>.tar.gz` (the module's files) |
| java | `qucelerate-ai-gateway-<v>.jar`, `-sources.jar`, `.pom` |
| c | `qucelerate-ai-gateway-c-<v>.tar.gz` (the library's source, built with CMake) |

Each carries the MIT license: every client directory holds a copy of the
repository's `LICENSE`, and the gate fails if one drifts.

## Locally

```sh
clients/release/package.sh python /tmp/dist/python
clients/release/smoke.sh python /tmp/dist/python
GITLEAKS=/path/to/gitleaks clients/release/scan.sh /tmp/dist/python
```

Each needs that client's toolchain (cargo, python with `build`, go, mvn and
a JDK 17+, or cmake, a C/C++ compiler, libcurl's headers and pkg-config).
