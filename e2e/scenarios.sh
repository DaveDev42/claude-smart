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
  idle_compact_no_supervisor
  idle_compact_supervisor_hands_off
  idle_compact_dead_supervisor
  idle_compact_interrupted_turn_hands_off
  idle_compact_metadata_after_stamp_still_hands_off
  migrate
  auto_fresh
  auto_adopt_live
  auto_pane_quiet
  auto_untouched
  auto_live_defers
  auto_crash
  auto_sqlite
  auto_floor_early
  auto_floor_reset
  auto_fresher
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
  ! cat "$@" 2>/dev/null | grep -qE '(rt|at)-(alice|bob|carol|erin)-[0-9]'
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

# ─── idle-compact ───────────────────────────────────────────────────────────
# No supervised claude is needed: idle_compact's two entry points, `csm hook`
# (mark_turn_ended) and `csm usage capture`, are stateless one-shot calls —
# sc_auto_untouched already established that pattern for a bare `hook` call.
# Every scenario starts from fresh_world + usage_healthy (so the Stop
# event's own usage-% check never trips) and sets the mode via `csm config
# set idle-compact <mode>`. csm itself no longer types into anything: these
# scenarios only prove the hand-off to CSM_SUPERVISOR_PID (or the lack of
# one), never a terminal delivery — see idle_compact/deliver.rs's own unit
# tests (including a property test) for the typing protocol that the future
# pty-relay supervisor runs on the request these scenarios produce.

IC_SID=idle-compact-sid-1

# No CSM_SUPERVISOR_PID at all: every gate passes but there is nothing to
# hand off to, so the tick logs no-delivery-path exactly once for the idle
# period (a second tick against the same idle period must not re-log).
sc_idle_compact_no_supervisor() {
  fresh_world
  usage_healthy
  csm config set idle-compact on
  idle_compact_turn_ended "$IC_SID"
  tick "$(idle_compact_json "$IC_SID" 120 150000)"
  check "the tick prints nothing" test ! -s "$LOGS/stdout"
  check "no-delivery-path is logged" has_fixed "$(idle_compact_log)" "outcome=no-delivery-path"
  check "the idle period is claimed" test -f "$STATE/$IC_SID.idle-compacted"
  check "no request dir was created" test ! -d "$STATE/idle-compact-requests"

  tick "$(idle_compact_json "$IC_SID" 120 150000)"
  check "the second tick does not re-log" eq "$(lines "$(idle_compact_log)")" 1
}

# CSM_SUPERVISOR_PID names a live process: the tick writes a request file
# under that pid, claims the idle period, and logs handed-off.
sc_idle_compact_supervisor_hands_off() {
  fresh_world
  usage_healthy
  csm config set idle-compact on
  idle_compact_turn_ended "$IC_SID"
  local sup_pid
  sup_pid=$(idle_compact_stand_in_supervisor)
  EXTRA=("CSM_SUPERVISOR_PID=$sup_pid")
  tick "$(idle_compact_json "$IC_SID" 120 150000)"
  EXTRA=()
  idle_compact_stop_stand_in_supervisor "$sup_pid"

  check "handed-off is logged" has_fixed "$(idle_compact_log)" "outcome=handed-off"
  check "the idle period is claimed" test -f "$STATE/$IC_SID.idle-compacted"
  local req
  req=$(idle_compact_request_file "$sup_pid")
  check "a request file was written for that pid" test -f "$req"
  check "the request names this session" has_fixed "$req" "\"sid\":\"$IC_SID\""
  check "the request carries the mode" has_fixed "$req" "\"mode\":\"on\""
  check "the request carries the remaining seconds" has_fixed "$req" "\"remaining_secs\":120"
  check "the request carries the recache estimate" has_fixed "$req" "\"recache_tokens\":150000"
}

# CSM_SUPERVISOR_PID names a pid that is not running: treated exactly like
# no supervisor at all — logged no-delivery-path, no request file.
sc_idle_compact_dead_supervisor() {
  fresh_world
  usage_healthy
  csm config set idle-compact on
  idle_compact_turn_ended "$IC_SID"
  local dead_pid
  dead_pid=$(idle_compact_dead_pid)
  EXTRA=("CSM_SUPERVISOR_PID=$dead_pid")
  tick "$(idle_compact_json "$IC_SID" 120 150000)"
  EXTRA=()

  check "no-delivery-path is logged" has_fixed "$(idle_compact_log)" "outcome=no-delivery-path"
  check "no request file was written" test ! -f "$(idle_compact_request_file "$dead_pid")"
}

