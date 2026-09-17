#!/usr/bin/env bash
# Regenerate this directory's usage fixtures matching src/usage/model.rs's
# UsageData shape. Not run by e2e/run.sh -- the checked-in fixtures are static
# (the switch decisions in src/account/scoring.rs key off the pct fields only,
# never wall-clock reset times), so this is a maintenance script for when the
# fixture shape needs to change, not part of the test run itself.
set -euo pipefail
FIX="$(cd "$(dirname "$0")" && pwd)"

# iso_from_epoch <epoch> -- portable across GNU date (`-d @epoch`) and BSD date
# (`-r epoch`); tries GNU first since that's what CI (ubuntu-latest) has.
iso_from_epoch() {
  local epoch="$1"
  if date -u -d "@$epoch" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null; then
    return 0
  fi
  date -u -r "$epoch" +%Y-%m-%dT%H:%M:%SZ
}

NOW_EPOCH=$(date -u +%s)
NOW_ISO=$(iso_from_epoch "$NOW_EPOCH")
RESET_EPOCH=$((NOW_EPOCH + 7*24*3600))
RESET_ISO="Sep 21 at 9pm (Asia/Seoul)"

profile_json() {
  local sess=$1 week_all=$2 week_fable=$3
  cat <<EOF
    {
      "captured_at": "$NOW_ISO",
      "session":  { "pct": $sess, "resets": "9pm (Asia/Seoul)", "resets_at": $RESET_EPOCH },
      "week_all": { "pct": $week_all, "resets": "$RESET_ISO", "resets_at": $RESET_EPOCH },
      "week_fable": { "pct": $week_fable, "resets": "$RESET_ISO", "resets_at": $RESET_EPOCH },
      "week_model_label": "Fable",
      "session_stats": [],
      "source": "cmd"
    }
EOF
}

# a over the model-scoped weekly cap (week_fable 100), b healthy.
cat > "$FIX/a_capped_b_healthy.json" <<EOF
{
  "captured_at": "$NOW_ISO",
  "profiles": {
    "a": $(profile_json 10 40 100),
    "b": $(profile_json 5 10 5)
  }
}
EOF

# both a and b over the model-scoped cap.
cat > "$FIX/both_capped.json" <<EOF
{
  "captured_at": "$NOW_ISO",
  "profiles": {
    "a": $(profile_json 10 40 100),
    "b": $(profile_json 8 35 100)
  }
}
EOF

echo "wrote fixtures:"
ls -la "$FIX"
