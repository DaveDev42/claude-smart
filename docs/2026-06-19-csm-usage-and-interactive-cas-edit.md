# csm usage (multi-profile metering) + interactive `profiles edit` + offline posture

Status: **IMPLEMENTED** (2026-06-19). Surface shipped as `csm usage` +
`csm profiles edit` (the human-facing noun-verb; `csm cas edit` kept as a
back-compat alias). Owner directives this cycle:
- *"여러 프로필의 사용량을 계측하는 기능 + 별도 명령어"* → `csm usage`. ✅
- *"profiles.json을 interactive하게 편집하는 기능"* → `csm profiles edit`. ✅
- *"shell script / workstation 서버 로직 간소화 + 오프라인 fallback"* → §4 (follow-on).

Implementation: `src/usage/report.rs` (pure `build_report` + `render_table` +
`render_json`, 11 unit tests), `src/cas/edit.rs` (pure `apply_edit_action` +
thin `run_interactive` I/O shell, 19 unit tests), `main::cmd_usage` +
`main::cmd_profiles`. Full CLI surface in
`docs/2026-06-19-csm-cli-surface-design.md`. mac E2E verified: profiles
lifecycle, `usage` (configured table w/ stale header + `--json`, unconfigured
disabled msg), interactive editor driven through a real pty (add/set-default/
rename + persistence), claude-subcommand forwarding (no collision).

## Verified ground truth

- `usage::fetch() -> Result<UsageData, FetchError>` already returns the **whole
  multi-profile blob** (hub `/cc-usage/api/data/limits`). `UsageData.profiles:
  HashMap<name, ProfileUsage>` with `session / week_all / week_sonnet: Option<{pct,
  resets}>` + `errors: Option<HashMap<name, String>>`. Everything `csm usage`
  needs is in one fetch — no per-profile network calls.
- Fetch already has 5 resilience layers (transport.rs): hub-local fast-path →
  positive TTL cache (60 s) → negative cooldown → HTTP → SSH fallback (unix).
  So **workstation offline** already degrades; the new command must *surface*
  the degraded state (stale age), not hide it.
- `ProfileMap` (account/profiles.rs) owns insert/remove/save/is_valid_name/
  default_name — the interactive editor is a thin loop over these, no new
  persistence code.
- Public-crate hygiene: no account names / hub identifiers compiled in. Both
  features inherit this (read names from the registry, hub from env).

## 1. `csm usage` — multi-profile usage table

Surface: `csm usage [--json] [--no-fetch]`

Human table (default), one row per **registry** profile (joined with hub data):
```
PROFILE    SESSION  WEEK(all)  WEEK(sonnet)  RESETS              STATUS
personal   12%      34%        8%            session 9pm (KST)   ok
work    87%      91%        —             week Jun 22         ⚠ near-limit
work       —        —          —             —                   ✖ errored: <msg>
```
- Rows come from `ProfileMap` (so a registered-but-never-used profile shows as
  `—`/unknown rather than silently missing). A hub profile NOT in the registry
  is appended with a `(unregistered)` tag — visibility over silent drop.
- STATUS: `ok` / `⚠ near-limit` (any pct ≥ a WARN threshold, reuse scoring
  consts) / `✖ errored` (in `errors` map) / `· no data`.
- Sorted by profile name (stable, matches `names_sorted()`).

`--json`: emit the joined view as JSON (registry ∪ hub), for statusline /
scripts. Shape: `{ "captured_at", "stale_secs", "profiles": { name: {registered,
session_pct, week_all_pct, week_sonnet_pct, resets, status, error} } }`.

### Offline / stale posture (owner's fallback point — first-class)

`csm usage` calls `fetch()`. On success it stamps freshness from the cache file
mtime and prints a header line when stale:
```
⚠ hub data is 7m old (workstation unreachable; showing last-known)
```
On hard `FetchError` with NO usable cache: print the registry table with all
usage columns `—` and a footer `usage metering unavailable (hub down, no cache)`
— the registry/default info is still useful offline. `--no-fetch` forces the
cache-only path (never touches network) for fast scripted reads.