# A `[Request interrupted by user` row newer than the <sid>.idle Stop stamp
# ends the turn at its own timestamp, with no fresh Stop hook at all: the
# busy check must see this and still hand off.
sc_idle_compact_interrupted_turn_hands_off() {
  fresh_world
  usage_healthy
  csm config set idle-compact on
  idle_compact_turn_ended "$IC_SID"
  /bin/sleep 1.1
  idle_compact_append_metadata_rows "$IC_SID" \
    "$(printf '{"type":"user","timestamp":"%s","message":{"role":"user","content":"[Request interrupted by user]"}}' "$(idle_compact_iso_now)")"
  local sup_pid
  sup_pid=$(idle_compact_stand_in_supervisor)
  EXTRA=("CSM_SUPERVISOR_PID=$sup_pid")
  tick "$(idle_compact_json "$IC_SID" 120 150000)"
  EXTRA=()
  idle_compact_stop_stand_in_supervisor "$sup_pid"

  check "handed-off is logged despite no fresh Stop" has_fixed "$(idle_compact_log)" "outcome=handed-off"
}

# metadata rows Claude Code keeps writing after a Stop (turn_duration,
# away_summary, mode) land after the <sid>.idle stamp and bump the
# transcript's mtime past it, but the last real assistant row is still
# before the stamp: the busy check's tail read must see through the
# metadata and hand off anyway (Fix 1 for the "false busy forever" bug). No
# supervisor is needed to observe this — no-delivery-path is just as good
# proof the busy check concluded not-busy, since Skip is silent.
sc_idle_compact_metadata_after_stamp_still_hands_off() {
  fresh_world
  usage_healthy
  csm config set idle-compact on
  idle_compact_turn_ended "$IC_SID"
  /bin/sleep 1.1
  idle_compact_append_metadata_rows "$IC_SID" \
    '{"type":"system","subtype":"turn_duration"}' \
    '{"type":"system","subtype":"away_summary"}' \
    '{"type":"mode"}'
  tick "$(idle_compact_json "$IC_SID" 120 150000)"
  check "no-delivery-path is logged (not silently skipped as busy)" has_fixed "$(idle_compact_log)" "outcome=no-delivery-path"
  check "the idle period is claimed" test -f "$STATE/$IC_SID.idle-compacted"
}

# ─── the automatic migration ───────────────────────────────────────────────────
# The world these start from is legacy_world (lib.sh): two registered
# profiles, work (carol, the floor) and home (erin), ~/.claude.shared linked
# from both, the login session's floor naming ~/.claude.work, an Orca store
# with no account, and no ~/.claude.

WORK=""
HOMEP=""
set_dirs() { WORK="$HOME_DIR/.claude.work"; HOMEP="$HOME_DIR/.claude.home"; }
n_ids() { world ids | wc -l | tr -d ' '; }
carol() { world id-of carol@example.com; }
erin() { world id-of erin@example.com; }
phase_is() { [ "$(marker phase)" = "$1" ]; }
# no_line <file> <text>: the file does not mention <text>.
no_line() { ! grep -qF -- "$2" "$1" 2>/dev/null; }

