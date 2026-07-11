#!/usr/bin/env bash
# One-command local verification for switchboard.
# Usage: ./run_local_test.sh
#
# Works on macOS and Ubuntu (and most Linux).
# Postgres + Redis: docker compose.
# Ollama (default OLLAMA_MODE=auto):
#   - GPU detected  → install/use host Ollama (Metal / NVIDIA / ROCm)
#   - no GPU        → Docker Ollama (CPU only)
# Override: OLLAMA_MODE=host|docker|auto  OLLAMA_GPU=1|0
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

log() { printf '\033[1;34m[run_local_test]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[run_local_test]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[run_local_test] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

SWITCHBOARD_PID=""
OLLAMA_PID=""
OLLAMA_DOCKER=false

cleanup() {
  if [[ -n "$SWITCHBOARD_PID" ]] && kill -0 "$SWITCHBOARD_PID" 2>/dev/null; then
    log "Stopping switchboard (pid $SWITCHBOARD_PID)"
    kill "$SWITCHBOARD_PID" 2>/dev/null || true
    wait "$SWITCHBOARD_PID" 2>/dev/null || true
  fi
  # Only stop Ollama if we started it in this script (not user's existing serve).
  if [[ -n "$OLLAMA_PID" ]] && kill -0 "$OLLAMA_PID" 2>/dev/null; then
    log "Stopping ollama serve started by this script (pid $OLLAMA_PID)"
    kill "$OLLAMA_PID" 2>/dev/null || true
    wait "$OLLAMA_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

psql_exec() {
  if command -v psql >/dev/null 2>&1; then
    psql "$DATABASE_URL" "$@"
  else
    docker compose exec -T postgres psql -U switchboard -d switchboard "$@"
  fi
}

wait_tcp() {
  local host="$1" port="$2" label="$3" timeout="${4:-60}"
  local deadline=$((SECONDS + timeout))
  while (( SECONDS < deadline )); do
    if (echo >/dev/tcp/"$host"/"$port") 2>/dev/null; then
      log "$label is up ($host:$port)"
      return 0
    fi
    sleep 1
  done
  die "$label did not become ready at $host:$port within ${timeout}s"
}

free_port() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1 && lsof -ti :"$port" >/dev/null 2>&1; then
    warn "Port $port already in use — stopping existing listener"
    lsof -ti :"$port" | xargs kill 2>/dev/null || true
    sleep 1
  fi
}

wait_http() {
  local url="$1" label="$2" timeout="${3:-60}"
  local deadline=$((SECONDS + timeout))
  while (( SECONDS < deadline )); do
    if curl -sf "$url" >/dev/null 2>&1; then
      log "$label is up ($url)"
      return 0
    fi
    sleep 1
  done
  die "$label did not become ready at $url within ${timeout}s"
}

postgres_accepts_connections() {
  docker compose exec -T postgres pg_isready -U switchboard -d switchboard >/dev/null 2>&1
}

wait_postgres_ready() {
  local timeout="${1:-120}"
  local deadline=$((SECONDS + timeout))
  while (( SECONDS < deadline )); do
    if postgres_accepts_connections; then
      log "Postgres accepting connections"
      return 0
    fi
    sleep 1
  done
  return 1
}

postgres_logs_show_disk_full() {
  docker compose logs postgres --tail 20 2>&1 | grep -q "No space left on device"
}

redis_logs_show_disk_full() {
  docker compose logs redis --tail 20 2>&1 | grep -q "No space left on device"
}

prune_docker_build_cache() {
  log "Pruning Docker build cache to free disk"
  docker builder prune -f >/dev/null 2>&1 || true
}

reset_postgres_volume() {
  log "Resetting Postgres volume (local test data only)"
  docker compose rm -sfv postgres 2>/dev/null || true
  docker compose up -d postgres
}

reset_redis_volume() {
  log "Resetting Redis volume (local test data only)"
  docker compose rm -sfv redis 2>/dev/null || true
  docker compose up -d redis
}

recover_docker_infra() {
  local reason="${1:-unhealthy}"

  warn "Recovering Docker infra ($reason)"

  if postgres_logs_show_disk_full || redis_logs_show_disk_full; then
    warn "Docker disk full — pruning cache and resetting local DB volumes"
    prune_docker_build_cache
  fi

  if [[ "${RESET_DB:-}" == "1" ]] || postgres_logs_show_disk_full || ! postgres_accepts_connections; then
    reset_postgres_volume
  fi
  if redis_logs_show_disk_full; then
    reset_redis_volume
  fi

  wait_postgres_ready 120 || {
    die "Postgres still down. Free Docker disk: docker system prune -a --volumes"
  }
  wait_tcp "${REDIS_HOST:-localhost}" "${REDIS_PORT:-6379}" "Redis" 60
}

ensure_docker_infra() {
  wait_postgres_ready 30 || recover_docker_infra "postgres not ready"
  if ! docker compose exec -T redis redis-cli ping 2>/dev/null | grep -q PONG; then
    recover_docker_infra "redis not ready"
  fi
}

run_migrate() {
  local attempt
  for attempt in 1 2 3; do
    ensure_docker_infra
    if cargo run --quiet --bin sbctl -- migrate; then
      return 0
    fi
    warn "migrate attempt $attempt/3 failed"
    recover_docker_infra "migrate PoolTimedOut"
    sleep 2
  done
  die "migrations failed — try: docker system prune -a --volumes && ./run_local_test.sh"
}

ollama_reachable() {
  curl -sf "${OLLAMA_BASE_URL}/api/tags" >/dev/null 2>&1
}

docker_ollama_running() {
  docker compose ps --status running ollama 2>/dev/null | grep -q ollama
}

# GPU available for Ollama acceleration (host install path).
has_gpu() {
  if [[ "${OLLAMA_GPU:-}" == "1" ]]; then
    return 0
  fi
  if [[ "${OLLAMA_GPU:-}" == "0" ]] || [[ "${OLLAMA_FORCE_CPU:-}" == "1" ]]; then
    return 1
  fi

  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  # Apple Silicon — Metal
  if [[ "$os" == "Darwin" && "$arch" == "arm64" ]]; then
    return 0
  fi

  # NVIDIA (Ubuntu / Linux servers, CUDA)
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
    return 0
  fi
  if [[ -e /dev/nvidia0 ]] || [[ -e /dev/nvidiactl ]]; then
    return 0
  fi

  # AMD ROCm (some Ubuntu GPU boxes)
  if [[ -e /dev/kfd ]] && [[ -e /dev/dri/renderD128 ]]; then
    return 0
  fi

  return 1
}

gpu_description() {
  if ! has_gpu; then
    echo "none (CPU only)"
    return
  fi
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  if [[ "$os" == "Darwin" && "$arch" == "arm64" ]]; then
    echo "Apple Silicon (Metal)"
  elif command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
    echo "NVIDIA ($(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1 || echo CUDA))"
  elif [[ -e /dev/kfd ]]; then
    echo "AMD ROCm"
  else
    echo "detected"
  fi
}

install_host_ollama() {
  if command -v ollama >/dev/null 2>&1; then
    return 0
  fi
  if [[ "${OLLAMA_INSTALL:-1}" == "0" ]]; then
    return 1
  fi

  case "$(uname -s)" in
    Darwin)
      if command -v brew >/dev/null 2>&1; then
        log "Ollama not found — installing via Homebrew"
        NONINTERACTIVE=1 HOMEBREW_NO_AUTO_UPDATE=1 brew install ollama
        return 0
      fi
      warn "Homebrew not found; install from https://ollama.com/download"
      return 1
      ;;
    Linux)
      log "Ollama not found — installing via https://ollama.com/install.sh (may prompt for sudo)"
      require_cmd curl
      if ! command -v sudo >/dev/null 2>&1; then
        warn "sudo not found; run as root or install ollama manually"
        return 1
      fi
      curl -fsSL https://ollama.com/install.sh | sh
      return 0
      ;;
    *)
      warn "Auto-install not supported on $(uname -s)"
      return 1
      ;;
  esac
}

