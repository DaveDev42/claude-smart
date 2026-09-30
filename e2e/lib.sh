#!/usr/bin/env bash
# e2e/lib.sh -- helpers for csm's end-to-end harness. Sourced by run.sh after
# it has built the binaries and laid out the sandbox; scenarios.sh uses them.
#
# Globals run.sh sets before sourcing: REPO SANDBOX HOME_DIR D STATE UD FAKES
# BIN KC_ROOT HTTP_DIR HTTP_PORT HOST_OS (mac|linux) ORCA_EXE. Per scenario
# (set by run_scenario): SC (its name) and LOGS (its log dir).
#
# Rules every helper keeps:
# - csm, the fakes and the fixture helper run under `env -i` with an explicit
#   environment whose HOME is the sandbox's; sbx_check refuses anything else,
#   and csm's own e2e build exits when HOME is outside CSM_E2E_SANDBOX.
# - No network: the OAuth and usage endpoints point at http.pl on loopback,
#   usage comes from CSM_USAGE_CMD, and the Keychain is security.pl.
# - Nothing is signalled by name. A scenario kills the pids it recorded, and
#   run.sh's sweep only touches processes whose command line names the
#   sandbox.
# - Sleeps are /bin/sleep in bounded polls.
#
# Portable to macOS (BSD userland) and Linux (GNU userland): no `stat -c/-f`,
# no `sed -i`, no `readlink -f`, no `timeout`, no `pkill`.

# ─── reporting ─────────────────────────────────────────────────────────────────

say() { printf '  %s\n' "$*"; }

# check <description> <command...>: run the command; record a failed check.
check() {
  local what="$1"
  shift
  if "$@"; then
    say "ok    $what"
  else
    say "FAIL  $what"
    FAILED=$((FAILED + 1))
  fi
}

# check_not <description> <command...>: the command must fail.
check_not() {
  local what="$1"
  shift
  if "$@"; then
    say "FAIL  $what"
    FAILED=$((FAILED + 1))
  else
    say "ok    $what"
  fi
}

has() { grep -q -- "$2" "$1" 2>/dev/null; }
has_fixed() { grep -qF -- "$2" "$1" 2>/dev/null; }
eq() { [ "$1" = "$2" ] || { say "      want [$2] got [$1]"; return 1; }; }
lines() { if [ -f "$1" ]; then wc -l <"$1" | tr -d ' '; else echo 0; fi; }

# show <file>: print a file into the scenario log, indented.
show() {
  [ -f "$1" ] || { say "($1 absent)"; return 0; }
  say "--- $1"
  sed 's/^/      /' "$1"
}

# ─── the sandbox environment ───────────────────────────────────────────────────

