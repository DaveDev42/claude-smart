#!/usr/bin/env bash
# e2e/scenarios.sh -- the scenarios. Sourced by run.sh after lib.sh; each
# `sc_<name>` runs in its own subshell with a fresh sandbox home (see
# run_scenario in run.sh), records failed checks in FAILED, and must stop
# every process it started (supervisors through stop_sup, the fake Orca
# through stop_orca). SCENARIOS lists them in run order.
#
# The world every scenario starts from (fresh_world): Orca's store holds
# alice (active) and bob, both stashed; D (~/.claude) is logged in as alice;
# Orca is not running; csm's state dir is empty.

SCENARIOS=(
  guard
  rate_limit_switch
  overloaded_no_switch
  stop_pct_cooldown
  stop_pct_switch
  both_capped_notify
  relaunch_off
  leader_follower
  statusline_switch
  fable_fallback
  auto_switch_off
  carry_flags
  separator
  hop_cap
  switch_then_fallback
  switch_via_orca
  gui_switch_follow
  store_orca_at_l1
  store_orca_at_l2
  readback_owner
  quarantine_401
  quarantine_refresh_owner
  quarantine_mismatch
  accounts_import_rm
  passthrough
  print_context
  orca_pane_resume
  alias_dispatch
  sessionend_budget
  migrate
)

now_ms() { /usr/bin/perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }
iso_days_ago() { /usr/bin/perl -MPOSIX=strftime -e 'print strftime("%Y-%m-%dT%H:%M:%SZ", gmtime(time - $ARGV[0] * 86400))' "$1"; }
lt() { [ "$1" -lt "$2" ] || { say "      $1 is not below $2"; return 1; }; }
count_is() { eq "$(lines "$1")" "$2"; }
the_new_id() { world ids | grep -vxF -e "$A_ID" -e "$B_ID"; }

# switched_to_bob <label>: the checks every A->B limit switch shares, after
# the hook fired for $SID under supervisor <label>.
switched_to_bob() {
  check "the capped claude got SIGTERM" poll 10 has_fixed "$FLOG" "=== SIGTERM pid=$CHILD "
  check "the supervisor relaunched claude" poll 15 inv_at_least "$FLOG" 2
  check "the relaunch resumes the same session" inv_pair "$FLOG" 2 --resume "$SID"
  check "the relaunch runs in D (no CLAUDE_CONFIG_DIR pin)" eq "$(inv_field "$FLOG" 2 config_dir)" "(unset)"
  check "the supervisor says what it did" poll 5 has_fixed "$LOGS/$1.sup.log" "csm: account alice capped; resumed on bob"
  check "Orca's store now has bob active" eq "$(active)" "$B_ID"
  check "D holds bob's grant" eq "$(d_refresh)" rt-bob-1
  check "D's identity is bob" eq "$(d_uuid)" uuid-bob
}

# ─── guard ─────────────────────────────────────────────────────────────────────

# The e2e build refuses to run with HOME outside the sandbox (exit 97).
sc_guard() {
  fresh_world
  env -i "HOME=/nonexistent-csm-e2e-home" "PATH=$E2E_PATH_BASE" "CSM_E2E_SANDBOX=$SANDBOX" \
    "$BIN/csm" --version >"$LOGS/out" 2>&1
  check "csm exits 97 with HOME outside the sandbox" eq "$?" 97
  env -i "HOME=$HOME_DIR" "PATH=$E2E_PATH_BASE" "$BIN/csm" --version >"$LOGS/out" 2>&1
  check "csm exits 97 without CSM_E2E_SANDBOX" eq "$?" 97
  env -i "HOME=$HOME_DIR" "PATH=$E2E_PATH_BASE" "CSM_E2E_SANDBOX=$SANDBOX" \
    "XDG_STATE_HOME=/nonexistent-csm-e2e-state" "$BIN/csm" --version >"$LOGS/out" 2>&1
  check "csm exits 97 with an XDG dir outside the sandbox" eq "$?" 97
  csm --version
  check "csm runs inside the sandbox" eq "$RC" 0
}

# ─── limit switch (ported from the profile-era harness) ───────────────────────

# StopFailure rate_limit is definitive: it switches even inside the cooldown.
sc_rate_limit_switch() {
  fresh_world
  usage_healthy
  stamp_last_switch
  start_sup s run -n || return
  usage_a_capped
  hook "$(rate_limit_json "$SID")"
  check "the hook exits 0" eq "$RC" 0
  switched_to_bob s
  check "the switch is logged" has "$(switch_log)" "limit-switch sid=${SID:0:8} to=$B_ID"
  check "the session is marked switched" test -f "$STATE/$SID.switched"
  stop_sup "$SUP_PID" "$FLOG"
}

