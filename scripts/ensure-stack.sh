#!/usr/bin/env bash
# Bring the crw-camofox stack up if the Docker daemon is ready.
#
# Client-agnostic replacement for the old Pi extension's lazy auto-start: it
# runs at login (and on a timer) so the stack is there for *any* MCP client —
# Pi, Hermes, Claude Code, Cursor — instead of being started by whichever
# client happened to call first. Containers carry `restart: unless-stopped`,
# so this only has to cover "daemon came up" and "was stopped by hand".
#
# Idempotent: `docker compose up -d` does nothing when the stack is running.
# Exit codes: 0 = up, 1 = daemon never became ready, otherwise compose's code.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${CRW_PROJECT_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
DOCKER="${DOCKER_BIN:-$(command -v docker || echo /usr/local/bin/docker)}"
WAIT_SECS="${CRW_DOCKER_WAIT_SECS:-300}"

cd "$PROJECT_DIR" || exit 1

deadline=$((SECONDS + WAIT_SECS))
until "$DOCKER" info >/dev/null 2>&1; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "$(date '+%F %T') docker daemon not ready after ${WAIT_SECS}s" >&2
    exit 1
  fi
  sleep 10
done

# Only log when something actually changed; the every-15-minutes "Running"
# lines would otherwise grow the launchd log without bound.
out="$("$DOCKER" compose up -d 2>&1)"
rc=$?
if [ "$rc" -ne 0 ]; then
  printf '%s\n' "$out" >&2
  exit "$rc"
fi
changed="$(printf '%s\n' "$out" | grep -vE ' (Running|Waiting|Healthy) *$' || true)"
if [ -n "$changed" ]; then
  echo "$(date '+%F %T') stack started"
  printf '%s\n' "$changed"
fi
