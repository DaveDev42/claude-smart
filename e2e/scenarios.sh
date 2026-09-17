#!/usr/bin/env bash
# e2e/scenarios.sh -- the 10 limit-switch scenarios, ported 1:1 from the
# original standalone harness (same assertions, same numbering). Sourced by
# run.sh after lib.sh; expects the same globals as lib.sh plus FIX_HEALTHY and
# FIX_BOTH (paths to the two fixture JSON files).
#
# Coverage: 1/3/4 = hook-driven switch (StopFailure rate_limit / Stop
# usage-pct, with and without the cooldown stamp); 2 = StopFailure overloaded
# must NOT switch; 5 = both profiles capped -> notify-only, no relaunch; 6 =
# two concurrent supervisors on profile a both switch independently; 7 =
# CLAUDE_AUTO_SWITCH_RELAUNCH=0 detect-only; 8/9/10 = the statusline-tick
# switch path (cold-launch --model/--effort carry, merge of a stored
# model-scoped cap, duplicate-tick no-op, and the CLAUDE_AUTO_SWITCH=0
# kill-switch).

run_all_scenarios() {

CUR_FIXTURE="$FIX_HEALTHY"

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 1: StopFailure rate_limit, a capped, b healthy, cooldown active -----"
CUR_FIXTURE="$FIX_HEALTHY"
set_last_switch_fresh
rpt "  pre: .last-switch=$(cat "$SMART_DIR/.last-switch")"
start_supervisor "s1" "$FIX_HEALTHY"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s1","permission_mode":"default","hook_event_name":"StopFailure","error":"rate_limit","error_details":"You have reached your weekly limit for Fable."}
EOF
)
  SENTINEL_PATH="$SMART_DIR/$SID.relaunch"
  run_hook "$A_DIR" "$JSON"
  # try to catch the sentinel before the supervisor consumes it (race, best effort)
  SENTINEL_CONTENT=$(cat "$SENTINEL_PATH" 2>/dev/null || echo "(already consumed or never seen)")
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  rpt "  sentinel (best-effort capture): $SENTINEL_CONTENT"
  wait_for_pattern "$FAKE_LOG" "SIGTERM pid=$CHILD_PID" 10
  rpt "  original fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no)"
  wait_for_invocation_count "$FAKE_LOG" 2 10
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count now: $N"
  if (( N >= 2 )); then
    RELAUNCH_CFG=$(get_invocation_configdir "$FAKE_LOG" 2)
    RELAUNCH_VERB=$(get_invocation_field "$FAKE_LOG" 2 1)
    RELAUNCH_SID=$(get_invocation_field "$FAKE_LOG" 2 2)
    RELAUNCH_PID=$(get_invocation_pid "$FAKE_LOG" 2)
    rpt "  relaunch: verb=$RELAUNCH_VERB sid=$RELAUNCH_SID config_dir=$RELAUNCH_CFG pid=$RELAUNCH_PID"
  fi
  rpt "  limit-switch.log tail:"
  rpt "$(tail -5 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  rpt "  .switched marker: $(ls "$SMART_DIR/$SID.switched" 2>/dev/null && cat "$SMART_DIR/$SID.switched" || echo MISSING)"
  # verdict
  if (( N >= 2 )) && [[ "$RELAUNCH_VERB" == "--resume" && "$RELAUNCH_SID" == "$SID" && "$RELAUNCH_CFG" == "$B_DIR" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  # cleanup: terminate whichever fake-claude is now running for this sid
  if (( N >= 2 )); then safe_term "$RELAUNCH_PID"; else safe_term "$CHILD_PID"; fi
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 2: StopFailure overloaded, a capped, b healthy -----"
CUR_FIXTURE="$FIX_HEALTHY"
start_supervisor "s2" "$FIX_HEALTHY"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s2","permission_mode":"default","hook_event_name":"StopFailure","error":"overloaded","error_details":"The API is temporarily overloaded."}
EOF
)
  run_hook "$A_DIR" "$JSON"
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  rpt "  fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no)"
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count: $N (expect 1, no relaunch)"
  rpt "  sentinel present? $(test -f "$SMART_DIR/$SID.relaunch" && echo yes || echo no)"
  rpt "  .switched present? $(test -f "$SMART_DIR/$SID.switched" && echo yes || echo no)"
  rpt "  limit-switch.log tail:"
  rpt "$(tail -3 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  if (( N == 1 )) && is_alive "$CHILD_PID" && [[ ! -f "$SMART_DIR/$SID.relaunch" ]] && [[ -z "$HOOK_STDOUT" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  safe_term "$CHILD_PID"
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 3: Stop payload, a capped, b healthy, cooldown active -----"
CUR_FIXTURE="$FIX_HEALTHY"
set_last_switch_fresh
rpt "  pre: .last-switch=$(cat "$SMART_DIR/.last-switch")"
start_supervisor "s3" "$FIX_HEALTHY"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s3","permission_mode":"default","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"ok, done for now"}
EOF
)
  LAST_SWITCH_BEFORE=$(cat "$SMART_DIR/.last-switch" 2>/dev/null || echo MISSING)
  run_hook "$A_DIR" "$JSON"
  LAST_SWITCH_AFTER=$(cat "$SMART_DIR/.last-switch" 2>/dev/null || echo MISSING)
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  rpt "  .last-switch before=$LAST_SWITCH_BEFORE after=$LAST_SWITCH_AFTER (unchanged means the hook never reached the step-9 cooldown claim -- see report notes)"
  rpt "  fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no)"
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count: $N (expect 1, no relaunch -- cooldown blocks pct path)"
  rpt "  sentinel present? $(test -f "$SMART_DIR/$SID.relaunch" && echo yes || echo no)"
  rpt "  limit-switch.log tail:"
  rpt "$(tail -3 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  if (( N == 1 )) && is_alive "$CHILD_PID" && [[ ! -f "$SMART_DIR/$SID.relaunch" ]] && [[ -z "$HOOK_STDOUT" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  safe_term "$CHILD_PID"
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 4: Stop payload, a capped, b healthy, no cooldown stamp -----"
CUR_FIXTURE="$FIX_HEALTHY"
clear_last_switch
rpt "  pre: .last-switch present? $(test -f "$SMART_DIR/.last-switch" && echo yes || echo no)"
start_supervisor "s4" "$FIX_HEALTHY"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s4","permission_mode":"default","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"ok, done for now"}
EOF
)
  SENTINEL_PATH="$SMART_DIR/$SID.relaunch"
  run_hook "$A_DIR" "$JSON"
  SENTINEL_CONTENT=$(cat "$SENTINEL_PATH" 2>/dev/null || echo "(already consumed or never seen)")
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  rpt "  sentinel (best-effort capture): $SENTINEL_CONTENT"
  wait_for_pattern "$FAKE_LOG" "SIGTERM pid=$CHILD_PID" 10
  wait_for_invocation_count "$FAKE_LOG" 2 10
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count now: $N"
  if (( N >= 2 )); then
    RELAUNCH_CFG=$(get_invocation_configdir "$FAKE_LOG" 2)
    RELAUNCH_VERB=$(get_invocation_field "$FAKE_LOG" 2 1)
    RELAUNCH_SID=$(get_invocation_field "$FAKE_LOG" 2 2)
    RELAUNCH_PID=$(get_invocation_pid "$FAKE_LOG" 2)
    rpt "  relaunch: verb=$RELAUNCH_VERB sid=$RELAUNCH_SID config_dir=$RELAUNCH_CFG pid=$RELAUNCH_PID"
  fi
  rpt "  limit-switch.log tail:"
  rpt "$(tail -5 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  if (( N >= 2 )) && [[ "$RELAUNCH_VERB" == "--resume" && "$RELAUNCH_SID" == "$SID" && "$RELAUNCH_CFG" == "$B_DIR" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  if (( N >= 2 )); then safe_term "$RELAUNCH_PID"; else safe_term "$CHILD_PID"; fi
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 5: StopFailure rate_limit, both a and b capped -----"
CUR_FIXTURE="$FIX_BOTH"
start_supervisor "s5" "$FIX_BOTH"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s5","permission_mode":"default","hook_event_name":"StopFailure","error":"rate_limit","error_details":"You have reached your weekly limit for Fable."}
EOF
)
  run_hook "$A_DIR" "$JSON"
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  rpt "  fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no)"
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count: $N (expect 1, notify-only)"
  rpt "  sentinel present? $(test -f "$SMART_DIR/$SID.relaunch" && echo yes || echo no)"
  rpt "  .detected present? $(test -f "$SMART_DIR/$SID.detected" && echo yes || echo no)"
  rpt "  limit-switch.log tail:"
  rpt "$(tail -3 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  if (( N == 1 )) && is_alive "$CHILD_PID" && [[ ! -f "$SMART_DIR/$SID.relaunch" ]] && [[ -n "$HOOK_STDOUT" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  safe_term "$CHILD_PID"
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 6: two supervisors on a, both capped, both StopFailure rate_limit -----"
CUR_FIXTURE="$FIX_HEALTHY"
clear_last_switch
start_supervisor "s6a" "$FIX_HEALTHY"
SUP_PID_A="$SUP_PID"; FAKE_LOG_A="$FAKE_LOG"; SID_A="$SID"; CHILD_PID_A="$CHILD_PID"
start_supervisor "s6b" "$FIX_HEALTHY"
SUP_PID_B="$SUP_PID"; FAKE_LOG_B="$FAKE_LOG"; SID_B="$SID"; CHILD_PID_B="$CHILD_PID"

