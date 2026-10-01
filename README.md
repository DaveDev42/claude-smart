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
csm accounts                    # Orca's accounts with their usage, the active one marked
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

csm accounts [list] [--no-usage]         Orca's accounts with usage (* active, D = the account D holds)
csm accounts use <id|prefix|email>       make that account active
csm accounts add                         log in a new account
csm accounts import <dir>...             import the logins held by Claude config dirs
csm accounts rm <id|prefix|email>        remove an account that is not active
csm accounts doctor [--fix] [--offline]  check the store, stashes, quarantine and D

csm orca [status]                        what csm sees of Orca (never prints secrets)
csm orca setup                           create the `claude` alias for Orca panes

csm migrate [--dry-run]                  finish the move off the old profile layout now

csm usage [--json] [--no-fetch] [--refresh]   usage per account
csm usage capture                        read a statusLine payload on stdin, record it

csm config [show]                        csm's own config
csm config get|set|unset launch-command  what `csm run` starts instead of `claude`
csm config get|set|unset min-claude-version   lowest claude a limit switch accepts next to unsupervised sessions
csm config get|set|unset idle-compact    off|dry-run|on, default off: send /compact before an idle session's prompt cache expires

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
active. Before claude starts, csm touches only files and the process table: no
Keychain, no network, no RPC, no picker, no prompt. The only files it
writes there are the migration's (see [Migrating from the profile setup](#migrating-from-the-profile-setup): it may move
the shared entries and merge `~/.claude.json` first). `csm --resume <id>`, which is
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

## Idle compact

`csm usage capture` and `csm statusline` both see the statusLine payload
Claude Code sends on every refresh. When that payload carries prompt cache
information, csm can use the gap while a session sits idle to keep the
cache from going cold: it sends `/compact` to that session's terminal
shortly before the cache expires, so the request after the gap re-writes a
compacted context instead of the full one.

This is opt-in: `csm config set idle-compact off|dry-run|on`, default
`off`. `dry-run` and `on` decide identically; only the relay's own
delivery (below) treats them differently.

csm's own part is a hand-off, not a delivery. On each statusLine tick it
acts on a session when all of this holds: the mode is not off, the prompt
cache is reported warm with 300 seconds or fewer left before it expires,
the re-write the payload predicts is at least 100000 tokens, the turn has
actually ended, and it has not already acted for this idle period. The
turn-ended check starts cheap (the transcript's mtime at or before the
last `Stop`) and only reads further when that mtime moved past the stop:
Claude Code keeps appending rows to the transcript well after a turn ends
(`turn_duration`, `away_summary`, and other bookkeeping), so csm reads the
transcript's tail and looks at the last real `user`/`assistant` row's own
timestamp rather than treating every later write as a new turn. A `[Request
interrupted by user` row newer than the `Stop` stamp also ends the turn, at
that row's own timestamp, even with no fresh `Stop` event at all.

When every condition holds, csm checks whether a pty-relay supervisor is
alive (`CSM_SUPERVISOR_PID` names a running process) and, if so, writes a
hand-off request file under `<state>/idle-compact-requests/<supervisor
pid>.json` (session id, mode, remaining seconds, recache estimate, a
deadline, and the statusLine payload's vim mode when it has one) and logs
`outcome=handed-off`. With no live supervisor it logs
`outcome=no-delivery-path` instead and still claims the idle period, so
either outcome is logged at most once per idle period. The tick itself never
types into a terminal, checks a screen, or reads session status.

### The relay

`csm run` puts itself between the real terminal and claude when the mode is
not `off`, stdin and stdout are both terminals, csm is in the terminal's
foreground group and `CSM_RELAY` is not `0`. Bytes pass both ways unchanged;
csm also feeds claude's output into a screen model (the `vt100` crate) and
watches the clock of the last real keystroke. `CSM_RELAY=0` forces the
direct launcher (claude gets the terminal itself, no relay, no delivery).
A relay that cannot be set up falls back to the direct launcher for that
run and says so in the limit-switch log. The supervisor sets
`CSM_SUPERVISOR_PID` in claude's environment; the direct launcher removes it.

Windows: the relay is a ConPTY. When the mode is not `off`, stdin and stdout
are both consoles and `CSM_RELAY` is not `0`, csm creates a pseudoconsole at
the console's size and starts claude inside it (through a small hidden
`csm __conpty-leader` helper that runs in the pseudoconsole, starts claude
and reports its pid back). The outer console goes into raw VT mode for the
session and gets its original modes back on every exit path, a panic
included. Keys go to the pseudoconsole as VT input, held back while csm
types; Ctrl-C reaches claude as the `0x03` key, the same as on a pty.
Resizing the window resizes the pseudoconsole (polled about ten times a
second; the smallest size passed on is 5 rows by 20 columns). Closing the
window ends claude and csm. csm exits with claude's exit code. The same
screen checks, delivery rules, log lines and OSC 777 notifications apply.
`CSM_RELAY=0`, a redirected stdin or stdout, and any ConPTY setup failure
use the direct launcher as before.

