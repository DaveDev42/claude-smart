#!/bin/sh
# e2e/fakes/usage-cmd.sh -- the CSM_USAGE_CMD the harness installs: print
# the usage fixture the running scenario last wrote, and note the call in
# $E2E_USAGE_CALLS so a scenario can prove a path never fetched. Never
# touches the network.
[ -n "${E2E_USAGE_CALLS:-}" ] && echo call >>"$E2E_USAGE_CALLS"
exec cat "$E2E_USAGE_FIXTURE"
