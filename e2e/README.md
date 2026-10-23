# csm end-to-end harness

`e2e/run.sh` runs a real `csm` build against a fake Orca, a fake Keychain, a
fake `claude` and a loopback stand-in for Anthropic's OAuth endpoints, all
inside a throwaway sandbox under `/tmp`. Nothing reaches the network, the
real Keychain, a running Orca or your home directory.

```sh
bash e2e/run.sh                          # every scenario
bash e2e/run.sh rate_limit_switch migrate  # only these
bash e2e/run.sh --keep                   # keep the sandbox for inspection
bash e2e/run.sh --timeout 120            # per-scenario limit (default 90 s)
```

It needs `cargo`, `cc`, `/usr/bin/perl` (with the core modules JSON::PP,
Digest::SHA, Time::HiRes and IO::Socket::UNIX) and `/usr/bin/script`, and
runs on macOS and Linux. The last line reads
`csm e2e: N passed, M failed (of T), Xs`, and the exit status is non-zero
when anything failed. Failed scenarios have their logs dumped after the
summary.

## How it keeps to the sandbox

- `csm` is built once per run with `cargo build --features e2e`. The `e2e`
  feature (see `src/e2e.rs`) adds a start-up guard: the binary exits 97
  unless `HOME` is inside the sandbox named by `CSM_E2E_SANDBOX`. It also
  adds the seams the harness needs: the Keychain runner calls
  `/usr/bin/perl e2e/fakes/security.pl` instead of `/usr/bin/security`, the
  Orca process-table scan only counts processes under the sandbox, the Orca
  version comes from the sandbox's bundle or `CSM_E2E_ORCA_VERSION`, the
  login session's `CLAUDE_CONFIG_DIR` (the migration's floor) is the
  content of the file `CSM_E2E_SESSION_FLOOR_FILE` names, and clearing
  the floor empties that file instead of calling `launchctl` or the
  registry, the boot id comes from `CSM_E2E_BOOT_ID` (default `boot-1`)
  so a scenario can stage a reboot, and `point(name)` runs
  `e2e/fakes/point-hook.sh` at the store writer's two commit points and at
  the migration's `migrate-b1-moved`, `migrate-b1-unlinked`,
  `migrate-b2-write`, `migrate-cutover-cleared` and
  `migrate-retire-quarantined`. A default build has none of this.
- Every `csm` call runs under `env -i` with a whitelisted environment:
  `HOME` in the sandbox, `CLAUDE_CONFIG_DIR` and `ORCA_USER_DATA_PATH`
  unset, `CSM_USAGE_API_BASE` and `CSM_OAUTH_TOKEN_URL` on the loopback
  port, and `CSM_USAGE_CMD` pointing at a script that prints a fixture.
- Binaries are built once: `csm` and the fake `claude`. The fake Orca's
  main process is a hard link of the fake `claude` under Orca's name. The
  other fakes are Perl and shell scripts run through `/usr/bin/perl` and
  `/bin/sh`, so no scenario writes an executable.
- Each scenario runs in its own subshell under the time limit. Afterwards
  the runner kills (TERM, then KILL) every process whose command line names
  the sandbox, and a scenario that left one behind fails. The sandbox is
  removed at exit unless `--keep` is given.

## The fakes

