# Contributing to QuCelerate AI Gateway

Thanks for helping. Bug reports, documentation fixes and pull requests are
all welcome. Please read this first: how changes reach a release is a little
different from most repositories.

## How this repository works

`qucelerate/ai-gateway` is the public release repository. Development happens
in a separate repository, and each release lands here as **one commit**,
`Release vX.Y.Z`, holding that release's whole tree. A pull request here is
therefore not merged as-is into a development branch; instead:

1. You open a pull request against `main` here, as usual. CI runs on it.
2. A maintainer reviews it here. Once it is ready, the maintainer applies your
   commits to the development repository (`git format-patch` / `git am`),
   keeping you as the author, or crediting you with a `Co-authored-by:`
   trailer if the change had to be adapted.
3. The pull request is then merged here (or closed with a link to the ported
   change), and your change ships in the next `Release` commit, with a
   line in [CHANGELOG.md](CHANGELOG.md).

So the history of `main` is release commits, plus the pull requests merged
between them. Each release commit's tree supersedes everything before it.

## Before you start

- For anything larger than a small fix, open an issue first so we can agree
  on the approach.
- Security issues: never in a public issue or pull request. See
  [SECURITY.md](SECURITY.md).
- Be kind: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Sign your commits (DCO)

Every commit must carry a `Signed-off-by:` line, certifying the
[Developer Certificate of Origin 1.1](https://developercertificate.org/):
that you wrote the change, or otherwise have the right to submit it under
the project's MIT license.

```sh
git commit -s -m "Fix the thing"
```

The sign-off must use your real name (or the name you are known by) and an
email address you can be reached at. Commits without it cannot be ported.

## Building and testing

You need stable Rust (`rustup`), Docker with Compose v2 (for PostgreSQL), and
Node.js 22.22.2 or later for the web apps. These are the checks CI runs; run
the ones your change touches before you open a pull request.

### Gateway (Rust)

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Without a database the integration tests in `tests/` skip themselves. To run
them, start PostgreSQL from `compose.yaml` (it creates the least-privilege
`gateway_app` role from your `.env`; see the README's quickstart for the
placeholders) and point the tests at it:

```sh
cp .env.example .env    # set POSTGRES_ADMIN_PASSWORD and GATEWAY_APP_PASSWORD
docker compose up -d postgres
export GATEWAY_TEST_DATABASE_URL="postgres://gateway_app:<GATEWAY_APP_PASSWORD>@localhost:5432/gateway"
cargo test --all-targets
```

Tests never call real model providers: provider adapters are tested against
local fakes. Please keep it that way.

Dependency and contract checks:

```sh
cargo audit                                   # exceptions in .cargo/audit.toml
cargo deny check licenses advisories bans sources   # policy in deny.toml
python3 openapi/generate.py --check           # published OpenAPI is in sync
python3 -I scripts/check-model-card-dates.py  # model-card facts are fresh
```

### Admin console (`apps/admin`)

```sh
cd apps/admin
npm ci
npx tsc -b
npx vitest run
npm run build
```

### Documentation site (`apps/docs`)

```sh
cd apps/docs
npm ci
npm run check:contract
npm run build
```

### Client libraries (`clients/`)

Each client's README has its own commands; in short:

| Client | Checks |
| --- | --- |
| Rust | `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all-targets --locked` (in `clients/rust`) |
| Python | `python -m pip install -e ".[test]"`, `python -m pytest -q` (in `clients/python`) |
| Go | `gofmt -l .`, `go vet ./...`, `go test -race ./...` (in `clients/go`) |
| Java | `mvn -B -ntp verify` (in `clients/java`) |
| C/C++ | `cmake -B build`, `cmake --build build --parallel`, `ctest --test-dir build --output-on-failure` (in `clients/c`) |

### Functional tests

`tests/functional/run.sh` runs the browser suite (Playwright) against its own
Compose stack with fake providers; see `tests/functional/README.md`.

## Pull requests

- Keep a pull request to one change, with tests for new behavior and docs for
  anything user-visible (`apps/docs`, `openapi/`, `.env.example`).
- Describe what changed and why, and how you tested it; the pull request
  template lists the checks.
- Database changes are new files in `migrations/`; never edit a released
  migration.
- Don't commit secrets, real account IDs or real hostnames. CI scans for
  secrets with gitleaks (`.gitleaks.toml`); use obvious placeholders
  (`123456789012`, `example-project-123456`, `00000000-0000-4000-8000-000000000001`).

## Releases

Maintainers cut releases from the development repository: a CHANGELOG entry,
a `vX.Y.Z` tag, and the exported tree pushed here as one commit with the same
tag. The release workflows in this repository then publish the container
images, the Helm chart, the client packages and the documentation site.

## License

By contributing you agree that your contribution is licensed under the
project's [MIT license](LICENSE).