# A StopFailure that is not a rate limit does nothing.
sc_overloaded_no_switch() {
  fresh_world
  usage_healthy
  start_sup s run -n || return
  usage_a_capped
  hook "$(hook_json StopFailure "$SID" '"error":"overloaded","error_details":"The API is overloaded."')"
  /bin/sleep 1
  check "no relaunch" eq "$(count_inv "$FLOG")" 1
  check "claude still runs" alive "$CHILD"
  check "no sentinel" test ! -e "$STATE/sentinel/$SID.json"
  check "the hook prints nothing" test ! -s "$LOGS/stdout"
  check "alice stays active" eq "$(active)" "$A_ID"
  stop_sup "$SUP_PID" "$FLOG"
}

# A usage-% trip at Stop is not definitive: the cooldown blocks it.
sc_stop_pct_cooldown() {
  fresh_world
  usage_healthy
  stamp_last_switch
  start_sup s run -n || return
  usage_a_capped
  hook "$(stop_json "$SID")"
  /bin/sleep 1
  check "no relaunch" eq "$(count_inv "$FLOG")" 1
  check "claude still runs" alive "$CHILD"
  check "no sentinel" test ! -e "$STATE/sentinel/$SID.json"
  check "alice stays active" eq "$(active)" "$A_ID"
  stop_sup "$SUP_PID" "$FLOG"
}

# The same trip with no cooldown stamp switches.
sc_stop_pct_switch() {
  fresh_world
  usage_healthy
  start_sup s run -n || return
  usage_a_capped
  hook "$(stop_json "$SID")"
  switched_to_bob s
  stop_sup "$SUP_PID" "$FLOG"
}

# Both accounts capped: notify only, no relaunch, no switch.
sc_both_capped_notify() {
  fresh_world
  usage_healthy
  start_sup s run -n || return
  usage_both_capped
  hook "$(rate_limit_json "$SID")"
  /bin/sleep 1
  check "no relaunch" eq "$(count_inv "$FLOG")" 1
  check "claude still runs" alive "$CHILD"
  check "the hook emits a notification" test -s "$LOGS/stdout"
  check "no sentinel" test ! -e "$STATE/sentinel/$SID.json"
  check "the log says notify-only" has "$(switch_log)" "notify-only sid=${SID:0:8}"
  check "alice stays active" eq "$(active)" "$A_ID"
  stop_sup "$SUP_PID" "$FLOG"
}

# CLAUDE_AUTO_SWITCH_RELAUNCH=0: detect and notify, never relaunch.
sc_relaunch_off() {
  fresh_world
  usage_healthy
  start_sup s run -n || return
  usage_a_capped
  EXTRA=("CLAUDE_AUTO_SWITCH_RELAUNCH=0")
  hook "$(rate_limit_json "$SID")"
  EXTRA=()
  /bin/sleep 1
  check "no relaunch" eq "$(count_inv "$FLOG")" 1
  check "claude still runs" alive "$CHILD"
  check "the hook emits a notification" test -s "$LOGS/stdout"
  check "no sentinel" test ! -e "$STATE/sentinel/$SID.json"
  check "alice stays active" eq "$(active)" "$A_ID"
  stop_sup "$SUP_PID" "$FLOG"
}

# Two sessions on alice. The first to hit the cap leads: it switches D and
# leaves a follow file for the other, which relaunches at its next turn
# boundary without a second switch.
sc_leader_follower() {
  fresh_world
  usage_healthy
  start_sup lead run -n || return
  local l_sup=$SUP_PID l_log=$FLOG l_sid=$SID l_child=$CHILD
  start_sup follow run -n || { stop_sup "$l_sup" "$l_log"; return; }
  local f_sup=$SUP_PID f_log=$FLOG f_sid=$SID
  usage_a_capped
  hook "$(rate_limit_json "$l_sid")"
  SID=$l_sid FLOG=$l_log CHILD=$l_child switched_to_bob lead
  check "the leader left a follow file for its peer" poll 5 test -f "$STATE/follow/$f_sid.json"
  check "the follower keeps running until its turn ends" eq "$(count_inv "$f_log")" 1
  hook "$(stop_json "$f_sid")"
  check "the follower relaunched" poll 15 inv_at_least "$f_log" 2
  check "the follower resumes its own session" inv_pair "$f_log" 2 --resume "$f_sid"
  check "the follower says it followed" poll 5 has "$LOGS/follow.sup.log" "resumed on bob"
  check "the follow is logged" has "$(switch_log)" "follow sid=${f_sid:0:8} to=$B_ID"
  check "the follow file is consumed" poll 5 test ! -e "$STATE/follow/$f_sid.json"
  check "bob stays active" eq "$(active)" "$B_ID"
  stop_sup "$l_sup" "$l_log"
  stop_sup "$f_sup" "$f_log"
}

