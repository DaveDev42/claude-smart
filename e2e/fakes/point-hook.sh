#!/bin/sh
# e2e/fakes/point-hook.sh -- csm's e2e build runs `/bin/sh point-hook.sh
# <point>` at named points: the store-write protocol's commit points
# (store-L1, store-L2) and the migration's (migrate-b1-moved,
# migrate-cutover-cleared, migrate-retire-quarantined, ...). Each action
# fires once per scenario ($E2E_POINT_MARK absent), and only at the point
# the scenario armed:
# - E2E_POINT_AT=<point>: start the fake Orca and wait for it, so Orca
#   appears at exactly that step;
# - E2E_POINT_KILL=<point>: SIGKILL the csm that runs this hook (our
#   parent), a crash at exactly that step.
# Otherwise do nothing.
set -u
if [ "${E2E_POINT_KILL:-}" = "$1" ] && [ ! -e "$E2E_POINT_MARK" ]; then
  : >"$E2E_POINT_MARK"
  kill -KILL "$PPID"
  exit 0
fi
[ "${E2E_POINT_AT:-}" = "$1" ] || exit 0
[ -e "$E2E_POINT_MARK" ] && exit 0
: >"$E2E_POINT_MARK"
exec /bin/sh "$E2E_FAKES/start-orca.sh"
