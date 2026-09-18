#!/usr/bin/env bash
# e2e/run.sh -- csm limit-switch end-to-end harness.
#
# Builds (or reuses) a `csm` binary and a fake, sleeping `claude` binary,
# stands up an isolated sandbox (its own HOME, its own profile registry, no
# network -- CSM_USAGE_API_BASE points at an unrouted local port), runs the
# 14 numbered scenarios (plus 9b, 15 VERDICT blocks in total) against it,
# prints the report, and tears down:
# only PIDs this script itself started are ever signalled, and the sandbox is
# removed unless --keep is given. Never runs the real `claude`.
#
# Usage:
#   bash e2e/run.sh [--csm <path-to-csm-binary>] [--keep]
#
# CSM_BIN (env) is an alternative to --csm. With neither, this builds
# `cargo build --bin csm` from the repo root (respects CARGO_TARGET_DIR if
# already set in the environment).
#
# Portable to macOS (BSD userland) and Linux (GNU userland, ubuntu-latest in
# CI): see e2e/lib.sh's header for the specific BSD/GNU traps avoided.
set -uo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

KEEP=0
CSM_ARG=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --csm)
      CSM_ARG="$2"; shift 2 ;;
    --csm=*)
      CSM_ARG="${1#--csm=}"; shift ;;
    --keep)
      KEEP=1; shift ;;
    -h|--help)
      sed -n '2,20p' "$0"; exit 0 ;;
    *)
      echo "e2e/run.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

# abspath <path> -- portable absolute-path resolution (no reliance on GNU
# `readlink -f`, which older BSD/macOS readlink lacks).
abspath() {
  local p="$1"
  if [[ -d "$p" ]]; then
    (cd "$p" && pwd)
  else
    local dir base
    dir=$(cd "$(dirname "$p")" 2>/dev/null && pwd) || return 1
    base=$(basename "$p")
    printf '%s/%s\n' "$dir" "$base"
  fi
}

# ── resolve csm binary (build unless given) ─────────────────────────────────
CSM_BIN="${CSM_ARG:-${CSM_BIN:-}}"
if [[ -n "$CSM_BIN" ]]; then
  CSM_BIN=$(abspath "$CSM_BIN") || { echo "e2e/run.sh: --csm path not found: $CSM_ARG" >&2; exit 2; }
else
  echo "== building csm (cargo build --bin csm) =="
  ( cd "$REPO_ROOT" && cargo build --bin csm ) || exit 1
  TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
  CSM_BIN="$TARGET_DIR/debug/csm"
fi
if [[ ! -x "$CSM_BIN" ]]; then
  echo "e2e/run.sh: csm binary not found or not executable: $CSM_BIN" >&2
  exit 1
fi
echo "== csm binary: $CSM_BIN =="

# ── sandbox ──────────────────────────────────────────────────────────────────
SANDBOX=$(mktemp -d "${TMPDIR:-/tmp}/csm-e2e.XXXXXX")
HOME_DIR="$SANDBOX/home"
A_DIR="$HOME_DIR/.claude.a"
B_DIR="$HOME_DIR/.claude.b"
SMART_DIR="$HOME_DIR/.claude.shared/smart"
LOG_DIR="$SANDBOX/logs"
TRANSCRIPTS_DIR="$SANDBOX/fake_transcripts"
FAKE_BIN="$SANDBOX/bin/claude"
USAGE_CMD_SCRIPT="$REPO_ROOT/e2e/bin/usage_cmd.sh"
FIX_HEALTHY="$REPO_ROOT/e2e/fixtures/a_capped_b_healthy.json"
FIX_BOTH="$REPO_ROOT/e2e/fixtures/both_capped.json"
REPORT="$LOG_DIR/report.txt"

mkdir -p "$HOME_DIR" "$SMART_DIR" "$LOG_DIR" "$TRANSCRIPTS_DIR" "$(dirname "$FAKE_BIN")"

ALL_PIDS=()

# ── teardown (always runs: normal exit, error, or signal) ──────────────────
cleanup() {
  local exit_code=$?
  for pid in "${ALL_PIDS[@]:-}"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null
    fi
  done
  if (( KEEP )); then
    echo "== --keep set: sandbox left at $SANDBOX =="
  else
    rm -rf "$SANDBOX"
  fi
  exit "$exit_code"
}
trap cleanup EXIT INT TERM

# ── build the fake claude ───────────────────────────────────────────────────
echo "== building fake claude (cc -std=c11 -Wall -Wextra) =="
CC_BIN="${CC:-cc}"
"$CC_BIN" -std=c11 -Wall -Wextra -O2 -o "$FAKE_BIN" "$REPO_ROOT/e2e/fake-claude/claude.c" || exit 1