| File | Plays |
|---|---|
| `fake-claude/claude.c` | `claude`: logs its argv and `CLAUDE_CONFIG_DIR`, then either prints and exits (print mode, `mcp`, or stdin not a terminal) or blocks until SIGTERM. With `FAKE_EXPECT=<path>` the record says whether that path existed when claude started, and with `FAKE_COUNT=<path>` how many lines that file had, so a scenario can prove what happened before the spawn. With `FAKE_ORCA_HOLDER` it is Orca's main process holding `SingletonLock`. |
| `fakes/World.pm`, `fakes/world.pl` | Builds and reads the sandbox state in Orca 1.4.214's layout: the profile index, `orca-data.json`, stashes under `claude-accounts/<id>/auth`, and `D` (`~/.claude`) logged in as the active account. For the migration scenarios it also builds the legacy layout: `legacy-reset` (a store with no accounts and no active id, no `D`), `legacy` (a registered `~/.claude.<name>` profile with a login), `shared <sid>` (`~/.claude.shared` with a transcript, history and todos, linked from every profile), `set-active`, `id-of`, `rotate-dir` (a fresher grant in a profile dir), `sqlite` (a `profile-state.db` beside the store) and `marker phase|boot|cutover` (reads csm's `migration.json`). |
| `fakes/orca.pl`, `fakes/start-orca.sh` | Orca's runtime: `orca-runtime.json`, a unix socket speaking Orca's NDJSON protocol, and the four methods csm calls (`accounts.list`, `accounts.selectClaude`, `accounts.addClaudeFromConfigDir`, `accounts.removeClaude`). They edit the store and `D` the way Orca's source does. `orca.pl call` stands in for the Orca GUI. With `ORCA_D=<dir>` set, `start_orca` runs Orca's main process with that `CLAUDE_CONFIG_DIR` (Orca started under a legacy floor), and with `ORCA_MATERIALIZE=1` Orca writes its active account into `D` at start, as a real Orca does after a restart. |
| `fakes/security.pl` | `/usr/bin/security`: items are files holding the exact bytes. It handles `find`, `add` (`-w` and `-X`), `delete` and `-i`, and logs the verb only. |
| `fakes/http.pl` | `/api/oauth/profile`, `/api/oauth/usage` and `/v1/oauth/token` on 127.0.0.1, answering from rule files a scenario writes. A request with no rule gets status 599. |
| `fakes/usage-cmd.sh` | `CSM_USAGE_CMD`: prints the scenario's usage fixture and counts its calls. |
| `fakes/point-hook.sh` | At one named point: starts the fake Orca (`E2E_POINT_AT`), or kills the csm process once (`E2E_POINT_KILL`) so a scenario can check the next run recovers. |

The accounts are alice (`aaaaaaaa-…-00000000000a`, active) and bob
(`bbbbbbbb-…-00000000000b`). Tokens are fixture strings such as `rt-bob-1`.
Every scenario starts from that state with Orca stopped and csm's state dir
empty.

## Scenarios

Limit switch (ported from the profile-era harness):

- `rate_limit_switch`: a `StopFailure` `rate_limit` switches to bob even
  inside the switch cooldown, and the relaunch resumes the same session in
  `D`.
- `overloaded_no_switch`: a `StopFailure` that is not a rate limit does
  nothing.
- `stop_pct_cooldown`, `stop_pct_switch`: a usage-% trip at `Stop` waits
  out the cooldown, and switches when there is none.
- `both_capped_notify`: with both accounts capped the hook notifies and
  changes nothing.
- `relaunch_off`: `CLAUDE_AUTO_SWITCH_RELAUNCH=0` detects and notifies but
  never relaunches.
- `leader_follower`: two sessions on alice; the first to hit the cap
  switches, and the second follows at its next turn boundary without
  switching again.
- `statusline_switch`: the statusLine tick switches, keeps `--model` and
  `--effort`, and ignores a late duplicate tick.
- `fable_fallback`: a stored `week_fable` cap relaunches on the same account
  with `--model opus`, once.
- `auto_switch_off`: `CLAUDE_AUTO_SWITCH=0` turns the statusline switch off.
- `carry_flags`, `separator`: the relaunch replays session flags, drops the
  prompt without logging it, and closes an open `--add-dir` with `--`.
- `hop_cap`: one automatic switch per chain, enforced by the hook and again
  by the supervisor.
- `switch_then_fallback`: after a switch, a model-scoped cap on the new
  account still gets its same-account fallback.

Orca running:

- `switch_via_orca`: every account change goes through Orca's RPC.
- `gui_switch_follow`: the Orca GUI switches during a session; when that
  session hits the cap its supervisor follows the new active account.
- `store_orca_at_l1`, `store_orca_at_l2`: Orca starts in the middle of an
  offline store write. At L1 (temp file written, not renamed) csm restores
  `D` and redoes the switch over RPC; at L2 (renamed) it keeps `D` and
  confirms over RPC.

Read-back and quarantine:

- `readback_owner`: `D` holds a newer grant for alice; the profile check
  confirms it and it goes to her stash.
