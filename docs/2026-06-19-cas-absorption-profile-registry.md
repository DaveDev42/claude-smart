# csm: Absorb CAS, user-configurable profile registry, own per-profile CC dirs

Status: design locked (judge panel + synthesis, 2026-06-19). Implements the
owner directive: *"CSM이 CAS의 기능까지 아예 흡수하고, 프로필 별 claude code
디렉터리를 설정하는 등의 기능도 완결성 있게 가져가는게 맞겠어"* — csm fully
absorbs the claude-as (CAS) logic, makes profiles user-configurable, and owns
per-profile Claude Code directory management self-containedly. Also closes the
last public-crate leak: the hardcoded `personal|work` allowlist.

## Verified ground truth

- The **only remaining leak** is the `personal|work` allowlist in
  `src/cas/mod.rs` (`default_profile`, `write_default_profile`,
  `is_valid_profile`). The hub-hostname/URL leak is **already fixed** (commit
  `3d5372d`): `usage::transport::hub_hostname()` reads `CLAUDE_HUB_HOSTNAME`,
  `resolve_usage_url()` reads `CLAUDE_USAGE_URL`, `main::is_hub_local_machine()`
  delegates to the former. No machine name compiled in. **No transport work in scope.**
- `eval_mode` in `cmd_cas` already gates non-eval mode to `Op::Status` only —
  the seam management verbs route through.
- `Op::Minus` (`cas -`) prev-tracking is irreducibly shell-side (binary cannot
  read the non-exported `_CLAUDE_AS_PREV`). Stays in the shim.
- `resolve_profile` / `resolve_profile_dir` already synthesize `~/.claude.<name>`
  on an empty map — generic, no leak. The toss/external-user path. **Kept.**

## 1. Scope

**claude-smart:**
- `src/account/profiles.rs`: add `contains`, `preferred_default`, `default_name`,
  `default_dir`, `is_valid_name`, `insert`, `remove`, `save` (atomic, sorted).
  Add a `default_name_with(&Path)` inner seam for tests.
- `src/cas/mod.rs`: thread `&ProfileMap` into `default_profile`/`write_default_profile`;
  **delete** free `is_valid_profile` (→ `ProfileMap::contains`); add
  `Op::{List,Add,Set,Remove,SetDefault}` + non-eval `manage_emit`; add
  `--print-default-dir`.
- `src/main.rs`: thread `&ProfileMap` into `current_profile_dir`/
  `derive_current_profile_name`/`cmd_hook`; extend `parse_cas_op` with verbs;
  route non-eval management ops; `--print-default-dir` dispatch.
- `src/cli/completions.rs`: add verbs to the `Cas` clap doc subcommand.
- `tests/no_private_names.rs`: CI grep guard (the load-bearing leak-killer).

**Not in scope:** no transport/hub code change; no profiles.json schema change
(stays flat `{name:dir}`); no `rename`/`--purge-dir`/`init` wizard/`doctor`
(cut — destructive/sprawl, not in legacy CAS); `cas -` stays shell-side.

## 2. Leak fix (exact)

`ProfileMap` becomes the validity/default authority:
- `contains(name) -> bool`
- `preferred_default() -> Option<String>` — sorted-first (deterministic;
  documented alphabetical tie-break, e.g. `work` < `personal`). The
  Ansible-seeded `default` file pins intent so this only fires on corruption.
- `default_name() -> String` — state-file token if (empty-map: any non-empty
  token | populated-map: a configured name), else `preferred_default()`, else "".
- `default_dir() -> PathBuf` — `get(default_name)` else synth `~/.claude.<name>`.
- `is_valid_name(name) -> bool` — `[A-Za-z0-9._-]+`, no path separators.
- `insert`/`remove`/`save` (tmp+rename in the SAME dir; `serde_json::to_string_pretty`
  over a `BTreeMap` for sorted-key idempotent Ansible diffing; trailing newline).

`cas::default_profile(profiles) -> String` delegates to `profiles.default_name()`.
`cas::is_valid_profile` deleted. `cas::write_default_profile(profile, profiles)`:
empty-map accepts any non-empty token, populated-map requires `contains`; error
message lists `profiles.names_sorted()` ("configured: …"), never literal names.
`resolve_profile` unchanged. All callers thread `&ProfileMap`.

## 3. Profile registry

Schema: flat `{ "<name>": "<absolute_dir>" }` at `~/.config/claude-as/profiles.json`.
Global default NAME stays in the separate `~/.config/claude-as/default` file
(read by shell floors without JSON parsing). No self-seed on launch (would flip
toss `is_empty()` silently). Authored by csm verbs + dave-environment Ansible.

New subcommands (under `cas`, non-eval; reserved verb words):