# The single verb: --dry-run writes nothing and exits 75, the former verbs
# point at it, and two runs (one after a reboot) take the machine from the
# legacy layout to Orca's accounts with ~/.claude as D.
sc_migrate() {
  legacy_world
  set_dirs
  csm migrate --dry-run
  check "the dry run exits 75 (pending)" eq "$RC" 75
  check "it names the floor profile's dir" has_fixed "$LOGS/out" "$WORK"
  check "it names the other profile's dir" has_fixed "$LOGS/out" "$HOMEP"
  check "it writes no marker" eq "$(marker phase)" none
  check "it imports nothing" eq "$(n_ids)" 0
  check "the registry stays" test -f "$HOME_DIR/.config/claude-as/profiles.json"
  check "the shared transcripts stay" test -d "$HOME_DIR/.claude.shared/projects" -a ! -L "$HOME_DIR/.claude.shared/projects"
  check "the floor stays" eq "$(floor_value)" "$WORK"

  for verb in plan import retire; do
    csm migrate "$verb"
    check "\`migrate $verb\` exits 1" eq "$RC" 1
    check "and points at the single verb" has_fixed "$LOGS/out" "csm migrate --dry-run"
  done

  # Orca, started while the floor was set, runs in ~/.claude.work.
  ORCA_D="$WORK" start_orca || return
  csm migrate
  check "exits 0 once the cutover is recorded" eq "$RC" 0
  check "both logins are Orca accounts now" eq "$(n_ids)" 2
  check "carol, the floor profile's account, is active" eq "$(active)" "$(carol)"
  check "the transcripts moved into ~/.claude" transcript_at "$D"
  check "~/.claude.shared/projects is a compat link now" test -L "$HOME_DIR/.claude.shared/projects"
  check "the floor profile still reads them through it" transcript_at "$WORK"
  check "the history moved too" has_fixed "$D/history.jsonl" "hello from the shared history"
  check "~/.claude.json carries the floor's MCP servers" has_fixed "$HOME_DIR/.claude.json" '"docs-work"'
  check "and the other profile's" has_fixed "$HOME_DIR/.claude.json" '"docs-home"'
  check "but no identity" no_line "$HOME_DIR/.claude.json" oauthAccount
  check "the floor is cleared" eq "$(floor_value)" ""
  check "the cutover is recorded in this boot" eq "$(marker boot)" boot-1
  check "home is retired" retired "$HOMEP"
  check "work stays while Orca runs in it" test -d "$WORK"
  check "the report says so" has_fixed "$LOGS/out" "moves to ~/.claude at Orca's next start"
  check "and asks for an Orca restart" has_fixed "$LOGS/out" "restart Orca"
  check "the phase is retire" eq "$(marker phase)" retire

  # Orca restarts (in ~/.claude now: the floor is gone), then a reboot.
  stop_orca
  ORCA_MATERIALIZE=1 start_orca || return
  check "Orca put carol into ~/.claude" eq "$(d_refresh)" rt-carol-1
  csm migrate
  check "without a reboot work still waits" test -d "$WORK"
  check "and the report says for what" has_fixed "$LOGS/out" "waits for a reboot"
  BOOT_ID=boot-2 csm migrate
  check "after the reboot: exit 0" eq "$RC" 0
  check "work is retired" retired "$WORK"
  check "the registry is gone" test ! -e "$HOME_DIR/.config/claude-as/profiles.json"
  check "~/.claude.shared is retired" retired "$HOME_DIR/.claude.shared"
  check "the transcript stays in ~/.claude" transcript_at "$D"
  check "the phase is done" eq "$(marker phase)" done
  BOOT_ID=boot-2 csm migrate
  check "a third run finds nothing" has_fixed "$LOGS/out" "nothing to migrate"
  check "and exits 0" eq "$RC" 0
  check "csm printed no token" no_secret_in "$LOGS/transcript"
  stop_orca
}

# No legacy layout: the first launch writes a done marker and says
# nothing; later runs change nothing.
sc_auto_fresh() {
  fresh_world
  usage_healthy
  check "no marker at first" eq "$(marker phase)" none
  start_sup s run -n || return
  stop_sup "$SUP_PID" "$FLOG"
  check "the first launch wrote a done marker" eq "$(marker phase)" done
  check "and printed nothing about a migration" no_line "$LOGS/s.sup.log" migrat
  cp "$STATE/migration.json" "$LOGS/marker.1"
  csm migrate
  check "csm migrate exits 0" eq "$RC" 0
  check "and finds nothing" has_fixed "$LOGS/out" "nothing to migrate"
  csm accounts list
  check "accounts list says nothing about a migration" no_line "$LOGS/out" migrat
  check "the marker did not change" cmp -s "$STATE/migration.json" "$LOGS/marker.1"
  check "D is untouched" eq "$(d_refresh)" rt-alice-1
  check "alice stays active" eq "$(active)" "$A_ID"
}

# Orca runs (in the floor dir, as the legacy floor started it). An
# interactive launch imports both logins over RPC and selects the floor's
# account before claude starts, then carries, cuts over and retires what
# it can after the spawn, logging only.
sc_auto_adopt_live() {
  legacy_world
  set_dirs
  ORCA_D="$WORK" start_orca || return
  EXTRA=("FAKE_COUNT=$LOGS/orca-requests.log")
  start_sup s run -n || { EXTRA=(); stop_orca; return; }
  EXTRA=()
  check "both logins were imported over RPC" eq "$(grep -c '^csm accounts.addClaudeFromConfigDir' "$LOGS/orca-requests.log")" 2
  check "the floor profile's account was selected" has "$LOGS/orca-requests.log" "^csm accounts.selectClaude"
  check "all of it before claude started" lt 2 "$(inv_field "$FLOG" 1 count)"
  check "carol is active" eq "$(active)" "$(carol)"
  check "claude runs in Orca's D" eq "$(inv_field "$FLOG" 1 config_dir)" "$WORK"
  check "the terminal got the one migration line" poll 5 has_fixed "$LOGS/s.sup.log" "csm: migration:"
  check "the rest ran after the spawn" poll 20 phase_is retire
  check "home was retired" retired "$HOMEP"
  check "work waits (claude runs in it)" test -d "$WORK"
  check "the transcripts are in ~/.claude" transcript_at "$D"
  check "the post-spawn run logged its lines" poll 5 has_fixed "$STATE/limit-switch.log" "migration"
  stop_sup "$SUP_PID" "$FLOG"
  stop_orca
}

