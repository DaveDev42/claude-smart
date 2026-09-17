#!/usr/bin/env bash
# e2e/lib.sh -- polling/assertion/process helpers for the csm limit-switch e2e
# harness. Sourced by run.sh after it has set up the sandbox; expects these
# globals to already be set: CSM_BIN, FAKE_BIN, USAGE_CMD_SCRIPT, HOME_DIR,
# A_DIR, B_DIR, SMART_DIR, LOG_DIR, TRANSCRIPTS_DIR, REPORT, and the ALL_PIDS
# array. Never runs the real `claude`; only ever kills PIDs this harness
# itself started and recorded (in ALL_PIDS or a scenario's own *_PID var).
#
# Portable to both macOS (BSD userland) and Linux (GNU userland, ubuntu-latest
# CI): no `stat -f`/`-c`, no `sed -i ''`, no `date -r` at run time, no
# `mktemp -t`, no `pkill`/`killall` (kills go through `kill -TERM <pid>` on a
# PID this script recorded itself), no GNU-only `timeout`.

rpt() { echo "$@" | tee -a "$REPORT"; }

# ── polling helpers ─────────────────────────────────────────────────────────

wait_for_invocation_count() {
  local f="$1" want="$2" timeout="${3:-10}"
  local max_iters=$(( timeout * 5 )); local i=0
  while (( i < max_iters )); do
    local c
    c=$(grep -c "^=== INVOCATION" "$f" 2>/dev/null || echo 0)
    if (( c >= want )); then return 0; fi
    sleep 0.2
    i=$((i+1))
  done
  return 1
}

wait_for_pattern() {
  local f="$1" pat="$2" timeout="${3:-10}"
  local max_iters=$(( timeout * 5 )); local i=0
  while (( i < max_iters )); do
    if [[ -f "$f" ]] && grep -q -- "$pat" "$f" 2>/dev/null; then
      return 0
    fi
    sleep 0.2
    i=$((i+1))
  done
  [[ -f "$f" ]] && grep -q -- "$pat" "$f" 2>/dev/null
}

get_invocation_pid() {
  # NOTE: must not match on "ppid=" -- "ppid=" contains "pid=" as a
  # substring, so a naive `grep -oE 'pid=[0-9]+'` over the whole line
  # matches both the child's own pid= field AND the ppid= field, producing
  # a corrupted 2-line value. Split into space-delimited tokens and match
  # only a token that is EXACTLY "pid=<digits>".
  local f="$1" idx="$2"
  awk -v want="$idx" '
    /^=== INVOCATION/{n++; if(n==want){line=$0}}
    END{
      nf = split(line, arr, " ");
      for (i=1; i<=nf; i++) {
        if (arr[i] ~ /^pid=[0-9]+$/) { sub(/^pid=/, "", arr[i]); print arr[i]; exit }
      }
    }
  ' "$f"
}

get_invocation_field() {
  local f="$1" idx="$2" argvidx="$3"
  awk -v want="$idx" -v ai="$argvidx" '
    /^=== INVOCATION/{n++}
    n==want && $0 ~ ("^argv\\[" ai "\\]="){
      sub("^argv\\[" ai "\\]=", ""); print; f=1
    }
    n==want && /^=== END/ && f{exit}
  ' "$f"
}

get_invocation_configdir() {
  local f="$1" idx="$2"
  awk -v want="$idx" '
    /^=== INVOCATION/{n++; if(n==want){line=$0}}
    END{print line}
  ' "$f" | sed -E 's/.*config_dir=([^ ]+) ===/\1/'
}

invocation_has_arg() {
  # invocation_has_arg <log> <idx> <value>  -> exit 0 if some argv[i]=<value> in that block
  local f="$1" idx="$2" val="$3"
  awk -v want="$idx" -v v="$val" '
    /^=== INVOCATION/{n++}
    n==want && /^argv\[[0-9]+\]=/ { sub(/^argv\[[0-9]+\]=/, ""); if ($0 == v) { found=1 } }
    n==want && /^=== END/ { exit }
    END { exit found ? 0 : 1 }
  ' "$f"
}

count_invocations() {
  grep -c "^=== INVOCATION" "$1" 2>/dev/null || echo 0
}

safe_term() {
  local pid="$1"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null
    local i=0
    while kill -0 "$pid" 2>/dev/null && (( i < 25 )); do sleep 0.2; i=$((i+1)); done
  fi
}