stop_docker_ollama_if_running() {
  if docker_ollama_running; then
    warn "Stopping Docker Ollama (free port ${OLLAMA_PORT:-11434} for host/GPU Ollama)"
    docker compose --profile ollama stop ollama 2>/dev/null || true
    sleep 2
  fi
}

stop_host_ollama_for_docker() {
  if [[ -n "$OLLAMA_PID" ]] && kill -0 "$OLLAMA_PID" 2>/dev/null; then
    kill "$OLLAMA_PID" 2>/dev/null || true
    wait "$OLLAMA_PID" 2>/dev/null || true
    OLLAMA_PID=""
    sleep 1
    return
  fi
  if ollama_reachable && ! docker_ollama_running; then
    warn "Stopping host Ollama on :${OLLAMA_PORT:-11434} for Docker CPU mode"
    if command -v ollama >/dev/null 2>&1; then
      ollama stop 2>/dev/null || true
    fi
    pkill -f '[o]llama serve' 2>/dev/null || true
    sleep 2
  fi
}

ensure_host_ollama() {
  OLLAMA_DOCKER=false
  stop_docker_ollama_if_running

  if ollama_reachable; then
    log "Using host Ollama at ${OLLAMA_BASE_URL} (GPU if configured)"
    return 0
  fi

  install_host_ollama || true

  if ! command -v ollama >/dev/null 2>&1; then
    return 1
  fi

  log "Starting host Ollama (ollama serve) — uses Apple Silicon / NVIDIA GPU when available"
  ollama serve >/tmp/switchboard-ollama.log 2>&1 &
  OLLAMA_PID=$!
  sleep 2
  wait_http "${OLLAMA_BASE_URL}/api/tags" "Ollama (host)" 60
  return 0
}