# In an Orca pane nothing runs before the spawn and nothing is printed;
# the migration runs afterwards and logs.
sc_auto_pane_quiet() {
  legacy_world
  set_dirs
  ORCA_D="$WORK" start_orca || return
  local t0 t1
  EXTRA=("ORCA_PANE_KEY=pane-e2e" "ORCA_TERMINAL_HANDLE=term-e2e" "FAKE_COUNT=$LOGS/orca-requests.log")
  t0=$(now_ms)
  start_sup pane --resume "$LEGACY_SID" || { EXTRA=(); stop_orca; return; }
  t1=$(now_ms)
  EXTRA=()
  check "claude started within 3 s" lt $((t1 - t0)) 3000
  check "it resumes the session" inv_pair "$FLOG" 1 --resume "$LEGACY_SID"
  check "no RPC before the spawn" eq "$(inv_field "$FLOG" 1 count)" 0
  check "the migration ran after the spawn" poll 20 phase_is retire
  check "carol is active" eq "$(active)" "$(carol)"
  check "its lines went to the log" poll 5 has_fixed "$STATE/limit-switch.log" "migration"
  check "the pane printed nothing of csm's" no_line "$LOGS/pane.sup.log" "csm"
  stop_sup "$SUP_PID" "$FLOG"
  stop_orca
}

# NONE-class invocations never probe or migrate: no marker, no change.
sc_auto_untouched() {
  legacy_world
  set_dirs
  usage_healthy
  : >"$LOGS/all-out"
  hook "$(stop_json "$LEGACY_SID")"
  cat "$LOGS/stdout" "$LOGS/stderr" >>"$LOGS/all-out"
  hook "$(hook_json SessionEnd "$LEGACY_SID" '"reason":"other"')"
  cat "$LOGS/stdout" "$LOGS/stderr" >>"$LOGS/all-out"
  csm_stdin "$(statusline_json "$LEGACY_SID" 10 20)" statusline
  cat "$LOGS/stdout" "$LOGS/stderr" >>"$LOGS/all-out"
  tick "$(statusline_json "$LEGACY_SID" 10 20)"
  cat "$LOGS/stdout" "$LOGS/stderr" >>"$LOGS/all-out"
  local args
  for args in "-p hi" "run -p hi" "claude --version" "cas --eval" "cas --print-default-dir" \
    "config show" "newuuid" "completions zsh" "--version" "--help" "migrate --help" "usage capture"; do
    # shellcheck disable=SC2086
    csm $args
    cat "$LOGS/out" >>"$LOGS/all-out"
  done
  check "no marker" eq "$(marker phase)" none
  check "no migrate lock either" test ! -e "$STATE/migrate.lock"
  check "the registry stays" test -f "$HOME_DIR/.config/claude-as/profiles.json"
  check "the shared dir stays" test -d "$HOME_DIR/.claude.shared/projects" -a ! -L "$HOME_DIR/.claude.shared/projects"
  check "no ~/.claude was made" test ! -e "$D"
  check "no ~/.claude.json was made" test ! -e "$HOME_DIR/.claude.json"
  check "Orca's store has no account" eq "$(n_ids)" 0
  check "the floor stays" eq "$(floor_value)" "$WORK"
  check "no Keychain access" eq "$(lines "$KC_ROOT/calls")" 0
  check "no migration line" no_line "$LOGS/all-out" "csm: migration"
  check "no legacy-layout note" no_line "$LOGS/all-out" "legacy profile layout"
}

