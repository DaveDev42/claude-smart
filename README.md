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
  -r, --resume [<id>|<alias>]        resume a session (csm also reads the id)
  --session-id <uuid>                forwarded to claude; csm tracks it for sidecar/relaunch state
  --model <m>                        forwarded to claude; remembered across a limit-switch hop
  --effort <e>                       forwarded to claude; remembered across a limit-switch hop
  --permission-mode <p>              forwarded to claude; remembered across a limit-switch hop
  # the six flags above are forwarded to claude AND read by csm; every other claude
  #   flag passes through untouched — use `csm run -- <args>` to force passthrough
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
csm profiles bootstrap [<name>|--all] provision a profile's env (dir + shared plugins/projects)
csm profiles doctor [--fix] [--fix-home] [<name>|--all]
                                     check profile dirs / shared links; --fix repairs
                                     profiles, --fix-home repairs the ~/.claude shim

csm config [show]                    print csm's own config JSON (~/.config/claude-smart/config.json)
csm config get launch-command        print the resolved launch command
csm config set launch-command <cmd>...   launch <cmd> instead of `claude` (e.g. happy)
csm config unset launch-command      revert to launching `claude`

csm usage [--json] [--no-fetch] [--refresh] [--refresh-oauth]
                                     multi-profile usage table (see Usage metering)
csm usage capture                    read statusLine stdin, merge into the store

