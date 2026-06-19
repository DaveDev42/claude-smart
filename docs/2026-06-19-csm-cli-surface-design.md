# csm CLI surface — complete design (collision-safe with `claude`)

Status: **IMPLEMENTED + mac-E2E-verified** (2026-06-19). Owner directives: clean
noun-verb CLI for the WHOLE csm surface; `csm profiles edit` style; MUST NOT
collide with the `claude` CLI's subcommands/flags; implement a perfect CLI and
test real operation.

Verification (mac, sandboxed `$HOME`): `csm mcp|doctor|update|agents|auth|
plugin|project` all forwarded verbatim to `claude` (NOT hijacked) — collision
avoidance proven. `csm profiles list/add/set/use/rm/dir/edit` + `csm usage
[--json|--no-fetch]` all exercised end-to-end (editor via a real pty). 544 unit
tests + leak guard green, clippy clean.

## 1. Collision model (the load-bearing constraint)

csm is a multi-call launcher: `csm [flags] [prompt]` with no recognized
subcommand is an **implicit `csm run`**, which forwards everything to `claude`.
The dispatcher (main.rs) matches a reserved word ONLY at `args[1]`; any other
first token → `csm run` → `claude`.

Therefore the rule is absolute: **csm's reserved subcommand words MUST be
disjoint from `claude`'s subcommand set**, or a user typing `csm <word>`
expecting `claude <word>` would be hijacked.

`claude` subcommands (v2.1.183, ground-truth from `claude --help`):
`agents, auth, auto-mode, doctor, install, mcp, plugin|plugins, project,
setup-token, ultrareview, update|upgrade`.

`claude` short flags: `-c -d -h -n -p -r -v -w` (and `-V`? no — claude uses
`-v/--version`). csm intercepts `--version/-V` and `--help/-h` ONLY at args[1];
to pass those to claude use `csm run -- --version`.

### Reserved words (final) — verified disjoint from claude

`run, hook, profiles, usage, pick-account, scan, sidecar, statusline,
completions, newuuid` + back-comat alias `cas`, `current-usage`.

Cross-check vs claude subcommands: NONE of {run, hook, profiles, usage,
pick-account, scan, sidecar, statusline, completions, newuuid, cas,
current-usage} appears in claude's set. ✓ No collision.

`csm run` flags (consumed before `--`, then verbatim passthru): the launcher
flags are a DELIBERATELY SMALL set chosen to not shadow claude flags a user
commonly passes. csm consumes `-i/--interactive, -n/--new, -c/--continue,
-A/--pick-account, --no-pick, -r/--resume, --permission-mode, --effort, --model,
--session-id, --profile`. Of these, `-c/-n/-r` overlap claude's `-c/-n/-r` BY
DESIGN — csm interprets them (smart resume/continue/new) and re-emits the right
claude flags. A user who wants the raw claude flag uses `csm run -- -c`. `--`
always stops csm parsing. This is unchanged and already shipped; documented here
for completeness.

## 2. Final subcommand surface (noun-verb)

```
csm                              implicit `csm run` (bare = smart launch)
csm run     [flags] [-- …]       smart launcher (session select + account + relaunch)
csm hook    [--owner <dir>]      Stop/SubagentStop/SessionEnd hook (stdin JSON)

csm profiles                     alias for `csm profiles list`
csm profiles list                registry listing (+ current/default markers)
csm profiles add  <name> [<dir>] register a profile (dir default ~/.claude.<name>; mkdir)
csm profiles set  <name> <dir>   register/overwrite a profile dir
csm profiles rm   <name>         unregister (refuse if it is the default)
csm profiles use  <name>         set machine default + floor (no per-shell export)
csm profiles edit                interactive editor (TTY) — add/edit/rename/delete/default
csm profiles dir  [<name>]       print a profile's dir (default profile if omitted)

csm usage   [--json] [--no-fetch]  multi-profile usage table (+offline stale degrade)

csm pick-account [<current>] [--include-current]   scoring → winner profile name
csm scan    [<cwd>]              session TSV for the picker
csm sidecar {read|write|merge|flags} <sid> [k=v…]   session sidecar store
csm statusline                   `<profile>@<host>` for the shell prompt
csm completions {zsh|bash|pwsh}  shell completions
csm newuuid                      fresh lowercase UUID v4

# eval-class (shell shim contract — emit a line the shim evals; not for humans)
csm cas --eval --shell {zsh|pwsh} -- <profile>|-|-g <p>|resync|status
csm cas --print-default-dir      floor SSOT (used by zshenv / launchd / pwsh guard)
```

### `cas` → `profiles` consolidation

The human-facing management verbs move under the `profiles` NOUN (cleaner, the
owner's call). `cas` is RETAINED as the eval-class entry point (the shim contract
`csm cas --eval …` and `csm cas --print-default-dir` are unchanged — they are
machine interfaces, renaming them would churn the shims for no user benefit) AND
as a back-compat alias so `csm cas list/add/use/...` still work. Internally both
route to the same `Op` handlers. So:
- Humans type `csm profiles <verb>` (and the shell `cas` function still wraps
  `csm cas --eval` for switching — unchanged).
- `csm cas <management-verb>` keeps working (alias) → no breakage.

`current-usage <profile>` is kept as a hidden back-compat alias of the scriptable
single-profile path (the hook/statusline glue may call it); `csm usage --json`
is the new general surface.

## 3. Dispatch changes (main.rs)

Add to the args[1] match: `"profiles"`, `"usage"`. Keep `"cas"`,
`"current-usage"` (back-compat). New routing:
- `profiles` → `cmd_profiles(rest)` — parses the sub-verb (list/add/set/rm/use/
  edit/dir) and routes into the SAME `cas::manage_emit` / `ProfileMap` paths the
  `cas` management verbs already use (no duplicate logic). `edit` → interactive.
- `usage` → `cmd_usage(rest)` — `usage::report`.
- `--help` text rewritten to the noun-verb surface; `csm profiles --help` and
  `csm usage --help` give per-noun help.

## 4. Help & completions

`completions.rs` clap mirror updated: add `profiles` (with verb subcommands via a
nested enum) + `usage`. `cas` stays (eval + print-default-dir + back-compat
verbs). Help strings reflect noun-verb. `tests/no_private_names.rs` gates all new
code.

## 5. Non-goals / invariants

- No new claude-flag interception. The passthru boundary (`--`) is unchanged.
- No renaming of the eval-class `cas` machine interface (shim stability).
- Public-crate hygiene preserved (names from registry, hub from env).
- Backward compatible: every existing `csm cas …` invocation still works.
