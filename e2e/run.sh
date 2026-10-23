#!/usr/bin/env bash
# e2e/run.sh -- csm's end-to-end harness.
#
#   bash e2e/run.sh [--keep] [--timeout <secs>] [scenario...]
#
# Builds csm once with the `e2e` feature (the sandbox seams, see src/e2e.rs)
# and the fake claude once, lays out a sandbox under /tmp with its own HOME,
# and runs every scenario in e2e/scenarios.sh (or the ones named) against:
#   - a fake Orca: its userData with a profile index, orca-data.json and
#     stashes, a main process holding SingletonLock, and an NDJSON runtime
#     socket (e2e/fakes/orca.pl) that edits the store the way Orca does;
#   - a fake Keychain (e2e/fakes/security.pl, run through /usr/bin/perl);
#   - a loopback stand-in for Anthropic's OAuth endpoints (e2e/fakes/http.pl);
#   - a fake `claude` (e2e/fake-claude/claude.c).
# Nothing reaches the network, the real Keychain, the real Orca or the real
# home: csm runs under `env -i` with HOME in the sandbox, and the e2e build
# exits 97 when HOME is anywhere else.
#
# Each scenario runs in its own subshell with a time limit (--timeout,
# default 90 s). Afterwards any process whose command line names the sandbox
# is killed, and a scenario that left one behind fails. The sandbox is
# removed at exit unless --keep is given.
#
# Needs: cargo, cc, /usr/bin/perl (JSON::PP, Digest::SHA, Time::HiRes,
# IO::Socket::UNIX), /usr/bin/script. Runs on macOS and Linux.
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd -P)
KEEP=0
TIMEOUT=90
ONLY=()
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1; shift ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    -*) echo "e2e/run.sh: unknown flag $1" >&2; exit 2 ;;
    *) ONLY+=("$1"); shift ;;
  esac
done

case "$(uname -s)" in
  Darwin) HOST_OS=mac ;;
  Linux) HOST_OS=linux ;;
  *) echo "e2e/run.sh: macOS or Linux only" >&2; exit 2 ;;
esac
for tool in cargo cc; do
  command -v "$tool" >/dev/null || { echo "e2e/run.sh: $tool not found" >&2; exit 2; }
done
[ -x /usr/bin/perl ] && [ -x /usr/bin/script ] || { echo "e2e/run.sh: needs /usr/bin/perl and /usr/bin/script" >&2; exit 2; }

# ─── sandbox ───────────────────────────────────────────────────────────────────
# /tmp keeps the socket path short (unix sockets allow about 104 bytes) and
# `pwd -P` makes every path physical (/private/tmp on macOS), so the paths
# csm sees in the process table match the ones the harness writes.

SANDBOX=$(mktemp -d /tmp/csm-e2e.XXXXXX) || exit 1
SANDBOX=$(cd "$SANDBOX" && pwd -P)
case "$SANDBOX" in
  /tmp/csm-e2e.*|/private/tmp/csm-e2e.*) ;;
  *) echo "e2e/run.sh: unexpected sandbox path $SANDBOX" >&2; exit 1 ;;
esac
HOME_DIR="$SANDBOX/home"
D="$HOME_DIR/.claude"
STATE="$HOME_DIR/.local/state/csm"
if [ "$HOST_OS" = mac ]; then
  UD="$HOME_DIR/Library/Application Support/orca"
  # Case-insensitive: the late userData (see World.pm late_ud) is UD itself.
  STASH_UD="$UD"
else
  UD="$HOME_DIR/.config/orca"
  # Orca keeps stashes under the late userData <appData>/Orca.
  STASH_UD="$HOME_DIR/.config/Orca"
fi
FAKES="$REPO/e2e/fakes"
BIN="$SANDBOX/bin"
KC_ROOT="$SANDBOX/keychain"
HTTP_DIR="$SANDBOX/http"
mkdir -p "$BIN" "$KC_ROOT/items" "$HTTP_DIR" "$SANDBOX/logs" "$SANDBOX/tmp" "$SANDBOX/transcripts"

HTTP_PID=""

# sweep: TERM, then KILL, every process whose command line names the
# sandbox, except the HTTP stand-in (run-wide). Prints what it found.
sweep() {
  local pids p i
  pids=$(ps -A -o pid=,command= | S="$SANDBOX/" K="${HTTP_PID:-none}" awk 'index($0, ENVIRON["S"]) && $1 != ENVIRON["K"] { print $1 }')
  [ -z "$pids" ] && return 0
  for p in $pids; do
    ps -o pid=,command= -p "$p" 2>/dev/null | sed 's/^/    left behind: /'
    kill -TERM "$p" 2>/dev/null
  done
  i=0
  while [ "$i" -lt 40 ]; do
    local any=0
    for p in $pids; do kill -0 "$p" 2>/dev/null && any=1; done
    [ "$any" = 0 ] && break
    /bin/sleep 0.05
    i=$((i + 1))
  done
  for p in $pids; do kill -KILL "$p" 2>/dev/null; done
  return 1
}

