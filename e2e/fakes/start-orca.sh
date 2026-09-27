#!/bin/sh
# e2e/fakes/start-orca.sh -- start the fake Orca and wait until it answers.
#
# Two processes, like a real Orca: the main process (the fake-claude binary
# in holder mode, hard-linked to an Orca-named path) holds SingletonLock,
# and orca.pl serves the runtime socket and names the main pid in
# orca-runtime.json. Both pids go to $E2E_ORCA_PIDS (and $E2E_PIDS, the
# run-wide kill list). Run by the harness, or by csm's e2e point hook when a
# scenario wants Orca to appear in the middle of a store write.
#
# Needs: E2E_HOME E2E_UD E2E_FAKES E2E_ORCA_EXE E2E_ORCA_LOG E2E_ORCA_PIDS
# E2E_PIDS (and E2E_SEC_ROOT on macOS). Optional: E2E_ORCA_CONFIG_DIR (the
# main process's CLAUDE_CONFIG_DIR) and E2E_ORCA_MATERIALIZE (see orca.pl).
set -u
case "$E2E_UD" in "$E2E_HOME"/*) ;; *) echo "start-orca.sh: E2E_UD outside E2E_HOME" >&2; exit 2 ;; esac
mkdir -p "$E2E_UD"
rm -f "$E2E_UD/orca-runtime.json"
# E2E_ORCA_CONFIG_DIR: start Orca with CLAUDE_CONFIG_DIR set (an Orca a
# legacy login-session floor reached), so its D is that dir; otherwise
# without it, so its D is ~/.claude.
ORCA_D="${E2E_ORCA_CONFIG_DIR:-}"
case "$ORCA_D" in "" | "$E2E_HOME"/*) ;; *) echo "start-orca.sh: E2E_ORCA_CONFIG_DIR outside E2E_HOME" >&2; exit 2 ;; esac
if [ -n "$ORCA_D" ]; then
  env CLAUDE_CONFIG_DIR="$ORCA_D" HOME="$E2E_HOME" FAKE_ORCA_HOLDER="$E2E_UD" "$E2E_ORCA_EXE" </dev/null >/dev/null 2>&1 &
else
  env -u CLAUDE_CONFIG_DIR HOME="$E2E_HOME" FAKE_ORCA_HOLDER="$E2E_UD" "$E2E_ORCA_EXE" </dev/null >/dev/null 2>&1 &
fi
holder=$!
echo "$holder" >>"$E2E_PIDS"
i=0
while [ ! -L "$E2E_UD/SingletonLock" ] && [ "$i" -lt 200 ]; do /bin/sleep 0.05; i=$((i + 1)); done
E2E_ORCA_D="$ORCA_D" /usr/bin/perl -I "$E2E_FAKES" "$E2E_FAKES/orca.pl" serve "$holder" "$E2E_UD/o-e2e.sock" "$E2E_ORCA_LOG" \
  </dev/null >>"$E2E_ORCA_LOG.stderr" 2>&1 &
server=$!
echo "$server" >>"$E2E_PIDS"
echo "$holder $server" >"$E2E_ORCA_PIDS"
i=0
while [ ! -S "$E2E_UD/o-e2e.sock" ] || [ ! -f "$E2E_UD/orca-runtime.json" ]; do
  [ "$i" -ge 200 ] && { echo "start-orca.sh: the fake Orca did not come up" >&2; exit 1; }
  /bin/sleep 0.05
  i=$((i + 1))
done
exit 0