When the hub env is unset (external user / toss): `fetch()` → disabled → print
`usage metering disabled (set CLAUDE_USAGE_URL + CLAUDE_HUB_HOSTNAME)` and still
show the registry (names + dirs + default). Core stays portable.

## 2. `csm cas edit` — interactive registry editor

Surface: `csm cas edit` (non-eval management verb; routed like list/add/use).

Interactive menu loop (cross-platform `std::io::stdin().read_line` — NO fzf
dependency, so it works identically on Windows-native where fzf may be absent):
```
csm cas edit — profiles.json  (3 profiles, default: personal)
  1) personal   /Users/example/.claude.personal     [default]
  2) work    /Users/example/.claude.work
  3) work       /Users/example/.claude.work
Actions: [a]dd  [e]dit-dir  [r]ename  [d]elete  [*]set-default  [q]uit
> 
```
- **add**: prompt name (validated `is_valid_name`, reject dup) → prompt dir
  (blank = synth `~/.claude.<name>`) → mkdir + insert.
- **edit-dir**: pick # → prompt new dir → insert (overwrite).
- **rename**: pick # → prompt new name (validated) → remove old + insert new;
  if it was the default, rewrite `default` to the new name.
- **delete**: pick # → refuse if it is the current default (must set-default
  elsewhere first — same rule as `cas remove`); else remove (dir retained on disk).
- **set-default**: pick # → write `default` state file + apply floor (reuses
  the `use` path: `write_default_profile` + `platform::apply_global`).
- **quit**: save once if dirty, print summary.

TTY gate: if `!isatty(0) || !isatty(1)`, bail with
`csm cas edit: requires an interactive terminal (use add/set/remove/use for scripting)`.
Every mutating step persists immediately via `ProfileMap::save` (crash-safe; the
loop re-reads its in-memory map), so a mid-session Ctrl-C never corrupts.

Testability: the loop logic is split into a pure `apply_edit_action(&mut
ProfileMap, Action) -> Outcome` core (unit-tested with scripted Action sequences)
+ a thin `read_action()` I/O shell (the only untested part — keep it trivial).

## 3. Wiring

- `parse_cas_op`: add `Some("edit") => Op::Edit` BEFORE the catch-all. `Op::Edit`
  is a management op (non-eval) routed to `manage_emit`.
- `manage_emit`: `Op::Edit => cas::edit::run_interactive(profiles)`.
- New module `src/cas/edit.rs` (pure core + I/O shell + tests).
- New `cmd_usage` in main.rs + dispatch-table entry `"usage"`; new module
  `src/usage/report.rs` (pure join+format core + tests, formatting separated
  from fetch so it unit-tests against `UsageData` fixtures).
- `cli/completions.rs`: add `usage` subcommand + `edit` to the cas verb doc.
- `tests/no_private_names.rs` continues to gate both new modules.

## 4. Follow-on simplification (owner's observation — NOT this cycle)

As csm absorbs metering + registry editing, dave-environment can retire:
- `shared/claude/claude-smart-helper.sh.j2` (the legacy zsh usage-fetch helper —
  csm's `fetch()` + `usage`/`current-usage` supersede it).
- Per-profile collect/keepalive shell glue once `csm usage --json` can feed the
  dashboard directly. The hub-side **collection** (`shared/claude-code-usage/`,
  ccusage-rs) stays — that's the data SOURCE; only the client-side shell glue
  collapses. Tracked separately; needs its own audit + migration so the live
  dashboard never loses a data point. Listed here so it isn't forgotten.

## 5. Tests

`usage::report` — fixtures: all-ok, near-limit, errored profile, unregistered
hub profile, registered-but-no-data, empty registry, stale (age header), json
shape stable. `cas::edit::apply_edit_action` — add/dup-reject/invalid-name/
edit-dir/rename(+default-follow)/delete-default-refuse/delete-ok/set-default.
Both gated by `no_private_names.rs`.