# The statusLine tick is the switch path for caps that fire no hook. It is
# definitive (the cooldown does not block it), the relaunch keeps the
# launch's --model/--effort, and a late duplicate tick is a no-op.
sc_statusline_switch() {
  fresh_world
  usage_healthy
  stamp_last_switch
  start_sup s run -n --model fake-model-x --effort high || return
  usage_a_capped
  tick "$(statusline_json "$SID" 20 40)"
  /bin/sleep 1
  check "a healthy tick does nothing" eq "$(count_inv "$FLOG")" 1
  tick "$(statusline_json "$SID" 21 100)"
  switched_to_bob s
  check "the relaunch keeps --model" inv_pair "$FLOG" 2 --model fake-model-x
  check "the relaunch keeps --effort" inv_pair "$FLOG" 2 --effort high
  check "the switch is logged via=statusline" has "$(switch_log)" "limit-switch sid=${SID:0:8} to=$B_ID.*via=statusline"
  tick "$(statusline_json "$SID" 21 100)"
  /bin/sleep 2
  check "a duplicate tick does not relaunch again" eq "$(count_inv "$FLOG")" 2
  check "the tick prints nothing" test ! -s "$LOGS/stdout"
  stop_sup "$SUP_PID" "$FLOG"
}

# A stored model-scoped (week_fable) cap relaunches on the same account with
# --model opus, and a second tick on the same reading does nothing.
sc_fable_fallback() {
  fresh_world
  usage_healthy
  store_record "$A_ID" 10 40 100
  start_sup s run -n || return
  tick "$(statusline_json "$SID" 12 45)"
  check "the fallback relaunched claude" poll 15 inv_at_least "$FLOG" 2
  check "the relaunch resumes the session" inv_pair "$FLOG" 2 --resume "$SID"
  check "the relaunch asks for --model opus" inv_pair "$FLOG" 2 --model opus
  check "the supervisor names the model" poll 5 has_fixed "$LOGS/s.sup.log" "csm: model-scoped cap on alice; resumed on model opus"
  check "alice stays active" eq "$(active)" "$A_ID"
  check "no account switch was claimed" test ! -e "$STATE/$SID.switched"
  check "the fallback marker is set" test -f "$STATE/$SID.model-fallback"
  check "the fallback is logged" has "$(switch_log)" "model-fallback=opus account=$A_ID.*via=statusline"
  local before
  before=$(lines "$(switch_log)")
  tick "$(statusline_json "$SID" 12 45)"
  /bin/sleep 2
  check "a second tick on the same reading does not relaunch" eq "$(count_inv "$FLOG")" 2
  check "nor log anything" eq "$(lines "$(switch_log)")" "$before"
  stop_sup "$SUP_PID" "$FLOG"
}

# CLAUDE_AUTO_SWITCH=0 turns the statusline switch off.
sc_auto_switch_off() {
  fresh_world
  usage_healthy
  start_sup s run -n || return
  EXTRA=("CLAUDE_AUTO_SWITCH=0")
  tick "$(statusline_json "$SID" 21 100)"
  EXTRA=()
  /bin/sleep 1
  check "no relaunch" eq "$(count_inv "$FLOG")" 1
  check "claude still runs" alive "$CHILD"
  check "no sentinel" test ! -e "$STATE/sentinel/$SID.json"
  check "not marked switched" test ! -e "$STATE/$SID.switched"
  stop_sup "$SUP_PID" "$FLOG"
}

# The relaunch replays session-shaping claude flags and drops the prompt,
# and the log never quotes the prompt.
sc_carry_flags() {
  fresh_world
  usage_healthy
  mkdir -p "$SANDBOX/extra-dir"
  start_sup s run -n --add-dir "$SANDBOX/extra-dir" --dangerously-skip-permissions "do the thing" || return
  usage_a_capped
  hook "$(rate_limit_json "$SID")"
  check "relaunched" poll 15 inv_at_least "$FLOG" 2
  check "keeps --dangerously-skip-permissions" inv_has "$FLOG" 2 --dangerously-skip-permissions
  check "keeps --add-dir <dir>" inv_pair "$FLOG" 2 --add-dir "$SANDBOX/extra-dir"
  check_not "drops the prompt" inv_has "$FLOG" 2 "do the thing"
  check_not "the log never quotes the prompt" has_fixed "$(switch_log)" "do the thing"
  stop_sup "$SUP_PID" "$FLOG"
}

