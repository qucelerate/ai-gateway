# Client release tooling

Scripts that build, check and smoke-test the release artifacts of every
client, and the helper that keeps one version across every component. CI
runs them on each change (`.github/workflows/clients-release-gate.yml`), and
a release runs the same ones, so what is tested is what ships.

Development happens in the private repository (`qucelerate/gateway`);
each release is exported to the public one (`qucelerate/ai-gateway`), and
the same workflow files run in both. Only the public repository releases:
`.github/workflows/release.yml` runs there on a `v<version>` tag and
publishes everything (*Releasing*, below).

| Client | Package | Registry |
| --- | --- | --- |
| rust | `qucelerate-ai-gateway` (crate `qucelerate_ai_gateway`) | crates.io |
| python | `qucelerate-ai-gateway` (import `qucelerate_ai_gateway`) | PyPI |
| java | `com.qucelerate:qucelerate-ai-gateway` (package `com.qucelerate.aigateway`) | Maven Central |
| go | `github.com/qucelerate/ai-gateway/clients/go` (package `gateway`) | the Go module proxy |
| c | `libqucelerate-ai-gateway`, pkg-config `qucelerate-ai-gateway`, CMake `qucelerate::ai-gateway` | GitHub releases (source archive) |

| Script | What it does |
| --- | --- |
| `release.py` | The release version: `check v<version>` (a tag against every component's declarations and changelogs, printing the image tags, the Go module tag and the PyPI spelling), `current`, `notes <version>`, `bump [<version>]`. Tested by `test_release.py`. |
| `version.py <client>` | Prints the client's version after checking every declaration agrees (manifest, User-Agent constant, header macros). |
| `package.sh <client> <dist>` | Builds the client's artifacts into `<dist>` from its committed files, with timestamps from the last commit, so a rebuild of a commit gives the same bytes. Refuses a client with uncommitted changes (`ALLOW_DIRTY=1` to try anyway). |
| `smoke.sh <client> <dist>` | Installs the artifacts into a clean project in a temporary directory and runs `smoke/<client>`, which calls a closed port and expects the client's transport error. |
| `build-examples.sh <out> [client...]` | Builds each client's example and writes `<out>/examples.json`, which the functional suite's clients spec runs against a gateway. |
| `changelog.py <client> [version]` | Prints the client's `CHANGELOG.md` section for the version (default: the declared one); fails if it is missing or empty. |
| `compatibility.py ...` | Checks that compatibility results show the client passing against every gateway listed, and copies them into the release's artifacts. |
| `sign.sh <dist>` | Signs a release with the clients' cosign key: a SLSA v1 provenance statement per artifact and a signature on `SHA256SUMS` (needs `COSIGN_PRIVATE_KEY` and `COSIGN_PASSWORD`). |
| `verify.sh <dir> [cosign.pub]` | Verifies a downloaded release: the `SHA256SUMS` signature, every checksum, and each artifact's provenance. |
| `scan.sh <dist>` | Unpacks every artifact and runs gitleaks over them and the committed client sources, with `gitleaks.toml` (the default rules plus the gateway's `gw_` API keys). |

## Versions

Every client carries the gateway's version: a calendar version
`YYYY.M.PATCH` (`2026.11.0`; PATCH counts the releases of the month), with
`-rc.N` for a pre-release. `release.py` is the one place that knows the
scheme, and `release.py bump <version>` writes it everywhere. Each client's
`CHANGELOG.md` has a `## <version>` section for every release, with a
`### Breaking` heading for changes that need code changes.

The Go module cannot use a major version of 2 or more without a `/v2`
path suffix, so its versions are `v0.<YYYY><MM>.<PATCH>` with a two-digit
month (`2026.11.0` is `v0.202611.0`, `2027.1.2` is `v0.202701.2`), from
`clients/go/v0...` tags that the release creates. Rust users should pin the
exact version (`"=2026.11.0"`): cargo treats every release of a year as
compatible.

## Compatibility

`.github/workflows/clients-compatibility.yml` runs every client's example
(one chat call, one streamed call) against each gateway version in
`gateways.txt`: the published GHCR images on the functional suite's Compose
stack, with Bedrock faked (`tests/functional`, *The client libraries*). It
runs on client changes, daily, and on demand, and keeps each run's result,
naming every client and gateway version, as the `compatibility-<version>`
artifact. A release calls it with the release's candidate images instead
(inputs `gateway_image` and `admin_image`), and attaches the result.

## Releasing

Clients are no longer released on their own: one tag, `v<version>`, releases
the gateway and every client (`docs/release-process.md` in the private
repository has the whole procedure). In the public repository,
`.github/workflows/release.yml` checks the tag (`release.py check`) and that
the commit is on main, runs the release gate for every client and the
compatibility run against the candidate images, and then makes one GitHub
release, `v<version>`, holding every client's artifacts,
`compatibility-<version>.json` and `SHA256SUMS`, with every component's
changelog section as its notes. A pre-release (`-rc.N`) becomes a GitHub
pre-release; a full release becomes the repository's latest.

### Signatures and provenance

Each artifact `X` in a release comes with `X.provenance.sigstore.json`, a
cosign bundle holding a signed [SLSA v1 provenance](https://slsa.dev/provenance/v1)
statement: the artifact's digest, the repository, the tagged commit and the
workflow run that built it. `SHA256SUMS`, which also covers the
compatibility result, is signed in `SHA256SUMS.sigstore.json`. To check a
download (cosign 2.4 or later):

```sh
gh release download v2026.11.0 --repo qucelerate/ai-gateway --dir /tmp/release-2026.11.0
clients/release/verify.sh /tmp/release-2026.11.0   # a directory holding only the release
```

The signing key is the project's own: its public half is
`clients/release/cosign.pub`. The private half and its password are the
`client-release` environment's secrets, which only the release job, run
from a `v*` tag, can read. Nothing is uploaded to a transparency log (the
signing config names no services), so verification skips that check and
the key is the trust root. The provenance is stated by the release job
itself, on GitHub's hosted runners, not by an isolated builder.

If the key must be replaced, generate a new pair (`cosign generate-key-pair`),
replace the two secrets and `cosign.pub` in one change, and keep the old
public key with the releases it signed.

## Registries

After the GitHub release (which verifies the signatures) succeeds,
`release.yml` publishes each client; each step first checks whether the
version is already there and skips it, so a re-run completes a partial
release:

| Client | How | Environment | Credentials | Skipped when |
| --- | --- | --- | --- | --- |
| rust | `cargo publish --locked` | `crates-io` | crates.io trusted publishing (`rust-lang/crates-io-auth-action`, OIDC); the secret `CARGO_REGISTRY_TOKEN` instead, if set | the crates.io API has the version |
| python | the gate's wheel and sdist, `pypa/gh-action-pypi-publish` (`skip-existing`) | `pypi` | PyPI trusted publishing (OIDC), no token | the PyPI JSON API lists both files |
| java | `mvn -P release deploy` (javadoc jar, GPG signatures, `central-publishing-maven-plugin`) | `maven-central` | secrets `MAVEN_CENTRAL_USERNAME`, `MAVEN_CENTRAL_PASSWORD` (a Central Portal user token), `MAVEN_GPG_PRIVATE_KEY` (armored), `MAVEN_GPG_PASSPHRASE` | the pom is on repo1.maven.org, or the Central Portal reports it published |
| go | creates the tag `clients/go/v0.<YYYY><MM>.<PATCH>` on the tagged commit, then fetches `https://proxy.golang.org/github.com/qucelerate/ai-gateway/clients/go/@v/<module version>.info` so the proxy records it | none | the workflow's token (`contents: write`) | the tag exists on the same commit |
| c | the GitHub release only | `client-release` | the cosign key, as above | the release has the asset |

Registry publishing cannot be undone (a crates.io, PyPI or Maven Central
version can never be reused). The trusted publishers on PyPI and crates.io
name the workflow: `release.yml`, environment `pypi` or `crates-io`.

**The first crates.io publish.** crates.io trusted publishing can only be
configured for a crate that exists. For the very first version, put a
crates.io API token (scope `publish-new`, for `qucelerate-ai-gateway`) in
the `crates-io` environment as `CARGO_REGISTRY_TOKEN`; the job uses it
instead of OIDC. Then add the trusted publisher on crates.io (owner
`qucelerate`, repository `ai-gateway`, workflow `release.yml`, environment
`crates-io`), delete the secret, and revoke the token.

To publish the Go module by hand instead (the workflow does the same):

```sh
git tag clients/go/v0.202611.0 v2026.11.0^{commit}
git push origin clients/go/v0.202611.0
curl -fsS https://proxy.golang.org/github.com/qucelerate/ai-gateway/clients/go/@v/v0.202611.0.info
```

Never push a `clients/go/v*` tag in the private repository: the module path
names the public one.

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
python3 -m unittest discover -s clients/release -p 'test_*.py'
```

Each needs that client's toolchain (cargo, python with `build`, go, mvn and
a JDK 17+, or cmake, a C/C++ compiler, libcurl's headers and pkg-config).
