#!/usr/bin/env bash
# Runs the functional suite against its own compose stack (project
# gateway-functional, throwaway secrets and database), then removes it.
#
#   tests/functional/run.sh [--no-build] [playwright args…]
#
# KEEP_STACK=1 leaves the stack up for a rerun or a look around; the next run
# reuses it. GATEWAY_HTTP_PORT, POSTGRES_PORT and FAKES_PORT pick the host
# ports (18180, 15532, 14599).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
env_file="$here/.env"

build=1
if [ "${1:-}" = "--no-build" ]; then
  build=0
  shift
fi

if [ ! -f "$env_file" ]; then
  key() { openssl rand 32 | basenc --base64url | tr -d '='; }
  umask 077
  {
    echo "POSTGRES_ADMIN_PASSWORD=$(openssl rand -hex 16)"
    echo "GATEWAY_APP_PASSWORD=$(openssl rand -hex 16)"
    echo "API_KEY_PEPPER=$(openssl rand -hex 32)"
    echo "OIDC_SESSION_HMAC_SECRET=$(key)"
    echo "OIDC_SESSION_ENCRYPTION_KEY=$(key)"
    echo "GATEWAY_CREDENTIAL_ENCRYPTION_KEY=$(key)"
  } > "$env_file"
fi

export COMPOSE_PROJECT_NAME=gateway-functional
export GATEWAY_HTTP_PORT="${GATEWAY_HTTP_PORT:-18180}"
export POSTGRES_PORT="${POSTGRES_PORT:-15532}"
export FAKES_PORT="${FAKES_PORT:-14599}"
compose() {
  docker compose --project-directory "$root" --env-file "$env_file" \
    -f "$root/compose.yaml" -f "$here/compose.yaml" --profile app "$@"
}

if [ "${KEEP_STACK:-0}" != 1 ]; then
  trap 'status=$?; [ $status -ne 0 ] && compose logs --no-color gateway fakes | tail -200; compose down -v; rm -f "$env_file"; exit $status' EXIT
fi

# A stack an interrupted run left behind (a cancelled CI job on the
# persistent runner, say) would keep its old database: start clean, unless
# the stack was kept on purpose (KEEP_STACK=1, for debugging).
if [ "${KEEP_STACK:-0}" != 1 ]; then
  compose down -v --remove-orphans >/dev/null 2>&1 || true
fi

if [ "$build" = 1 ]; then
  compose up -d --build --wait --wait-timeout 300
else
  compose up -d --no-build --wait --wait-timeout 300
fi

# The gateway has no container health check: wait for it to answer.
timeout 120 bash -c "until curl -fsS http://localhost:$GATEWAY_HTTP_PORT/healthz >/dev/null; do sleep 2; done"

cd "$here"
GATEWAY_URL="http://localhost:$GATEWAY_HTTP_PORT" \
  FAKES_URL="http://127.0.0.1:$FAKES_PORT" \
  FUNCTIONAL_ENV_FILE="$env_file" \
  npx playwright test --project=stack "$@"
