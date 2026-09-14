# claude-smart (`csm`)

Cross-platform smart session manager for [Claude Code](https://claude.ai/code).

`csm` is a single binary that wraps the `claude` CLI with:

- **smart session selection** — an interactive picker, shown on every launch,
  to start fresh, continue the newest session, or pick an existing one (each row
  shows its short id, time, mode, and title; type to fuzzy-filter), per
  directory;
- **profile management** — multiple isolated Claude Code config homes
  (`CLAUDE_CONFIG_DIR`) with a one-command switcher;
- **account scoring + auto-switch** — pick the viable account (session,
  weekly, and model-scoped weekly caps all under threshold) whose weekly
  quota resets soonest, and relaunch on a rate-limit hit;
- **usage metering**: a multi-profile usage table, collected locally per
  profile (see *Usage metering*);
- a **limit-detection hook** and a **relaunch/handoff loop**.

It runs on macOS, Linux/WSL, and Windows-native with a single binary — no shell
implementation to keep in sync.

## Install

```sh
# Homebrew (macOS)
brew install davedev42/tap/claude-smart
#   `csm` and `smart-claude` are aliases for the same formula, so
#   `brew install csm` and `brew upgrade smart-claude` also work.

# from crates.io
cargo install claude-smart

# or build from source
git clone https://github.com/DaveDev42/claude-smart
cd claude-smart && cargo install --path .
```

The primary binary is named **`csm`**; the Homebrew formula also installs
**`smart-claude`** as an equivalent command name (they run the same binary — csm
only special-cases the `csm-hook` invocation name, so any other name behaves
identically). Use whichever reads better to you.

An optional shell function lets `cas` switch the active profile in your *current*
shell (a child process cannot mutate its parent's environment, so this part is a
tiny shim — see *Profiles* below).

## Usage

```
csm [claude-args...]                 bare = smart launch (implicit `csm run`)
csm run [csm-flags] [-- claude...]   smart launcher (session + account + relaunch)

# run flags (account + session selection)
  --profile <name>                   launch under this profile (skip all picking)
  -i, --interactive                  manual pick: force account + session pickers
  --no-pick                          keep current profile, no scoring
  -A, --pick-account                 force an account pick this launch (overrides --no-pick)
  -n, --new                          start a fresh session (skip the session picker)
  -c, --continue                     resume newest free session
  # default: ALWAYS opens the session picker (new / continue / pick existing) so the
  #   choice is never made silently, and auto-picks the best account by usage; if
  #   usage is unavailable (no usable usage data) it opens the account picker
  #   instead of silently staying put. `-i` skips the account auto-pick entirely.

csm profiles [list]                  list configured profiles
csm profiles add  <name> [<dir>]     register (dir defaults to ~/.claude.<name>)
csm profiles set  <name> <dir>       register/overwrite a profile dir
csm profiles rm   <name>             unregister (refused if it is the default)
csm profiles use  <name>             set the machine default profile (+ floor)
csm profiles edit                    interactive editor (TTY)
csm profiles dir  [<name>]           print a profile's config dir
csm profiles bootstrap [<name>|--all] provision a profile's env (dir + shared plugins)
csm profiles doctor [--fix] [<name>|--all] diagnose/repair provisioning

csm usage [--json] [--no-fetch] [--refresh]   multi-profile usage table (see Usage metering)
csm usage capture                    read statusLine stdin, merge into the store

csm pick-account [<cur>] [--include-current]
csm scan [<cwd>]                     session listing (TSV)
csm sidecar {read|write|merge|flags} <sid> [k=v...]   per-session state store
csm statusline                       `<profile>@<host>` for the shell prompt
csm completions {zsh|bash|pwsh}      shell completions
```

> `csm` also recognizes a few **machine-interface** subcommands meant for
> automation, not hand typing: `csm hook` (the Stop/StopFailure/SubagentStop/
> SessionEnd limit-switch hook, wired from Claude Code `settings.json`), `csm cas` (the
> `eval`-shim contract behind the shell `cas` function), and `csm current-usage`
> (a raw usage probe used by the shims). They work without a profile registry.

### No collision with `claude`

`csm` only treats a known word as its own subcommand. **Any word it doesn't
recognize is forwarded verbatim to `claude`** — so `csm mcp`, `csm doctor`,
`csm update`, `csm /login`, etc. all reach the real `claude` untouched. The
reserved word set is deliberately disjoint from claude's subcommands. To pass a
flag that `csm` would otherwise interpret (`-c`, `-r`, `-n`, `--model`, …),
put it after `--`: `csm run -- -c`.

## Profiles

A *profile* is a named Claude Code config home (`CLAUDE_CONFIG_DIR`). The
registry is a flat JSON map at `~/.config/claude-as/profiles.json`:

```json
{
  "personal": "/home/you/.claude.personal",
  "work":     "/home/you/.claude.work"
}
```

The machine default profile NAME lives in `~/.config/claude-as/default`. Manage
the registry with `csm profiles …` (or the interactive `csm profiles edit`). No
account names are compiled into the binary — everything comes from your
registry.

### Switching the active profile in your shell

Add a tiny function so `cas <name>` switches `CLAUDE_CONFIG_DIR` in the *current*
shell (the binary prints an `export` line; the shell evals it):

```zsh
# ~/.zshrc
cas() { eval "$(command csm cas --eval --shell zsh -- "$@")"; }
```

```powershell
# $PROFILE
function cas { Invoke-Expression ((Get-Command csm -CommandType Application).Source + " cas --eval --shell pwsh -- " + ($args -join ' ')) }
```

`cas <name>` switches this shell; `cas -g <name>` / `csm profiles use <name>`
sets the machine-wide default; `cas status` shows the current/default/available
profiles.

Setting the machine default also updates a **floor** — a platform-level default
`CLAUDE_CONFIG_DIR` (a `launchctl setenv` on macOS, an `HKCU\Environment` value
on Windows) so that GUI / launchd / non-shell launches of `claude` land on the
real profile too, not just shells that sourced the `cas` function. On systems
without such a mechanism the floor step is a no-op.

### Shared plugins (provisioning)

Claude Code stores its plugins and marketplace cache *under* `CLAUDE_CONFIG_DIR`.
If each profile kept its own copy, switching profiles would leave the active
profile's marketplace index pointing at the wrong store — Claude Code then fails
to load marketplaces (`cache-miss`, "Run /reload-plugins"). To avoid that, `csm`
makes every profile's `plugins/` a symlink to one shared store at
`~/.claude.shared/plugins` (the same `~/.claude.shared` root that already holds
your transcripts and history), so the marketplace cache stays consistent across
switches.

This is **provisioned automatically**: every launch / profile switch / registry
add ensures the symlink exists (idempotent, best-effort — a hiccup never blocks
the launch). You can also do it explicitly:

```sh
csm profiles bootstrap --all     # provision every profile (dir + shared plugins)
csm profiles doctor              # read-only: report what's broken
csm profiles doctor --fix        # repair anything unhealthy
```

The first time a profile with an existing real `plugins/` dir is provisioned,
`csm` seeds the shared store from it (or backs the dir up if the shared store
already has content) before replacing it with the symlink — no plugin data is
lost. `settings.json` stays per-profile; `doctor` is where cross-profile drift
(e.g. divergent marketplace registrations) gets surfaced. On Windows the symlink
step is delegated to OS-native tooling and `csm` treats it as a no-op.

### Without a registry (degraded mode)

The registry is **optional**. With no `~/.config/claude-as/profiles.json` (a
fresh machine, or a "toss" box you never set up), `csm` runs in a degraded mode:
the plain smart launcher still works, and the registry-dependent commands fail
*safe* rather than erroring out —

| Command | Without a registry |
|---|---|
| `csm run` (and bare `csm`) | works — launches `claude` under the current `CLAUDE_CONFIG_DIR` |
| `csm scan`, `statusline`, `newuuid`, `completions`, `sidecar`, `hook` | work — they don't need the registry (`hook` falls back to the current `CLAUDE_CONFIG_DIR`) |
| `csm profiles list` | prints `(profiles.json absent — CAS/pick features disabled)` |
| `csm usage` | prints `(no profiles configured — `csm profiles add <name>`)` |
| `csm pick-account` | no-op (empty stdout), prints the `csm profiles add <name>` hint, exits 0 |

So account scoring / auto-switch / pick-account simply don't engage until you
`csm profiles add` at least one profile — nothing crashes.

## Configuration

`csm` keeps its own settings in `~/.config/claude-smart/config.json` (separate
from the `~/.config/claude-as/` profile registry above). Today the only setting
is the **launch command**: which binary `csm run` spawns instead of `claude`.
This lets you point `csm` at a drop-in Claude Code wrapper — e.g.
[`happy`](https://github.com/slopus/happy-cli) (mobile/web client) or `tp` —
that accepts the same arguments as `claude`:

```sh
csm config set launch-command happy   # csm run now launches `happy`
csm config get launch-command         # prints the effective launch command
csm config show                       # prints the whole config JSON
csm config unset launch-command       # revert to launching `claude`
```

The value is stored as an **argv token array**, so multi-token commands work
too — `csm config set launch-command npx happy` writes
`{ "launchCommand": ["npx", "happy"] }` and spawns `npx happy …`. Tokens are
never shell-split; pass each word separately.

Resolution precedence (highest first): the `CLAUDE_SMART_CLAUDE_BIN` environment
variable (a single binary, for tests / one-off overrides) → the config file's
`launchCommand` → the default `claude`. An absent or empty config launches
`claude` as before.

## Usage metering (local, per profile)

`csm usage` and account scoring collect usage data directly on this machine,
per profile: there is no separate service to run and nothing to opt into.
For each profile in your registry, `csm` reads that profile's own Claude Code
OAuth credentials and calls Anthropic's usage API
(`GET /api/oauth/usage`) with them:

- **macOS**: read from the login Keychain, service name
  `Claude Code-credentials` (or `Claude Code-credentials-<hash>` for a
  non-default `CLAUDE_CONFIG_DIR`). This is the same entry Claude Code itself
  reads, so no separate login step and no prompt.
- **Other platforms**: read from `<profile-dir>/.credentials.json`.

This is **read-only**: `csm` never writes, refreshes, or rotates a token. It
only reads the access token Claude Code already stored and asks the API for
the current usage percentages. If a profile's token has expired, `csm` does
not attempt a refresh itself (that would rotate the token and could race
Claude Code's own refresh, risking a surprise logout). Instead it serves the
last-known value and warns you, with the exact command to run, everywhere you
would see that profile: the usage table, the account picker, and the moment
you launch under it. See *Dead or expired credentials* below.

**Fetch order**, cheapest first:

1. **Positive cache** (`CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS`, default 60s):
   served as-is, no work done.
2. **`CSM_USAGE_CMD`**, if set: your own override command (see below).
3. **Local collection**: for each profile, serve a fresh per-profile store
   record, or probe the live credentials + API, or serve a stale record with
   window decay, or record an error. This is the terminal layer; see below.

Each profile's collected usage is written to its own store file at
`<smart-dir>/usage/<profile>.json` (JSON, atomic write) so that per-profile
writes never race each other. A profile's own record is itself served fresh
for `CSM_USAGE_PROFILE_TTL` (default 300s) before `csm` re-probes it live.

**Rate limits and staleness.** The usage API rate-limits per token. On a 429,
`csm` stamps that profile with a cooldown (`CSM_USAGE_RATE_LIMIT_COOLDOWN`,
default 900s) and serves its last-known store record instead of erroring.
Any served record whose window has since rolled over (`resets_at` is in the
past) has that window's percentage decayed to 0 rather than shown stale. The
account is assumed to have reset, even though `csm` hasn't re-probed it yet.

**Statusline capture (free, no extra API calls).** If you use a custom
`statusLine` script, Claude Code passes it live rate-limit data on stdin for
the active profile. Add one line to feed that into csm's store:

```sh
printf '%s' "$input" | csm usage capture &
```

`csm usage capture` reads the statusLine JSON from stdin, merges any
`rate_limits` it finds into that profile's store record, and exits (always
0, no stdout), so it's safe to run in the background. If you use `csm
statusline` itself as your `statusLine` command, this capture happens
automatically (disable it with `CSM_STATUSLINE_NO_CAPTURE=1`).

| Variable | Meaning |
|---|---|
| `CSM_USAGE_PROFILE_TTL` | Seconds a profile's own store record is served without a live probe (default `300`). |
| `CSM_USAGE_RATE_LIMIT_COOLDOWN` | Seconds to back off a profile after a 429 from the usage API (default `900`). |
| `CSM_USAGE_API_BASE` | Override the usage API base URL (default `https://api.anthropic.com`). Mainly for tests. |
| `CSM_STATUSLINE_NO_CAPTURE` | `1`/`true` disables the automatic statusline capture in `csm statusline`. |
| `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` | Positive-cache lifetime in seconds (default `60`). The legacy name wins if both are set. |
| `CLAUDE_USAGE_FAIL_COOLDOWN` | Negative-cache cooldown in seconds after every profile fails at once (default `120`). |

**Known limitation.** An idle profile you haven't run `claude` under in a
while can have an expired access token. `csm` never refreshes a token itself,
so it keeps showing the last values it collected (with window decay applied)
until either Claude Code refreshes the token on its next run, or you log back
in by hand. See *Dead or expired credentials* below for exactly what to run.

Use `csm usage --refresh` to bypass both the positive cache and every
profile's own TTL and re-probe live (cooldowns are still respected).

### Dead or expired credentials

When a profile's credentials go bad, `csm` tells you exactly what to run
instead of quietly serving stale numbers. If only the access token expired
and the refresh token is still alive, run `csm --profile <name>` once and
Claude Code refreshes it automatically on that run. If the refresh token is
also dead, or you were never logged in under that profile at all, nothing but
`CLAUDE_CONFIG_DIR=<dir> claude auth login` will fix it. Both warnings show up
in the `csm usage` table, the account picker, and right before `csm` launches
`claude`, so you never have to guess why a profile stopped scoring.

**Account picker (no usable usage data).** Account auto-selection opens the
account picker (rather than silently keeping the current account) whenever
it cannot score, which is two distinct cases: (1) **fetch failure**: local
collection couldn't produce any data at all; (2) **no scorable data**: data
came back but no profile yields a usable percentage (every profile errored,
or none has a `week_all` section, or the profile map is empty). Both surface
the picker in an interactive terminal so you can choose deliberately against
the last-known (stale) usage; in a non-interactive context (the Stop hook,
scripts) both fail safe to the current profile instead of blocking on a
picker. This is distinct from **all-saturated**: when real percentages exist
but *every* account is over the limit, there is nothing better to pick, so
`csm` keeps the current profile with a warning and does **not** open the
picker. Passing **`-i` / `--interactive`** forces the picker in all of these
cases too: it skips the auto-pick entirely and always asks (and also forces
the session picker). `--profile <name>` still wins over everything: explicit,
no picking. **The picker is ordered by recommendation, not
alphabetically:** rows are ranked exactly as the live scorer (`pick_best`)
would choose: viable accounts first (no cap over its threshold, see *What
counts as a viable account* below; soonest weekly reset, then higher
`week_all.pct`), then saturated / session-limited / errored / no-data rows
below. Rows that carry a model-scoped weekly reading show it as `model NN%`. The row the account auto-pick *would* have selected
leads the list and is flagged with a **`★`** marker. Because the picker's
cursor starts on the first row, **pressing Enter takes the recommendation**;
you only need to move when you want a different one. (When every account is
saturated / errored / dataless there is no recommendation, so no row gets
the `★`.) Pressing **Escape / Ctrl-C in any picker cancels the launch
entirely** (`csm` exits without starting `claude`). It does not silently
fall through to a default.

### What counts as a viable account

Anthropic reports up to three usage windows per account, and `csm` weighs all
three wherever it picks or ranks a profile:

| Window | Field | Not viable when |
|---|---|---|
| 5-hour session | `session` | `>= 99%` (`CLAUDE_LIMIT_PCT`) |
| weekly, all models | `week_all` | `>= 95%` (`CLAUDE_PICK_SATURATION_PCT`) |
| weekly, one model tier | `week_fable` (tier name from the API, shown as the table's tier column) | `>= 95%` (same variable) |

A profile the API reports no model-scoped window for is simply not
constrained by that dimension; absence is never read as "limited". One
predicate (`scoring::is_viable_pcts`) makes this call for the launch-time
auto-pick, `csm pick-account`, the account picker's ordering, and the Stop
hook's relaunch target, so an account whose model-scoped weekly cap is
exhausted is skipped everywhere even while its session and all-model weekly
readings look healthy.

**Reactive switch while a session is running.** Register
`csm hook --owner <profile-dir>` on Claude Code's `Stop`, `SubagentStop`,
`SessionEnd`, and `StopFailure` hook events. When a request fails because the
account hit a usage limit (session, weekly, or model-scoped weekly), Claude
Code ends the turn with `StopFailure` and `error: "rate_limit"` instead of
`Stop`, then waits for the limit to reset. `csm hook` takes that event as a
definitive limit: it asks the scorer for the best other viable profile and
writes a relaunch sentinel, and the `csm run` supervisor restarts
`claude --resume` under that profile. Other `StopFailure` errors (overloaded,
server errors, auth failures) are ignored. A matcher keeps the hook to the
one case that matters:

```json
"StopFailure": [
  { "matcher": "rate_limit",
    "hooks": [ { "type": "command",
                 "command": "csm hook --owner '/Users/example/.claude.work'" } ] }
]
```

On `Stop` the hook compares the running profile's three percentages against
`CLAUDE_LIMIT_PCT` (99) instead, which catches a cap crossed during a turn
that still succeeded. Both paths honour `CLAUDE_AUTO_SWITCH`,
`CLAUDE_AUTO_SWITCH_RELAUNCH`, the live-supervisor check, and the per-session
hop cap. The machine-wide switch cooldown (`CLAUDE_SWITCH_COOLDOWN`) only
throttles the percentage path, so several sessions sharing an exhausted
account can all move off it. Transcript-text detection is kept as a fallback,
but Claude Code does not currently write usage-limit notices into
transcripts. The model-scoped weekly percentage refreshes only when the
per-profile usage-API probe runs (`CSM_USAGE_PROFILE_TTL`, default 300s), so
on the `Stop` path a model-scoped cap can take up to about five minutes to
register. The `StopFailure` path does not depend on stored percentages.

### Custom usage command (`CSM_USAGE_CMD`)

You don't have to rely on local collection. Set `CSM_USAGE_CMD` to any command
that prints a usage JSON blob (the same shape local collection produces) on
stdout, and `csm` will use it as a usage source instead. It runs after the
positive cache and before local collection (the explicit "check via my own
script" path takes precedence over the built-in collector), and its result is
cached like any other fetch, so a slow command is not re-run within the cache
TTL.

| Variable | Meaning |
|---|---|
| `CSM_USAGE_CMD` | Shell command whose stdout is a usage JSON blob. Empty/unset = disabled. Runs via `sh -c` (POSIX) / `cmd /C` (Windows) — so on Windows the value must be `cmd.exe`-safe (single-quote quoting and Unix pipelines won't work; wrap complex logic in a `.cmd`/`.ps1` script and point at that). |
| `CSM_USAGE_CMD_TIMEOUT` | Hard deadline in seconds for that command (default `10`). On timeout `csm` falls through to local collection. |
| `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` | Positive-cache lifetime in seconds (default `60`). The legacy name wins if both are set. |

`csm` does the scoring and the account choice itself — the command only reports
the **facts** (each profile's usage); you do not pick a profile in it.

The command is **not** compiled in — the extraction mechanism is yours to own,
because a robust one is environment-specific. (Note: `csm`'s own built-in
collector already calls Anthropic's OAuth usage API directly, so you only need
`CSM_USAGE_CMD` for exotic setups: a shared cache your own tooling maintains,
a proxy through infrastructure you already run, or a source other than the
per-profile credentials `csm` reads by default.)

See [`examples/usage-collector.sh`](examples/usage-collector.sh) for a
reference `CSM_USAGE_CMD` that shows three practical strategies: proxy an
existing endpoint (one `curl`), re-emit a cache file, or synthesize the JSON
from per-profile facts. Its header comment also documents the **full usage JSON
shape** (`profiles[<name>].session.pct` / `.week_all.pct` / `.resets`) and the
scoring rules csm applies to it, so it doubles as the format reference.

## License

BSD 3-Clause License. See [`LICENSE`](LICENSE).
