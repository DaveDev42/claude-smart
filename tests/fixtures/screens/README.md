# Screen check fixtures

Raw `vt100`-replayable byte streams captured from a real Claude Code session,
used by `src/screen_check.rs`'s tests to check `input_box`/`box_text`/
`compaction_started` against the real UI instead of hand-guessed ANSI.

Claude Code version: **2.1.283** (`~/.local/share/claude/versions/2.1.283
--version` -> `2.1.283 (Claude Code)`).

## How these were captured

A Python `pty.fork()` harness (based on an existing prompt-style probe in
this deployment's scratchpad) drove the real `claude` binary directly, one
short-lived session per scenario, answering the initial device-attributes
query and exiting itself (Ctrl-C x2, then SIGTERM/SIGKILL on its own pid,
never `pkill`/`killall`) within a few minutes. Every session ran with
`--model haiku --permission-mode default`, cwd `~/Projects/github.com/…`
(an already-trusted directory: no trust dialog appeared in any session), and
a stripped environment (`CLAUDE_CODE_*`, `CLAUDECODE`, `ORCA_*`, `ZELLIJ*`,
`WEZTERM*` unset; `CLAUDE_CONFIG_DIR` intentionally kept, since the point was
to capture the operator's real account/config). No prompt approved anything;
every dialog/picker/menu was answered with Esc only, except the one place the
task called for pressing Enter: submitting `/compact` itself.

For each scenario the harness recorded byte-offset "marks" into the raw pty
stream at the states of interest. Each shipped fixture here is the stream
**prefix up to one mark** (`bytes[:mark]`) from one of 7 capture sessions
(`basic-flow`, `busy-interrupt`, `ask-user-question`, `permission-prompt`,
`compact`, `history-model`, `size-80x24` — see `index.json`'s
`source_session`/`source_mark`). Replaying a prefix through
`vt100::Parser::new(rows, cols, 0)` reproduces the exact screen at that
moment, because vt100 terminal state is a pure fold over the byte stream from
the start.

## Sanitization

Every fixture byte stream was substring-replaced (case-sensitive, ASCII,
same byte length so the screen layout — every subsequent column position —
is unchanged) before being written here:

| found | replaced with |
|---|---|
| `Dave` / `dave` | `User` / `user` |
| the private account-profile name (see `forbidden()` in `tests/no_private_names.rs`) | `acctusr` |
| `MBP16` (host-name fragment) | `HostA` |

This clears every leak-guard `forbidden()` substring that appeared in a raw
capture (the operator's real home path only ever appeared already-abbreviated
as `~/…` by Claude Code's own UI, except inside one absolute scratchpad path
used for the permission-prompt/Write-tool test, which is what the `dave`→
`user` substitution above cleans up). All 21 shipped `.bin` files were
grepped afterward for every `forbidden()` string in `tests/no_private_names.rs`
plus an `@`-address-like pattern; the only remaining `@`-shaped match is the
already-sanitized `acctusr@HostA` statusline token itself, not a real
address. `tests/no_private_names.rs` now also scans this directory.

One capture is deliberately **not** shipped: a Ctrl-R history search
(`history-model` session) with scope "everywhere" rendered several lines of
real, unrelated prompt text from the operator's actual prior Claude Code
usage (including non-English text) pulled in from other sessions/projects.
That content can't be safely reduced to a known-word substitution list the
way a host/profile name can, so it was discarded rather than shipped even in
"sanitized" form. The `❯`-used-as-a-list-cursor trap that fixture would have
covered is already exercised by three other real dialogs below (AskUserQuestion,
the Write-tool permission prompt, and the `/model` picker), so coverage isn't
lost — see `after-esc-history-search-120x40` for the (clean) recovery screen
after cancelling that search.

Total fixture size: ~227 KiB across 21 files, well under the 2 MB budget.

## Vim mode indicator (what state 4 showed)

Real Claude Code (v2.1.283, vim keybindings on) draws its mode line as the
terminal's last row, together with a permission-mode marker (`⏸ manual mode
on` for `--permission-mode default`) and a `· ← for agents` hint. The whole
line is one composite, redrawn together with a trailing erase-to-end-of-line
on every mode change:

- **Insert**: `  -- INSERT -- ⏸ manual mode on`
- **Normal**: `  ⏸ manual mode on` — the `-- INSERT --` text is simply
  absent; there is no `-- NORMAL --` (or any other) marker.

This means Normal mode is not positively distinguishable from "vim mode is
off" using only this row's text — both would render identically (an absent
`-- INSERT --` prefix). `VimState::Normal` in `input_box`'s output is
therefore "the indicator row has content, but not `-- INSERT --`" (true for
every real capture here, since this account always has vim mode on), and
`VimState::Off` is reserved for "nothing at all is drawn on that row" — not
observed in a real capture in this deployment's normal configuration, but
kept as the conservative fallback and exercised by a synthetic test.

## Compaction (state 5): what we found

- Typing `/compact` opens Claude Code's slash-command menu above the box
  (filtered to `/compact`, `/autocompact`, `/computer-use`); the box itself
  shows plain (non-dim) `❯ /compact` — `Draft`, and `box_text` is exactly
  `"/compact"` with no trailing space or other autocomplete artifact.
- Pressing Enter ran `/compact` as typed: the box cleared and
  `✻ Compacting conversation… (1s)` appeared immediately above it. Since the
  typed text exactly matched the top-ranked/highlighted menu entry in this
  capture, this single trial can't fully distinguish "ran the literal typed
  text" from "ran the highlighted entry" — but the outcome the typing
  protocol's step 6 actually needs (compaction starts; nothing unexpected
  lands in the box) is confirmed either way.
- "In progress" looks like `✻`/`✽`/`✢` (a rotating spinner glyph)
  `Compacting conversation… (Ns [· ↓ Nk tokens])`, drawn in the same status
  row other busy states use (e.g. `✳ Contemplating…`, `✻ Bunning…`). The
  input box stays visible and `Empty` throughout — `input_box` alone can't
  tell "idle" from "busy compacting", which is why `compaction_started`
  exists as a separate check (`screen_check.rs` looks for the substring
  `"compacting"`, case-insensitive, anywhere on screen).
- Finished (~24s after Enter in this capture, well under the 120s cap): the
  `Compacting…` line disappears, context usage in the statusline resets to
  `0/167k`, and the box returns to plain idle `Empty`.
- The statusline's `[⏱ NNmNNs]` countdown updates every ~1-2s the whole time
  (even at idle), so a "buffer stopped growing" heuristic never fires during
  compaction — the harness just polled on a fixed ~2s cadence for up to 120s
  instead.

## Added for the relay wiring

Seven more captures from the same Claude Code 2.1.283 setup, taken in five
short sessions (same rules: `--model haiku --permission-mode default`, nothing
approved, each session killed by its own pid), sanitized with the same
same-width substitutions. `index.json` gained a `busy` field.

- `draft-multiline-120x40`: two lines pasted (bracketed paste), the box grows
  to two content rows. `Draft`.
- `draft-wrapped-120x40`: one long typed line soft-wrapped onto a second row.
  `Draft`.
- `draft-paste-placeholder-120x40`: a 58-line paste collapses to a
  `[Pasted text #1 +58 lines]` placeholder in normal text. `Draft`. The footer
  row reads `paste again to expand` instead of the mode line, so the vim
  state reads Normal; the classification does not depend on it.
- `busy-activity-line-120x40`: `✽ Fermenting… (3s · thinking)` above the
  box. There is no `esc to interrupt` text in 2.1.283; the timer in
  parentheses is the marker.
- `busy-streaming-title-only-120x40`: mid-reply with no activity line drawn
  yet.
- `compact-finished-vim-insert-120x40` and `compact-finished-vim-normal-120x40`:
  the idle box after `/compact` finished, before and after Esc.

### What the busy and menu rules stand on

- The window title (OSC 0) is `✳ <title>` while idle and `◐`/`◑ <title>`
  while a turn runs or compaction is going. The title is set even in frames
  where no activity line is on screen (`busy-generating-120x40` has none), so
  it is the primary busy signal; the activity line is the second.
  `interrupted-normal-120x40` was captured before the title went back to `✳`,
  so it counts as busy too.
- In the slash-command menu (`compact-menu-typed-120x40`) the selected row is
  drawn entirely in the accent colour (`38;2;177;185;249`), the others grey.
  Bold is not the selection: it marks the substring typed so far in every
  entry that contains it (`/autocompact` has `compact` in bold).

## Claude Code 2.1.296: all themes, sandbox API-key capture (2026-10-10)

38 captures (`theme-*`, 120x40) from Claude Code **2.1.296**, taken without the
operator's account or config. The real `claude` binary ran in a Python
`pty.fork()` harness with a stripped environment: `HOME` a sandbox dir,
`CLAUDE_CONFIG_DIR` unset, `ANTHROPIC_API_KEY=sk-ant-dummy-not-real`,
`ANTHROPIC_BASE_URL=http://127.0.0.1:9`, telemetry/error-reporting/autoupdater
off, `TERM=xterm-256color`, `COLORTERM=truecolor`. A pre-seeded sandbox
`.claude.json` (`hasCompletedOnboarding`, `theme`, the last 20 characters of the
dummy key under `customApiKeyResponses.approved`, the cwd's
`hasTrustDialogAccepted`) skipped every dialog, so nothing talked to a model
or the network. The harness answered the device-attributes query, never pressed
Enter on `/compact`, and exited with Ctrl-C twice and then its own pid.
Sanitization is the same-width substitution above (`dave`, the host name, the
session uuid in the cwd shown by the header).

Themes offered by the picker: `dark`, `light`, `dark-daltonized`,
`light-daltonized`, `dark-ansi`, `light-ansi` (plus `auto`, which follows the
terminal and was not captured). Per theme: `fresh-idle`, `comp-menu-typed`
(`/comp`, four entries), `comp-menu-down` (Down from there),
`compact-menu-typed` (two entries) and `compact-menu-down`. Also, dark theme
only: `background-shell-*` (`!sleep 300` run from bash mode, then Ctrl+B twice;
a single Ctrl+B only shows the hint; the footer reads `1 shell · ← for agents ·
↓ to manage`; idle, `/compact` typed, and Down) and `vim-*` (`editorMode: vim`:
insert, normal, `/compact` typed, and Esc, which only closes the menu and
leaves the draft in insert mode). `index.json` rows with a menu carry
`menu_entries` and `menu_highlight`.

What the captures showed:

- **The slash menu layout changed.** 2.1.296 draws the selected row as
  `  ❯ /compact` and the others as `    /autocompact`, so every `/` is at
  column 4. 2.1.287 drew `/` at column 2 with no pointer. The old rule only
  recognised column 2, found no entry, and reported `Menu::Absent`, which
  `enter_runs` reads as "Enter submits the typed text": it answered `true`
  even with `/autocompact` selected. `command_menu` now accepts both layouts;
  in the pointer layout the single row with the `❯` pointer is the selection
  and zero, several, or mixed-layout pointers leave it undecided (refuse).
- **The colour rule is theme specific and is not used for 2.1.296.** The
  selected row's accent is `177,185,249` (dark), `87,105,247` (light),
  `153,204,255` (dark-daltonized), `51,102,255` (light-daltonized), ANSI 12
  (dark-ansi) and ANSI 4 (light-ansi); the others are grey (`153,153,153`,
  `102,102,102`, ANSI 7, ANSI 8). The command typed in the box is now drawn in
  the default colour, so the old two-entry rule (take the accent from the box)
  could not decide either. The legacy colour rules stay for column-2 menus.
- With vim keys off, the footer row still has content without `-- INSERT --`,
  so those captures read as vim `Normal` (as the vim section above explains).
- A running foreground shell (`theme-dark-foreground-shell-running-120x40`)
  shows `esc to interrupt` in the footer (busy by the activity-line rule); a
  backgrounded one does not, and the idle box and the menu are classified as
  usual.
