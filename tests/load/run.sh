#!/usr/bin/env bash
# Load and resilience testing on one machine (D-030; docs/operations.md,
# *Load and resilience testing*): PostgreSQL in its own Compose project
# (gateway-load), and as host processes the fake provider, N gateways
# (profile api), one worker and the generator.
#
#   tests/load/run.sh all [profile] [gateway-load run flags...]
#       build, start the stack, seed the tenants, run, stop and remove it
#   tests/load/run.sh up | setup [flags] | run [profile] [flags] | down
#   tests/load/run.sh restart-gateway N | restart-worker | restart-postgres | crash-postgres
#   tests/load/run.sh status
#
# Profiles: smoke (1 min, 20/s), nightly (10 min, 100/s), gate-single,
# gate-spread, soak; see `gateway-load help` and the flags in the docs.
#
# Settings (environment):
#   LOAD_GATEWAYS (2), LOAD_POOL (20 connections per gateway), LOAD_ORGS (10),
#   LOAD_TIGHT_ORGS (1), LOAD_KEYS (10), LOAD_TTL (reservation TTL, 120 s),
#   LOAD_FAILURES ("gateway-restart@3m postgres-restart@6m": events at those
#   times into the run; also worker-restart and postgres-crash, a SIGKILL),
#   LOAD_PROVIDER_TIMEOUT (the gateways' provider timeout, 10 s),
#   GATEWAY_BUDGET_LEASES, GATEWAY_RATE_LIMIT_LEASES and
#   GATEWAY_INFERENCE_MAX_BODY_BYTES (passed to the gateways and the worker
#   as set),
#   LOAD_HTTP_PORT (28180: gateway N on 28180+N), LOAD_METRICS_PORT (28280),
#   LOAD_FAKE_PORT (24599), LOAD_PG_PORT (25532), LOAD_STATE_DIR,
#   LOAD_NO_BUILD=1 (use target/release as built), KEEP_STACK=1 (`all`
#   leaves the stack up), LOAD_REPORT (report path), LOAD_HISTORY (a file
#   each run appends its figures to).
#   An external database instead of the container: LOAD_DATABASE_URL (the
#   application role), LOAD_ADMIN_DATABASE_URL (a superuser or pg_monitor,
#   for sampling), LOAD_PG_RESTART_CMD and LOAD_PG_CRASH_CMD.
#
# Every other LOAD_* variable reaches `gateway-load` as its flag
# (LOAD_RATE=50 is --rate 50).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
self="$here/run.sh"
# Outside the repository: the gateway reads a .env from its working
# directory and every parent, and the stack must never pick up a
# developer's real provider keys.
export LOAD_STATE_DIR="${LOAD_STATE_DIR:-${XDG_RUNTIME_DIR:-/tmp}/gateway-load-$(id -u)}"
state="$LOAD_STATE_DIR"
gateways="${LOAD_GATEWAYS:-2}"
http_base="${LOAD_HTTP_PORT:-28180}"
metrics_base="${LOAD_METRICS_PORT:-28280}"
fake_port="${LOAD_FAKE_PORT:-24599}"
export LOAD_PG_PORT="${LOAD_PG_PORT:-25532}"
pool="${LOAD_POOL:-20}"
bin="${LOAD_BIN_DIR:-$root/target/release}"
export COMPOSE_PROJECT_NAME=gateway-load

log() { echo "[run.sh] $*" >&2; }

compose() {
  docker compose --project-directory "$here" -f "$here/compose.yaml" \
    --env-file "$state/secrets.env" "$@"
}