sbx_check() {
  case "$HOME_DIR" in
    "$SANDBOX"/home) ;;
    *) echo "e2e: HOME_DIR is not the sandbox's; refusing" >&2; exit 97 ;;
  esac
  case "$UD" in
    "$HOME_DIR"/*) ;;
    *) echo "e2e: Orca userData is outside the sandbox home; refusing" >&2; exit 97 ;;
  esac
}

E2E_PATH_BASE="/usr/bin:/bin:/usr/sbin:/sbin"

# The whole environment of one csm call, in ENVV. EXTRA (an array the caller
# may set) is appended last; PATH_PREFIX (a string) goes in front of the
# sandbox bin dir. The login session's CLAUDE_CONFIG_DIR (the legacy floor)
# is the file $LOGS/session-floor (floor_set, floor_value), and the boot id
# is $BOOT_ID (default boot-1; a scenario reboots by changing it).
build_env() {
  sbx_check
  ENVV=(
    "HOME=$HOME_DIR" "USER=e2e" "LOGNAME=e2e" "LANG=C" "TERM=xterm"
    "PATH=${PATH_PREFIX:+$PATH_PREFIX:}$BIN:$E2E_PATH_BASE"
    "TMPDIR=$SANDBOX/tmp"
    "CSM_E2E_SANDBOX=$SANDBOX"
    "CSM_E2E_SECURITY=$FAKES/security.pl"
    "CSM_E2E_SECURITY_ROOT=$KC_ROOT"
    "CSM_E2E_POINT_HOOK=$FAKES/point-hook.sh"
    "CSM_USAGE_API_BASE=http://127.0.0.1:$HTTP_PORT"
    "CSM_OAUTH_TOKEN_URL=http://127.0.0.1:$HTTP_PORT/v1/oauth/token"
    "CSM_USAGE_CMD=/bin/sh $FAKES/usage-cmd.sh"
    "CSM_USAGE_TTL_SECS=0" "CLAUDE_USAGE_TTL=0"
    "E2E_USAGE_FIXTURE=$LOGS/usage.json"
    "E2E_USAGE_CALLS=$LOGS/usage-calls"
    "E2E_HOME=$HOME_DIR" "E2E_UD=$UD" "E2E_FAKES=$FAKES" "E2E_SEC_ROOT=$KC_ROOT"
    "E2E_ORCA_EXE=$ORCA_EXE" "E2E_ORCA_LOG=$LOGS/orca-requests.log"
    "E2E_ORCA_PIDS=$LOGS/orca.pids" "E2E_PIDS=$LOGS/pids"
    "E2E_POINT_MARK=$LOGS/point.fired"
    "CSM_E2E_SESSION_FLOOR_FILE=$LOGS/session-floor"
    "CSM_E2E_BOOT_ID=${BOOT_ID:-boot-1}"
    "FAKE_LOG=${FAKE_LOG:-$LOGS/claude.log}"
  )
  if [ "$HOST_OS" = linux ]; then
    ENVV+=("CSM_E2E_ORCA_VERSION=1.4.214")
  fi
  if [ "${#EXTRA[@]}" -gt 0 ]; then
    ENVV+=("${EXTRA[@]}")
  fi
}

EXTRA=()

# csm <args...>: run csm to completion; stdout+stderr go to $OUT (default
# $LOGS/out), the exit code to RC. stdin is /dev/null. PROG replaces the
# program (the `claude` alias).
csm() {
  local out="${OUT:-$LOGS/out}"
  build_env
  env -i "${ENVV[@]}" "${PROG:-$BIN/csm}" "$@" </dev/null >"$out" 2>&1
  RC=$?
  { printf '$ %s' "${PROG:-csm}"; printf ' %q' "$@"; printf '   (exit %s)\n' "$RC"; sed 's/^/    /' "$out"; } >>"$LOGS/transcript"
  return 0
}

# csm_stdin <input> <args...>: csm with <input> on stdin (a pipe, so not a
# terminal). Stdout to $LOGS/stdout, stderr to $LOGS/stderr, exit code to RC.
csm_stdin() {
  local input="$1"
  shift
  build_env
  printf '%s' "$input" | env -i "${ENVV[@]}" "$BIN/csm" "$@" >"$LOGS/stdout" 2>"$LOGS/stderr"
  RC=$?
  { printf '$ csm'; printf ' %q' "$@"; printf '   (stdin, exit %s)\n' "$RC"; sed 's/^/    out: /' "$LOGS/stdout"; sed 's/^/    err: /' "$LOGS/stderr"; } >>"$LOGS/transcript"
  return 0
}

# world <command...>: the fixture helper (e2e/fakes/world.pl).
world() {
  sbx_check
  env -i "HOME=$HOME_DIR" "USER=e2e" "PATH=$E2E_PATH_BASE" \
    "E2E_HOME=$HOME_DIR" "E2E_UD=$UD" "E2E_SEC_ROOT=$KC_ROOT" \
    /usr/bin/perl -I "$FAKES" "$FAKES/world.pl" "$@"
}

A_ID=aaaaaaaa-0000-4000-8000-00000000000a
B_ID=bbbbbbbb-0000-4000-8000-00000000000b

# fresh_world: wipe the sandbox home and lay out a new one: Orca's store with
# alice (active) and bob, their stashes, D logged in as alice, an Orca.app
# for version detection on macOS, and an empty csm state dir.
fresh_world() {
  sbx_check
  rm -rf "$HOME_DIR"
  mkdir -p "$HOME_DIR" "$STATE" "$SANDBOX/tmp" "$KC_ROOT/items"
  : >"$KC_ROOT/calls"
  if [ "$HOST_OS" = mac ]; then
    mkdir -p "$HOME_DIR/Applications/Orca.app/Contents"
    cp "$SANDBOX/Info.plist" "$HOME_DIR/Applications/Orca.app/Contents/Info.plist"
  fi
  world reset || { say "FAIL  world reset"; FAILED=$((FAILED + 1)); }
  rm -f "$HTTP_DIR"/profile/* "$HTTP_DIR"/token/* "$HTTP_DIR"/usage/*
}

# ─── usage fixtures ────────────────────────────────────────────────────────────

now_iso() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# section <pct> <resets_at>
section() { printf '{"pct":%s,"resets":null,"resets_at":%s}' "$1" "$2"; }

# profile_json <captured_at> <session> <week_all> <week_fable>
profile_json() {
  local at="$1" r=$(($(date +%s) + 3 * 86400))
  printf '{"captured_at":"%s","session":%s,"week_all":%s,"week_fable":%s,"week_model_label":"Fable","session_stats":[],"source":"cmd"}' \
    "$at" "$(section "$2" "$r")" "$(section "$3" "$r")" "$(section "$4" "$r")"
}

# usage <a_session> <a_week> <a_fable> <b_session> <b_week> <b_fable> [captured_at]
# Writes the CSM_USAGE_CMD fixture and csm's positive cache (what the hook
# reads; it never runs the command) with the same reading.
usage() {
  local at="${7:-$(now_iso)}"
  local body
  body=$(printf '{"captured_at":"%s","profiles":{"%s":%s,"%s":%s}}' "$at" \
    "$A_ID" "$(profile_json "$at" "$1" "$2" "$3")" \
    "$B_ID" "$(profile_json "$at" "$4" "$5" "$6")")
  printf '%s\n' "$body" >"$LOGS/usage.json"
  mkdir -p "$STATE"
  printf '%s\n' "$body" >"$STATE/.usage-cache.json"
}

usage_healthy() { usage 10 20 10 10 20 10 "$@"; }
usage_a_capped() { usage 10 100 20 5 10 5 "$@"; }
usage_b_capped() { usage 5 10 5 10 100 20 "$@"; }
usage_both_capped() { usage 10 100 20 10 100 20 "$@"; }

# store_record <id> <session> <week_all> <week_fable> [source]: csm's own
# per-account usage record, as an API probe would leave it.
store_record() {
  local at
  at=$(now_iso)
  mkdir -p "$STATE/usage"
  printf '{"profile":"%s","captured_at":"%s","source":"api","api_captured_at":"%s","cooldown_until":null,"usage":%s}\n' \
    "$1" "$at" "$at" "$(profile_json "$at" "$2" "$3" "$4" | sed 's/"source":"cmd"/"source":"api"/')" \
    >"$STATE/usage/$1.json"
}

stamp_last_switch() { date +%s >"$STATE/.last-switch"; }

# ─── the fake HTTP endpoints ───────────────────────────────────────────────────

# http_rule <profile|token|usage> <key> <status> <body>
http_rule() {
  mkdir -p "$HTTP_DIR/$1"
  printf '%s\n%s' "$3" "$4" >"$HTTP_DIR/$1/$2"
}

# ─── the fake Orca ─────────────────────────────────────────────────────────────

# start_orca: the fake Orca. ORCA_D=<dir> starts its main process with
# CLAUDE_CONFIG_DIR=<dir> (an Orca the legacy floor reached), so its D is
# that dir; ORCA_MATERIALIZE=1 makes it put the active account into D at
# start, as Orca does.
start_orca() {
  build_env
  [ -n "${ORCA_D:-}" ] && ENVV+=("E2E_ORCA_CONFIG_DIR=$ORCA_D")
  [ -n "${ORCA_MATERIALIZE:-}" ] && ENVV+=("E2E_ORCA_MATERIALIZE=1")
  env -i "${ENVV[@]}" /bin/sh "$FAKES/start-orca.sh" || {
    say "FAIL  the fake Orca did not start"
    FAILED=$((FAILED + 1))
    return 1
  }
}

# stop_orca: TERM both fake Orca processes and wait until they are gone and
# the runtime file is removed.
stop_orca() {
  [ -f "$LOGS/orca.pids" ] || return 0
  local p
  for p in $(cat "$LOGS/orca.pids"); do kill -TERM "$p" 2>/dev/null; done
  for p in $(cat "$LOGS/orca.pids"); do wait_dead "$p" 5; done
  rm -f "$LOGS/orca.pids"
}

# orca_call <method> <params-json>: the Orca GUI doing something.
orca_call() {
  sbx_check
  env -i "HOME=$HOME_DIR" "USER=e2e" "PATH=$E2E_PATH_BASE" \
    "E2E_HOME=$HOME_DIR" "E2E_UD=$UD" "E2E_SEC_ROOT=$KC_ROOT" \
    /usr/bin/perl -I "$FAKES" "$FAKES/orca.pl" call "$1" "$2" >>"$LOGS/gui-calls" 2>&1
}

# ─── processes ─────────────────────────────────────────────────────────────────

alive() { [ -n "$1" ] && kill -0 "$1" 2>/dev/null; }

# wait_dead <pid> <seconds>
wait_dead() {
  local i=0 n=$(($2 * 20))
  while alive "$1" && [ "$i" -lt "$n" ]; do /bin/sleep 0.05; i=$((i + 1)); done
  ! alive "$1"
}

# poll <seconds> <command...>: true as soon as the command succeeds.
poll() {
  local n=$(($1 * 10)) i=0
  shift
  while [ "$i" -lt "$n" ]; do
    "$@" && return 0
    /bin/sleep 0.1
    i=$((i + 1))
  done
  "$@"
}

# ─── the fake claude's log ─────────────────────────────────────────────────────
# INVOCATION blocks are interactive sessions; CALL blocks are one-shot runs
# (print mode, `mcp`, a non-terminal stdin). Blocks count from 1.

count_inv() { if [ -f "$1" ]; then grep -c '^=== INVOCATION' "$1"; else echo 0; fi; }
count_calls() { if [ -f "$1" ]; then grep -c '^=== CALL' "$1"; else echo 0; fi; }
inv_at_least() { [ "$(count_inv "$1")" -ge "$2" ]; }

# inv_field <log> <n> <key>: pid | ppid | config_dir of the nth INVOCATION.
inv_field() {
  awk -v want="$2" -v key="$3" '
    /^=== INVOCATION/ { n++; if (n == want) { for (i = 1; i <= NF; i++) if (index($i, key "=") == 1) { print substr($i, length(key) + 2); exit } } }
  ' "$1"
}

# inv_argv <log> <n>: the nth INVOCATION's argv, one per line, argv[0] first.
inv_argv() {
  awk -v want="$2" '
    /^=== INVOCATION/ { n++ }
    n == want && /^argv\[[0-9]+\]=/ { sub(/^argv\[[0-9]+\]=/, ""); print }
    n == want && /^=== END/ { exit }
  ' "$1"
}

# inv_arg <log> <n> <i>: argv[i] of the nth INVOCATION.
inv_arg() { inv_argv "$1" "$2" | sed -n "$(($3 + 1))p"; }

# inv_has <log> <n> <value>
inv_has() { inv_argv "$1" "$2" | grep -qxF -- "$3"; }

# inv_pair <log> <n> <flag> <value>: <flag> immediately followed by <value>.
inv_pair() {
  inv_argv "$1" "$2" | awk -v f="$3" -v v="$4" 'prev == f && $0 == v { ok = 1 } { prev = $0 } END { exit !ok }'
}

# call_argv <log> <n>: the nth CALL's argv.
call_argv() {
  awk -v want="$2" '
    /^=== CALL/ { n++ }
    n == want && /^argv\[[0-9]+\]=/ { sub(/^argv\[[0-9]+\]=/, ""); print }
    n == want && /^=== END/ { exit }
  ' "$1"
}

# ─── supervisors ───────────────────────────────────────────────────────────────
# A supervised launch needs a terminal on stdin (otherwise csm takes it for
# print mode), so it runs under script(1), which gives it a pty.

# start_sup <label> <csm args...>: launch `csm <args>` (or `$PROG <args>`) under script with its
# own fake-claude log. Sets SUP_PID (the script process), FLOG, SID and
# CHILD (the first fake claude's pid). Returns 1 when claude never starts.
start_sup() {
  local label="$1"
  shift
  FLOG="$LOGS/$label.claude.log"
  local slog="$LOGS/$label.sup.log"
  rm -f "$FLOG"
  FAKE_LOG="$FLOG" build_env
  if [ "$HOST_OS" = mac ]; then
    env -i "${ENVV[@]}" /usr/bin/script -q /dev/null "${PROG:-$BIN/csm}" "$@" </dev/null >"$slog" 2>&1 &
  else
    local cmd
    cmd=$(printf '%q ' "${PROG:-$BIN/csm}" "$@")
    env -i "${ENVV[@]}" /usr/bin/script -qfc "$cmd" /dev/null </dev/null >"$slog" 2>&1 &
  fi
  SUP_PID=$!
  echo "$SUP_PID" >>"$LOGS/pids"
  SID=""
  CHILD=""
  if ! poll 10 inv_at_least "$FLOG" 1; then
    say "FAIL  [$label] the fake claude never started"
    show "$slog"
    FAILED=$((FAILED + 1))
    return 1
  fi
  CHILD=$(inv_field "$FLOG" 1 pid)
  SID=$(inv_argv "$FLOG" 1 | awk 'prev == "--session-id" || prev == "--resume" { print; exit } { prev = $0 }')
  say "[$label] supervisor=$SUP_PID claude=$CHILD sid=$SID"
  return 0
}

# stop_sup <sup_pid> <flog>: end a supervised launch the way a user does:
# TERM every fake claude it logged (the supervisor then exits with it), then
# wait for script to exit; TERM script as a last resort.
stop_sup() {
  local sup="$1" flog="$2" p
  if [ -f "$flog" ]; then
    for p in $(awk '/^=== INVOCATION/ { for (i = 1; i <= NF; i++) if ($i ~ /^pid=[0-9]+$/) { sub(/^pid=/, "", $i); print $i } }' "$flog"); do
      alive "$p" && kill -TERM "$p" 2>/dev/null
    done
  fi
  if ! wait_dead "$sup" 5; then
    kill -TERM "$sup" 2>/dev/null
    wait_dead "$sup" 2 || kill -KILL "$sup" 2>/dev/null
  fi
  wait "$sup" 2>/dev/null
  return 0
}

# ─── hook and statusline events ────────────────────────────────────────────────

# hook_json <event> <sid> [extra json members]
hook_json() {
  local tp="$SANDBOX/transcripts/$2.jsonl"
  printf '{"session_id":"%s","transcript_path":"%s","cwd":"/tmp/e2e-cwd","permission_mode":"default","hook_event_name":"%s"%s}' \
    "$2" "$tp" "$1" "${3:+,$3}"
}

rate_limit_json() {
  hook_json StopFailure "$1" '"error":"rate_limit","error_details":"You have reached your weekly limit."'
}
stop_json() { hook_json Stop "$1" '"stop_hook_active":false,"last_assistant_message":"done"'; }

# hook <json>: `csm hook` with the event on stdin.
hook() { csm_stdin "$1" hook; }

# statusline_json <sid> <five_hour_pct> <seven_day_pct>
statusline_json() {
  local r=$(($(date +%s) + 3 * 86400))
  printf '{"hook_event_name":"Status","session_id":"%s","transcript_path":"%s","cwd":"/tmp/e2e-cwd","model":{"id":"claude-fable-5-1","display_name":"Fable 5.1"},"workspace":{"current_dir":"/tmp/e2e-cwd","project_dir":"/tmp/e2e-cwd"},"version":"2.1.283","rate_limits":{"five_hour":{"used_percentage":%s,"resets_at":%s},"seven_day":{"used_percentage":%s,"resets_at":%s}}}' \
    "$1" "$SANDBOX/transcripts/$1.jsonl" "$2" "$r" "$3" "$r"
}

# tick <json>: one statusLine tick (`csm usage capture`).
tick() { csm_stdin "$1" usage capture; }

# ─── idle-compact fixtures ─────────────────────────────────────────────────────

# idle_compact_json <sid> <remaining_secs> <recache_tokens> [context_window]:
# a minimal statusLine payload carrying only what idle_compact reads
# (session_id, transcript_path, prompt_cache, context_window) — deliberately
# not statusline_json's shape, since a Stop/usage-capture-side interest in
# rate_limits would be a distraction here and cmd_usage_capture's own
# attribution failing on a payload this small is fine (idle_compact runs
# regardless, see cmd/usage.rs).
idle_compact_json() {
  local sid="$1" remaining="$2" recache="$3" cw="${4:-200000}"
  local exp=$(($(date +%s) + remaining))
  printf '{"session_id":"%s","transcript_path":"%s","prompt_cache":{"warm":true,"expires_at":%s,"recache_tokens_if_cold":%s},"context_window":%s}' \
    "$sid" "$SANDBOX/transcripts/$sid.jsonl" "$exp" "$recache" "$cw"
}

# idle_compact_iso_now: the current time as the UTC ISO 8601 shape csm's
# busy check parses (transcript rows carry milliseconds; this omits them,
# which RFC 3339 allows and csm's parser still accepts).
idle_compact_iso_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# idle_compact_turn_ended <sid>: write the transcript's last real turn row
# (a timestamped `assistant` row), then a Stop hook event, so the
# transcript's mtime lands at or before the <sid>.idle marker `csm hook`
# stamps (busy_state's cheap path: mtime > idle_marker is "still busy";
# landing at or before it is "idle" with no tail read at all).
idle_compact_turn_ended() {
  local sid="$1"
  mkdir -p "$SANDBOX/transcripts"
  printf '{"type":"assistant","timestamp":"%s"}\n' "$(idle_compact_iso_now)" \
    >"$SANDBOX/transcripts/$sid.jsonl"
  hook "$(stop_json "$sid")"
}

# idle_compact_append_metadata_rows <sid> <row_json...>: append rows to the
# transcript after idle_compact_turn_ended already ran, the way Claude
# Code's own post-Stop bookkeeping (turn_duration, away_summary, mode, and
# more) keeps writing well after a turn ends. This bumps the transcript's
# mtime past the <sid>.idle stamp, so busy_state's cheap path no longer
# applies and it must read the tail instead.
idle_compact_append_metadata_rows() {
  local sid="$1"
  shift
  printf '%s\n' "$@" >>"$SANDBOX/transcripts/$sid.jsonl"
}

# idle_compact_log: the shared idle-compact.log's path under this
# scenario's STATE (mirrors switch_log's pattern for limit-switch.log).
idle_compact_log() { printf '%s\n' "$STATE/idle-compact.log"; }

# idle_compact_request_file <pid>: the hand-off request file idle_compact's
# tick would write for a supervisor running as <pid>.
idle_compact_request_file() { printf '%s\n' "$STATE/idle-compact-requests/$1.json"; }

# idle_compact_stand_in_supervisor: start a bare `/bin/sleep` in the
# background and echo its pid. idle_compact's tick only checks
# CSM_SUPERVISOR_PID's liveness (crate::platform::proc::is_running scans the
# real process table, unsandboxed) — it never execs or talks to the pid in
# any way, so any real process stands in for the future pty-relay
# supervisor. The caller stops it with idle_compact_stop_stand_in_supervisor.
# stdout/stderr are redirected away from /dev/null explicitly: left
# inherited, the backgrounded sleep would hold this function's own stdout
# pipe open (it is called as `sup_pid=$(idle_compact_stand_in_supervisor)`),
# so the command substitution would not return until sleep itself exited 60
# seconds later.
idle_compact_stand_in_supervisor() {
  /bin/sleep 60 >/dev/null 2>&1 &
  echo $!
}

# idle_compact_stop_stand_in_supervisor <pid>: terminate and reap a pid
# idle_compact_stand_in_supervisor started, mirroring stop_sup's shape.
idle_compact_stop_stand_in_supervisor() {
  kill -TERM "$1" 2>/dev/null
  wait "$1" 2>/dev/null
  return 0
}

# idle_compact_dead_pid: a pid guaranteed not to be running right now — a
# trivial subshell, started and reaped in place, so
# crate::platform::proc::is_running sees no such process. Deliberately a
# real (recently) exited pid rather than a fixed literal, so this exercises
# the same process-table scan a genuinely dead supervisor pid would.
idle_compact_dead_pid() {
  (: ) &
  local p=$!
  wait "$p" 2>/dev/null
  echo "$p"
}

# ─── the legacy layout ─────────────────────────────────────────────────────────

LEGACY_SID=0f0f0f0f-1111-4222-8333-444444444444

# legacy_world: a machine on the legacy per-profile layout. Orca's store
# holds no account and no active id; ~/.config/claude-as/profiles.json
# registers work (carol, the floor profile) and home (erin); both dirs link
# projects, history.jsonl and todos to ~/.claude.shared, which holds the
# transcript $LEGACY_SID; the login session's CLAUDE_CONFIG_DIR names
# ~/.claude.work; ~/.claude does not exist. The profile endpoint confirms
# both logins, so retire can verify their stashes.
legacy_world() {
  fresh_world
  world legacy-reset
  world legacy work carol@example.com uuid-carol at-carol-1 rt-carol-1 floor
  world legacy home erin@example.com uuid-erin at-erin-1 rt-erin-1
  world shared "$LEGACY_SID"
  floor_set "$HOME_DIR/.claude.work"
  http_rule profile at-carol-1 200 '{"account":{"uuid":"uuid-carol","email":"carol@example.com"},"organization":{"uuid":"org-acme"}}'
  http_rule profile at-erin-1 200 '{"account":{"uuid":"uuid-erin","email":"erin@example.com"},"organization":{"uuid":"org-acme"}}'
}

floor_set() { printf '%s\n' "$1" >"$LOGS/session-floor"; }
floor_value() { if [ -f "$LOGS/session-floor" ]; then tr -d '\n' <"$LOGS/session-floor"; fi; }
marker() { world marker "$1"; }
retired() { [ -d "$1.retired" ] && [ ! -e "$1" ]; }
transcript_at() { [ -f "$1/projects/-tmp-e2e-cwd/$LEGACY_SID.jsonl" ]; }

# ─── state readers ─────────────────────────────────────────────────────────────

active() { world active; }
d_refresh() { world d-refresh; }
d_uuid() { world d-uuid; }
stash_refresh() { world stash-refresh "$1"; }
switch_log() { printf '%s\n' "$STATE/limit-switch.log"; }