- `quarantine_401`: the grant gets a 401 and the refresh fails; it goes to
  quarantine, alice's stash stays as it was, and `accounts doctor` lists
  the entry.
- `quarantine_refresh_owner`: a 401, then a refresh that works; the new
  grant goes to alice's stash.
- `quarantine_mismatch`: the grant belongs to another account; it goes to
  quarantine and never to alice's stash.
- `accounts_import_rm`: `accounts import` and `accounts rm`, offline and
  through Orca.

Launch contexts:

- `passthrough`: `csm claude <args…>` runs claude with exactly those
  arguments.
- `print_context`: `-p`, or a stdin that is not a terminal, runs claude
  as-is with no sidecar, usage fetch or supervisor.
- `orca_pane_resume`: in an Orca pane, `--resume <id>` starts claude within
  3 s under a pty, with no picker, usage fetch, Keychain call or account
  decision, even with every account capped.
- `alias_dispatch`: invoked as `claude`, csm passes claude's own words and
  flags to the real claude further down `PATH` and launches everything else.
- `sessionend_budget`: a `SessionEnd` hook with a stale cache and Orca
  running finishes in under 1 s without touching the network, the RPC
  socket, the Keychain or the usage command, and one with no turn finishes
  in under 0.5 s.

Migration off the legacy profile layout. Except for `auto_fresh`, each
starts from two registered profiles, work (carol, the floor) and home
(erin), a `~/.claude.shared` linked from both, the floor naming
`~/.claude.work`, an Orca store with no account and no `~/.claude`:

- `migrate`: `csm migrate --dry-run` exits 75 and writes nothing, the
  former `plan`/`import`/`retire` exit 1 with a pointer, and with Orca
  running in the floor dir the first run imports, carries, cuts over and
  retires home (exit 0). After Orca restarts in `~/.claude`, a run in the
  same boot still waits for a reboot; after one the floor dir retires and
  the marker reaches `done`. A later run finds nothing to migrate.
- `auto_fresh`: no legacy layout; the first launch writes a `done`
  marker and prints nothing, and later runs change nothing.
- `auto_adopt_live`: with Orca running, an interactive launch imports
  both logins over RPC and selects the floor's account before claude
  starts, then finishes after the spawn, logging only.
- `auto_pane_quiet`: in an Orca pane nothing runs before the spawn and
  nothing is printed; the migration runs afterwards and logs.
- `auto_untouched`: the hook, the status line, `usage capture`, `-p`,
  piped stdin, `csm claude`, `cas`, `completions` and `--help` leave no
  marker and change nothing.
- `auto_live_defers`: a claude running in the home profile keeps it from
  retiring until it ends; the cutover does not wait for it.
- `auto_crash`: csm is killed at `migrate-b1-moved`,
  `migrate-cutover-cleared` and `migrate-retire-quarantined` in turn; the
  following runs reach the same end state.
- `auto_sqlite`: Orca stopped with a SQLite-backed profile and no active
  account gives 75; with Orca running, 0.
- `auto_floor_early`: Orca already runs in `~/.claude`; a pane's resume
  finds its transcript because the shared dirs and `~/.claude.json` move
  before the spawn, without RPC and without a line.
- `auto_floor_reset`: a floor set again after the cutover is cleared
  again, and the floor dir retires only after the boot id changes.
- `auto_fresher`: a profile dir's grant fresher than its stash is
  quarantined while Orca runs and filed into the stash once Orca is
  stopped.

Plus `guard`, which checks that the e2e build refuses a `HOME` outside the
sandbox.

## Writing a scenario

Add `sc_<name>` to `scenarios.sh` and its name to `SCENARIOS`. Use the
helpers in `lib.sh`: `csm` and `csm_stdin` run the binary (output in
`$LOGS/out`, status in `$RC`), `start_sup`/`stop_sup` run a supervised
session under `script`, `start_orca`/`stop_orca` run the fake Orca, `hook`
and `tick` send hook and statusLine events, and `check`/`check_not` record
results. Stop every process the scenario starts. Put fixture tokens only in
`World.pm` and the scenario, and never print a token in a check message.