secrets() {
  mkdir -p "$state"
  chmod 700 "$state"
  if [ ! -f "$state/secrets.env" ]; then
    (
      umask 077
      {
        echo "POSTGRES_ADMIN_PASSWORD=$(openssl rand -hex 16)"
        echo "GATEWAY_APP_PASSWORD=$(openssl rand -hex 16)"
        echo "API_KEY_PEPPER=$(openssl rand -hex 32)"
      } >"$state/secrets.env"
    )
  fi
  set -a
  # shellcheck disable=SC1091
  . "$state/secrets.env"
  set +a
  pepper="${LOAD_PEPPER:-$API_KEY_PEPPER}"
  if [ -n "${LOAD_DATABASE_URL:-}" ]; then
    db_url="$LOAD_DATABASE_URL"
    admin_url="${LOAD_ADMIN_DATABASE_URL:-$LOAD_DATABASE_URL}"
  else
    db_url="postgres://gateway_app:$GATEWAY_APP_PASSWORD@127.0.0.1:$LOAD_PG_PORT/gateway"
    admin_url="postgres://gateway_bootstrap_admin:$POSTGRES_ADMIN_PASSWORD@127.0.0.1:$LOAD_PG_PORT/gateway"
  fi
}

wait_http() {
  local url="$1" tries="${2:-120}"
  for _ in $(seq 1 "$tries"); do
    if curl -fs -o /dev/null --max-time 2 "$url"; then return 0; fi
    sleep 0.5
  done
  log "no answer from $url"
  return 1
}

wait_postgres() {
  for _ in $(seq 1 120); do
    if [ -n "${LOAD_DATABASE_URL:-}" ]; then
      pg_isready -d "$db_url" >/dev/null 2>&1 && return 0
    else
      compose exec -T postgres pg_isready -U gateway_bootstrap_admin -d gateway >/dev/null 2>&1 && return 0
    fi
    sleep 0.5
  done
  log "PostgreSQL did not come back"
  return 1
}

# The gateway's environment, and nothing else from ours but the trace
# store's settings (for `setup --trace-content true`), the ClickHouse sink's
# and a few tuning settings.
gateway_env() {
  local profile="$1" passed=()
  shift
  mapfile -t passed < <(env | grep -E '^(GATEWAY_TRACE_STORE_[A-Z_]*|GATEWAY_CLICKHOUSE_[A-Z_]*|GATEWAY_TRACE_CONTENT_MAX_BYTES|GATEWAY_CREDENTIAL_ENCRYPTION_KEY|GATEWAY_BUDGET_LEASES|GATEWAY_RATE_LIMIT_LEASES|GATEWAY_INFERENCE_MAX_BODY_BYTES)=' || true)
  # `exec`: started in the background, the subshell becomes the gateway, so
  # `$!` (and its pid file) is the gateway's own pid.
  exec env -i PATH="$PATH" HOME="$HOME" RUST_LOG="${RUST_LOG:-warn}" "${passed[@]}" \
    DATABASE_URL="$db_url" GATEWAY_PROFILE="$profile" API_KEY_PEPPER="$pepper" \
    OPENAI_API_KEY=sk-load OPENAI_BASE_URL="http://127.0.0.1:$fake_port/v1" \
    OPENAI_REQUEST_TIMEOUT_SECONDS="${LOAD_PROVIDER_TIMEOUT:-10}" \
    GATEWAY_DATABASE_MAX_CONNECTIONS="$pool" "$@"
}

start_gateway() {
  local index="$1" port=$((http_base + $1)) metrics=$((metrics_base + $1))
  (
    cd "$state"
    gateway_env api BIND_ADDR="127.0.0.1:$port" GATEWAY_METRICS_ADDR="127.0.0.1:$metrics" \
      "$bin/gateway" >>"$state/gateway-$index.log" 2>&1 &
    echo $! >"$state/gateway-$index.pid"
  )
  wait_http "http://127.0.0.1:$port/healthz"
}

start_worker() {
  (
    cd "$state"
    gateway_env worker GATEWAY_METRICS_ADDR="127.0.0.1:$((metrics_base + 99))" \
      "$bin/gateway" >>"$state/worker.log" 2>&1 &
    echo $! >"$state/worker.pid"
  )
  wait_http "http://127.0.0.1:$((metrics_base + 99))/metrics"
}