# A claude someone started in the home profile keeps it from retiring
# until it ends; the cutover does not wait for it.
sc_auto_live_defers() {
  legacy_world
  set_dirs
  ORCA_D="$WORK" start_orca || return
  EXTRA=("CLAUDE_CONFIG_DIR=$HOMEP")
  PROG="$BIN/claude" start_sup other || { EXTRA=(); stop_orca; return; }
  EXTRA=()
  local other_sup=$SUP_PID other_log=$FLOG
  csm migrate
  check "exits 0: the cutover is recorded" eq "$RC" 0
  check "home stays while its claude runs" test -d "$HOMEP"
  check "the report names the process" has_fixed "$LOGS/out" "CLAUDE_CONFIG_DIR=$HOMEP"
  csm migrate
  check "a rerun still leaves it" test -d "$HOMEP"
  stop_sup "$other_sup" "$other_log"
  csm migrate
  check "home retires once its claude ended" retired "$HOMEP"
  check "and its login is still carol's and erin's accounts" eq "$(n_ids)" 2
  stop_orca
}

# csm dies at each migration point; the next runs reach the same end.
sc_auto_crash() {
  local p
  set_dirs
  for p in migrate-b1-unlinked migrate-b1-moved migrate-b2-write migrate-cutover-cleared \
    migrate-retire-quarantined; do
    say "-- crash at $p"
    legacy_world
    rm -f "$LOGS/point.fired"
    if [ "$p" = migrate-b1-unlinked ]; then
      # ~/.claude links into the shared dir too (a machine whose default
      # profile was ~/.claude): B1 removes that link before the move.
      mkdir -p "$D" && ln -s "$HOME_DIR/.claude.shared/projects" "$D/projects"
    fi
    ORCA_D="$WORK" start_orca || return
    EXTRA=("E2E_POINT_KILL=$p")
    csm migrate
    EXTRA=()
    check "[$p] csm was killed there" test -e "$LOGS/point.fired"
    check "[$p] with SIGKILL" eq "$RC" 137
    if [ "$p" = migrate-b1-unlinked ]; then
      check "[$p] the kill left ~/.claude without the link" test ! -e "$D/projects"
      check "[$p] the transcripts are still in the shared dir" \
        test -f "$HOME_DIR/.claude.shared/projects/-tmp-e2e-cwd/$LEGACY_SID.jsonl"
    fi
    if [ "$p" = migrate-b2-write ]; then
      # Killed while holding Claude Code's config lock: the dir stays
      # behind. Age it past the 10 s stale time instead of sleeping.
      check "[$p] the kill left the config lock" test -d "$HOME_DIR/.claude.json.lock"
      /usr/bin/perl -e 'my $t = time - 60; utime $t, $t, @ARGV' "$HOME_DIR"/.claude*.lock
    fi
    csm migrate
    check "[$p] the rerun exits 0" eq "$RC" 0
    stop_orca
    if [ "$HOST_OS" = linux ]; then
      # No Keychain mirror: the floor dir holds the only login csm
      # launches reach until Orca has started in ~/.claude once.
      ORCA_MATERIALIZE=1 start_orca || return
      stop_orca
    fi
    BOOT_ID=boot-2 csm migrate
    check "[$p] after a reboot: exit 0" eq "$RC" 0
    check "[$p] the phase is done" eq "$(marker phase)" done
    check "[$p] work is retired" retired "$WORK"
    check "[$p] home is retired" retired "$HOMEP"
    check "[$p] ~/.claude.shared is retired" retired "$HOME_DIR/.claude.shared"
    check "[$p] the transcript is in ~/.claude" transcript_at "$D"
    check "[$p] the history is in ~/.claude once" eq "$(grep -c 'hello from the shared history' "$D/history.jsonl" 2>/dev/null)" 1
    check "[$p] both accounts, carol active" eq "$(n_ids) $(active)" "2 $(carol)"
    check "[$p] no leftover lock dir" test ! -e "$HOME_DIR/.claude.json.lock"
  done
}

# Orca stopped with a SQLite-backed profile whose database fails csm's
# offline checks (here a bare header) and no active account: csm cannot
# import or select offline, so it exits 75; with Orca up, 0. A database
# that passes them is written offline (unit tests in orca::add/switch).
sc_auto_sqlite() {
  legacy_world
  set_dirs
  world sqlite
  csm migrate
  check "exits 75 while Orca is stopped" eq "$RC" 75
  if [ "$HOST_OS" = mac ]; then
    check "the report names SQLite" has_fixed "$LOGS/out" "SQLite"
  fi
  check "nothing was imported" eq "$(n_ids)" 0
  check "the floor stays" eq "$(floor_value)" "$WORK"
  check "no cutover" eq "$(marker cutover)" no
  ORCA_D="$WORK" start_orca || return
  csm migrate
  check "with Orca up: exit 0" eq "$RC" 0
  check "carol is active" eq "$(active)" "$(carol)"
  check "the floor is cleared" eq "$(floor_value)" ""
  stop_orca
}

