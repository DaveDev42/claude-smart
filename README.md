# claude-smart (`csm`)

Cross-platform smart session manager for [Claude Code](https://claude.ai/code).

`csm` is a single binary that wraps the `claude` CLI with:

- **smart session selection** — resume the right session, or start fresh, per
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

csm profiles [list]                  list configured profiles
csm profiles add  <name> [<dir>]     register (dir defaults to ~/.claude.<name>)
csm profiles set  <name> <dir>       register/overwrite a profile dir
csm profiles rm   <name>             unregister (refused if it is the default)
csm profiles use  <name>             set the machine default profile
csm profiles edit                    interactive editor (TTY)
csm profiles dir  [<name>]           print a profile's config dir

csm usage [--json] [--no-fetch]      multi-profile usage table (opt-in; see Hub)

csm pick-account [<cur>] [--include-current]
csm scan [<cwd>]                     session listing (TSV)
csm statusline                       `<profile>@<host>` for the shell prompt
csm completions {zsh|bash|pwsh}      shell completions
```

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

### Without a registry (degraded mode)

The registry is **optional**. With no `~/.config/claude-as/profiles.json` (a
fresh machine, or a "toss" box you never set up), `csm` runs in a degraded mode:
the plain smart launcher still works, and the registry-dependent commands fail
*safe* rather than erroring out —

| Command | Without a registry |
|---|---|
| `csm run` (and bare `csm`) | works — launches `claude` under the current `CLAUDE_CONFIG_DIR` |
| `csm scan`, `statusline`, `newuuid`, `completions`, `sidecar` | work — they don't need the registry |
| `csm profiles list` | prints `(profiles.json absent — CAS/pick features disabled)` |
| `csm usage` | prints `usage metering disabled …` + `(no profiles configured — `csm profiles add <name>`)` |
| `csm pick-account` | no-op (empty stdout), prints the `csm profiles add <name>` hint, exits 0 |

So account scoring / auto-switch / pick-account simply don't engage until you
`csm profiles add` at least one profile — nothing crashes.

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

**Hub-down account selection.** When account auto-selection needs fresh usage but
the hub can't be reached, `csm` does *not* silently keep the current account. In
an interactive terminal it opens an fzf account picker showing the last-known
(stale) usage so you can choose deliberately; in a non-interactive context (the
Stop hook, scripts) it fails safe to the current profile instead of blocking on a
picker.

### Custom usage command (`CSM_USAGE_CMD`)

You don't have to run a hub. Set `CSM_USAGE_CMD` to any command that prints a
usage JSON blob (the same shape the hub serves) on stdout, and `csm` will use it
as a usage source. It runs ahead of the hub HTTP/SSH transports — the explicit
"check via my own script" path — and its result is cached like a hub fetch, so a
slow command is not re-run within the cache TTL.

| Variable | Meaning |
|---|---|
| `CSM_USAGE_CMD` | Shell command whose stdout is a usage JSON blob. Empty/unset = disabled. Runs via `sh -c` (POSIX) / `cmd /C` (Windows). |
| `CSM_USAGE_CMD_TIMEOUT` | Hard deadline in seconds for that command (default `10`). On timeout `csm` falls through to the hub. |
| `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` | Positive-cache lifetime in seconds (default `60`). The legacy name wins if both are set. |

The command is **not** compiled in — the extraction mechanism is yours to own,
because a robust one is environment-specific. (Note: at the time of writing, the
`claude` CLI has no usage subcommand, and its `/usage` slash command does not
reliably emit the session/week percent gauges in non-interactive mode — so a
plain `claude -p`-based recipe is *not* recommended as a usage source. If you
script one, validate that your command actually yields the gauges before relying
on it for account scoring.)

(The usage JSON shape, the collector, and a reference hub deployment are
documented in the design notes under `docs/`.)

## License

BSD 3-Clause License. See [`LICENSE`](LICENSE).