# An open --add-dir is closed with `--` before the handoff prompt.
sc_separator() {
  fresh_world
  usage_healthy
  mkdir -p "$SANDBOX/extra-dir"
  start_sup s run -n --add-dir "$SANDBOX/extra-dir" || return
  usage_a_capped
  hook "$(rate_limit_json "$SID")"
  check "relaunched" poll 15 inv_at_least "$FLOG" 2
  check "\`--\` sits right before the handoff" eq "$(inv_argv "$FLOG" 2 | tail -2 | head -1)" "--"
  stop_sup "$SUP_PID" "$FLOG"
}

# One automatic switch per chain. The hook refuses a second hop, and when
# the hook is told to allow more (CLAUDE_MAX_HOPS), the supervisor still
# stops at its own cap.
sc_hop_cap() {
  fresh_world
  usage_healthy
  start_sup s run -n || return
  usage_a_capped
  hook "$(rate_limit_json "$SID")"
  switched_to_bob s
  rm -f "$STATE/$SID.switched" "$STATE/.last-switch"
  usage_b_capped
  hook "$(rate_limit_json "$SID")"
  /bin/sleep 2
  check "the hook refuses a second hop" eq "$(count_inv "$FLOG")" 2
  check "no sentinel" test ! -e "$STATE/sentinel/$SID.json"
  rm -f "$STATE/$SID.switched" "$STATE/.last-switch" "$STATE/$SID.detected"
  EXTRA=("CLAUDE_MAX_HOPS=5")
  hook "$(rate_limit_json "$SID")"
  EXTRA=()
  check "the supervisor stops at its cap" poll 10 has_fixed "$LOGS/s.sup.log" "csm: limit-switch hop cap (1) reached"
  check "and exits" wait_dead "$SUP_PID" 10
  check "without a third launch" eq "$(count_inv "$FLOG")" 2
  check "bob stays active" eq "$(active)" "$B_ID"
  stop_sup "$SUP_PID" "$FLOG"
}

# After a switch has spent the chain's hop, a model-scoped cap on the new
# account still gets its same-account fallback.
sc_switch_then_fallback() {
  fresh_world
  usage_healthy
  start_sup s run -n || return
  usage_a_capped
  hook "$(rate_limit_json "$SID")"
  switched_to_bob s
  store_record "$B_ID" 10 40 100
  /bin/sleep 1
  tick "$(statusline_json "$SID" 12 45)"
  check "the fallback relaunched claude" poll 15 inv_at_least "$FLOG" 3
  check "on the same session" inv_pair "$FLOG" 3 --resume "$SID"
  check "with --model opus" inv_pair "$FLOG" 3 --model opus
  check "the fallback is on bob" has "$(switch_log)" "model-fallback=opus account=$B_ID"
  check "bob stays active" eq "$(active)" "$B_ID"
  stop_sup "$SUP_PID" "$FLOG"
}

# ─── Orca running ──────────────────────────────────────────────────────────────

# With Orca running every account change goes through its RPC.
sc_switch_via_orca() {
  fresh_world
  start_orca || return
  csm accounts
  check "\`csm accounts\` lists both accounts" eq "$(grep -c -e alice@example.com -e bob@example.com "$LOGS/out")" 2
  csm accounts use bob@example.com
  check "the switch exits 0" eq "$RC" 0
  check "it went through Orca" has_fixed "$LOGS/out" "csm: switched to bob (via Orca)"
  check "Orca got selectClaude from csm" has_fixed "$LOGS/orca-requests.log" "csm accounts.selectClaude {\"accountId\":\"$B_ID\"}"
  check "bob is active" eq "$(active)" "$B_ID"
  check "D holds bob's grant" eq "$(d_refresh)" rt-bob-1
  csm accounts use bob@example.com
  check "a second use succeeds" eq "$RC" 0
  check "and bob stays active" eq "$(active)" "$B_ID"
  csm accounts rm bob@example.com
  check_not "the active account cannot be removed" eq "$RC" 0
  stop_orca
}

# The Orca GUI switches while a session runs. When that session then hits
# the cap, its supervisor sees the new active account and follows it
# instead of switching again.
sc_gui_switch_follow() {
  fresh_world
  usage_healthy
  start_orca || return
  start_sup s run -n || { stop_orca; return; }
  orca_call accounts.selectClaude "{\"accountId\":\"$B_ID\"}"
  check "the GUI switched Orca's store" eq "$(active)" "$B_ID"
  usage_a_capped
  hook "$(rate_limit_json "$SID")"
  switched_to_bob s
  check "csm asked Orca for the active account" has "$LOGS/orca-requests.log" "^csm accounts.list"
  check_not "csm did not switch again" has "$LOGS/orca-requests.log" "^csm accounts.selectClaude"
  stop_sup "$SUP_PID" "$FLOG"
  stop_orca
}