ensure_docker_ollama() {
  OLLAMA_DOCKER=true
  stop_host_ollama_for_docker
  log "Starting Ollama in Docker (CPU)"
  docker compose --profile ollama up -d ollama
  wait_http "${OLLAMA_BASE_URL}/api/tags" "Ollama (docker)" 180
}

ensure_ollama() {
  local mode="${OLLAMA_MODE:-auto}"

  case "$mode" in
    host)
      log "OLLAMA_MODE=host — using host Ollama (install if missing)"
      ensure_host_ollama || die "Host Ollama failed (try OLLAMA_MODE=docker)"
      ;;
    docker)
      log "OLLAMA_MODE=docker — using Docker Ollama (CPU)"
      ensure_docker_ollama
      ;;
    auto)
      if has_gpu; then
        log "GPU detected: $(gpu_description) — host Ollama (auto-install if missing)"
        ensure_host_ollama || {
          warn "Host Ollama failed — falling back to Docker (CPU)"
          ensure_docker_ollama
        }
      else
        log "No GPU detected — Docker Ollama (CPU, no host install)"
        ensure_docker_ollama
      fi
      ;;
    *)
      die "Invalid OLLAMA_MODE=$mode (use auto, host, or docker)"
      ;;
  esac
}

ollama_pull() {
  local model="$1"
  if [[ "$OLLAMA_DOCKER" == true ]]; then
    log "Pulling $model (docker ollama)"
    docker compose exec -T ollama ollama pull "$model"
  else
    log "Pulling $model (host ollama)"
    ollama pull "$model"
  fi
}

# ── prerequisites ────────────────────────────────────────────────────────────
log "Checking prerequisites"
require_cmd docker
require_cmd cargo
require_cmd curl
require_cmd python3

if [[ -f .env ]]; then
  # shellcheck disable=SC1091
  set -a && source .env && set +a
fi