When a request arrives the supervisor looks about once a second and types
`/compact` only if every one of these holds:

- The request's deadline has not passed.
- Claude Code's own session record (`sessions/<pid>.json` under the config
  dir csm launched claude with) does not say the session is busy or waiting.
  A missing file vetoes nothing.
- No real keystroke in the last 60 seconds and no output in the last 2
  seconds. Terminal replies, focus and mouse reports do not count as
  keystrokes.
- The screen shows the main input box, empty (a dim placeholder still counts
  as empty). A draft, including a multi-line or wrapped one and a `[Pasted
  text ...]` placeholder, is never typed over: csm notifies once and gives
  up on that request. A dialog, menu or picker (no box) is retried until the
  deadline.
- The screen is not busy. The input box looks empty while a reply generates,
  so csm also refuses when the window title carries Claude Code's spinner
  glyph or an activity line such as `(3s · thinking)` is on screen. Refused
  requests log `vetoed-screen-busy`.
- The vim mode agrees with the request. The statusLine payload's vim mode is
  the reference: no vim reported means the box is treated as plain and `i` is
  never sent; INSERT requires the `-- INSERT --` marker on screen; NORMAL
  requires it absent, and csm then sends `i` and waits for the marker before
  typing. Any disagreement is treated as no box and retried.

It then holds the user's input back, types `/compact`, waits for the output
to settle and checks that the box holds exactly `/compact`. If the slash
menu is open, its highlighted entry must be `/compact` too, otherwise Enter
would run something else. Only then does it press Enter. Otherwise it erases
what it typed (one DEL per character, plus Esc if it entered insert mode
itself), tells the user and logs `verify-failed`. Held input is flushed in
order afterwards, on every exit path. Within 10 seconds of Enter, the
compaction line on screen (or a busy session record) makes it `delivered`;
without either it is `sent-unconfirmed`.

Notifications (`draft`, `verify-failed`, a request that ran out of time
after typing) are an OSC 777 sequence csm writes into claude's output only
at a sequence boundary after 500 ms of quiet, and dropped if that does not
happen within a few seconds. `dry-run` mode runs the same checks but types
and notifies nothing.

Every finished request appends one line to `<state>/idle-compact.log`. The
`outcome=` words are `handed-off` and `no-delivery-path` (the tick),
`delivered`, `sent-unconfirmed`, `draft`, `verify-failed`, `expired`,
`vetoed-<reason>` (the deadline passed while the session record or the
screen kept saying no; `<reason>` is claude's status word or
`screen-busy`), and the dry-run words `dry-run-would-type`, `dry-run-draft`
and `dry-run-expired`. Delivery lines also carry `box=`, `vim=` and
`status=` when known.

The screen checks were written against real captures of Claude Code
2.1.283 (`tests/fixtures/screens/`, see its README); a Claude Code release
that redraws the box, the mode line or the window title differently would
make the supervisor refuse (it never types on a screen it does not
recognise), not misfire.

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

## Idle compact