stop_pid() {
  local file="$1" signal="${2:-TERM}"
  if [ -f "$file" ]; then
    kill -s "$signal" "$(cat "$file")" 2>/dev/null || true
    for _ in $(seq 1 40); do
      kill -0 "$(cat "$file")" 2>/dev/null || break
      sleep 0.25
    done
    kill -s KILL "$(cat "$file")" 2>/dev/null || true
    rm -f "$file"
  fi
}

targets() {
  local list=""
  for index in $(seq 0 $((gateways - 1))); do
    list="${list:+$list,}http://127.0.0.1:$((http_base + index))"
  done
  echo "$list"
}

cmd_build() {
  if [ "${LOAD_NO_BUILD:-0}" != 1 ]; then
    log "building gateway and gateway-load (release)"
    (cd "$root" && env -u ANTHROPIC_BASE_URL nice cargo build --release --locked \
      --bin gateway --bin gateway-load)
  fi
}

cmd_up() {
  secrets
  if [ -z "${LOAD_DATABASE_URL:-}" ]; then
    log "starting PostgreSQL (project $COMPOSE_PROJECT_NAME, port $LOAD_PG_PORT)"
    compose up -d --wait --wait-timeout 120
  fi
  wait_postgres
  (cd "$state" && gateway_env api "$bin/gateway" migrate >>"$state/migrate.log" 2>&1)
  log "starting the fake provider on 127.0.0.1:$fake_port"
  (
    cd "$state"
    "$bin/gateway-load" fake-provider --listen "127.0.0.1:$fake_port" \
      >>"$state/fake.log" 2>&1 &
    echo $! >"$state/fake.pid"
  )
  wait_http "http://127.0.0.1:$fake_port/healthz"
  for index in $(seq 0 $((gateways - 1))); do
    start_gateway "$index"
  done
  start_worker
  log "up: gateways $(targets), worker, pool $pool each; state in $state"
}

cmd_setup() {
  secrets
  "$bin/gateway-load" setup --database-url "$db_url" --pepper "$pepper" \
    --tenants "$state/tenants.json" --orgs "${LOAD_ORGS:-10}" \
    --tight-orgs "${LOAD_TIGHT_ORGS:-1}" --keys "${LOAD_KEYS:-10}" \
    --reservation-ttl "${LOAD_TTL:-120}" "$@"
}

# LOAD_FAILURES ("gateway-restart@3m postgres-restart@6m") as --event flags.
failure_events() {
  local item name at
  for item in ${LOAD_FAILURES:-}; do
    name="${item%@*}"
    at="${item#*@}"
    case "$name" in
      gateway-restart) echo "--event=$at:$name:$self restart-gateway 0" ;;
      worker-restart) echo "--event=$at:$name:$self restart-worker" ;;
      postgres-restart) echo "--event=$at:$name:$self restart-postgres" ;;
      postgres-crash) echo "--event=$at:$name:$self crash-postgres" ;;
      *) log "unknown failure $name" && return 1 ;;
    esac
  done
}

