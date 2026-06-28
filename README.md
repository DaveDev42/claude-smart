# claude-smart (`csm`)

Cross-platform smart session manager for [Claude Code](https://claude.ai/code).

`csm` is a single binary that wraps the `claude` CLI with:

- **smart session selection** — an interactive picker, shown on every launch,
  to start fresh, continue the newest session, or pick an existing one (each row
  shows its short id, time, mode, and title; type to fuzzy-filter), per
  directory;
- **profile management** — multiple isolated Claude Code config homes
  (`CLAUDE_CONFIG_DIR`) with a one-command switcher;
- **account scoring + auto-switch** — pick the least-saturated account to launch
  under, and relaunch on a rate-limit hit;
- **usage metering** — a multi-profile usage table (opt-in; see *Hub*);
- a **limit-detection hook** and a **relaunch/handoff loop**.

It runs on macOS, Linux/WSL, and Windows-native with a single binary — no shell
implementation to keep in sync.

## Install

```sh
# from crates.io
cargo install claude-smart

# or build from source
git clone https://github.com/DaveDev42/claude-smart
cd claude-smart && cargo install --path .
```

The binary is named `csm`. An optional shell function lets `cas` switch the
active profile in your *current* shell (a child process cannot mutate its
parent's environment, so this part is a tiny shim — see *Profiles* below).

## Usage

```
csm [claude-args...]                 bare = smart launch (implicit `csm run`)
csm run [csm-flags] [-- claude...]   smart launcher (session + account + relaunch)

# run flags (account + session selection)
  --profile <name>                   launch under this profile (skip all picking)
  -i, --interactive                  manual pick: force account + session pickers
  --no-pick                          keep current profile, no scoring
  -A, --pick-account                 force an account pick this launch (overrides --no-pick)
  -c, --continue                     resume newest free session
  # default: ALWAYS opens the session picker (new / continue / pick existing) so the
  #   choice is never made silently, and auto-picks the best account by usage; if
  #   usage is unavailable (hub down OR no scorable data) it opens the account picker
  #   instead of silently staying put. `-i` skips the account auto-pick entirely.

csm profiles [list]                  list configured profiles
csm profiles add  <name> [<dir>]     register (dir defaults to ~/.claude.<name>)
csm profiles set  <name> <dir>       register/overwrite a profile dir
csm profiles rm   <name>             unregister (refused if it is the default)
csm profiles use  <name>             set the machine default profile (+ floor)
csm profiles edit                    interactive editor (TTY)
csm profiles dir  [<name>]           print a profile's config dir

csm usage [--json] [--no-fetch]      multi-profile usage table (opt-in; see Hub)

csm pick-account [<cur>] [--include-current]
csm scan [<cwd>]                     session listing (TSV)
csm sidecar {read|write|merge|flags} <sid> [k=v...]   per-session state store
csm statusline                       `<profile>@<host>` for the shell prompt
csm completions {zsh|bash|pwsh}      shell completions
```

> `csm` also recognizes a few **machine-interface** subcommands meant for
> automation, not hand typing: `csm hook` (the Stop/SubagentStop/SessionEnd
> limit-switch hook, wired from Claude Code `settings.json`), `csm cas` (the
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
| `csm usage` | prints `usage metering disabled …` + `(no profiles configured — `csm profiles add <name>`)` |
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

## Hub (usage metering — opt-in)

`csm usage` and account scoring read usage data from a **hub** — a machine you
designate that periodically scrapes Claude Code usage limits and serves them.
This is entirely opt-in via two environment variables; **with neither set, the
hub features are simply disabled** and `csm` works as a plain smart launcher:

| Variable | Meaning |
|---|---|
| `CLAUDE_USAGE_URL` | HTTP endpoint that returns the usage JSON blob. Empty/unset = HTTP disabled. |
| `CLAUDE_HUB_HOSTNAME` | Short hostname of the hub. When it matches the local host, `csm` reads the hub's cache directly (no network). |

When unset, `csm usage` prints a "metering disabled" line and still shows your
registry. When the hub is unreachable, `csm usage` serves the last-known cache
with a staleness header (`⚠ hub data is 7m old …`); `--no-fetch` reads only the
cache. No hub identifiers are baked into the binary — it ships clean for anyone
to use.

**Account picker (hub-down or no scorable data).** Account auto-selection opens
the account picker — rather than silently keeping the current account —
whenever it cannot score, which is two distinct cases: (1) **hub-down** — the hub
can't be reached / returns no usable blob (a *fetch* failure); (2) **no scorable
data** — the hub responds but no profile yields a usable percentage (every
profile errored, or none has a `week_all` section, or the profile map is empty).
Both surface the picker in an interactive terminal so you can choose deliberately
against the last-known (stale) usage; in a non-interactive context (the Stop
hook, scripts) both fail safe to the current profile instead of blocking on a
picker. This is distinct from **all-saturated** — when real percentages exist but
*every* account is over the limit, there is nothing better to pick, so `csm`
keeps the current profile with a warning and does **not** open the picker.
Passing **`-i` / `--interactive`** forces the picker in all of these cases too:
it skips the auto-pick entirely and always asks (and also forces the session
picker). `--profile <name>` still wins over everything — explicit, no picking.
**The picker is ordered by recommendation, not alphabetically:** rows are
ranked exactly as the live scorer (`pick_best`) would choose — viable accounts
first (highest `week_all.pct`, soonest-reset tie-break), saturated / session-limited
/ errored / no-data rows below — so the account auto-pick *would* have selected
leads the list and is flagged with a **`★`** marker. Because the picker's cursor
starts on the first row, **pressing Enter takes the recommendation**; you only
need to move when you want a different one. (When every account is saturated /
errored / dataless there is no recommendation, so no row gets the `★`.)
Pressing **Escape / Ctrl-C in any picker cancels the launch entirely**
(`csm` exits without starting `claude`) — it does not silently fall through to a
default.

### Custom usage command (`CSM_USAGE_CMD`)

You don't have to run a hub. Set `CSM_USAGE_CMD` to any command that prints a
usage JSON blob (the same shape the hub serves) on stdout, and `csm` will use it
as a usage source. It runs ahead of the hub HTTP/SSH transports — the explicit
"check via my own script" path — and its result is cached like a hub fetch, so a
slow command is not re-run within the cache TTL.

| Variable | Meaning |
|---|---|
| `CSM_USAGE_CMD` | Shell command whose stdout is a usage JSON blob. Empty/unset = disabled. Runs via `sh -c` (POSIX) / `cmd /C` (Windows) — so on Windows the value must be `cmd.exe`-safe (single-quote quoting and Unix pipelines won't work; wrap complex logic in a `.cmd`/`.ps1` script and point at that). |
| `CSM_USAGE_CMD_TIMEOUT` | Hard deadline in seconds for that command (default `10`). On timeout `csm` falls through to the hub. |
| `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` | Positive-cache lifetime in seconds (default `60`). The legacy name wins if both are set. |

`csm` does the scoring and the account choice itself — the command only reports
the **facts** (each profile's usage); you do not pick a profile in it.

The command is **not** compiled in — the extraction mechanism is yours to own,
because a robust one is environment-specific. (Note: at the time of writing, the
`claude` CLI has no usage subcommand, and its `/usage` slash command does not
reliably emit the session/week percent gauges in non-interactive mode — so a
plain `claude -p`-based recipe is *not* recommended as a usage source. If you
script one, validate that your command actually yields the gauges before relying
on it for account scoring.)

See [`examples/usage-collector.sh`](examples/usage-collector.sh) for a reference
`CSM_USAGE_CMD`: it shows the three practical strategies — proxy an existing hub
endpoint (one `curl`), re-emit a cache file, or synthesize the JSON from
per-profile facts. Its header comment also documents the **full usage JSON
shape** (`profiles[<name>].session.pct` / `.week_all.pct` / `.resets`) and the
scoring rules csm applies to it, so it doubles as the format reference.

## License

BSD 3-Clause License. See [`LICENSE`](LICENSE).