# ── register the two profiles the way `csm profiles add` does it (creates
# the dir, symlinks plugins/projects to the shared SSOT, writes profiles.json
# under $HOME_DIR/.config/claude-as/) ───────────────────────────────────────
echo "== registering profiles a/b under isolated HOME=$HOME_DIR =="
env -u CLAUDE_CONFIG_DIR HOME="$HOME_DIR" "$CSM_BIN" profiles add a "$A_DIR" >"$LOG_DIR/profiles-add-a.log" 2>&1 \
  || { echo "e2e/run.sh: csm profiles add a failed:" >&2; cat "$LOG_DIR/profiles-add-a.log" >&2; exit 1; }
env -u CLAUDE_CONFIG_DIR HOME="$HOME_DIR" "$CSM_BIN" profiles add b "$B_DIR" >"$LOG_DIR/profiles-add-b.log" 2>&1 \
  || { echo "e2e/run.sh: csm profiles add b failed:" >&2; cat "$LOG_DIR/profiles-add-b.log" >&2; exit 1; }

# ── source helpers + scenarios, then run ────────────────────────────────────
# shellcheck source=e2e/lib.sh
source "$REPO_ROOT/e2e/lib.sh"
# shellcheck source=e2e/scenarios.sh
source "$REPO_ROOT/e2e/scenarios.sh"

: > "$REPORT"
echo "=== csm limit-switch e2e report ===" > "$REPORT"
echo "generated: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$REPORT"
echo "csm --version: $("$CSM_BIN" --version 2>&1)" >> "$REPORT"
echo "sandbox: $SANDBOX" >> "$REPORT"
echo "" >> "$REPORT"

START_TS=$(date +%s)
run_all_scenarios
END_TS=$(date +%s)

# ── final safety sweep: only PIDs this run recorded, plus a ps check scoped
# to binaries under this sandbox (never a name-based pkill/killall) ────────
rpt "----- final safety sweep -----"
for pid in "${ALL_PIDS[@]:-}"; do
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    rpt "  WARNING: pid $pid still alive, sending TERM (harness-owned pid)"
    kill -TERM "$pid" 2>/dev/null
  fi
done
# The supervisors' own children are not in ALL_PIDS -- the supervisor spawned
# them, not this script. They are still addressable by pid rather than by name,
# because the fake claude logs its own pid on every invocation. Under load a
# scenario can finish while one of them is between SIGTERM and exit, which is
# how a run occasionally left a sleeping fake behind.
for f in "$LOG_DIR"/*.fakeclaude.log; do
  [[ -e "$f" ]] || continue
  while read -r logged_pid; do
    if [[ -n "$logged_pid" ]] && kill -0 "$logged_pid" 2>/dev/null; then
      rpt "  WARNING: fake claude pid $logged_pid still alive, sending TERM (pid from $f)"
      kill -TERM "$logged_pid" 2>/dev/null
    fi
  done < <(awk '/^=== INVOCATION/{for(i=1;i<=NF;i++) if($i ~ /^pid=[0-9]+$/){sub(/^pid=/,"",$i); print $i}}' "$f")
done
sleep 1
rpt "  ps check (this sandbox's binaries only):"
rpt "$(ps -ax -o pid,ppid,stat,command 2>/dev/null | grep -E "$SANDBOX/(bin/claude)" | grep -v grep || echo '  (none found)')"
rpt ""
rpt "=== end of report (${START_TS:+$((END_TS - START_TS))s}) ==="

echo ""
echo "############################################################"
cat "$REPORT"
echo "############################################################"

PASS_COUNT=$(grep -c "VERDICT: PASS" "$REPORT" || true)
FAIL_COUNT=$(grep -c "VERDICT: FAIL" "$REPORT" || true)
echo ""
echo "csm e2e: $PASS_COUNT passed, $FAIL_COUNT failed (of $((PASS_COUNT + FAIL_COUNT)) scenarios), $((END_TS - START_TS))s"

if (( FAIL_COUNT > 0 )); then
  # No artifact upload to rely on (the sandbox is removed on every exit path
  # unless --keep) -- so dump every per-invocation log to stdout here, which
  # is what CI actually has to debug from.
  echo ""
  echo "############################################################"
  echo "## FAILURE: dumping every per-scenario log under $LOG_DIR"
  echo "############################################################"
  for f in "$LOG_DIR"/*.log; do
    [[ -e "$f" ]] || continue
    echo "----- $f -----"
    cat "$f"
  done
  exit 1
fi
exit 0