if [[ -n "$SID_A" && -n "$SID_B" ]]; then
  TP_A="$TRANSCRIPTS_DIR/$SID_A.jsonl"
  TP_B="$TRANSCRIPTS_DIR/$SID_B.jsonl"
  JSON_A=$(cat <<EOF
{"session_id":"$SID_A","transcript_path":"$TP_A","cwd":"/tmp/e2e-cwd-s6a","permission_mode":"default","hook_event_name":"StopFailure","error":"rate_limit","error_details":"weekly limit for Fable"}
EOF
)
  JSON_B=$(cat <<EOF
{"session_id":"$SID_B","transcript_path":"$TP_B","cwd":"/tmp/e2e-cwd-s6b","permission_mode":"default","hook_event_name":"StopFailure","error":"rate_limit","error_details":"weekly limit for Fable"}
EOF
)
  run_hook "$A_DIR" "$JSON_A"
  HOOK_EXIT_A=$HOOK_EXIT; HOOK_STDOUT_A=$HOOK_STDOUT
  run_hook "$A_DIR" "$JSON_B"
  HOOK_EXIT_B=$HOOK_EXIT; HOOK_STDOUT_B=$HOOK_STDOUT
  rpt "  hookA exit=$HOOK_EXIT_A stdout=$HOOK_STDOUT_A"
  rpt "  hookB exit=$HOOK_EXIT_B stdout=$HOOK_STDOUT_B"

  wait_for_invocation_count "$FAKE_LOG_A" 2 10
  wait_for_invocation_count "$FAKE_LOG_B" 2 10
  NA=$(count_invocations "$FAKE_LOG_A")
  NB=$(count_invocations "$FAKE_LOG_B")
  rpt "  invocations: A=$NA B=$NB"
  RELAUNCH_CFG_A=""; RELAUNCH_CFG_B=""; RELAUNCH_PID_A=""; RELAUNCH_PID_B=""
  if (( NA >= 2 )); then
    RELAUNCH_CFG_A=$(get_invocation_configdir "$FAKE_LOG_A" 2)
    RELAUNCH_VERB_A=$(get_invocation_field "$FAKE_LOG_A" 2 1)
    RELAUNCH_SID_A=$(get_invocation_field "$FAKE_LOG_A" 2 2)
    RELAUNCH_PID_A=$(get_invocation_pid "$FAKE_LOG_A" 2)
    rpt "  relaunch A: verb=$RELAUNCH_VERB_A sid=$RELAUNCH_SID_A config_dir=$RELAUNCH_CFG_A"
  fi
  if (( NB >= 2 )); then
    RELAUNCH_CFG_B=$(get_invocation_configdir "$FAKE_LOG_B" 2)
    RELAUNCH_VERB_B=$(get_invocation_field "$FAKE_LOG_B" 2 1)
    RELAUNCH_SID_B=$(get_invocation_field "$FAKE_LOG_B" 2 2)
    RELAUNCH_PID_B=$(get_invocation_pid "$FAKE_LOG_B" 2)
    rpt "  relaunch B: verb=$RELAUNCH_VERB_B sid=$RELAUNCH_SID_B config_dir=$RELAUNCH_CFG_B"
  fi
  rpt "  limit-switch.log tail:"
  rpt "$(tail -6 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  if (( NA >= 2 && NB >= 2 )) && [[ "$RELAUNCH_CFG_A" == "$B_DIR" && "$RELAUNCH_CFG_B" == "$B_DIR" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  if (( NA >= 2 )); then safe_term "$RELAUNCH_PID_A"; else safe_term "$CHILD_PID_A"; fi
  if (( NB >= 2 )); then safe_term "$RELAUNCH_PID_B"; else safe_term "$CHILD_PID_B"; fi