export DATABASE_URL="${DATABASE_URL:-postgres://switchboard:switchboard@127.0.0.1:5432/switchboard}"
export REDIS_URL="${REDIS_URL:-redis://127.0.0.1:6379}"
export OLLAMA_BASE_URL="${OLLAMA_BASE_URL:-http://127.0.0.1:${OLLAMA_PORT:-11434}}"
export SWITCHBOARD_PORT="${SWITCHBOARD_PORT:-8080}"
export SWITCHBOARD_BASE_URL="http://127.0.0.1:${SWITCHBOARD_PORT}"
export SB__PROVIDERS__OLLAMA__BASE_URL="${SB__PROVIDERS__OLLAMA__BASE_URL:-$OLLAMA_BASE_URL}"

# ── reset: every run starts from an empty database ────────────────────────────
#
# Fresh volumes for Postgres and Redis. That wipes everything that carries state between
# runs — tenants, api keys, usage_records, the budget counters, the exact cache, the
# idempotency keys, the rate-limit buckets — so a run can never pass because of something
# a previous run left behind, and the third-party balance sheet at the end starts at zero.
#
# The Ollama volume is deliberately kept. It holds several gigabytes of model weights and
# no state that any test asserts on; deleting it would turn a two-minute script into a
# re-download. `RESET_OLLAMA=1` drops it too, for when the model cache is what is suspect.
if [[ "${SKIP_RESET:-0}" == "1" ]]; then
  log "SKIP_RESET=1 — keeping existing Postgres + Redis volumes"
elif [[ "${RESET_OLLAMA:-0}" == "1" ]]; then
  log "Resetting ALL volumes, including the Ollama model cache (expect a re-download)"
  docker compose down -v --remove-orphans >/dev/null 2>&1 || true
else
  log "Resetting Postgres + Redis volumes (keeping the Ollama model cache)"
  docker compose rm -sfv postgres redis >/dev/null 2>&1 || true
  docker volume rm -f switchboard_pgdata switchboard_redisdata >/dev/null 2>&1 || true
fi

# ── docker compose: postgres + redis only (Ollama on host by default) ────────
log "Starting Postgres + Redis (docker compose)"
docker compose up -d postgres redis

PG_HOST="$(python3 -c "import os,urllib.parse as u; p=u.urlparse(os.environ['DATABASE_URL']); print(p.hostname or '127.0.0.1')")"
PG_PORT="$(python3 -c "import os,urllib.parse as u; p=u.urlparse(os.environ['DATABASE_URL']); print(p.port or 5432)")"
REDIS_HOST="$(python3 -c "import os,urllib.parse as u; p=u.urlparse(os.environ['REDIS_URL']); print(p.hostname or '127.0.0.1')")"
REDIS_PORT="$(python3 -c "import os,urllib.parse as u; p=u.urlparse(os.environ['REDIS_URL']); print(p.port or 6379)")"

wait_tcp "$PG_HOST" "$PG_PORT" "Postgres"
wait_tcp "$REDIS_HOST" "$REDIS_PORT" "Redis"
ensure_docker_infra

ensure_ollama

# ── build + migrate ────────────────────────────────────────────────────────────
log "Building switchboard + sbctl"
cargo build --bin switchboard --bin sbctl

log "Applying database migrations"
ensure_docker_infra
run_migrate

log "Pulling local models (qwen3:4b, nomic-embed-text)"
ollama_pull qwen3:4b
ollama_pull nomic-embed-text

# ── rust quality gates ─────────────────────────────────────────────────────────
log "cargo fmt"
cargo fmt --all

log "cargo clippy"
cargo clippy --all-targets --all-features -- -D warnings

log "cargo check"
cargo check --all-features

log "cargo test"
cargo test --all-features

# ── seed third-party tenant + API key + funding ────────────────────────────────
TEST_TENANT_ID="00000000-0000-7000-8000-000000000099"
# What the third party puts in. Deliberately small: the point of the simulation below is to
# run it down and see what it bought. At qwen3:4b rates ($0.05/$0.10 per Mtok) this funds a
# hundred answers, which a local Ollama cannot produce in the time we allow — so the run
# ends on the clock, not on the money. Lower it to watch the fleet halt on `insufficient_quota`.
TEST_TENANT_FUNDING_USD="${TEST_TENANT_FUNDING_USD:-0.002}"