csm pick-account [<cur>] [--include-current]
csm scan [<cwd>]                     session listing (TSV)
csm reap [--dry-run] [--term] [--all|--session <sid>]   kill orphan processes left by claude
csm sidecar {read|write|merge|flags} <sid> [k=v...]   per-session state store
csm statusline                       `<profile>@<host>` for the shell prompt
csm completions {zsh|bash|pwsh}      shell completions
csm newuuid                          fresh lowercase UUID v4
```

`csm run --help` prints the run flags above; `csm run -- --help` asks claude for
claude's.

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

### Shared plugins and projects (provisioning)

Claude Code stores its plugins and marketplace cache *under* `CLAUDE_CONFIG_DIR`.
If each profile kept its own copy, switching profiles would leave the active
profile's marketplace index pointing at the wrong store — Claude Code then fails
to load marketplaces (`cache-miss`, "Run /reload-plugins"). To avoid that, `csm`
makes every profile's `plugins/` a symlink to one shared store at
`~/.claude.shared/plugins` (the same `~/.claude.shared` root that already holds
your transcripts and history), so the marketplace cache stays consistent across
switches. The same mechanism also links each profile's `projects/` to
`~/.claude.shared/projects`, so every profile sees the same transcript history.

This is **provisioned automatically**: every launch / profile switch / registry
add ensures both symlinks exist (idempotent, best-effort — a hiccup never
blocks the launch). You can also do it explicitly:

```sh
csm profiles bootstrap --all     # provision every profile (dir + shared plugins/projects)
csm profiles doctor              # read-only: report what's broken
csm profiles doctor --fix        # repair anything unhealthy
csm profiles doctor --fix-home   # repair the ~/.claude shim (see below)
```

The first time a profile with an existing real `plugins/` dir is provisioned,
`csm` seeds the shared store from it (or backs the dir up if the shared store
already has content) before replacing it with the symlink — no plugin data is
lost. `settings.json` stays per-profile; `doctor` is where cross-profile drift
(e.g. divergent marketplace registrations) gets surfaced. On Windows the symlink
step is delegated to OS-native tooling and `csm` treats it as a no-op.

### Third-party integration contract

`csm` honors `CLAUDE_CONFIG_DIR` on every launch, so the active profile decides
where Claude Code reads and writes. Much of the surrounding tooling never reads
that variable: GUI session browsers and transcript indexers resolve
`~/.claude/projects` by hand and scan nothing else, and other tools write their
own hooks or settings into `~/.claude/settings.json`.

`csm` therefore keeps `~/.claude` as a credential-free compatibility shim. Its
`projects` entry is a symlink to `~/.claude.shared/projects`, the same shared
transcript store every profile links to, so a tool that hardcodes
`~/.claude/projects` sees every profile's sessions. Everything else in
`~/.claude` stays as you or another tool left it: `csm` never creates, renames,
or removes an entry there other than `projects`, and it never places
credentials in that directory.

Every launch, every `csm profiles add` / `set` / `use`, and every change of the
global default creates the shim when it is missing, and changes nothing that
already exists, so the launch path can never move your files. A per-shell
`cas <profile>` switch only exports `CLAUDE_CONFIG_DIR` and leaves the shim to
the next launch. Set `CSM_NO_HOME_SHIM=1` to turn the step off; `doctor` still
reports the shim either way.

Repairs are explicit:

```sh
csm profiles doctor              # reports the shim state on its first line
csm profiles doctor --fix-home   # repairs it
```

`--fix-home` creates a missing link, repoints one aimed somewhere else, recreates
the shared store when the link dangles, and backs up a stray file named
`projects`. When `~/.claude/projects` is a real directory holding transcripts,
`--fix-home` merges those into `~/.claude.shared/projects` entry by entry and
then links it. Nothing is copied and nothing is deleted. The merge never makes a
backup copy, because a backup would hide that history from a plain
`claude --resume`.

The merge is all or nothing. If any name is already taken in the shared store,
`--fix-home` moves nothing, prints the colliding paths, and leaves
`~/.claude/projects` exactly as it found it — resolve those names by hand and run
it again. Moving the rest would take sessions out of `~/.claude/projects` without
leaving a link behind, so the default home would show fewer sessions than before
the repair.

`--fix` and `--fix-home` are independent; neither implies the other.

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

By default this is **read-only**: `csm` never writes, refreshes, or rotates a
token. It only reads the access token Claude Code already stored and asks the
API for the current usage percentages. If a profile's token has expired, `csm`
does not attempt a refresh itself (that would rotate the token and could race
Claude Code's own refresh, risking a surprise logout). Instead it serves the
last-known value and warns you, with the exact command to run, everywhere you
would see that profile: the usage table, the account picker, and the moment
you launch under it. See *Dead or expired credentials* below. The one way to
change that is the explicit opt-in in *Headless collectors* below.

**Fetch order**, cheapest first:

1. **Positive cache** (`CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS`, default 60s):
   served as-is, no work done.
2. **`CSM_USAGE_CMD`**, if set: your own override command (see below).
3. **Negative cooldown**: after a total collection failure (every profile
   failed), `csm` serves nothing new until `CLAUDE_USAGE_FAIL_COOLDOWN`
   (default 120s) lapses, rather than re-hammering local collection.
4. **Local collection**: for each profile, serve a fresh per-profile store
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
automatically (disable it with `CSM_STATUSLINE_NO_CAPTURE=1`). The same
capture is what moves a running session off an account that just hit its
weekly cap (see *Reactive switch* below), so a `csm run` session without it
only gets the hook-based paths.

See *Usage collection and caching* in [Environment variables](#environment-variables)
for every variable named above.

**Known limitation.** An idle profile you haven't run `claude` under in a
while can have an expired access token. `csm` does not refresh a token unless
you turn the opt-in on, so it keeps showing the last values it collected
(with window decay applied) until either Claude Code refreshes the token on
its next run, or you log back in by hand. See *Dead or expired credentials*
below for exactly what to run.

Use `csm usage --refresh` to bypass both the positive cache and every
profile's own TTL and re-probe live (cooldowns are still respected).

### Headless collectors (opt-in token refresh)

An access token lives about 8 hours, and normally only a running Claude Code
process mints a new one. On a headless host that collects usage for profiles
no Claude Code ever runs under, every profile therefore goes stale 8 hours
after login and stays that way.

`csm usage --refresh-oauth` (or `CSM_OAUTH_REFRESH=1`) lets the collector mint
a new access token itself. It applies to that command only: the statusline,
the account picker, the sidecar and the hook never refresh, whatever the
environment says. A refresh is attempted for a profile only when all of these
hold:

- the opt-in is on for this invocation;
- the profile's access token has expired and its refresh token has not;
- no live Claude Code session exists for the profile (its own
  `<profile-dir>/sessions/*.json` registry is scanned, and a live `claude`
  or `node` process there means `csm` stands down and lets Claude Code
  refresh);
- an exclusive lock file next to the credentials is free (60s staleness
  takeover), so two collectors can't refresh the same profile at once;
- the platform stores credentials in `<profile-dir>/.credentials.json`.
  **macOS is not supported**: there the Keychain holds the live copy, so
  `csm` reports the profile as unsupported and writes nothing.

On success the credential file is rewritten atomically at mode `0600`, with
every unrelated key preserved, and the same collection tick goes on to fetch
usage with the new token. On any refusal or failure nothing is written and
the profile behaves exactly as it does with the opt-in off.

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

**Reactive switch while a session is running.** Three paths feed the same
decision; all of them end with the `csm run` supervisor restarting
`claude --resume` under the best other viable profile, with a short handoff
prompt so the resumed session knows why it moved.

*What the switch carries.* The hop builds claude's argv fresh rather than
repeating the one it was launched with. It resumes the same session id,
re-applies the `--permission-mode`, `--effort` and `--model` the sidecar
remembers, and then replays the launch flags that shape the session:
`--dangerously-skip-permissions` and its `--allow-` form, `--add-dir`,
`--settings` and `--setting-sources`, `--mcp-config` and `--strict-mcp-config`,
the tool allow and deny lists, the system-prompt flags and their file and
snapshot forms, `--agent` and `--agents`, `--plugin-dir` and `--plugin-url`,
`--fallback-model`, `--autocompact`, `--max-budget-usd`, `--verbose` and the
other valueless switches. A session started
`csm --dangerously-skip-permissions --add-dir /x "do the thing"` therefore
comes back after a switch still bypassing prompts and still able to read
`/x`, instead of stopping on the first permission dialog with nobody watching.

The initial prompt is not replayed: `--resume` already carries that
conversation, and the handoff prompt is the hop's first turn. Nor are the
flags that would fight the hop's own argv, among them `--resume`,
`--continue`, `--session-id` and `--fork-session`, `--print` with its input
and output formats, and the background, cloud, worktree and tmux launchers.
Anything after a bare `--` stops the scan, and a flag `csm` does not
recognise is dropped as well, since replaying it without knowing whether it
takes a value would either swallow the following argument or strand one on
the argv. What the hop leaves behind is written to `limit-switch.log` as
`dropped passthru: …`, so a session that comes back without something it was
launched with is explainable after the fact. Dropped flags are named there;
everything else, the initial prompt included, is only counted, because that
log holds no conversation text.

One shape needs care. A flag that takes a list, such as `--add-dir`, keeps
collecting values until something stops it, so when the replayed flags end
inside one of those lists the hop writes `--` before the handoff prompt.
Otherwise claude reads the handoff as one more directory and the resumed
session has no first turn.

*Statusline tick.* This is the path that fires for a subscription cap. When
the account's session, weekly, or model-scoped weekly limit is reached,
Claude Code (2.1.270) does not end the turn: it shows "Weekly limit reached ·
Retrying in 6h" and keeps retrying internally, and no hook runs at all for
the duration. What does keep running is the statusLine command, once a
second, with the live `rate_limits` for the account. `csm usage capture`
(or `csm statusline`) compares that reading, plus the stored model-scoped
weekly percentage, against `CLAUDE_LIMIT_PCT` (99) on every tick, and on a
hit runs the full switch — same kill-switches, target pick, live-supervisor
check and hop cap as the hook. Only one tick per session commits (the
`.switched` marker is claimed first), and the tick never writes to stdout.

*`StopFailure` hook.* Register `csm hook --owner <profile-dir>` on Claude
Code's `Stop`, `SubagentStop`, `SessionEnd`, and `StopFailure` hook events.
When a 429 does end the turn, Claude Code fires `StopFailure` with
`error: "rate_limit"` instead of `Stop`; `csm hook` takes that as a
definitive limit. Other `StopFailure` errors (overloaded, server errors, auth
failures) are ignored. A matcher keeps the hook to the one case that
matters:

```json
"StopFailure": [
  { "matcher": "rate_limit",
    "hooks": [ { "type": "command",
                 "command": "csm hook --owner '/Users/example/.claude.work'" } ] }
]
```

*`Stop` hook.* On `Stop` the hook compares the running profile's three
percentages against `CLAUDE_LIMIT_PCT` instead, which catches a cap crossed
during a turn that still succeeded.

All three honour `CLAUDE_AUTO_SWITCH`, `CLAUDE_AUTO_SWITCH_RELAUNCH`, the
live-supervisor check, and the per-session hop cap. The machine-wide switch
cooldown (`CLAUDE_SWITCH_COOLDOWN`) only throttles the `Stop` percentage
path; the statusline tick and `StopFailure` are each session's own live
evidence, so several sessions sharing an exhausted account can all move off
it. The model-scoped weekly
percentage refreshes only when the per-profile usage-API probe runs
(`CSM_USAGE_PROFILE_TTL`, default 300s), so a cap on that dimension alone can
take up to about five minutes to register on the tick and `Stop` paths; the
session and all-model weekly readings are live on every tick.

### Custom usage command (`CSM_USAGE_CMD`)

You don't have to rely on local collection. Set `CSM_USAGE_CMD` to any command
that prints a usage JSON blob (the same shape local collection produces) on
stdout, and `csm` will use it as a usage source instead. It runs after the
positive cache and before local collection (the explicit "check via my own
script" path takes precedence over the built-in collector), and its result is
cached like any other fetch, so a slow command is not re-run within the cache
TTL.

`CSM_USAGE_CMD` runs via `sh -c` (POSIX) / `cmd /C` (Windows) — so on Windows
the value must be `cmd.exe`-safe (single-quote quoting and Unix pipelines
won't work; wrap complex logic in a `.cmd`/`.ps1` script and point at that).
See *Custom usage source* in [Environment variables](#environment-variables)
for `CSM_USAGE_CMD`, `CSM_USAGE_CMD_TIMEOUT`, and the shared cache TTL.

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

## Environment variables

Every environment variable `csm` reads, grouped by what it affects. Names are
frozen — this section documents them, it never renames or deprecates one.
Defaults shown are what applies when the variable is unset or unparseable.

### Launch and profile

| Variable | Meaning |
|---|---|
| `CLAUDE_CONFIG_DIR` | The active profile's Claude Code config home. Set by the shell `cas` shim (or the platform floor) before `csm` runs; `csm` reads it to resolve the current profile name and directory. See *Profiles*. |
| `CLAUDE_SMART_CLAUDE_BIN` | A single binary path/name that overrides what `csm run` spawns instead of `claude`. Highest precedence (above `csm config set launch-command`); mainly for tests and one-off overrides. See *Configuration*. |
| `CSM_HOST_REPLACE` | A literal, case-insensitive, first-match `find/replace` pair (e.g. `Acme-/`) applied to the short hostname `csm statusline` shows as `<profile>@<host>`. Unset = the raw short hostname, no rewrite; `csm` carries no built-in naming convention. |
| `CSM_NO_HOME_SHIM` | Any non-empty value turns off the launch-time create-only step for the `~/.claude` compatibility shim. `csm profiles doctor` still reports the shim and `--fix-home` still repairs it. See *Third-party integration contract*. |
| `CLAUDE_TITLE_INDEX_TTL` | Seconds the session title index (`titles.tsv`) is served without a rebuild (default `300`). |

### Account scoring and the limit switch

| Variable | Meaning |
|---|---|
| `CLAUDE_LIMIT_PCT` | The 5-hour session window's "not viable" threshold, percent (default `99`). Gates both scoring/pick and the hook's/statusline tick's rate-limit check. See *What counts as a viable account*. |
| `CLAUDE_PICK_SATURATION_PCT` | The weekly (all-model and model-scoped) "not viable" threshold, percent (default `95`). |
| `CLAUDE_USAGE_MAX_AGE` / `CSM_USAGE_MAX_AGE_SECS` | Max age, in seconds, of usage data that auto-pick will still trust (default `1800`); `0` disables the gate. `CLAUDE_USAGE_MAX_AGE` wins if both are set. |
| `CLAUDE_AUTO_SWITCH` | `0` disables the whole limit-switch decision (a kill-switch). Anything else, or unset, leaves it enabled. |
| `CLAUDE_AUTO_SWITCH_RELAUNCH` | `1` (default) actually relaunches under the target profile on a switch. Any other value only notifies — it prints the manual switch command instead of relaunching. |
| `CLAUDE_SWITCH_COOLDOWN` | Seconds the machine-wide switch cooldown enforces between percentage-based switches (default `300`). Throttles the `Stop` percentage path only — never the statusline tick or a `StopFailure(rate_limit)` hit, each of which is a session's own live evidence. See *Reactive switch*. |
| `CLAUDE_MAX_HOPS` | Max switch hops one session chain may take before the hook gives up and skips (default `1`). |
| `CLAUDE_SMART_RESUME_PROMPT` | Overrides the handoff message injected into a session after a switch. Unset = `csm`'s default handoff text; set to an empty string = no handoff prompt at all; any other value is used verbatim. |
| `CLAUDE_SWITCH_GRACE_MS` | Milliseconds the process supervisor waits for the launched child to exit gracefully after a stop signal before escalating (default `5000`). |

### Usage collection and caching

| Variable | Meaning |
|---|---|
| `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` | Positive-cache lifetime for a fetched usage snapshot, seconds (default `60`). The legacy name wins if both are set. |
| `CLAUDE_USAGE_FAIL_COOLDOWN` | Negative-cache cooldown in seconds after every profile fails to fetch at once (default `120`). |
| `CSM_USAGE_PROFILE_TTL` | Seconds a profile's own store record is served without a live probe (default `300`). |
| `CSM_USAGE_RATE_LIMIT_COOLDOWN` | Seconds to back off a profile after a 429 from the usage API (default `900`). |
| `CSM_USAGE_API_BASE` | Override the usage API base URL (default `https://api.anthropic.com`). Mainly for tests. |
| `CSM_OAUTH_REFRESH` | `1` enables the opt-in headless OAuth access-token refresh (same as `csm usage --refresh-oauth`). Default off. See *Headless collectors*. |
| `CSM_OAUTH_TOKEN_URL` | Override the OAuth token endpoint used by that refresh (default `https://platform.claude.com/v1/oauth/token`). Mainly for tests; an override is announced on stderr. |
| `CSM_STATUSLINE_NO_CAPTURE` | `1`/`true` disables the automatic statusline usage capture in `csm statusline`. |

### Custom usage source

| Variable | Meaning |
|---|---|
| `CSM_USAGE_CMD` | Shell command whose stdout is a usage JSON blob, overriding local collection. Empty/unset = disabled. See *Custom usage command*. |
| `CSM_USAGE_CMD_TIMEOUT` | Hard deadline in seconds for that command (default `10`). On timeout `csm` falls through to local collection. |

## Testing

Unit tests (including the `no_private_names` leak guard) run with `cargo
test`. `bash e2e/run.sh` runs the end-to-end limit-switch harness: it builds
`csm` and a fake, sleeping `claude` binary, drives both through an isolated
sandbox HOME with no network access, and exercises the hook-driven and
statusline-tick-driven profile-switch paths (relaunch argv, cooldowns, the
`CLAUDE_AUTO_SWITCH`/`CLAUDE_AUTO_SWITCH_RELAUNCH` kill-switches, and the
flags a relaunch carries) across 12 scenarios. See [`e2e/README.md`](e2e/README.md) for what each scenario covers
and how to run it against a prebuilt binary.

## License

BSD 3-Clause License. See [`LICENSE`](LICENSE).