`csm usage capture` and `csm statusline` both see the statusLine payload
Claude Code sends on every refresh. When that payload carries prompt cache
information, csm can use the gap while a session sits idle to keep the
cache from going cold: it sends `/compact` to that session's terminal
shortly before the cache expires, so the request after the gap re-writes a
compacted context instead of the full one.

This is opt-in: `csm config set idle-compact off|dry-run|on`, default
`off`. `dry-run` and `on` decide identically; only the relay's own
delivery (below) treats them differently.

csm's own part is a hand-off, not a delivery. On each statusLine tick it
acts on a session when all of this holds: the mode is not off, the prompt
cache is reported warm with 300 seconds or fewer left before it expires,
the re-write the payload predicts is at least 100000 tokens, the turn has
actually ended, and it has not already acted for this idle period. The
turn-ended check starts cheap (the transcript's mtime at or before the
last `Stop`) and only reads further when that mtime moved past the stop:
Claude Code keeps appending rows to the transcript well after a turn ends
(`turn_duration`, `away_summary`, and other bookkeeping), so csm reads the
transcript's tail and looks at the last real `user`/`assistant` row's own
timestamp rather than treating every later write as a new turn. A `[Request
interrupted by user` row newer than the `Stop` stamp also ends the turn, at
that row's own timestamp, even with no fresh `Stop` event at all.

When every condition holds, csm checks whether its own future pty-relay
supervisor is alive (`CSM_SUPERVISOR_PID` names a running process) and, if
so, writes a hand-off request file under
`<state>/idle-compact-requests/<supervisor pid>.json` (session id, mode,
remaining seconds, recache estimate, a deadline) and logs
`outcome=handed-off`. With no live supervisor it logs
`outcome=no-delivery-path` instead and still claims the idle period, so
either outcome is logged at most once per idle period. csm itself never
types into a terminal, checks a screen, or reads session status; that is
the supervisor's job once it picks up the request, using the typing
protocol in `src/idle_compact/deliver.rs` (screen classification, a vim
NORMAL/INSERT distinction, a session-status veto, retry until the request's
deadline, and rollback on a failed verify).

### Limits

A request file past its deadline, unparseable, or of an unknown schema
version is dropped rather than acted on late; pruning piggybacks on the
existing marker sweep, so a supervisor that never picks one up (crashed,
or was never really alive despite a live-looking pid) does not leave it
behind forever.

`/compact` raises no `Stop` event, so the idle marker from before the
compaction outlives it and only clears once the next real turn ends
(checked on 15 manual compactions).

Manual compaction itself took 21 to 226 seconds in those 15 runs, median
85, across contexts of 54000 to 268000 tokens.

## Accounts from the terminal

- `csm accounts list` prints one aligned row per account: markers, email,
  the first 8 characters of the id, session, weekly and model-weekly
  percent, both reset times and the same status `csm usage` shows. It reads
  usage through the same path as `csm usage` (cache, cooldown, offline
  fallback), so it stays quick with the network down. Colors appear only on
  a terminal with `NO_COLOR` unset. `--no-usage` prints identity only (full
  id and organization) and does no fetch.
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
stash holds the same grant and every MCP server login the entry carries
(Claude Code keeps those in the same credential entry).

## Migrating from the profile setup

Earlier csm versions kept named profiles (`~/.claude.<name>` directories
listed in `~/.config/claude-as/profiles.json`), shared transcripts through
`~/.claude.shared/`, and pinned `CLAUDE_CONFIG_DIR` machine-wide to one
profile, called the floor profile here. Orca then inherited that pin.

There is no manual procedure any more. The first `csm` launch after the
upgrade moves the machine onto Orca's accounts, and later launches finish
the job. Nothing needs to be stopped first.

### What happens

The work runs in phases. Each one can stop at any point and picks up where
it left off on the next run; progress is kept in `<state>/migration.json`,
and a lock (`<state>/migrate.lock`) keeps two csm processes from migrating
at once.

1. Adopt. Every profile login becomes an Orca account, and if Orca has no
   active account, the floor profile's account becomes active. This runs
   before the first interactive launch starts claude (it waits at most 3
   seconds, then starts claude and finishes afterwards), so that session
   already switches through Orca. While Orca runs, csm starts claude in
   Orca's current config dir, which is still the floor profile's dir at
   this point.