# Orca starts in the middle of an offline store write. At L1 (the new store
# is written to its temp file, not yet renamed) csm restores D and redoes
# the switch over RPC; at L2 (renamed) it leaves D and confirms over RPC.
store_orca_at() {
  fresh_world
  EXTRA=("E2E_POINT_AT=$1")
  csm accounts use bob@example.com
  EXTRA=()
  check "the switch exits 0" eq "$RC" 0
  check "Orca appeared at $1" test -f "$LOGS/point.fired"
  check "the route says offline, then via Orca" has_fixed "$LOGS/out" "csm: switched to bob (offline, then via Orca)"
  check "bob is active" eq "$(active)" "$B_ID"
  check "D holds bob's grant" eq "$(d_refresh)" rt-bob-1
  check "D's identity is bob" eq "$(d_uuid)" uuid-bob
  check "csm talked to the new Orca" has "$LOGS/orca-requests.log" "^csm accounts\."
  check "no temp store left behind" eq "$(find "$UD/profiles" -name '*.tmp*' | wc -l | tr -d ' ')" 0
}

sc_store_orca_at_l1() {
  store_orca_at store-L1
  check "at L1 the switch is redone with selectClaude" has "$LOGS/orca-requests.log" "^csm accounts.selectClaude"
  stop_orca
}

sc_store_orca_at_l2() {
  store_orca_at store-L2
  stop_orca
}

# ─── read-back and quarantine (offline switch, step 3) ────────────────────────

# D holds a newer grant for alice than her stash (Claude Code refreshed it).
rotated_d() {
  fresh_world
  world rotate-d at-alice-2 rt-alice-2
}

# no_secret_in <file...>: no refresh or access token of the fixtures.
no_secret_in() {
  ! cat "$@" 2>/dev/null | grep -qE '(rt|at)-(alice|bob)-[0-9]'
}

# The profile check confirms the grant is alice's: it goes to her stash.
sc_readback_owner() {
  rotated_d
  http_rule profile at-alice-2 200 '{"account":{"uuid":"uuid-alice","email":"alice@example.com"},"organization":{"uuid":"org-acme"}}'
  csm accounts use bob@example.com
  check "the switch exits 0" eq "$RC" 0
  check "offline" has_fixed "$LOGS/out" "csm: switched to bob (offline)"
  check "the profile endpoint was asked about D's grant" has_fixed "$HTTP_DIR/requests.log" "GET /api/oauth/profile at-alice-2"
  check "alice's stash holds the newer grant" eq "$(stash_refresh "$A_ID")" rt-alice-2
  check "D holds bob's grant" eq "$(d_refresh)" rt-bob-1
  check "csm printed no token" no_secret_in "$LOGS/out"
}

# 401 and the refresh fails: quarantined, alice's stash untouched, and
# doctor lists it.
sc_quarantine_401() {
  rotated_d
  http_rule profile at-alice-2 401 '{"error":"unauthorized"}'
  http_rule token rt-alice-2 400 '{"error":"invalid_grant"}'
  csm accounts use bob@example.com
  check "the switch exits 0" eq "$RC" 0
  check "offline" has_fixed "$LOGS/out" "csm: switched to bob (offline)"
  check "the refresh was tried" has_fixed "$HTTP_DIR/requests.log" "POST /v1/oauth/token rt-alice-2"
  check "alice's stash is untouched" eq "$(stash_refresh "$A_ID")" rt-alice-1
  check "D holds bob's grant" eq "$(d_refresh)" rt-bob-1
  csm accounts doctor --offline
  check "doctor lists the quarantined grant" has_fixed "$LOGS/out" "quarantined grant"
  check "csm printed no token" no_secret_in "$LOGS/transcript"
}

# 401, then the refresh works and the new grant is alice's: it goes to her
# stash and the quarantine entry is dropped.
sc_quarantine_refresh_owner() {
  rotated_d
  http_rule profile at-alice-2 401 '{"error":"unauthorized"}'
  http_rule token rt-alice-2 200 '{"access_token":"at-alice-3","refresh_token":"rt-alice-3","expires_in":28800}'
  http_rule profile at-alice-3 200 '{"account":{"uuid":"uuid-alice","email":"alice@example.com"},"organization":{"uuid":"org-acme"}}'
  csm accounts use bob@example.com
  check "the switch exits 0" eq "$RC" 0
  check "alice's stash holds the refreshed grant" eq "$(stash_refresh "$A_ID")" rt-alice-3
  check "D holds bob's grant" eq "$(d_refresh)" rt-bob-1
  csm accounts doctor --offline
  check_not "nothing stays quarantined" has_fixed "$LOGS/out" "quarantined grant"
  check "csm printed no token" no_secret_in "$LOGS/transcript"
}

