#!/usr/bin/env bash
# Prune stale camofox browser profiles inside the running container.
#
# Why this exists: camofox keys its persistent Firefox profile directory by
# `userId` (one ~30-60 MB dir under $CAMOFOX_PROFILES_DIR) and deliberately
# never deletes them on session close — that is what makes logins survive a
# restart. Any client that mints a fresh userId per session/process therefore
# leaves a profile behind forever. Before the userIds were pinned, this stack
# accumulated 89 profiles / 3.4 GB in two months.
#
# Profiles in KEEP are always preserved. Everything else is removed once its
# newest file is older than TTL_DAYS, which means an actively used profile
# (touched within the TTL) is never touched. Removal happens inside the
# container because the profile directory lives in the camofox volume.
#
# Usage:
#   scripts/prune-camofox-profiles.sh              # prune, TTL 7 days
#   scripts/prune-camofox-profiles.sh --dry-run    # report only
#   PROFILE_TTL_DAYS=30 scripts/prune-camofox-profiles.sh
#   CAMOFOX_PROFILE_KEEP="crw crw-search default" ...   # override keep list
#
# Env: CAMOFOX_CONTAINER (default crw-camofox-camofox-1), PROFILE_TTL_DAYS (7),
#      CAMOFOX_PROFILE_KEEP ("crw crw-search default"), CRW_PROJECT_DIR.
set -euo pipefail

DRY_RUN="${DRY_RUN:-0}"
for arg in "$@"; do
  case "$arg" in
    --dry-run|-n) DRY_RUN=1 ;;
    -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${CRW_PROJECT_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
CONTAINER="${CAMOFOX_CONTAINER:-crw-camofox-camofox-1}"
TTL_DAYS="${PROFILE_TTL_DAYS:-7}"
KEEP="${CAMOFOX_PROFILE_KEEP:-crw crw-search default}"

# Fall back to the compose project's container name when the default misses
# (compose prefixes service names with the project directory).
if ! docker inspect "$CONTAINER" >/dev/null 2>&1; then
  CONTAINER="$(cd "$PROJECT_DIR" && docker compose ps -q camofox 2>/dev/null | head -1 | xargs -r docker inspect --format '{{.Name}}' 2>/dev/null | sed 's#^/##')"
fi
if [ -z "$CONTAINER" ] || ! docker inspect "$CONTAINER" >/dev/null 2>&1; then
  echo "camofox container not found (set CAMOFOX_CONTAINER)" >&2
  exit 1
fi

echo "camofox: $CONTAINER | ttl: ${TTL_DAYS}d | keep: $KEEP | dry-run: $DRY_RUN"

docker exec -i "$CONTAINER" python3 - "$TTL_DAYS" "$DRY_RUN" $KEEP <<'PY'
import os
import shutil
import sys
import time

root = "/home/node/.camofox/profiles"
ttl_days = float(sys.argv[1])
dry_run = sys.argv[2] == "1"
keep = set(sys.argv[3:])
now = time.time()

def newest_mtime(path: str) -> float:
    newest = os.path.getmtime(path)
    for dirpath, dirnames, filenames in os.walk(path):
        for name in dirnames + filenames:
            try:
                newest = max(newest, os.path.getmtime(os.path.join(dirpath, name)))
            except OSError:
                pass
    return newest

def size_bytes(path: str) -> int:
    total = 0
    for dirpath, _dirnames, filenames in os.walk(path):
        for name in filenames:
            try:
                total += os.path.getsize(os.path.join(dirpath, name))
            except OSError:
                pass
    return total

if not os.path.isdir(root):
    print(f"no profiles directory at {root}")
    raise SystemExit(0)

removed = kept = 0
freed = 0
for name in sorted(os.listdir(root)):
    path = os.path.join(root, name)
    if not os.path.isdir(path):
        continue
    if name in keep:
        kept += 1
        continue
    age_days = (now - newest_mtime(path)) / 86400
    if age_days < ttl_days:
        kept += 1
        continue
    size = size_bytes(path)
    verb = "would remove" if dry_run else "removing"
    print(f"  {verb} {name} ({age_days:.1f}d, {size / 1e6:.0f} MB)")
    if not dry_run:
        shutil.rmtree(path, ignore_errors=True)
    removed += 1
    freed += size

print(f"{'would free' if dry_run else 'freed'} {freed / 1e6:.0f} MB; "
      f"{'matched' if dry_run else 'removed'} {removed}, kept {kept}")
PY