2. Carry. `projects`, `sessions`, `todos` and the other shared entries move
   from `~/.claude.shared/` into `~/.claude`, and a link is left at each old
   path, so profile dirs and sessions that are still open keep working.
   `~/.claude.json` is created from the floor profile's copy (without its
   `oauthAccount`), or gets that profile's trust settings, MCP servers and
   any top-level keys it lacks; other profiles add their trust settings and
   MCP servers. The floor profile's `settings.json`, `CLAUDE.md`, hooks,
   agents, commands, skills, output styles, key bindings and statusline
   script are copied into `~/.claude` where `~/.claude` has none. csm's old
   sidecars and indexes move to the new state dir, and plugin registries
   are rewritten to the new install paths (again on each run, since a
   session still running in a profile dir records paths through it).
3. Cutover. csm prepares `~/.claude` for the active account and clears the
   machine-wide `CLAUDE_CONFIG_DIR` (`launchctl unsetenv` on macOS, the
   `HKCU\Environment` value on Windows; Linux has none to clear). If Orca
   is running, it keeps its current dir until you restart it; its next
   start uses `~/.claude`. csm never restarts Orca. On Linux and Windows,
   where no Keychain item mirrors the active account, a csm launch with
   Orca quit keeps running in the floor profile's dir until Orca has
   started in `~/.claude` once and put its active account there.
4. Retire. Each profile dir whose login Orca has (or that holds no login
   at all), and that no running claude uses, gets its credentials moved
   into csm's quarantine and is renamed `<dir>.retired`. The floor
   profile's dir waits for a reboot after the cutover, so no program
   started with the old value is still using it. It also waits while a
   shell startup file (`~/.zshenv`, `~/.bashrc`, a PowerShell profile and
   the like) still runs `csm cas --print-default-dir`: that old guard's
   fallback would export the renamed dir in every new shell. Remove the
   guard and the retire goes ahead. A dir also waits while a plugin
   registry records an install path that exists only inside it (reinstall
   that plugin with `/plugin`). On Windows, a dir whose login is fresher
   than Orca's copy waits until you log in to that account again in Orca.
   On Linux and Windows it also waits
   until `~/.claude` holds a login (start Orca once). Once every dir is retired, the
   registry files under `~/.config/claude-as/` are removed and
   `~/.claude.shared` becomes `~/.claude.shared.retired` when nothing
   links into it.

csm never deletes a credential or a directory. Credentials go to the
quarantine (see `csm accounts doctor`), dirs are renamed, and a
`~/.claude.<name>` dir that was not in the registry is left alone and only
listed. If a profile dir holds a newer login than Orca's stash while Orca
runs, csm files it in the quarantine and copies it into the stash the next
time it runs with Orca stopped.

### What you see

An interactive `csm` prints one line on stderr when a run changed
something (`csm: migration: ...`), and at most one line a day for each
thing that is still waiting (`csm: migration to Orca's accounts is not
finished: ...`). A run that finished after claude started leaves its line
for the next launch. Inside an Orca pane nothing is printed; the lines go
to csm's log.