# The grant belongs to someone else: quarantined, never stashed as alice.
sc_quarantine_mismatch() {
  rotated_d
  http_rule profile at-alice-2 200 '{"account":{"uuid":"uuid-mallory","email":"mallory@example.com"},"organization":{"uuid":"org-acme"}}'
  csm accounts use bob@example.com
  check "the switch exits 0" eq "$RC" 0
  check "alice's stash is untouched" eq "$(stash_refresh "$A_ID")" rt-alice-1
  check "D holds bob's grant" eq "$(d_refresh)" rt-bob-1
  csm accounts doctor --offline
  check "doctor lists the quarantined grant" has_fixed "$LOGS/out" "quarantined grant"
  check "csm printed no token" no_secret_in "$LOGS/transcript"
}

# ─── accounts import / rm ──────────────────────────────────────────────────────

sc_accounts_import_rm() {
  fresh_world
  local dir="$HOME_DIR/extra-login"
  world login-dir "$dir" carol@example.com uuid-carol at-carol-1 rt-carol-1
  csm accounts import "$dir"
  check "offline import exits 0" eq "$RC" 0
  check "offline import says so" has "$LOGS/out" "csm: imported carol@example.com (.*) (offline)"
  local c
  c=$(the_new_id)
  check "carol has a store record" test -n "$c"
  check "carol's stash holds her grant" eq "$(stash_refresh "$c")" rt-carol-1
  check "alice stays active" eq "$(active)" "$A_ID"
  csm accounts rm carol@example.com
  check "offline rm exits 0" eq "$RC" 0
  check "offline rm says so" has "$LOGS/out" "csm: removed .*(offline)"
  check "carol's record is gone" test -z "$(the_new_id)"
  check "carol's stash is gone" test ! -e "$STASH_UD/claude-accounts/$c"

  start_orca || return
  csm accounts import "$dir"
  check "import with Orca running exits 0" eq "$RC" 0
  check "it went through Orca" has "$LOGS/out" "csm: imported .*(via Orca)"
  check "Orca got addClaudeFromConfigDir" has "$LOGS/orca-requests.log" "^csm accounts.addClaudeFromConfigDir"
  c=$(the_new_id)
  csm accounts rm carol@example.com
  check "rm with Orca running exits 0" eq "$RC" 0
  check "Orca got removeClaude" has_fixed "$LOGS/orca-requests.log" "csm accounts.removeClaude {\"accountId\":\"$c\"}"
  check "carol's record is gone" test -z "$(the_new_id)"
  stop_orca
}

# ─── launch contexts ───────────────────────────────────────────────────────────

sc_passthrough() {
  fresh_world
  csm claude --version
  check "csm claude --version reaches claude" has_fixed "$LOGS/out" "2.1.283 (Claude Code)"
  FAKE_LOG="$LOGS/claude.log" csm claude -p "hello there"
  check "csm claude -p runs claude once" eq "$(count_calls "$LOGS/claude.log")" 1
  check "with the arguments verbatim" eq "$(call_argv "$LOGS/claude.log" 1 | tail -n +2 | tr '\n' ' ')" "-p hello there "
  csm newuuid
  check "csm newuuid prints a uuid" grep -qE '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' "$LOGS/out"
  csm run --help
  check "csm run --help prints run's usage" has_fixed "$LOGS/out" "csm run [csm-flags]"
  check "nothing was launched" eq "$(count_inv "$LOGS/claude.log")" 0
}

# Print mode: -p, or a stdin that is not a terminal. claude runs verbatim;
# no sidecar, no usage fetch, no supervisor.
sc_print_context() {
  fresh_world
  usage_healthy
  csm -p "summarize this"
  check "csm -p exits 0" eq "$RC" 0
  check "claude ran once, one-shot" eq "$(count_calls "$LOGS/claude.log")" 1
  check "with the arguments verbatim" eq "$(call_argv "$LOGS/claude.log" 1 | tail -n +2 | tr '\n' ' ')" "-p summarize this "
  csm_stdin "some piped text" "explain"
  check "a piped stdin is print mode too" eq "$(count_calls "$LOGS/claude.log")" 2
  check "with the arguments verbatim" eq "$(call_argv "$LOGS/claude.log" 2 | tail -n +2 | tr '\n' ' ')" "explain "
  check "no interactive launch" eq "$(count_inv "$LOGS/claude.log")" 0
  check "no usage fetch" eq "$(lines "$LOGS/usage-calls")" 0
  check "no sidecar or pid file" eq "$(find "$STATE" -maxdepth 1 \( -name '*.pid' -o -name '*-*-*-*-*.json' \) | wc -l | tr -d ' ')" 0
}