# The database is empty: the volumes were dropped at the top of this script. So the tenant
# is new, its ledger is empty, and the Redis budget counter — which is seeded from
# `usage_hourly` with `SET NX` and a 40-day TTL, and would otherwise survive a mere row
# delete — does not exist yet.
log "Seeding test tenant $TEST_TENANT_ID"
cargo run --quiet --bin sbctl -- create-tenant "acme-corp" "$TEST_TENANT_ID" >/dev/null

log "Funding tenant with \$${TEST_TENANT_FUNDING_USD}"
cargo run --quiet --bin sbctl -- set-budget "$TEST_TENANT_ID" "$TEST_TENANT_FUNDING_USD"

THIRD_PARTY_API_KEY="$(cargo run --quiet --bin sbctl -- create-key "$TEST_TENANT_ID" "local-test" | tail -n1)"
export THIRD_PARTY_API_KEY
export TEST_TENANT_ID
log "Created third-party API key (prefix: ${THIRD_PARTY_API_KEY:0:16}...)"

# ── start API server ───────────────────────────────────────────────────────────
log "Starting switchboard on port ${SWITCHBOARD_PORT}"
free_port "$SWITCHBOARD_PORT"
export SB__SERVER__PORT="$SWITCHBOARD_PORT"
export SB__AUTH__REQUIRE_AUTH="${SB__AUTH__REQUIRE_AUTH:-false}"

# The agent loop, and therefore the tools.
#
# `config/switchboard.toml` ships with `agent.enabled = false`, which is the right default
# for a gateway nobody has configured. But the `fast` eval suite this script runs contains
# tool cases and prompt-injection cases, and with no agent there are no tools: the tool
# cases fail outright, and the injection cases pass *vacuously* — nothing was called
# because nothing could be. A defence that cannot be exercised has not been tested.
export SB__AGENT__ENABLED="${SB__AGENT__ENABLED:-true}"

export RUST_LOG="${RUST_LOG:-switchboard=info}"

cargo run --quiet --bin switchboard >/tmp/switchboard-server.log 2>&1 &
SWITCHBOARD_PID=$!
wait_http "${SWITCHBOARD_BASE_URL}/health" "switchboard" 60

READY_JSON="$(curl -sf "${SWITCHBOARD_BASE_URL}/ready" || echo '{}')"
if echo "$READY_JSON" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get("providers") else 1)' 2>/dev/null; then
  if [[ "$OLLAMA_DOCKER" == true ]]; then
    log "Ollama provider healthy (docker / CPU)"
  else
    log "Ollama provider healthy (host / GPU if available)"
  fi
else
  warn "No healthy providers — chat/embed/RAG live tests will be skipped"
  if [[ "$OLLAMA_DOCKER" == true ]]; then
    warn "Check: docker compose --profile ollama logs ollama"
  else
    warn "Check: ollama list && curl ${OLLAMA_BASE_URL}/api/tags"
  fi
  if [[ "${OLLAMA_REQUIRED:-}" == "1" ]]; then
    die "OLLAMA_REQUIRED=1 but no healthy provider"
  fi
fi

# ── python conformance (HTTP + OpenAI SDK) ────────────────────────────────────
VENV_DIR="${ROOT}/.venv"
if [[ ! -d "$VENV_DIR" ]]; then
  log "Creating Python venv at $VENV_DIR"
  python3 -m venv "$VENV_DIR"
fi
# shellcheck disable=SC1091
source "$VENV_DIR/bin/activate"

log "Installing Python conformance dependencies"
python3 -m pip install -q -r tests/conformance/requirements.txt

log "Running HTTP conformance tests"
python3 -m pytest tests/conformance -v --tb=short

# ── eval fast suite against live server ───────────────────────────────────────
log "Running eval fast suite + benchmark"
cargo run --quiet --bin eval -- --suite fast --base-url "$SWITCHBOARD_BASE_URL" --benchmark