# Orca already runs in ~/.claude (the floor was dropped before csm got
# here). A pane's resume must find its transcript: B1 and B2 run before
# the spawn, without RPC and without a line.
sc_auto_floor_early() {
  legacy_world
  set_dirs
  floor_set ""
  start_orca || return
  EXTRA=("ORCA_PANE_KEY=pane-e2e" "ORCA_TERMINAL_HANDLE=term-e2e"
    "FAKE_EXPECT=$D/projects/-tmp-e2e-cwd/$LEGACY_SID.jsonl" "FAKE_COUNT=$LOGS/orca-requests.log")
  start_sup pane --resume "$LEGACY_SID" || { EXTRA=(); stop_orca; return; }
  EXTRA=()
  check "the transcript was in ~/.claude when claude started" eq "$(inv_field "$FLOG" 1 expect)" present
  check "no RPC before the spawn" eq "$(inv_field "$FLOG" 1 count)" 0
  check "claude runs in ~/.claude" eq "$(inv_field "$FLOG" 1 config_dir)" "(unset)"
  check "~/.claude.json has the floor's config" has_fixed "$HOME_DIR/.claude.json" '"docs-work"'
  check "the pane printed nothing of csm's" no_line "$LOGS/pane.sup.log" "csm"
  check "the rest ran after the spawn" poll 20 phase_is retire
  stop_sup "$SUP_PID" "$FLOG"
  stop_orca
}

# A floor set again after the cutover (a LaunchAgent not yet removed) is
# cleared again, and the floor dir retires only after a later boot.
sc_auto_floor_reset() {
  legacy_world
  set_dirs
  ORCA_D="$WORK" start_orca || return
  csm migrate
  check "the cutover is recorded" eq "$RC $(marker boot)" "0 boot-1"
  stop_orca
  floor_set "$WORK"
  BOOT_ID=boot-2 csm migrate
  check "the re-set floor is cleared again" eq "$(floor_value)" ""
  check "the report says so" has_fixed "$LOGS/out" "set again"
  check "the cutover's boot moved to this one" eq "$(marker boot)" boot-2
  check "work waits: the floor was seen in this boot" test -d "$WORK"
  BOOT_ID=boot-2 csm migrate
  check "still waits without a reboot" test -d "$WORK"
  BOOT_ID=boot-3 csm migrate
  if [ "$HOST_OS" = linux ]; then
    # Orca never started in ~/.claude, which holds no login (no Keychain
    # mirror on Linux): csm launches still run in work, so it stays.
    check "work waits while ~/.claude holds no login" test -d "$WORK"
    check "the report says why" has_fixed "$LOGS/out" "no login yet"
    ORCA_MATERIALIZE=1 start_orca || return
    stop_orca
    BOOT_ID=boot-3 csm migrate
  fi
  check "work retires after the next reboot" retired "$WORK"
  check "the phase is done" eq "$(marker phase)" done
}

# A retired dir's grant fresher than its stash (claude refreshed it after
# the import) is quarantined while Orca runs, then settled into the stash
# once Orca is stopped.
sc_auto_fresher() {
  legacy_world
  set_dirs
  http_rule profile at-erin-2 200 '{"account":{"uuid":"uuid-erin","email":"erin@example.com"},"organization":{"uuid":"org-acme"}}'
  ORCA_D="$WORK" start_orca || return
  EXTRA=("CLAUDE_CONFIG_DIR=$HOMEP")
  PROG="$BIN/claude" start_sup other || { EXTRA=(); stop_orca; return; }
  EXTRA=()
  csm migrate
  check "erin is imported" test -n "$(erin)"
  check "home waits for its claude" test -d "$HOMEP"
  # That claude refreshed its grant, then ended.
  world rotate-dir "$HOMEP" at-erin-2 rt-erin-2
  stop_sup "$SUP_PID" "$FLOG"
  csm migrate
  check "home is retired" retired "$HOMEP"
  check "its fresher grant is not in the stash yet (Orca runs)" eq "$(stash_refresh "$(erin)")" rt-erin-1
  stop_orca
  csm migrate
  check "with Orca stopped the fresher grant is settled into the stash" eq "$(stash_refresh "$(erin)")" rt-erin-2
  check "csm printed no token" no_secret_in "$LOGS/transcript"
}