cleanup() {
  local rc=$?
  if [ -n "$HTTP_PID" ]; then
    kill -TERM "$HTTP_PID" 2>/dev/null
    wait "$HTTP_PID" 2>/dev/null
  fi
  HTTP_PID=""
  sweep >/dev/null
  if [ "$KEEP" = 1 ]; then
    echo "e2e: sandbox kept at $SANDBOX"
  else
    rm -rf "$SANDBOX"
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ─── build once ────────────────────────────────────────────────────────────────

echo "== cargo build --features e2e --bin csm"
(cd "$REPO" && cargo build -q --features e2e --bin csm) || exit 1
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
cp "$TARGET_DIR/debug/csm" "$BIN/csm" || exit 1

echo "== cc e2e/fake-claude/claude.c"
"${CC:-cc}" -std=c11 -Wall -Wextra -Werror -O2 -o "$BIN/claude" "$REPO/e2e/fake-claude/claude.c" || exit 1

# The fake Orca's main process is the same binary under Orca's name, so
# csm's main-executable match accepts it. macOS also gets the bundle's
# Info.plist, where csm reads Orca's version; Linux has no such file and
# gets the version from CSM_E2E_ORCA_VERSION.
cat >"$SANDBOX/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleShortVersionString</key>
  <string>1.4.214</string>
</dict>
</plist>
EOF
if [ "$HOST_OS" = mac ]; then
  mkdir -p "$SANDBOX/Orca.app/Contents/MacOS"
  cp "$SANDBOX/Info.plist" "$SANDBOX/Orca.app/Contents/Info.plist"
  ORCA_EXE="$SANDBOX/Orca.app/Contents/MacOS/Orca"
else
  mkdir -p "$SANDBOX/orca-app"
  ORCA_EXE="$SANDBOX/orca-app/orca-ide"
fi
ln "$BIN/claude" "$ORCA_EXE" || exit 1

# ─── the HTTP stand-in (run-wide) ──────────────────────────────────────────────

/usr/bin/perl "$FAKES/http.pl" "$HTTP_DIR" </dev/null >"$SANDBOX/logs/http.log" 2>&1 &
HTTP_PID=$!
i=0
while [ ! -s "$HTTP_DIR/port" ] && [ "$i" -lt 100 ]; do /bin/sleep 0.05; i=$((i + 1)); done
HTTP_PORT=$(cat "$HTTP_DIR/port" 2>/dev/null)
[ -n "$HTTP_PORT" ] || { echo "e2e/run.sh: the HTTP stand-in did not start" >&2; exit 1; }

# shellcheck source=e2e/lib.sh
. "$REPO/e2e/lib.sh"
# shellcheck source=e2e/scenarios.sh
. "$REPO/e2e/scenarios.sh"

if [ "${#ONLY[@]}" -gt 0 ]; then
  for n in "${ONLY[@]}"; do
    declare -F "sc_$n" >/dev/null || { echo "e2e/run.sh: no scenario $n" >&2; exit 2; }
  done
  SCENARIOS=("${ONLY[@]}")
fi

# ─── run ───────────────────────────────────────────────────────────────────────

# run_scenario <name>: run sc_<name> in a subshell under the time limit,
# then sweep. Prints one PASS/FAIL line.
run_scenario() {
  local name="$1" sp i start verdict
  LOGS="$SANDBOX/logs/$name"
  mkdir -p "$LOGS"
  : >"$LOGS/pids"
  start=$(date +%s)
  (
    FAILED=0
    SC=$name
    echo "== $name"
    "sc_$name"
    wait 2>/dev/null
    cp "$STATE/limit-switch.log" "$LOGS/limit-switch.log" 2>/dev/null
    echo "$FAILED" >"$LOGS/failed"
  ) >"$LOGS/scenario.log" 2>&1 &
  sp=$!
  i=0
  while kill -0 "$sp" 2>/dev/null && [ "$i" -lt $((TIMEOUT * 10)) ]; do
    /bin/sleep 0.1
    i=$((i + 1))
  done
  verdict=PASS
  if kill -0 "$sp" 2>/dev/null; then
    echo "  TIMEOUT after ${TIMEOUT}s" >>"$LOGS/scenario.log"
    kill -TERM "$sp" 2>/dev/null
    verdict=FAIL
  fi
  wait "$sp" 2>/dev/null
  if ! sweep >>"$LOGS/scenario.log" 2>&1; then
    echo "  FAIL  the scenario left processes behind" >>"$LOGS/scenario.log"
    verdict=FAIL
  fi
  [ "$(cat "$LOGS/failed" 2>/dev/null)" = 0 ] || verdict=FAIL
  printf '%-4s %-28s %3ss\n' "$verdict" "$name" "$(($(date +%s) - start))"
  [ "$verdict" = PASS ]
}

echo "== $(uname -s) $(uname -m), sandbox $SANDBOX"
PASSED=0
FAILED_NAMES=()
T0=$(date +%s)
for name in "${SCENARIOS[@]}"; do
  if run_scenario "$name"; then
    PASSED=$((PASSED + 1))
  else
    FAILED_NAMES+=("$name")
  fi
done

for name in "${FAILED_NAMES[@]:-}"; do
  [ -n "$name" ] || continue
  echo
  echo "################ $name"
  cat "$SANDBOX/logs/$name/scenario.log"
  for f in "$SANDBOX/logs/$name"/transcript "$SANDBOX/logs/$name"/*.sup.log "$SANDBOX/logs/$name"/*.claude.log \
    "$SANDBOX/logs/$name"/claude.log "$SANDBOX/logs/$name"/orca-requests.log "$SANDBOX/logs/$name"/orca-requests.log.stderr \
    "$SANDBOX/logs/$name"/limit-switch.log; do
    [ -f "$f" ] || continue
    echo "---- $f"
    cat "$f"
  done
done

echo
echo "csm e2e: $PASSED passed, ${#FAILED_NAMES[@]} failed (of ${#SCENARIOS[@]}), $(($(date +%s) - T0))s"
[ "${#FAILED_NAMES[@]}" -eq 0 ]