is_alive() {
  local pid="$1"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

set_last_switch_fresh() { date +%s > "$SMART_DIR/.last-switch"; }
clear_last_switch() { rm -f "$SMART_DIR/.last-switch"; }

# ── supervisor lifecycle ────────────────────────────────────────────────────
# start_supervisor <label> <fixture> [extra csm-run args...]
# Sets globals: SUP_PID, FAKE_LOG, SID, CHILD_PID
start_supervisor() {
  local label="$1" fixture="$2"; shift 2
  local extra_csm_args=("$@")
  FAKE_LOG="$LOG_DIR/${label}.fakeclaude.log"
  local suplog="$LOG_DIR/${label}.supervisor.log"
  rm -f "$FAKE_LOG"

  env -u CLAUDE_CONFIG_DIR \
    HOME="$HOME_DIR" \
    CSM_USAGE_API_BASE="http://127.0.0.1:9" \
    CLAUDE_SMART_CLAUDE_BIN="$FAKE_BIN" \
    CSM_USAGE_CMD="$USAGE_CMD_SCRIPT" \
    CSM_USAGE_FIXTURE="$fixture" \
    CLAUDE_USAGE_TTL=0 CSM_USAGE_TTL_SECS=0 \
    FAKE_LOG="$FAKE_LOG" \
    "$CSM_BIN" run --profile a -n "${extra_csm_args[@]}" > "$suplog" 2>&1 &
  SUP_PID=$!
  ALL_PIDS+=("$SUP_PID")

  if ! wait_for_invocation_count "$FAKE_LOG" 1 10; then
    rpt "  [$label] FAIL: fake claude never logged an invocation. supervisor log:"
    rpt "$(cat "$suplog" 2>/dev/null)"
    SID=""; CHILD_PID=""
    return 1
  fi
  SID=$(get_invocation_field "$FAKE_LOG" 1 2)
  CHILD_PID=$(get_invocation_pid "$FAKE_LOG" 1)
  rpt "  [$label] supervisor pid=$SUP_PID fake-claude pid=$CHILD_PID sid=$SID"
  return 0
}

# run_hook <owner_dir> <json_payload> [extra env "K=V" ...]
# Sets globals: HOOK_STDOUT, HOOK_STDERR, HOOK_EXIT
run_hook() {
  local owner="$1" json="$2"; shift 2
  local extra_env=("$@")
  local outfile errfile
  outfile=$(mktemp "$LOG_DIR/hookout.XXXXXX")
  errfile=$(mktemp "$LOG_DIR/hookerr.XXXXXX")
  printf '%s' "$json" | env -u CLAUDE_CONFIG_DIR \
    HOME="$HOME_DIR" \
    CSM_USAGE_API_BASE="http://127.0.0.1:9" \
    CLAUDE_SMART_CLAUDE_BIN="$FAKE_BIN" \
    CSM_USAGE_CMD="$USAGE_CMD_SCRIPT" \
    CSM_USAGE_FIXTURE="${CUR_FIXTURE}" \
    CLAUDE_USAGE_TTL=0 CSM_USAGE_TTL_SECS=0 \
    "${extra_env[@]}" \
    "$CSM_BIN" hook --owner "$owner" >"$outfile" 2>"$errfile"
  HOOK_EXIT=$?
  HOOK_STDOUT=$(cat "$outfile")
  HOOK_STDERR=$(cat "$errfile")
  rm -f "$outfile" "$errfile"
}

# run_capture <owner_dir> <statusline_json> [extra env "K=V" ...]
# The statusline tick: `csm usage capture` with CLAUDE_CONFIG_DIR set to the
# owning profile (that is how the real statusLine wrapper runs it).
# Sets globals: CAP_STDOUT, CAP_EXIT
run_capture() {
  local owner="$1" json="$2"; shift 2
  local extra_env=("$@")
  local outfile
  outfile=$(mktemp "$LOG_DIR/capout.XXXXXX")
  printf '%s' "$json" | env \
    HOME="$HOME_DIR" \
    CLAUDE_CONFIG_DIR="$owner" \
    CSM_USAGE_API_BASE="http://127.0.0.1:9" \
    CLAUDE_SMART_CLAUDE_BIN="$FAKE_BIN" \
    CSM_USAGE_CMD="$USAGE_CMD_SCRIPT" \
    CSM_USAGE_FIXTURE="${CUR_FIXTURE}" \
    CLAUDE_USAGE_TTL=0 CSM_USAGE_TTL_SECS=0 \
    "${extra_env[@]}" \
    "$CSM_BIN" usage capture >"$outfile" 2>&1
  CAP_EXIT=$?
  CAP_STDOUT=$(cat "$outfile")
  rm -f "$outfile"
}

statusline_json() {
  # statusline_json <sid> <cwd> <five_hour_pct> <seven_day_pct>
  local sid="$1" cwd="$2" fh="$3" sd="$4"
  local tp="$TRANSCRIPTS_DIR/$sid.jsonl"
  cat <<EOF
{"hook_event_name":"Status","session_id":"$sid","transcript_path":"$tp","cwd":"$cwd","model":{"id":"claude-fable-5-1","display_name":"Fable 5.1"},"workspace":{"current_dir":"$cwd","project_dir":"$cwd"},"version":"2.1.270","rate_limits":{"five_hour":{"used_percentage":$fh,"resets_at":1789985119},"seven_day":{"used_percentage":$sd,"resets_at":1789985119}}}
EOF
}