fi
safe_term "$SUP_PID_A"
safe_term "$SUP_PID_B"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 7: StopFailure rate_limit, CLAUDE_AUTO_SWITCH_RELAUNCH=0 -----"
CUR_FIXTURE="$FIX_HEALTHY"
start_supervisor "s7" "$FIX_HEALTHY"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s7","permission_mode":"default","hook_event_name":"StopFailure","error":"rate_limit","error_details":"weekly limit for Fable"}
EOF
)
  run_hook "$A_DIR" "$JSON" "CLAUDE_AUTO_SWITCH_RELAUNCH=0"
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  rpt "  fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no)"
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count: $N (expect 1, detect-only)"
  rpt "  sentinel present? $(test -f "$SMART_DIR/$SID.relaunch" && echo yes || echo no)"
  rpt "  .detected present? $(test -f "$SMART_DIR/$SID.detected" && echo yes || echo no)"
  rpt "  .switched present? $(test -f "$SMART_DIR/$SID.switched" && echo yes || echo no)"
  rpt "  limit-switch.log tail:"
  rpt "$(tail -3 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  if (( N == 1 )) && is_alive "$CHILD_PID" && [[ ! -f "$SMART_DIR/$SID.relaunch" ]] && [[ -n "$HOOK_STDOUT" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  safe_term "$CHILD_PID"
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 8: statusline tick, seven_day 100% on a, b healthy, cooldown active, no hook -----"
CUR_FIXTURE="$FIX_HEALTHY"
set_last_switch_fresh
rm -f "$SMART_DIR/usage/a.json"
start_supervisor "s8" "$FIX_HEALTHY" --model fake-model-x --effort high
if [[ -n "$SID" ]]; then
  rpt "  cold launch carries flags? model=$(invocation_has_arg "$FAKE_LOG" 1 fake-model-x && echo yes || echo no) effort=$(invocation_has_arg "$FAKE_LOG" 1 high && echo yes || echo no)"
  # a healthy tick first: must record and do nothing else
  run_capture "$A_DIR" "$(statusline_json "$SID" /tmp/e2e-cwd-s8 20 40)"
  rpt "  healthy tick exit=$CAP_EXIT stdout='$CAP_STDOUT' store=$(python3 -c "import json;d=json.load(open('$SMART_DIR/usage/a.json'));u=d['usage'];print('session',u['session']['pct'],'week_all',u['week_all']['pct'],'src',d['source'])" 2>&1)"
  rpt "  after healthy tick: sentinel? $(test -f "$SMART_DIR/$SID.relaunch" && echo yes || echo no) switched? $(test -f "$SMART_DIR/$SID.switched" && echo yes || echo no) fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no)"
  # the capped tick
  run_capture "$A_DIR" "$(statusline_json "$SID" /tmp/e2e-cwd-s8 21 100)"
  rpt "  capped tick exit=$CAP_EXIT stdout='$CAP_STDOUT'"
  wait_for_pattern "$FAKE_LOG" "SIGTERM pid=$CHILD_PID" 10
  rpt "  original fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no)"
  wait_for_invocation_count "$FAKE_LOG" 2 10
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count now: $N"
  if (( N >= 2 )); then
    RELAUNCH_CFG=$(get_invocation_configdir "$FAKE_LOG" 2)
    RELAUNCH_VERB=$(get_invocation_field "$FAKE_LOG" 2 1)
    RELAUNCH_SID=$(get_invocation_field "$FAKE_LOG" 2 2)
    RELAUNCH_PID=$(get_invocation_pid "$FAKE_LOG" 2)
    rpt "  relaunch: verb=$RELAUNCH_VERB sid=$RELAUNCH_SID config_dir=$RELAUNCH_CFG pid=$RELAUNCH_PID"
    FLAGS_OK=no
    if invocation_has_arg "$FAKE_LOG" 2 --model && invocation_has_arg "$FAKE_LOG" 2 fake-model-x \
       && invocation_has_arg "$FAKE_LOG" 2 --effort && invocation_has_arg "$FAKE_LOG" 2 high; then FLAGS_OK=yes; fi
    rpt "  relaunch carries --model/--effort? $FLAGS_OK"
  fi
  # a late duplicate tick (the overlapping-process case) must be a no-op
  run_capture "$A_DIR" "$(statusline_json "$SID" /tmp/e2e-cwd-s8 21 100)"
  N2=$(count_invocations "$FAKE_LOG")
  rpt "  duplicate tick: exit=$CAP_EXIT invocation count still $N2 (expect $N)"
  rpt "  limit-switch.log tail:"
  rpt "$(tail -3 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  rpt "  .switched marker: $(cat "$SMART_DIR/$SID.switched" 2>/dev/null || echo MISSING)"
  if (( N >= 2 )) && (( N2 == N )) && [[ "$RELAUNCH_VERB" == "--resume" && "$RELAUNCH_SID" == "$SID" && "$RELAUNCH_CFG" == "$B_DIR" ]] \
     && [[ "$FLAGS_OK" == yes ]] && [[ -z "$CAP_STDOUT" ]] && grep -q "via=statusline" "$SMART_DIR/limit-switch.log"; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  if (( N >= 2 )); then safe_term "$RELAUNCH_PID"; else safe_term "$CHILD_PID"; fi
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 9: statusline tick, seven_day healthy but stored week_fable 100% on a -----"
CUR_FIXTURE="$FIX_HEALTHY"
clear_last_switch
mkdir -p "$SMART_DIR/usage"
# seed a's store with an api probe that saw the model-scoped cap; the tick
# carries only five_hour/seven_day and must merge this in
cat > "$SMART_DIR/usage/a.json" <<EOF
{"profile":"a","captured_at":"2026-09-14T10:00:00Z","source":"api","api_captured_at":"2026-09-14T10:00:00Z","cooldown_until":null,"usage":{"captured_at":"2026-09-14T10:00:00Z","session":{"pct":10,"resets":null,"resets_at":1789985119},"week_all":{"pct":40,"resets":null,"resets_at":1789985119},"week_fable":{"pct":100,"resets":null,"resets_at":1789985119},"week_model_label":"Fable","session_stats":[],"source":"api","attention":null}}
EOF
start_supervisor "s9" "$FIX_HEALTHY"
if [[ -n "$SID" ]]; then
  run_capture "$A_DIR" "$(statusline_json "$SID" /tmp/e2e-cwd-s9 12 45)"
  rpt "  tick exit=$CAP_EXIT stdout='$CAP_STDOUT'"
  wait_for_pattern "$FAKE_LOG" "SIGTERM pid=$CHILD_PID" 10
  wait_for_invocation_count "$FAKE_LOG" 2 10
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count now: $N"
  if (( N >= 2 )); then
    RELAUNCH_CFG=$(get_invocation_configdir "$FAKE_LOG" 2)
    RELAUNCH_VERB=$(get_invocation_field "$FAKE_LOG" 2 1)
    RELAUNCH_SID=$(get_invocation_field "$FAKE_LOG" 2 2)
    RELAUNCH_PID=$(get_invocation_pid "$FAKE_LOG" 2)
    rpt "  relaunch: verb=$RELAUNCH_VERB sid=$RELAUNCH_SID config_dir=$RELAUNCH_CFG pid=$RELAUNCH_PID"
  fi
  rpt "  limit-switch.log tail:"
  rpt "$(tail -2 "$SMART_DIR/limit-switch.log" 2>/dev/null)"
  if (( N >= 2 )) && [[ "$RELAUNCH_VERB" == "--resume" && "$RELAUNCH_SID" == "$SID" && "$RELAUNCH_CFG" == "$B_DIR" ]] \
     && tail -2 "$SMART_DIR/limit-switch.log" | grep -q "week_fable 100%.*via=statusline\|limit-switch sid=.*via=statusline"; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  if (( N >= 2 )); then safe_term "$RELAUNCH_PID"; else safe_term "$CHILD_PID"; fi
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 10: statusline tick, a capped, CLAUDE_AUTO_SWITCH=0 kill-switch -----"
CUR_FIXTURE="$FIX_HEALTHY"
rm -f "$SMART_DIR/usage/a.json"
start_supervisor "s10" "$FIX_HEALTHY"
if [[ -n "$SID" ]]; then
  run_capture "$A_DIR" "$(statusline_json "$SID" /tmp/e2e-cwd-s10 21 100)" "CLAUDE_AUTO_SWITCH=0"
  sleep 1
  N=$(count_invocations "$FAKE_LOG")
  rpt "  tick exit=$CAP_EXIT invocation count: $N (expect 1) fake-claude alive? $(is_alive "$CHILD_PID" && echo yes || echo no) sentinel? $(test -f "$SMART_DIR/$SID.relaunch" && echo yes || echo no) switched? $(test -f "$SMART_DIR/$SID.switched" && echo yes || echo no)"
  rpt "  store recorded anyway: $(python3 -c "import json;d=json.load(open('$SMART_DIR/usage/a.json'));print('week_all',d['usage']['week_all']['pct'])" 2>&1)"
  if (( N == 1 )) && is_alive "$CHILD_PID" && [[ ! -f "$SMART_DIR/$SID.relaunch" && ! -f "$SMART_DIR/$SID.switched" ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  safe_term "$CHILD_PID"
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 11: relaunch carries the launch's session-shaping flags, not its prompt -----"
CUR_FIXTURE="$FIX_HEALTHY"
set_last_switch_fresh
EXTRA_DIR="$SANDBOX/extra-dir"
mkdir -p "$EXTRA_DIR"
# --add-dir's values end at the next flag, so the prompt here is a plain
# positional: carried flags yes, prompt no, and no separator needed.
start_supervisor "s11" "$FIX_HEALTHY" \
  --add-dir "$EXTRA_DIR" --dangerously-skip-permissions "do the thing"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s11","permission_mode":"default","hook_event_name":"StopFailure","error":"rate_limit","error_details":"You have reached your weekly limit for Fable."}
EOF
)
  run_hook "$A_DIR" "$JSON"
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  wait_for_invocation_count "$FAKE_LOG" 2 10
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count now: $N"
  CARRIES_SKIP=no; CARRIES_ADDDIR=no; CARRIES_DIR=no; CARRIES_PROMPT=no
  if (( N >= 2 )); then
    RELAUNCH_PID=$(get_invocation_pid "$FAKE_LOG" 2)
    invocation_has_arg "$FAKE_LOG" 2 "--dangerously-skip-permissions" && CARRIES_SKIP=yes
    invocation_has_arg "$FAKE_LOG" 2 "--add-dir" && CARRIES_ADDDIR=yes
    invocation_has_arg "$FAKE_LOG" 2 "$EXTRA_DIR" && CARRIES_DIR=yes
    invocation_has_arg "$FAKE_LOG" 2 "do the thing" && CARRIES_PROMPT=yes
    rpt "  relaunch argv: $(get_invocation_argv "$FAKE_LOG" 2 | tail -n +2 | tr '\n' ' ')"
    rpt "  carries: skip-permissions=$CARRIES_SKIP add-dir=$CARRIES_ADDDIR dir=$CARRIES_DIR prompt=$CARRIES_PROMPT (prompt must be no)"
  fi
  rpt "  limit-switch.log dropped line: $(grep 'dropped passthru' "$SMART_DIR/limit-switch.log" 2>/dev/null | tail -1)"
  # The log names flags and counts everything else -- it must never quote the prompt.
  PROMPT_IN_LOG=no
  grep -q 'do the thing' "$SMART_DIR/limit-switch.log" 2>/dev/null && PROMPT_IN_LOG=yes
  rpt "  prompt text in limit-switch.log? $PROMPT_IN_LOG (must be no)"
  if (( N >= 2 )) && [[ "$CARRIES_SKIP" == yes && "$CARRIES_ADDDIR" == yes && "$CARRIES_DIR" == yes \
        && "$CARRIES_PROMPT" == no && "$PROMPT_IN_LOG" == no ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  if (( N >= 2 )); then safe_term "$RELAUNCH_PID"; else safe_term "$CHILD_PID"; fi
fi
safe_term "$SUP_PID"
rpt ""

# ═══════════════════════════════════════════════════════════════════════════
rpt "----- Scenario 12: an open --add-dir run is closed with -- before the handoff -----"
CUR_FIXTURE="$FIX_HEALTHY"
set_last_switch_fresh
# --add-dir last: claude would read the handoff prompt as one more directory,
# so the hop must emit `--` between them.
start_supervisor "s12" "$FIX_HEALTHY" --add-dir "$EXTRA_DIR"
if [[ -n "$SID" ]]; then
  TP="$TRANSCRIPTS_DIR/$SID.jsonl"
  JSON=$(cat <<EOF
{"session_id":"$SID","transcript_path":"$TP","cwd":"/tmp/e2e-cwd-s12","permission_mode":"default","hook_event_name":"StopFailure","error":"rate_limit","error_details":"You have reached your weekly limit for Fable."}
EOF
)
  run_hook "$A_DIR" "$JSON"
  rpt "  hook exit=$HOOK_EXIT stdout=$HOOK_STDOUT"
  wait_for_invocation_count "$FAKE_LOG" 2 10
  N=$(count_invocations "$FAKE_LOG")
  rpt "  invocation count now: $N"
  SEP_BEFORE_HANDOFF=no
  if (( N >= 2 )); then
    RELAUNCH_PID=$(get_invocation_pid "$FAKE_LOG" 2)
    RELAUNCH_ARGV=$(get_invocation_argv "$FAKE_LOG" 2)
    rpt "  relaunch argv: $(printf '%s\n' "$RELAUNCH_ARGV" | tail -n +2 | tr '\n' ' ')"
    # the last two tokens must be `--` then the handoff prompt
    if [[ "$(printf '%s\n' "$RELAUNCH_ARGV" | tail -2 | head -1)" == "--" ]]; then
      SEP_BEFORE_HANDOFF=yes
    fi
    rpt "  separator immediately before the handoff? $SEP_BEFORE_HANDOFF"
  fi
  if (( N >= 2 )) && [[ "$SEP_BEFORE_HANDOFF" == yes ]]; then
    rpt "  VERDICT: PASS"
  else
    rpt "  VERDICT: FAIL"
  fi
  if (( N >= 2 )); then safe_term "$RELAUNCH_PID"; else safe_term "$CHILD_PID"; fi
fi
safe_term "$SUP_PID"
rpt ""

}
