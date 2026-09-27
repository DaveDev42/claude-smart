#!/bin/sh
# e2e/fakes/point-hook.sh -- csm's e2e build runs `/bin/sh point-hook.sh
# <point>` at named points of the store-write protocol (store-L1, store-L2).
# When the point is the one the scenario armed ($E2E_POINT_AT) and it has
# not fired yet ($E2E_POINT_MARK absent), start the fake Orca and wait for
# it, so Orca appears at exactly that step. Otherwise do nothing.
set -u
[ "${E2E_POINT_AT:-}" = "$1" ] || exit 0
[ -e "$E2E_POINT_MARK" ] && exit 0
: >"$E2E_POINT_MARK"
exec /bin/sh "$E2E_FAKES/start-orca.sh"