# An Orca pane resuming a session execs claude at once: no picker, no usage
# fetch, no Keychain, no account decision, even with every account capped.
sc_orca_pane_resume() {
  fresh_world
  usage_both_capped
  local r=11111111-2222-4333-8444-555555555555 t0 t1
  EXTRA=("ORCA_PANE_KEY=pane-e2e" "ORCA_TERMINAL_HANDLE=term-e2e")
  t0=$(now_ms)
  start_sup pane --resume "$r" || { EXTRA=(); return; }
  t1=$(now_ms)
  EXTRA=()
  check "claude started within 3 s" lt $((t1 - t0)) 3000
  check "it resumes the session" inv_pair "$FLOG" 1 --resume "$r"
  check "no usage fetch before claude started" eq "$(lines "$LOGS/usage-calls")" 0
  check "no Keychain access" eq "$(lines "$KC_ROOT/calls")" 0
  check "alice stays active" eq "$(active)" "$A_ID"
  stop_sup "$SUP_PID" "$FLOG"
}

# Invoked as `claude` (the alias `csm orca setup` creates), csm hands
# claude's own words and flags to the real claude further down PATH and
# treats everything else as a launch.
sc_alias_dispatch() {
  fresh_world
  usage_healthy
  csm orca setup
  check "setup exits 0" eq "$RC" 0
  check "setup names the alias" has_fixed "$LOGS/out" "$STATE/bin/claude"
  check "setup prints the agentCmdOverrides value" has_fixed "$LOGS/out" "agentCmdOverrides.claude"
  check "the alias exists" test -x "$STATE/bin/claude"
  PATH_PREFIX="$STATE/bin"
  PROG="$STATE/bin/claude" csm --version
  check "claude --version reaches the real claude" has_fixed "$LOGS/out" "2.1.283 (Claude Code)"
  PROG="$STATE/bin/claude" csm mcp list
  check "claude mcp reaches the real claude" eq "$(call_argv "$LOGS/claude.log" 1 | tail -n +2 | tr '\n' ' ')" "mcp list "
  PROG="$STATE/bin/claude" csm -p hi
  check "claude -p reaches the real claude" eq "$(call_argv "$LOGS/claude.log" 2 | tail -n +2 | tr '\n' ' ')" "-p hi "
  PROG="$STATE/bin/claude" start_sup alias || { PATH_PREFIX=; return; }
  PATH_PREFIX=
  check "a bare \`claude\` is a supervised launch" inv_has "$FLOG" 1 --session-id
  check "the real claude ran, not the alias" eq "$(inv_arg "$FLOG" 1 0)" "$BIN/claude"
  check "csm supervises it" test -f "$STATE/$SID.pid"
  stop_sup "$SUP_PID" "$FLOG"
}

# SessionEnd hooks together get about 1.5 s. The hook reads csm's own files
# only: no network, no RPC, no Keychain, no usage command, even with a stale
# cache and Orca running.
sc_sessionend_budget() {
  fresh_world
  usage_healthy
  start_orca || return
  start_sup s run -n || { stop_orca; return; }
  usage_both_capped "$(iso_days_ago 2)"
  mkdir -p "$SANDBOX/transcripts"
  printf '{"type":"user"}\n' >"$SANDBOX/transcripts/$SID.jsonl"
  local http0 kc0 orca0 use0 t0 t1
  http0=$(lines "$HTTP_DIR/requests.log")
  kc0=$(lines "$KC_ROOT/calls")
  orca0=$(lines "$LOGS/orca-requests.log")
  use0=$(lines "$LOGS/usage-calls")
  t0=$(now_ms)
  hook "$(hook_json SessionEnd "$SID" '"reason":"other"')"
  t1=$(now_ms)
  check "the hook exits 0" eq "$RC" 0
  check "it finished well under 1.5 s ($((t1 - t0)) ms)" lt $((t1 - t0)) 1000
  check "no HTTP request" eq "$(lines "$HTTP_DIR/requests.log")" "$http0"
  check "no Keychain access" eq "$(lines "$KC_ROOT/calls")" "$kc0"
  check "no RPC" eq "$(lines "$LOGS/orca-requests.log")" "$orca0"
  check "no usage command" eq "$(lines "$LOGS/usage-calls")" "$use0"
  t0=$(now_ms)
  hook '{"session_id":"'"$SID"'","hook_event_name":"SessionEnd","reason":"other"}'
  t1=$(now_ms)
  check "a SessionEnd with no turn returns at once ($((t1 - t0)) ms)" lt $((t1 - t0)) 500
  stop_sup "$SUP_PID" "$FLOG"
  stop_orca
}