# ── third-party simulation: a bot fleet, and whether it was worth it ──────────
#
# This is the only phase that runs code we do not own. `examples/bot-fleet` is excluded
# from the workspace and cannot `use switchboard::` — it reaches the gateway over HTTP,
# exactly as a customer does. See docs/16.
#
# It ends by reconciling three independently-kept sets of books. If they disagree by a
# single nanodollar, `simulate` exits non-zero and this script fails, because a latency
# number from a system that cannot count money is not worth reading.
log "Testing the third-party example (its own workspace, its own lockfile)"
(cd examples/bot-fleet && cargo test --quiet) || die "the bot-fleet example's own tests failed"

log "Simulating a third-party bot fleet against the live gateway"
(
  cd examples/bot-fleet
  SWITCHBOARD_BASE_URL="$SWITCHBOARD_BASE_URL" \
  SWITCHBOARD_API_KEY="$THIRD_PARTY_API_KEY" \
  SWITCHBOARD_MODEL="${SIM_MODEL:-qwen3:4b}" \
  REDIS_URL="$REDIS_URL" \
  RUST_LOG="${SIM_LOG:-warn,bot_fleet=info,simulate=info}" \
    cargo run --quiet --bin simulate -- \
      --bots "${SIM_BOTS:-20000}" \
      --per-bot-per-day "${SIM_PER_BOT_PER_DAY:-10}" \
      --conversations "${SIM_CONVERSATIONS:-40}" \
      --workers "${SIM_WORKERS:-8}" \
      --seconds "${SIM_SECONDS:-25}" \
      --drain-secs "${SIM_DRAIN_SECS:-90}"
) || die "the bot-fleet simulation failed — the gateway's books did not balance"

# ── the load test — docs/13 §P5 ───────────────────────────────────────────────
#
# The only phase of docs/13 that measures rather than changes a mechanism. It does not use the
# server started above, and it must not: it boots its own gateway, on its own port, against a
# stub upstream whose latency it chooses — because "gateway overhead" is `total − provider time`
# and you cannot do that subtraction against a real model whose time you do not know.
#
# `--release`, because a p50 measured on a debug build is a measurement of `-C opt-level=0`.
log "Load test (docs/13 §P5) — this boots its own gateway against a stub upstream"
cargo build --release --quiet --bin loadtest
cargo run --release --quiet --bin loadtest -- \
  --scenario "${LOADTEST_SCENARIO:-all}" \
  --duration-secs "${LOADTEST_SECONDS:-15}" \
  --json target/loadtest.json \
  || die "the load test failed — see the failing checks above"

log "Third-party balance sheet (what the bot sees)"
cargo run --quiet --bin sbctl -- budget "$TEST_TENANT_ID"

# The operator's view. Revenue is what the tenant paid; COGS is what we paid the provider.
# Neither `/v1/usage` nor `/v1/usage/records` exposes the margin, and migration 007 revokes
# the column from the role reachable via `sql_query`. A tenant that can see our margin can
# negotiate against it.
log "Gateway P&L and latency (what only you see)"
cargo run --quiet --bin sbctl -- stats "$TEST_TENANT_ID"

# Concurrency is not a latency percentile. It comes from an in-process high-water mark,
# because a Prometheus scrape every 15s cannot see a 400ms saturation spike.
log "Gateway peak concurrency"
if PEAK="$(curl -fsS "${SWITCHBOARD_BASE_URL}/metrics" 2>/dev/null \
    | awk '/^sb_inflight_requests_peak/ {print $2}' | tail -n1)"; then
  echo "max concurrent /v1 requests since boot: ${PEAK:-unknown}"
else
  echo "metrics endpoint unavailable"
fi

log "✅ All local verification passed"
log "Server log: /tmp/switchboard-server.log"
if [[ -n "$OLLAMA_PID" ]]; then
  log "Host Ollama log: /tmp/switchboard-ollama.log"
fi