cmd_run() {
  secrets
  local profile="${1:-smoke}"
  [ $# -gt 0 ] && shift
  local report="${LOAD_REPORT:-$state/report-$profile.json}"
  local pid_flags=() events=()
  for index in $(seq 0 $((gateways - 1))); do
    pid_flags+=("--pid-file=gateway-$index=$state/gateway-$index.pid")
  done
  pid_flags+=("--pid-file=worker=$state/worker.pid")
  failure_events >/dev/null
  mapfile -t events < <(failure_events)
  # The environment stays whole: the failure events run this script
  # again, and need its settings.
  "$bin/gateway-load" run --profile "$profile" --targets "$(targets)" \
    --tenants "$state/tenants.json" --database-url "$db_url" \
    --admin-database-url "$admin_url" \
    --gateway-role "$(echo "$db_url" | sed -E 's#^[a-z]+://([^:@/]+).*#\1#')" \
    --fake-stats "http://127.0.0.1:$fake_port/__stats" --report "$report" \
    "${pid_flags[@]}" "${events[@]}" "$@"
}

cmd_restart_gateway() {
  secrets
  local index="${1:-0}"
  log "killing gateway $index (SIGKILL, mid-request) and starting it again"
  stop_pid "$state/gateway-$index.pid" KILL
  sleep "${LOAD_RESTART_DOWN:-2}"
  # The old gateway must be gone, or the restart would test nothing.
  if curl -fsS -o /dev/null --max-time 1 "http://127.0.0.1:$((http_base + index))/healthz"; then
    log "gateway $index still answers after SIGKILL"
    return 1
  fi
  start_gateway "$index"
}

cmd_restart_worker() {
  secrets
  log "killing the worker (SIGKILL) and starting it again"
  stop_pid "$state/worker.pid" KILL
  sleep "${LOAD_RESTART_DOWN:-2}"
  start_worker
}

cmd_restart_postgres() {
  secrets
  log "restarting PostgreSQL (fast shutdown)"
  if [ -n "${LOAD_DATABASE_URL:-}" ]; then
    sh -c "${LOAD_PG_RESTART_CMD:?set LOAD_PG_RESTART_CMD for an external database}"
  else
    compose restart -t 10 postgres
  fi
  wait_postgres
}

cmd_crash_postgres() {
  secrets
  log "killing PostgreSQL (SIGKILL: crash recovery on start)"
  if [ -n "${LOAD_DATABASE_URL:-}" ]; then
    sh -c "${LOAD_PG_CRASH_CMD:?set LOAD_PG_CRASH_CMD for an external database}"
  else
    compose kill -s KILL postgres
    sleep "${LOAD_RESTART_DOWN:-2}"
    compose start postgres
  fi
  wait_postgres
}

cmd_down() {
  for file in "$state"/gateway-*.pid "$state/worker.pid" "$state/fake.pid"; do
    [ -e "$file" ] && stop_pid "$file"
  done
  if [ -z "${LOAD_DATABASE_URL:-}" ] && [ -f "$state/secrets.env" ]; then
    compose down -v --remove-orphans >/dev/null 2>&1 || true
  fi
  rm -f "$state/secrets.env" "$state/tenants.json"
}

cmd_status() {
  for file in "$state"/*.pid; do
    [ -e "$file" ] || continue
    if kill -0 "$(cat "$file")" 2>/dev/null; then
      echo "$(basename "$file" .pid): running ($(cat "$file"))"
    else
      echo "$(basename "$file" .pid): stopped"
    fi
  done
}

cmd_all() {
  local profile="${1:-smoke}"
  [ $# -gt 0 ] && shift
  cmd_build
  # A stack an interrupted run left behind starts clean.
  cmd_down
  if [ "${KEEP_STACK:-0}" != 1 ]; then
    trap 'status=$?; [ $status -ne 0 ] && tail -n 40 "$state"/gateway-0.log "$state"/worker.log 2>/dev/null; cmd_down; exit $status' EXIT
  fi
  cmd_up
  cmd_setup
  cmd_run "$profile" "$@"
}

command="${1:-}"
[ $# -gt 0 ] && shift
case "$command" in
  all) cmd_all "$@" ;;
  build) cmd_build ;;
  up) cmd_up ;;
  setup) cmd_setup "$@" ;;
  run) cmd_run "$@" ;;
  restart-gateway) cmd_restart_gateway "$@" ;;
  restart-worker) cmd_restart_worker ;;
  restart-postgres) cmd_restart_postgres ;;
  crash-postgres) cmd_crash_postgres ;;
  down) cmd_down ;;
  status) cmd_status ;;
  *) sed -n '2,33p' "$0" >&2 && exit 2 ;;
esac