| Command | Writes | mkdir |
|---|---|---|
| `csm cas list` | — | — |
| `csm cas add <name> [<dir>]` | profiles.json insert (dir default `~/.claude.<name>`) | yes |
| `csm cas set <name> <dir>` | profiles.json insert/overwrite | yes |
| `csm cas remove\|rm <name>` | profiles.json remove (refuse if it is the default) | no |
| `csm cas use <name>` | `default` file + platform floor (`apply_global`) | no |
| `csm cas --print-default-dir` | — (prints `default_dir()`) | — |

`cas use` sets the machine default + floor (launchctl/HKCU; WSL no-op) but emits
NO per-shell export — distinct from `cas -g` (live shell + global). Documented in `--help`.

Routing: the shim's `case`/`switch` on `$1` sends management verbs + `status`
**direct** (non-eval → `manage_emit`); switch/`-g`/`resync`/`-` via `--eval`.
Verb/profile-name collision eliminated structurally (reserved words, same
precedent as `status`/`resync`). `manage_emit` is separate from `eval_emit` so
the one-line eval contract stays intact.

## 4. Parity

All 5 environments (MBP16/MacMini/WSL zsh, Windows-native pwsh, toss) run the
**same binary** resolving through the **same ProfileMap + `default` file**.
Platform divergence isolated to `cas/platform.rs::apply_global` (already
implemented mac/win/other-unix). Shim owns only: `eval` of the export line +
`cas -` prev-tracking (~12 lines/shell — the OS-mandated minimum). toss-disable:
no profiles.json → empty map → picker early-returns on `is_empty()`; `cas <name>`
synthesizes. No regression, no leak.

## 5. dave-environment

- `inventory/group_vars/all.yml`: `claude_profiles: {personal: ".claude.personal",
  work: ".claude.work"}` + `claude_default_profile: personal` (private repo —
  names live here legitimately).
- `shared/claude/playbook.yml`: POSIX `copy` + win-self `win_copy` render
  `profiles.json` (sorted `to_nice_json`); seed `default` with `force: no`
  (preserve live `cas use` writes). `when: is_personal_machine`.
- `shared/zsh/claude-as.zsh`: replace `CLAUDE_PROFILES` + body with the dispatcher
  shim (verb `case` + `-` toggle). Keep `claude-sync()`.
- `shared/zsh/zshenv.j2` + `shared/claude/claude-config-dir-setenv.sh.j2`: replace
  `case personal|work` floor with `csm cas --print-default-dir` (guarded by
  `command -v csm`). No private names.
- `shared/claude/tasks/cleanup-claude-home.yml`: `in claude_profiles.keys()`.
- `windows/powershell-profile/*.ps1.j2`: pwsh dispatcher shim (both editions).
- `shared/gw/`: confirm `gw_ai_command: csm` (flip Windows after BLOCKING checks).
- Audit-only: confirm `CLAUDE_USAGE_URL` + `CLAUDE_HUB_HOSTNAME` in
  `settings*.json.j2` env under `is_personal_machine` (reuse `usage_http_default_url`).

## 6. Tests

Rewrite the allowlist-coupled cas tests to `ProfileMap` fixtures via the
`default_name_with(&Path)` seam (temp state file + `test_profiles()`): configured
token → returns it; unknown/empty → `preferred_default()` (assert `work` for
the 2-profile fixture, documenting the tie-break). `is_valid_profile_allows_exactly_two`
→ `contains` tests. `write_default_profile` error asserts "configured:" not literal
names; empty-map accepts any token. New tests: `preferred_default_*`,
`default_name_trusts_token_on_empty_map`, `insert`/`remove`, `save`+`load`
roundtrip, verb parsing, `add` synth dir, `remove` refuses default, `--print-default-dir`.
`tests/no_private_names.rs` greps non-test src for `"personal"|"work"` and fails.

## 7. Risks + migration order

Risks: `--print-default-dir` on shell hot path (~1-2ms, `command -v csm` guarded);
`preferred_default` is alphabetical-first not `"personal"` (seeded `default` pins
intent); `cas use` sets floor (no drift) but omits export; atomic `save` tmp must
be same-dir; toss `cas add` is explicit-user-only.

Migration (never break live mid-rollout, each step revertible):
1. Ship binary first (allowlist removal + verbs + `--print-default-dir`); install
   on all 4. Backward-compatible: no profiles.json → synth == today.
2. Author profiles.json + default via Ansible (behavior-preserving; dirs match synth).
   Verify `csm cas list` + `--print-default-dir` on each.
3. Swap shims + de-hardcode floors (depend on step 1+2).
4. New shells pick up shims; running sessions keep captured paths.
