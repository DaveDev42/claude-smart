# claude-smart (`csm`)

A launcher for [Claude Code](https://claude.ai/code) that works with
[Orca](https://github.com/stablyai/orca)'s Claude accounts.

`csm` is one binary that wraps the `claude` CLI. It adds:

- session selection: resume, continue or start fresh, with a fuzzy session
  picker when you ask for one;
- account switching over Orca's Claude account list, from the terminal,
  whether Orca is running or not;
- a limit switch: when the account in use hits a usage cap, the running
  session moves to another account and resumes where it stopped;
- usage metering across every account;
- launches from Orca panes that never stop to ask a question.

It runs on macOS, Linux (including WSL) and Windows from the same binary.
Windows support is partial; see *Platforms*.

## Install

```sh
# Homebrew (macOS)
brew install davedev42/tap/claude-smart
#   the formula also answers to `csm` and `smart-claude`

# crates.io
cargo install claude-smart

# from source
git clone https://github.com/DaveDev42/claude-smart
cd claude-smart && cargo install --path .
```

The binary is `csm`. Homebrew also installs `smart-claude` as a second name
for the same binary.

## How csm relates to Orca

Orca keeps a list of Claude accounts. For each one it stores a login (the
OAuth grant, in the macOS Keychain or in a file) and some profile metadata,
and it keeps one of them active. Claude Code runs in a single config
directory, called `D` here: Orca's `CLAUDE_CONFIG_DIR` if Orca has one set,
otherwise `~/.claude`. Switching accounts means writing another account's
login into `D`.

csm keeps no account list of its own. It reads Orca's list and changes it
the way Orca would:

- With Orca running, csm asks Orca over its local RPC socket
  (`accounts.list`, `accounts.selectClaude`, `accounts.addClaudeFromConfigDir`,
  `accounts.removeClaude`). Orca does the work with its own code.
- With Orca stopped, csm runs a port of Orca's switch. It writes the same
  files and Keychain items Orca would, so the next Orca start finds a state
  Orca itself could have produced. Once Orca 1.4.214 or later has run, it
  keeps its state in SQLite (`profile-state.db`), and every change
  (`accounts use`, `add`, `import`, `rm`, and the limit switch) needs Orca
  running. `csm orca status` shows which case applies.

One account is active per machine at a time. Every claude csm starts runs
in `D`, so all sessions on a machine share the active account.

You do not need Orca running to use csm, but you need Orca's account store:
csm has nothing to switch between until Orca knows at least two accounts.
Add them in Orca, or with `csm accounts add` / `csm accounts import`.

## Quick start

```sh
csm accounts                    # Orca's accounts, the active one marked
csm                             # start claude in D (a fresh session)
csm -c                          # continue the newest session in this directory
csm -i                          # pick a session
csm accounts use bob@example.com
```

Then install the hook and the status line (see *Limit auto-switch*) so a
capped session can move on its own.

## Commands

```
csm [claude-args...]                     bare = launch (implicit `csm run`)
csm run [run-flags] [-- claude-args...]  launch with session handling and the limit switch
csm claude <args...>                     run claude in D, arguments verbatim

csm accounts [list]                      Orca's accounts (* active, D = the account D holds)
csm accounts use <id|prefix|email>       make that account active
csm accounts add                         log in a new account
csm accounts import <dir>...             import the logins held by Claude config dirs
csm accounts rm <id|prefix|email>        remove an account that is not active
csm accounts doctor [--fix] [--offline]  check the store, stashes, quarantine and D

csm orca [status]                        what csm sees of Orca (never prints secrets)
csm orca setup                           create the `claude` alias for Orca panes

csm migrate [plan]                       read-only: what import/retire would do
csm migrate import [--dry-run]           move profile logins into Orca
csm migrate retire [--dry-run] [name...] retire verified profile dirs

csm usage [--json] [--no-fetch] [--refresh]   usage per account
csm usage capture                        read a statusLine payload on stdin, record it

csm config [show]                        csm's own config
csm config get|set|unset launch-command  what `csm run` starts instead of `claude`
csm config get|set|unset min-claude-version   lowest claude a limit switch accepts next to unsupervised sessions

csm hook                                 the Claude Code hook (Stop, StopFailure, SubagentStop, SessionEnd)
csm statusline                           Claude Code statusLine segment: <account>@<host>
csm scan [<cwd>]                         session list (TSV)
csm sidecar {read|write|merge|flags} <sid> [k=v...]   per-session state
csm reap [--dry-run] [--term] [--all|--session <sid>] kill processes orphaned by claude
csm completions {zsh|bash|pwsh}          shell completions
csm newuuid                              a fresh UUID v4
```

Accounts are named by Orca's account id, a unique prefix of it, or the full
email address.

### Run flags

```
-i, --interactive          open the session picker
-n, --new                  start a fresh session
-c, --continue             resume the newest session not open elsewhere
-r, --resume [<id>]        resume that session; with no id, open the picker
--session-id <uuid>        passed to claude; csm tracks the id
--model <m>                passed to claude; kept across a limit switch
--effort <e>               passed to claude; kept across a limit switch
--permission-mode <p>      passed to claude; kept across a limit switch
```

With none of these, `csm` starts a fresh session. csm stops reading flags at
the first positional argument, so a prompt and everything after it reach
claude untouched. Put claude flags that csm would read after `--`:
`csm run -- -c`. `csm run --help` prints the list above and
`csm run -- --help` shows claude's help.

`-n` means "new session" to csm. claude has its own `-n/--name`, which
`csm` and `csm run` therefore shadow; use `csm claude -n <name>` or
`csm run -- -n <name>` to reach it.

### Words csm leaves to claude

csm treats a word as its own only as the first argument, and its words never
overlap claude's subcommands. Anything else goes to claude: `csm mcp list`,
`csm doctor` and `csm update` all run claude's commands. `csm claude <args>`
skips csm entirely apart from running claude in `D`: no flag parsing, no
session handling, no limit switch.

## Launch contexts

Before anything else, `csm run` decides where it was started from. The
checks run in this order:

1. Print. `-p`/`--print` appears before `--`, or stdin is not a terminal.
   csm runs claude with the arguments and environment as given and does
   nothing else: no session handling, no usage check, no supervisor. Orca's
   source-control AI and its model discovery both launch this way.
2. Orca pane. `ORCA_PANE_KEY` is set (Orca exports it into every pane).
3. Orca structured session. `ORCA_AGENT_SESSION_SPAWN_TOKEN` is set.
4. Interactive. Anything else.

`CSM_ORCA=1` forces the Orca behaviour and `CSM_ORCA=0` turns detection
off. Neither changes Print. `ORCA_USER_DATA_PATH` and `ORCA_APP_VERSION` are
ignored on purpose: Orca sets them on its own process, so a tmux server or
an editor started inside a pane inherits them without being an Orca launch.

### Inside an Orca pane

csm makes no account decision. Claude runs on whatever account Orca has
active. Before claude starts, csm reads only files and the process table: no
Keychain, no network, no picker, no prompt. `csm --resume <id>`, which is
what Orca's session list sends, starts claude at once. Warnings go to csm's
log instead of the pane; only fatal errors and the one-line notice after a
limit switch are printed.

If a login dotfile changed `CLAUDE_CONFIG_DIR` in the pane, csm puts it back
to Orca's `D`. For a managed account it removes the variables Orca also
removes (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
`CLAUDE_CODE_OAUTH_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, and an
`ANTHROPIC_CUSTOM_HEADERS` that carries a credential), so they cannot
override the account.

The supervisor still runs, so the limit switch works in panes too.

### Interactive (outside Orca)

If the active account is over a limit, another account has room, and no
other claude is running in `D`, csm switches before it starts claude.
Otherwise it starts claude on the current account and prints one warning
line.

### The `claude` alias for Orca

Orca launches its Claude panes with `claude` unless its
`agentCmdOverrides.claude` setting names another command. Pointing it at
csm gets every pane the limit switch. `csm orca setup` creates the alias
(`<state>/bin/claude`, a symlink on macOS and Linux; `claude.exe`, a hard
link or copy, on Windows) and prints the value to put in Orca's settings:

```sh
csm orca setup
# csm: created /Users/example/.local/state/csm/bin/claude
#
# In Orca's settings, set the Claude agent command override to:
#   /Users/example/.local/state/csm/bin/claude
```

csm never edits Orca's settings. Set the override in Orca yourself. Orca's
`agentDefaultArgs.claude` may keep `-n`.

The alias points at the `csm` on your `PATH` (with Homebrew, the
`bin/csm` link rather than the versioned file behind it), so an upgrade
does not break it. `csm orca status` and `csm accounts doctor` report an
alias whose target is gone; `csm accounts doctor --fix` or another
`csm orca setup` repoints it.

Started under the name `claude`, csm reserves none of its own words:
claude's subcommands, `--version`, `-v`, `--help` and `-h` as the first
argument go straight to the real claude further down `PATH` (so Orca's
version probe and hook installer see claude itself), and everything else is
a `csm run` launch. Because the alias is named `claude`, Orca treats those
panes as Claude panes: it syncs the account before launch, holds launches
during its own switch, and tracks the pane as a live Claude session. A plain
`csm` override also works, but Orca then skips those steps.

## Limit auto-switch

A session that hits a usage cap stops, `D` moves to another account, and
the same session resumes with a short note explaining the move. Since there
is one active account per machine, the switch moves every session on the
machine.

Three signals start it:

- The statusLine tick. When a subscription cap is reached, Claude Code
  keeps the turn open and retries on its own, and no hook fires. The
  statusLine command still runs about once a second with the live
  `rate_limits`, and `csm statusline` (or `csm usage capture`) checks them
  on every tick. This is the path most caps take.
- `StopFailure` with `error: "rate_limit"`: a 429 ended the turn.
- `Stop`: the usage figures crossed a limit during a turn that finished.

Install both pieces once, in `~/.claude/settings.json` (next to the hooks
Orca installs there):

```json
{
  "statusLine": { "type": "command", "command": "csm statusline" },
  "hooks": {
    "Stop":         [ { "hooks": [ { "type": "command", "command": "csm hook" } ] } ],
    "SubagentStop": [ { "hooks": [ { "type": "command", "command": "csm hook" } ] } ],
    "SessionEnd":   [ { "hooks": [ { "type": "command", "command": "csm hook" } ] } ],
    "StopFailure":  [ { "matcher": "rate_limit",
                        "hooks": [ { "type": "command", "command": "csm hook" } ] } ]
  }
}
```

`csm hook` still accepts the `--owner <dir>` argument older installs passed
and ignores it. If you keep your own statusLine script, add
`printf '%s' "$input" | csm usage capture &` to it instead.

### Who switches

The hook never switches accounts itself. It records which account capped
and stops its claude. The `csm run` supervisor that started that claude
takes a lock and looks at the active account:

- If someone already moved `D` (another csm session, or you in Orca's GUI)
  and the new account has room, the session follows it and resumes there.
- Otherwise this session leads: it switches to the best other account
  (through Orca's RPC when Orca runs, offline otherwise), resumes with
  `claude --resume <sid>`, and prints
  `csm: account alice capped; resumed on bob`.

The leader leaves a note for every other csm session that was on the capped
account. Each of them moves over at its next turn boundary, without a
second switch, instead of being stopped in the middle of a tool call. A
claude that csm did not start keeps running and moves to the new account
with `D`, the same as after a switch in Orca's GUI. That relies on newer
Claude Code picking up a changed login instead of writing its old one
back, so when such a session is live the switch requires `claude
--version` to be at least `min-claude-version` (default 2.1.283) and
otherwise only notifies.

The resumed session keeps its `--model`, `--effort` and `--permission-mode`
and the launch flags that shape it (`--dangerously-skip-permissions`,
`--add-dir`, `--settings`, `--mcp-config`, the tool lists, the system-prompt
flags, `--agent`, `--plugin-dir` and so on). The first prompt is not
repeated, since `--resume` carries the conversation. Flags that would clash
with the resume (`--continue`, `--session-id`, `--print` and the like) and
flags csm does not know are dropped and named in `limit-switch.log`; the
log never quotes a prompt.

### Limits on switching

- One switch per session chain. A second cap after a switch only notifies.
  The limit is fixed at 1: `CLAUDE_MAX_HOPS=0` stops the hook from
  switching at all, but a larger value changes nothing, because the
  session's `.switched` marker and the relaunch loop's own cap both stay
  at one switch.
- A switch triggered by the `Stop` usage figures waits out a machine-wide
  cooldown (`CLAUDE_SWITCH_COOLDOWN`, default 300 s). A statusLine tick or a
  429 is the session's own evidence and is not held back.
- When every other account is capped too, csm notifies and changes nothing.
- `CLAUDE_AUTO_SWITCH=0` turns the switch off. `CLAUDE_AUTO_SWITCH_RELAUNCH=0`
  keeps detection and notifies instead of relaunching.

### What counts as capped

| Window | Not usable at |
|---|---|
| 5-hour session | `CLAUDE_LIMIT_PCT`, default 99% |
| weekly, all models | `CLAUDE_PICK_SATURATION_PCT`, default 95% |
| weekly, one model tier (`week_fable`) | never on its own; see below |

A cap on the model-scoped weekly window alone does not switch accounts.
The session resumes on the same account with `--model` set to
`CLAUDE_FABLE_FALLBACK_MODEL` (default `opus`), once per weekly window. This
spends no switch. `CLAUDE_FABLE_FALLBACK=0` turns it off, and the cap is
then handled like any other.

## Usage metering

`csm usage` prints one row per Orca account: email, organization, the three
usage windows and when they reset. `--json` prints the same data as JSON.
Usage comes from Anthropic's OAuth usage API (`GET /api/oauth/usage`),
called with each account's own grant:

- The active account: the statusLine captures first, else the grant in `D`.
  csm never refreshes that grant; Claude Code and Orca do.
- Other accounts with Orca running: Orca's own cached figures
  (`accounts.list`). csm does not touch their stashes.
- Other accounts with Orca stopped: the stashed grant. `csm usage --refresh`
  and the pick before a limit switch refresh a stashed grant that expires
  within five minutes, the way Orca does. Nothing else refreshes a grant.

Results are cached: the whole snapshot for `CSM_USAGE_TTL_SECS` (60 s), each
account's record for `CSM_USAGE_PROFILE_TTL` (300 s), with a back-off after
a 429 or a total failure. A window whose reset time has passed is shown as
0%. `--no-fetch` reads only the cache; `--refresh` skips the caches and asks
a running Orca to re-probe (up to 30 s).

`CSM_USAGE_CMD` replaces collection with your own command that prints the
usage JSON. See [`examples/usage-collector.sh`](examples/usage-collector.sh)
for the format.

`csm statusline` prints `<account>@<host>`: the local part of the account's
email and the short host name (`CSM_HOST_REPLACE=Acme-/` strips a prefix).
Inside an Orca pane it also forwards the payload to Orca's own statusLine
receiver, so Orca's usage display stays current even though your statusLine
command is csm's. The forward runs in a detached child after the segment
is printed, so it adds no render latency.

The hook (`csm hook`, every event) reads only csm's own files. It makes no
network call, no RPC call and no Keychain access, so a `SessionEnd` hook
returns well inside Claude Code's 1.5 s budget even with stale usage.

## Accounts from the terminal

- `csm accounts use <account>` makes that account active. It does not stop
  running sessions; they pick up the new account from `D`, as after a
  switch in Orca's GUI.
- `csm accounts add` logs in a new account. With Orca running it hands over
  to Orca's own CLI (`account add --agent claude`): the one bundled with the
  running Orca (`Orca.app/Contents/Resources/bin/orca` on macOS), else the
  one on `PATH` (`orca`, or `orca-ide` on Linux, where `orca` is the screen
  reader). With Orca stopped it runs `claude auth login` in a temporary
  config dir and files the result the way Orca would. `add` and `import`
  run claude through the configured launch command when one is set.
- `csm accounts import <dir>...` takes the login a Claude config dir holds
  and adds it as an account (or refreshes an existing one with the same
  email and organization). The active account does not change.
- `csm accounts rm <account>` removes an account other than the active one.
- `csm accounts doctor` reports an unfinished switch, quarantined grants,
  stashes without an account, two accounts sharing a refresh token, a
  stash whose grant belongs to someone else, a `D` that disagrees with
  the active account, and a `claude` alias whose target is gone. `--fix` repairs what it safely can. `--offline` skips
  the network checks.

### Quarantine

Before csm writes another login into `D`, it works out whose grant `D`
holds now (Claude Code may have refreshed it since Orca last saved it). It
asks Anthropic's profile endpoint who the grant belongs to. A grant that
belongs to the account on record goes back to that account's stash. A
grant it cannot attribute (another account, a 401 that a refresh does not
fix, no answer) goes to csm's quarantine under the state dir and is never
written to another account's stash. `accounts doctor` lists quarantine
entries by fingerprint. Nothing in the quarantine is deleted unless a live
stash holds the same grant.

## Migrating from the profile setup

Earlier csm versions kept named profiles (`~/.claude.<name>` directories
listed in `~/.config/claude-as/profiles.json`) and pinned
`CLAUDE_CONFIG_DIR` machine-wide to one of them. Orca then inherited that
pin. The move to Orca's accounts is one pass per machine:

1. `csm migrate plan`. Read-only. For each profile it shows whether the
   account is already in Orca, still to import, or has no login. It only
   checks that a grant is there (Keychain items are probed without reading
   the secret) and reads no secrets; `import` compares the profile dir's
   grant with Orca's stash by fingerprint and expiry. It also shows Orca's
   current `D`, the target `D` (`~/.claude`), and what steps 5 and 6 will
   do.
2. End every claude session, including panes Orca's terminal daemon keeps
   alive after Orca quits. Then quit Orca. `csm reap --dry-run` should find
   nothing.
3. Remove the machine-wide `CLAUDE_CONFIG_DIR` (the shell export, the
   launchd variable on macOS, the `HKCU\Environment` value on Windows) and
   open a new shell.
4. `csm migrate import`. It imports each profile Orca does not have yet.
   For a profile Orca already has, it reads back the newest grant from the
   profile dir into Orca's stash, after the profile endpoint confirms the
   owner. When Orca keeps its state in SQLite (1.4.214 and later), csm
   adds accounts only through a running Orca, so this step prints a
   `csm accounts import <dir>` line for each new profile instead; run
   those once Orca is started, before `csm migrate retire`. Step 7 then
   prints the `csm accounts use` line to run the same way.
5. If `~/.claude.json` does not exist yet, the same command creates it from
   the old default profile's `.claude.json` without `oauthAccount`, so
   claude keeps its onboarding state and settings. Either way it merges
   that profile's trust settings (`projects[<path>]`) and user MCP servers
   into `~/.claude.json`, keeping keys already there, so Orca panes do not
   ask to trust every folder again. Differences in other profiles are
   listed for you to merge by hand.
6. It turns `~/.claude/projects`, `sessions` and `plugins` from links into
   `~/.claude.shared/` into real directories, and moves csm's session
   sidecars, title index and scan indexes from `~/.claude.shared/smart` to
   the new state dir. Caches and per-profile records stay there unread;
   delete the dir once you have looked at it.
7. It makes the old default profile's account active.
8. `csm migrate retire`. For each profile whose stash was verified, it moves
   the dir's grants into the quarantine, renames the dir to `<dir>.retired`,
   and once no profile is left clears the machine-wide variable, then
   removes `~/.config/claude-as/`. If clearing the variable fails, the
   registry stays so that running `retire` again retries it. A profile
   whose stash cannot be verified is skipped and says so.
9. Start Orca (its `D` is now `~/.claude`), run `csm orca setup` and set
   `agentCmdOverrides.claude` as it says.

`import` and `retire` refuse to run while Orca runs, while a claude session
is live in a dir they would touch, or while `CLAUDE_CONFIG_DIR` still names
something other than `~/.claude`. `--dry-run` shows what they would do.

The old shell function `cas` no longer switches anything:
`csm cas --eval …` prints nothing and exits 0, so a leftover shim does not
break a shell, and `csm cas --print-default-dir` prints `D`. Remove the shim
when convenient.

## What csm never does

- It never keeps its own account list, and never selects "no account" in
  Orca.
- It never writes Orca's store while Orca runs; it asks Orca over RPC.
- It never writes Orca's store offline for an Orca version it was not
  tested with, or for a store whose format it does not recognize.
- It never edits Orca's settings, including `agentCmdOverrides`.
- It never deletes a grant it cannot attribute. It quarantines it.
- It never refreshes the grant `D` holds.
- It never prints or logs a token, a credential file, Orca's RPC token or
  Orca's hook token.
- It never makes a network, RPC or Keychain call from `csm hook`, or before
  claude starts in an Orca pane.
- It never stops a running session to switch accounts by hand.

## Configuration

csm's own settings live in `~/.config/claude-smart/config.json`. The main
setting is the launch command, the program `csm run` starts instead of
`claude`, for a drop-in wrapper such as
[`happy`](https://github.com/slopus/happy-cli):

```sh
csm config set launch-command happy
csm config set launch-command npx happy   # several words, stored as an argv array
csm config get launch-command
csm config unset launch-command
```

The other setting, `min-claude-version`, is the floor described under
*Who switches*: `csm config set min-claude-version 2.1.300`.

`CLAUDE_SMART_CLAUDE_BIN` overrides the launch command for one run. csm skips any candidate
that turns out to be csm itself (the `claude` alias, for instance) and uses
the next `claude` on `PATH`.

State lives in `$XDG_STATE_HOME/csm` (default `~/.local/state/csm`) on macOS
and Linux, and `%LOCALAPPDATA%\csm` on Windows.

## Environment variables

### Launch

| Variable | Meaning |
|---|---|
| `CSM_ORCA` | `1` treats the launch as an Orca pane, `0` turns Orca detection off. Other values: detect. Never overrides Print. |
| `CSM_EMBEDDED` | Older name for `CSM_ORCA`, read when `CSM_ORCA` is unset. |
| `CLAUDE_SMART_CLAUDE_BIN` | The program to start instead of `claude` (above the config file's launch command). |
| `CLAUDE_CONFIG_DIR` | Read to find `D`. csm sets it for claude only when the inherited value would put claude somewhere other than `D`. |
| `CSM_HOST_REPLACE` | `find/replace` applied to the host name in `csm statusline`, for example `Acme-/`. |
| `CLAUDE_TITLE_INDEX_TTL` | Seconds the session title index is reused (default `300`). |

### Limit switch

| Variable | Meaning |
|---|---|
| `CLAUDE_AUTO_SWITCH` | `0` turns the limit switch off. |
| `CLAUDE_AUTO_SWITCH_RELAUNCH` | `1` (default) relaunches after a switch; anything else only notifies. |
| `CLAUDE_LIMIT_PCT` | Session-window limit, percent (default `99`). |
| `CLAUDE_PICK_SATURATION_PCT` | All-models weekly limit, percent (default `95`). |
| `CLAUDE_MAX_HOPS` | `0` stops the hook from switching accounts. The default and upper limit is `1` switch per session chain; larger values have no effect. |
| `CLAUDE_SWITCH_COOLDOWN` | Seconds between switches started by the `Stop` usage check (default `300`). |
| `CLAUDE_FABLE_FALLBACK` | `0` turns off the same-account model fallback. |
| `CLAUDE_FABLE_FALLBACK_MODEL` | Model for that fallback (default `opus`). |
| `CLAUDE_SMART_RESUME_PROMPT` | The note sent to a resumed session. Empty sends none. |
| `CLAUDE_SWITCH_GRACE_MS` | Milliseconds to wait for claude to exit after the stop signal (default `5000`). |
| `CLAUDE_USAGE_MAX_AGE` / `CSM_USAGE_MAX_AGE_SECS` | Oldest usage data a switch decision trusts, seconds (default `1800`, `0` = no limit). |

### Usage

| Variable | Meaning |
|---|---|
| `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` | Snapshot cache lifetime, seconds (default `60`). |
| `CSM_USAGE_PROFILE_TTL` | Per-account record lifetime, seconds (default `300`). |
| `CLAUDE_USAGE_FAIL_COOLDOWN` | Back-off after every account failed, seconds (default `120`). |
| `CSM_USAGE_RATE_LIMIT_COOLDOWN` | Back-off for one account after a 429, seconds (default `900`). |
| `CSM_USAGE_CMD` | A command that prints usage JSON, used instead of collection. |
| `CSM_USAGE_CMD_TIMEOUT` | Its time limit, seconds (default `10`). |
| `CSM_STATUSLINE_NO_CAPTURE` | `1` stops `csm statusline` from reading stdin, which also turns off the statusLine switch and the Orca forward. |
| `CSM_USAGE_API_BASE` | Usage and profile API base URL (default `https://api.anthropic.com`). For tests. |
| `CSM_OAUTH_TOKEN_URL` | Token endpoint for the stash refresh (default `https://platform.claude.com/v1/oauth/token`). For tests. |

### Removed

| Variable or flag | Now |
|---|---|
| `CSM_OAUTH_REFRESH` | Ignored. The stash refresh is part of `csm usage --refresh` and the limit pick. |
| `csm usage --refresh-oauth` | Rejected as an unknown flag. Use `--refresh`. |
| `CSM_NO_HOME_SHIM` | Ignored. `~/.claude` is `D` now, not a shim. |
| `--profile`, `-A/--pick-account`, `--no-pick` | Gone with the profiles. |
| `csm profiles`, `csm pick-account`, `csm current-usage` | Gone. Use `csm accounts` and `csm usage --json`. |
| `csm cas` | Kept only as the quiet compat described under *Migrating*. |

## Platforms

- macOS: everything, with stashed grants in the login Keychain. csm calls
  `/usr/bin/security`, as Orca and Claude Code do, so items keep the same
  owner and no prompt appears.
- Linux and WSL: stashed grants are files. csm has no way yet to read the
  installed Orca version on Linux, so with Orca stopped it will not write
  Orca's store: switch with Orca running. Under WSL, a Windows Orca's
  userData is never written.
- Windows: the binary builds and its unit tests pass, but Orca detection
  (no `SingletonLock` there, the named pipe, telling `Orca.exe` from its
  helpers) follows Orca's source and has not been checked on a real
  machine. The relaunch loop is off until two checks pass on a real
  console (Ctrl-C handling, and a complete transcript after a limit stop),
  so a limit switch there switches and notifies instead of resuming.

## Verified Orca versions

csm's port follows Orca's source at v1.4.209 through v1.4.214. It writes
Orca's store offline only for Orca 1.4.x (read from the app bundle on
macOS and from `Orca.exe` on Windows), and only while the profile has no
`profile-state.db`; otherwise it uses RPC only. The end-to-end harness
models Orca 1.4.214.

From 1.4.214 Orca keeps its state in SQLite and writes `orca-data.json`
only as an export when it quits cleanly. csm never writes such an export,
so with Orca stopped a switch, add, import or rm is refused with a line
saying to start Orca. While Orca runs, csm reads the
account list over RPC (`csm usage`, and the supervisor's limit switch);
`csm hook` and `csm statusline` never make RPC calls, so they read the
export, which can lag until Orca next quits. `csm accounts doctor` lists
what it finds in an export but does not repair it; start Orca and check
again first. csm never creates a missing store while Orca's backups
(`.bak.N`, retained exports or database backups) are still there, since
Orca restores from them on its next start. It also refuses every offline
store write while `orca-profile-index.json` (or its `.bak`) exists but does
not parse, since Orca will not start then and csm cannot tell which
profile's store Orca will use once the index is repaired.

When no store exists at all (Orca installed but never started),
`csm accounts add` and `import` with Orca stopped create a minimal one
(`{"schemaVersion":1,"settings":{}}`) holding the new account. Orca's
source fills every missing field with its defaults on load, but a start of
a real Orca on such a store has not been tested yet. One visible side
effect is known from the source: Orca treats any existing store as an
upgrade and skips its first-run onboarding. Start Orca once before adding
accounts to avoid both. When Orca releases a new
version, `bash tools/orca-drift.sh <last-verified-tag> <new-tag>` prints the
changes to the files csm mirrors.

## Testing

`cargo test` runs the unit tests and the leak guard. `bash e2e/run.sh` runs
the end-to-end harness: a real csm build against a fake Orca (store, stashes
and RPC socket), a fake Keychain, a fake `claude` and a loopback stand-in
for the OAuth endpoints, in a sandbox with no network access. See
[`e2e/README.md`](e2e/README.md).

## License

BSD 3-Clause License. See [`LICENSE`](LICENSE).
