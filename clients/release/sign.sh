#!/usr/bin/env bash
# Signs a release directory with the clients' cosign key.
#
#   COSIGN_PRIVATE_KEY=... COSIGN_PASSWORD=... clients/release/sign.sh <dist>
#
# For each artifact (everything but SHA256SUMS, the compatibility results
# and existing bundles) it writes <artifact>.provenance.sigstore.json, a
# signed SLSA v1 provenance statement naming the artifact's digest, the
# repository, the commit and the workflow run that built it. SHA256SUMS,
# which covers every file including the compatibility results, gets a
# signature bundle, SHA256SUMS.sigstore.json.
#
# Nothing is sent to a transparency log or timestamp authority: the
# signing config names no services. The public key that verifies these is
# clients/release/cosign.pub (verify.sh).
set -euo pipefail

dist="$(cd "${1:?usage: sign.sh <dist>}" && pwd)"
cosign="${COSIGN:-cosign}"
: "${COSIGN_PRIVATE_KEY:?COSIGN_PRIVATE_KEY must hold the private key}"
: "${COSIGN_PASSWORD:?COSIGN_PASSWORD must hold its password}"
export COSIGN_YES=true
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

"$cosign" signing-config create --out "$work/signing-config.json" >/dev/null

# The provenance predicate, from the GitHub Actions run (placeholders
# outside one, for a local trial).
python3 -I - > "$work/predicate.json" <<'PY'
import json, os
env = os.environ.get
server = env("GITHUB_SERVER_URL", "https://github.com")
repo = env("GITHUB_REPOSITORY", "local/local")
ref = env("GITHUB_REF", "refs/heads/local")
workflow = env("GITHUB_WORKFLOW_REF", f"{repo}/.github/workflows/release.yml@{ref}")
path = workflow.split("@", 1)[0].removeprefix(repo + "/")
run = f"{server}/{repo}/actions/runs/{env('GITHUB_RUN_ID', '0')}/attempts/{env('GITHUB_RUN_ATTEMPT', '1')}"
print(json.dumps({
    "buildDefinition": {
        "buildType": "https://actions.github.io/buildtypes/workflow/v1",
        "externalParameters": {
            "workflow": {"ref": ref, "repository": f"{server}/{repo}", "path": path},
        },
        "internalParameters": {
            "github": {
                "event_name": env("GITHUB_EVENT_NAME", "local"),
                "repository_id": env("GITHUB_REPOSITORY_ID", ""),
                "repository_owner_id": env("GITHUB_REPOSITORY_OWNER_ID", ""),
                "runner_environment": env("RUNNER_ENVIRONMENT", "local"),
            },
        },
        "resolvedDependencies": [{
            "uri": f"git+{server}/{repo}@{ref}",
            "digest": {"gitCommit": env("GITHUB_SHA", "")},
        }],
    },
    "runDetails": {
        "builder": {"id": f"{server}/{workflow}"},
        "metadata": {"invocationId": run},
    },
}, indent=2))
PY

shopt -s nullglob
for file in "$dist"/*; do
  name="$(basename "$file")"
  case "$name" in
    SHA256SUMS | compatibility-*.json | *.sigstore.json) continue ;;
  esac
  "$cosign" attest-blob --key env://COSIGN_PRIVATE_KEY --signing-config "$work/signing-config.json" \
    --type slsaprovenance1 --predicate "$work/predicate.json" \
    --bundle "$dist/$name.provenance.sigstore.json" "$file" >/dev/null 2>"$work/log" ||
    { cat "$work/log" >&2; exit 1; }
  echo "attested $name"
done
"$cosign" sign-blob --key env://COSIGN_PRIVATE_KEY --signing-config "$work/signing-config.json" \
  --bundle "$dist/SHA256SUMS.sigstore.json" "$dist/SHA256SUMS" >/dev/null 2>"$work/log" ||
  { cat "$work/log" >&2; exit 1; }
echo "signed SHA256SUMS"
