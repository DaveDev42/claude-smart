# csm limit-switch end-to-end harness

Exercises the whole limit-switch loop against a real `csm` binary and a fake,
sleeping `claude` — no network, no real Claude Code, an isolated `HOME`. It
never runs the real `claude` binary and never touches your actual profile
registry or credentials.

## What it covers

Two trigger paths, each with a fake `claude` that records its own argv/env to
a log file and blocks on `SIGTERM`:

- **Hook-driven switch** (`csm hook`): a `StopFailure` payload with
  `error: "rate_limit"`, or a `Stop` payload once the seeded usage store shows
  a profile over its weekly cap, sends the running fake `claude` `SIGTERM` and
  relaunches it under the other profile with `--resume <sid>`.
- **Statusline-tick-driven switch** (`csm usage capture`): the same decision,
  fired instead by a `Status` hook payload on stdin (`five_hour`/`seven_day`
  percentages), the path a subscription cap actually takes since Claude Code
  parks that case in an auto-retry wait with no hook at all.

15 scenarios; the first ten keep the numbering of the original
standalone harness:

1. `StopFailure rate_limit`, profile `a` capped, `b` healthy, switch cooldown
   already active → still switches (the `StopFailure` path bypasses the
   percentage-switch cooldown; that's session-live evidence, not a poll).
2. `StopFailure overloaded` (not `rate_limit`) → must NOT switch.
3. `Stop` payload with the usage-pct path gated by an active cooldown → no
   switch.
4. Same `Stop` payload, no cooldown stamp → switches.
5. Both `a` and `b` capped → notify-only, no relaunch.
6. Two concurrent supervisors both running profile `a`, both hit
   `StopFailure rate_limit` → both switch independently to `b`.
7. `StopFailure rate_limit` with `CLAUDE_AUTO_SWITCH_RELAUNCH=0` → detects and
   notifies but does not relaunch.
8. Statusline tick: cold launch carries `--model`/`--effort` through; a
   healthy tick is a pure record; a `seven_day: 100` tick switches and the
   relaunch carries the same `--model`/`--effort`; a duplicate capped tick
   afterward is a no-op.
9. Statusline tick where `five_hour`/`seven_day` are healthy but the seeded
   store already recorded a model-scoped (`week_fable`) cap → the tick merges
   that in and still switches.
10. Statusline tick with `CLAUDE_AUTO_SWITCH=0` → records usage but the
    kill-switch suppresses the switch entirely.
11. A launch carrying `--add-dir <dir> --dangerously-skip-permissions` plus an
    initial prompt → the relaunch argv has both flags and the directory, has
    no prompt (`--resume` already carries that conversation), and the dropped
    prompt is counted in `limit-switch.log` rather than quoted.
12. A launch ending in `--add-dir <dir>` → the relaunch argv puts `--`
    between the directory and the handoff prompt, so claude cannot read the
    handoff as one more directory.
13. `csm --profile b claude --version --dangerously-skip-permissions` → claude
    runs under `b`'s config dir with exactly those two arguments and nothing
    the launcher would have added.
14. `csm --profile b newuuid` and `csm run --help` → both run csm's own code
    and start no claude at all, which is what the global `--profile` bug and
    the forwarded `--help` used to get wrong.
15. `a` hits a rate limit and switches to `b`, whose seeded store already
    shows `week_fable` at 100% (a Fable-saturated account stays a pick
    candidate). A statusline tick on `b` then falls back to Opus on `b`
    itself instead of being blocked by the switch `a` already spent — the
    fallback and the account-switch budget are independent.

## Running it

```sh
bash e2e/run.sh                        # builds csm (cargo build --bin csm) and runs
bash e2e/run.sh --csm target/debug/csm # reuse an already-built binary
bash e2e/run.sh --keep                 # leave the sandbox on disk for inspection (path printed at exit)
```

`CSM_BIN=<path> bash e2e/run.sh` is equivalent to `--csm`. With neither, it
builds; `CARGO_TARGET_DIR`, if already set in the environment, is respected.

Each run: builds the fake `claude` with `cc -std=c11 -Wall -Wextra`, creates a
fresh `mktemp -d` sandbox with its own `HOME` and its own
`~/.config/claude-as/profiles.json` (two profiles, `a` and `b`, registered
exactly the way `csm profiles add` does it — plugins/projects symlinked to a
shared SSOT, same as a real install), points `CSM_USAGE_API_BASE` at an
unrouted local port so no scenario can ever reach the network, runs all
scenarios, prints the full report, and tears down: only PIDs the harness
itself started and recorded are ever signalled (never a name-based
`pkill`/`killall`), and the sandbox is removed unless `--keep`. Exits non-zero
if any scenario's `VERDICT` line reads `FAIL`.

Whole run takes well under a minute locally.

## Layout

- `run.sh` — entry point (sandbox setup, build, teardown).
- `lib.sh` — polling/assertion/process helpers, plus the `csm hook` /
  `csm usage capture` / supervisor-launch wrappers each scenario calls.
- `scenarios.sh` — the 15 scenarios themselves.
- `fake-claude/claude.c` — the fake `claude`: logs its invocation, blocks on
  `SIGTERM`. POSIX-only (the relaunch loop it exercises is a POSIX
  fork/signal path); built fresh by `run.sh` for every run, never committed as
  a binary.
- `fixtures/*.json` — static usage-API-shaped fixtures fed through
  `CSM_USAGE_CMD`. `fixtures/gen_fixtures.sh` regenerates them (not run by
  `run.sh` — the switch decisions key off the percentage fields only, never
  wall-clock reset times, so the checked-in fixtures don't go stale).
- `bin/usage_cmd.sh` — the `CSM_USAGE_CMD` stub `run.sh` points `csm` at; cats
  whichever fixture the current scenario selected.

## Portability

Runs on macOS and Linux (`ubuntu-latest` in CI). Specifically avoided:
`stat -f`/`-c`, `sed -i ''`, `date -r <epoch>` at run time (BSD-only; the
maintenance-only `gen_fixtures.sh` tries the GNU form first and falls back),
`mktemp -t`, `readlink -f` (a hand-rolled `abspath` instead), GNU-only
`timeout`, and `pkill`/`killall` anywhere (every kill is `kill -TERM <pid>` on
a PID the harness recorded itself). `claude.c` uses only POSIX APIs and
compiles warning-free under `-Wall -Wextra` on both gcc and clang.

Windows is out of scope for this harness: the fake `claude` depends on
POSIX fork/`SIGTERM` semantics, and `csm`'s own Windows relaunch loop is
separately gated off pending manual console verification (see CLAUDE.md's
"Known gaps") — there is no relaunch loop there yet for this harness to drive.