# ─── migration ─────────────────────────────────────────────────────────────────

# Two legacy ~/.claude.<name> profiles, work being the floor.
sc_migrate() {
  fresh_world
  world legacy work carol@example.com uuid-carol at-carol-1 rt-carol-1 floor
  world legacy home erin@example.com uuid-erin at-erin-1 rt-erin-1
  csm migrate plan
  check "plan exits 0" eq "$RC" 0
  check "plan names both profiles" eq "$(grep -c -e work -e home "$LOGS/out" | awk '$1 >= 2 { print "yes" }')" yes
  check "plan names the target D" has_fixed "$LOGS/out" "target D: $D"
  check "plan changes nothing" eq "$(world ids | wc -l | tr -d ' ')" 2
  csm migrate import --dry-run
  check "the dry run exits 0" eq "$RC" 0
  check "the dry run lists the imports" has_fixed "$LOGS/out" "import work from $HOME_DIR/.claude.work"
  check "the dry run changes nothing" eq "$(world ids | wc -l | tr -d ' ')" 2

  start_orca || return
  csm migrate import
  check_not "import refuses while Orca runs" eq "$RC" 0
  check "and imports nothing" eq "$(world ids | wc -l | tr -d ' ')" 2
  stop_orca

  # The login session still names the floor dir although this shell does
  # not: an Orca started from the Dock would take it as D.
  EXTRA=("CSM_E2E_SESSION_FLOOR=$HOME_DIR/.claude.work")
  csm migrate import
  EXTRA=()
  check_not "import refuses while the session floor remains" eq "$RC" 0
  check "and names it" has_fixed "$LOGS/out" "login session's CLAUDE_CONFIG_DIR"
  check "and imports nothing" eq "$(world ids | wc -l | tr -d ' ')" 2

  # A leftover `cas` shim exports CLAUDE_CONFIG_DIR=~/.claude: the switch
  # would then write ~/.claude/.claude.json while step 5 merges into
  # ~/.claude.json, so import refuses that too.
  EXTRA=("CLAUDE_CONFIG_DIR=$HOME_DIR/.claude")
  csm migrate import
  EXTRA=()
  check_not "import refuses CLAUDE_CONFIG_DIR=~/.claude" eq "$RC" 0
  check "and says why" has_fixed "$LOGS/out" "~/.claude/.claude.json"
  check "and imports nothing" eq "$(world ids | wc -l | tr -d ' ')" 2

  csm migrate import
  check "import exits 0" eq "$RC" 0
  check "work imported" has "$LOGS/out" "^work: imported carol@example.com"
  check "home imported" has "$LOGS/out" "^home: imported erin@example.com"
  check "D switched to the floor's account" has "$LOGS/out" "^switched ~/.claude to work's account"
  check "four accounts now" eq "$(world ids | wc -l | tr -d ' ')" 4
  check "D holds carol's grant" eq "$(d_refresh)" rt-carol-1
  check "D's identity is carol" eq "$(d_uuid)" uuid-carol
  check "the floor's MCP servers merged into ~/.claude.json" has_fixed "$HOME_DIR/.claude.json" '"docs-work"'
  check "the next step is named" has_fixed "$LOGS/out" "next: \`csm migrate retire\`"

  # Retire checks each stash with the profile endpoint first; with no answer
  # nothing is retired.
  csm migrate retire
  check "an unverified stash is not retired" has_fixed "$LOGS/out" "work: skipped: stash"
  check "and its dir stays" test -d "$HOME_DIR/.claude.work"
  http_rule profile at-carol-1 200 '{"account":{"uuid":"uuid-carol","email":"carol@example.com"},"organization":{"uuid":"org-acme"}}'
  http_rule profile at-erin-1 200 '{"account":{"uuid":"uuid-erin","email":"erin@example.com"},"organization":{"uuid":"org-acme"}}'
  csm migrate retire
  check "retire exits 0" eq "$RC" 0
  check "work's dir is retired" test -d "$HOME_DIR/.claude.work.retired"
  check "home's dir is retired" test -d "$HOME_DIR/.claude.home.retired"
  check "the legacy registry is removed" test ! -e "$HOME_DIR/.config/claude-as/profiles.json"
  check "the floor is cleared" has_fixed "$LOGS/out" "cleared the CLAUDE_CONFIG_DIR floor"
  check "D still holds carol's grant" eq "$(d_refresh)" rt-carol-1
  check "csm printed no token" no_secret_in "$LOGS/transcript"
}
