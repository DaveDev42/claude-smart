#!/usr/bin/env sh
# usage-collector.sh — a reference CSM_USAGE_CMD for claude-smart (`csm`).
#
# `csm` collects usage locally by default (each profile's own Claude Code
# OAuth credentials → Anthropic's usage API; see the README's "Usage
# metering" section). `CSM_USAGE_CMD` is an override: when set, `csm` calls
# this command instead, after the positive cache and before local collection.
#
# `csm` calls this command, reads its stdout, and expects a single UsageData
# JSON object (the same shape local collection produces). `csm` does the
# SCORING and the account choice itself (src/account/scoring.rs); this
# script only reports the FACTS (each profile's current session/week usage).
# You do NOT pick a profile here; you just describe every profile's usage and
# let csm decide.
#
#   export CSM_USAGE_CMD="$HOME/path/to/usage-collector.sh"
#   export CSM_USAGE_CMD_TIMEOUT=10     # seconds (default 10); hard deadline
#   export CLAUDE_USAGE_TTL=60          # seconds (default 60); positive-cache TTL
#                                       # so a slow command is not re-run each call
#
# Output shape (percentages are 0..100 integers):
#
#   {
#     "captured_at": "2026-06-24T00:00:00Z",
#     "profiles": {
#       "<profile-name>": {
#         "session":    {"pct": <int>, "resets": "<string>"},
#         "week_all":   {"pct": <int>, "resets": "<string>"},
#         "week_fable": {"pct": <int>, "resets": "<string>"}   # optional: one model-scoped weekly window
#       },
#       ...
#     },
#     "errors": {}                       # "<profile>": "<why it couldn't be read>"
#   }
#
# csm's scoring (defaults, override via env):
#   - skip a profile if it is in "errors"
#   - skip if session.pct >= CLAUDE_LIMIT_PCT              (default 99)
#   - skip if week_all.pct >= CLAUDE_PICK_SATURATION_PCT   (default 95)
#   - skip if week_fable.pct >= CLAUDE_PICK_SATURATION_PCT (same variable; only
#     when the profile has a week_fable section at all — absence is never read
#     as "limited")
#   - among survivors: soonest week_all reset wins (budget on the account
#     that refills first is the cheapest to spend; a known reset beats an
#     unknown one); ties broken by highest week_all.pct.
#
# ── Three ways to source the facts ───────────────────────────────────────────
# Pick ONE strategy below (A is simplest; C is the fully self-contained shape).
#
# IMPORTANT: there is NO reliable `claude` CLI command that emits these gauges.
# `claude -p`/`/usage` does NOT print the session/week percentages in
# non-interactive mode, so a plain `claude -p` recipe will NOT work. csm's own
# built-in collector already solves this by calling Anthropic's OAuth usage
# API directly with each profile's stored credentials, so you only need a
# custom command like this one for exotic setups: a shared cache your own
# tooling maintains, a proxy through infrastructure you already run, or a
# source other than the per-profile credentials csm reads by default.
set -eu

# ── Strategy A: proxy an HTTP endpoint you control ───────────────────────────
# If some service you run already produces this exact JSON shape, the whole
# job is one curl:
#
#   exec curl -fsS --max-time 8 "https://your-service.example/usage.json"
#
# (Uncomment the line above and you are done. The rest of this file is only for
#  environments without such a service.)

# ── Strategy B: re-emit a cache file your own tooling refreshes ──────────────
# If some scheduled job on this machine writes the UsageData JSON to a file,
# just cat it (csm's own TTL avoids re-reading too often):
#
#   exec cat "$HOME/.cache/csm/usage-limits.json"

# ── Strategy C: synthesize from per-profile facts (self-contained skeleton) ──
# Fill in `read_profile_usage` with however YOU can obtain a profile's numbers
# (an API call, parsing a billing export, etc.). The loop assembles valid JSON.
#
# Profiles come from the csm registry so this stays in sync with `csm profiles`.
REGISTRY="${CLAUDE_CONFIG_HOME:-$HOME/.config/claude-as}/profiles.json"

# Echo "<session_pct> <week_pct> <session_reset> <week_reset>" for a profile,
# or print nothing + return non-zero if it cannot be read (→ goes to "errors").
read_profile_usage() {
    # name="$1"  dir="$2"
    # >>> REPLACE THIS STUB with a real source for $1's usage. <<<
    # Example contract (space-separated): "11 46 7:09pm 7:09pm"
    return 1   # stub: "no data" → every profile lands in errors{} until implemented
}

# Extract "name<TAB>dir" pairs from profiles.json without needing jq.
profile_pairs() {
    python3 - "$REGISTRY" <<'PY'
import json, sys
try:
    m = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
for name, d in m.items():
    print(f"{name}\t{d}")
PY
}

emit_json() {
    now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    profiles_json=""
    errors_json=""
    sep=""
    esep=""
    while IFS="$(printf '\t')" read -r name dir; do
        [ -z "$name" ] && continue
        if facts="$(read_profile_usage "$name" "$dir")"; then
            # facts = "<sess_pct> <week_pct> <sess_reset> <week_reset>"
            set -- $facts
            sp="${1:-0}"; wp="${2:-0}"; sr="${3:-}"; wr="${4:-}"
            profiles_json="${profiles_json}${sep}\"${name}\":{\"session\":{\"pct\":${sp},\"resets\":\"${sr}\"},\"week_all\":{\"pct\":${wp},\"resets\":\"${wr}\"}}"
            sep=","
        else
            errors_json="${errors_json}${esep}\"${name}\":\"usage unavailable\""
            esep=","
        fi
    done <<EOF
$(profile_pairs)
EOF
    printf '{"captured_at":"%s","profiles":{%s},"errors":{%s}}\n' \
        "$now" "$profiles_json" "$errors_json"
}

emit_json