The commands that change accounts run every phase that can run, the
cutover and the retire included, before they do their own work, and
print the same lines: `csm accounts use`, `add`, `import` and `rm`,
`csm accounts doctor --fix` and `csm orca setup`. `csm accounts list`,
`csm accounts doctor`, `csm usage` and `csm orca status` only mention,
once a day, that the machine still has the old layout, and
`csm orca status` shows a `migration` row. After the cutover it also warns
when Orca still runs the old `D` (restart Orca to adopt csm's), and both it and
`csm accounts doctor` name a live claude on a recorded legacy dir, which blocks
the retire step until it is closed. The hook, the status line,
`-p` and piped runs, `csm claude` and `csm cas` never start a migration.

When claude exits before a run it started is done, csm waits at most a
second for it (longer only while it copies across filesystems or holds
Claude Code's config lock) and exits; the next launch carries on from
where it stopped.

In an Orca pane, csm does nothing before claude starts, with one
exception: if Orca already runs in `~/.claude` while the shared entries
or `~/.claude.json` have not moved yet, csm moves them first so a resumed
session finds its transcript.

### `csm migrate`

`csm migrate` runs every phase that can run now and prints a report: one
block per profile dir with a line per phase, then what changed, what is
pending and why, errors, and dirs that are not registered. Logins appear
only as fingerprints. `csm migrate --dry-run` prints the same report
without writing anything.

| Exit | Meaning |
|---|---|
| `0` | Nothing legacy is left, or the cutover is recorded. The machine-wide `CLAUDE_CONFIG_DIR` setters (a LaunchAgent, a registry task, shell exports) can be removed now, even while the floor profile's dir waits for a reboot. |
| `75` | Something is pending (the report says what). Change nothing and run it again later. |
| `1` | An error before the cutover. After the cutover an error is still reported, but the exit is `0`. |

A fleet tool can run `csm migrate` after installing csm and remove its
old setters only on exit 0. The former `plan`, `import` and `retire`
verbs print a pointer to `csm migrate` and exit 1.

### What is not automatic

- Orca's `agentCmdOverrides.claude` setting. Orca has no way for another
  program to change it. A plain `csm` there keeps working; to use the
  alias instead, run `csm orca setup` and set the value it prints in
  Orca's settings.
- Accounts on a machine without Orca's store (Linux or WSL next to a
  Windows Orca, a headless box). csm moves the files there, and the floor
  profile's login into `~/.claude` when `~/.claude` has none, but other
  profile dirs stay until the accounts exist. Add them in Orca on the
  machine that runs it.
- Shell startup files. csm does not edit them. A shell that still exports
  `CLAUDE_CONFIG_DIR`, or an old guard that sets it, keeps plain `claude`
  in the old dir; remove those lines. csm itself replaces an inherited
  value that names an old profile dir or `~/.claude` with Orca's current
  dir while Orca runs (and with none after the cutover), and says so once
  a day. Any other value is left as it is.
- A login newer than Orca's stash while Orca never stops. Copying it into
  the stash needs Orca stopped: quit Orca, run `csm migrate`, start Orca.
  `csm accounts doctor` lists such entries.
- The time between the cutover and Orca's restart. A plain `claude` in a
  new shell then runs in `~/.claude` on the same login Orca holds in the
  floor profile's dir. If one side refreshes it, the other may ask you to
  log in again. Restart Orca soon after the cutover.

Settings a profile other than the floor kept for itself (`settings.json`,
`CLAUDE.md`, hooks and the like) are not copied; they stay in
`<dir>.retired` for you to move by hand. csm's statusLine and hook entries
must be in `~/.claude/settings.json` for the limit switch to work (see
[Limit auto-switch](#limit-auto-switch)).

### The `cas` compat

The old shell function `cas` no longer switches anything. `csm cas --eval
…` prints nothing and exits 0, so a leftover shim does not break a shell.
`csm cas --print-default-dir` prints Orca's current dir while that is
still an old profile dir, and before the cutover csm's own; after that it
prints nothing and a note on stderr, since exporting `~/.claude` would
make claude read the wrong `.claude.json`. `csm hook --owner <dir>`, which
old per-profile settings pass, is accepted and ignored. These go away in
the next major version.

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

`idle-compact` turns on the feature described under *Idle compact*:
`csm config set idle-compact off|dry-run|on` (default `off`).

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
| `csm migrate plan`, `import`, `retire` | Print a pointer to `csm migrate` and exit 1. The migration runs on its own. |

## Platforms

- macOS: everything, with stashed grants in the login Keychain. csm calls
  `/usr/bin/security`, as Orca and Claude Code do, so items keep the same
  owner and no prompt appears.
- Linux and WSL: stashed grants are files. csm has no way yet to read the
  installed Orca version on Linux, so with Orca stopped it will not write
  Orca's store: switch with Orca running. Under WSL, a Windows Orca's
  userData is never written.
- Windows: idle-compact's relay runs claude in a ConPTY (see *The
  relay*), and its end-to-end tests pass on a Windows machine. Orca detection
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
