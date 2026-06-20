---
name: release
description: Guide a claude-smart release. Explains/drives the release-please flow (conventional commits on main → release PR → merge tags vX.Y.Z → 4-target build matrix → assets + Homebrew tap bump), checks release readiness, and covers the crates.io publish caveat. Use when the user asks to "release", "cut a version", "publish", or "릴리스".
---

# /release — cutting a claude-smart release

Releases are driven by **release-please** + the `release-please.yml` workflow. The
human steps are small; most is automatic. Verify state, then guide the merge.

## How it works (don't fight it)

1. Conventional commits land on `main` (`feat:` → minor, `fix:` → patch;
   `feat!:`/`BREAKING CHANGE` → major once ≥1.0). Pre-1.0 bump rules live in
   `release-please-config.json` (`bump-patch-for-minor-pre-major: true`).
2. release-please maintains an open **release PR** titled like
   `chore(main): release X.Y.Z` that bumps `Cargo.toml`, `Cargo.lock`, and the
   CHANGELOG. It updates as more commits land.
3. **Merging that PR** is the release trigger: it tags `vX.Y.Z`, then the
   workflow builds 4 targets (linux-gnu via cross, aarch64/x86_64-darwin,
   x86_64-windows-msvc), packages tarballs/zip, generates `SHA256SUMS.txt`,
   uploads them to the GitHub release, flips it from draft to published, and bumps
   `Formula/claude-smart.rb` in `DaveDev42/homebrew-tap`.

So to release: ensure `main` is green and has the intended commits, then merge the
release PR. There is usually nothing to hand-edit.

## Procedure

1. **Pre-flight.** Run `/verify` (must PASS). Confirm `main` is clean and the
   commits you want are present:
   ```sh
   git status -sb
   git log --oneline origin/main..HEAD   # anything unpushed?
   ```
   If commits are unpushed and the user wants them released, they must be pushed
   first (push only with user consent — see CLAUDE.md).

2. **Find the release PR:**
   ```sh
   gh pr list --search "release" --state open
   ```
   - If present: show the user the version it proposes + the CHANGELOG diff
     (`gh pr view <n>`). The version is computed from the conventional-commit
     history — if it looks wrong, the fix is the commit messages, not the PR.
   - If absent: the last push may still be processing, or no releasable commits
     landed since the last release (`docs:`/`chore:`/`test:`/`ci:` alone don't
     bump). Check the Action run: `gh run list --workflow=release-please.yml`.

3. **Merge to release** (with user confirmation — this is outward-facing):
   ```sh
   gh pr merge <n> --squash
   ```
   Then watch the build/publish:
   ```sh
   gh run watch   # or: gh run list --workflow=release-please.yml
   ```
   Confirm the GitHub release published with all 4 assets + `SHA256SUMS.txt`, and
   that the tap formula bumped.

## crates.io (now in CI via Trusted Publishing)

The crate **is published** (`claude-smart` first went up as `0.1.1`, local
`cargo publish`). CI now carries a `publish-crate` job that authenticates via
**Trusted Publishing** (OIDC `id-token`, `rust-lang/crates-io-auth-action@v1` →
short-lived token, no static `CARGO_REGISTRY_TOKEN`). It runs after `build`
succeeds on a release and is version-guarded — if the Cargo.toml version is
already on the index it **skips** (so `workflow_dispatch` of an old tag won't 409).

So for a normal release you do **nothing** for crates.io — merging the release PR
bumps the version and the job publishes it. Just confirm afterward that the new
version appears on crates.io.

Manual publish is the fallback only (token issue, CI down, or a version CI
skipped). When doing it by hand, verify hygiene first:
```sh
cargo package --list   # must show LICENSE + README.md + src/; NO docs/ .github/
                       # IN_PROGRESS.md CLAUDE.md .claude/ (the Cargo.toml exclude)
cargo publish --dry-run   # isolated build, safe
cargo publish             # needs `cargo login` token; via `rustup run stable cargo …`
                          # if the cargo shim mis-parses the subcommand
```

> One-time setup already done: Trusted Publishing is registered in the crates.io
> UI (owner `DaveDev42`, repo `claude-smart`, workflow `release-please.yml`, no
> environment). If the publisher config is ever removed, re-add it there or the
> OIDC exchange fails.

## Don't

- Don't hand-edit `Cargo.toml` version / CHANGELOG / `.release-please-manifest.json`
  to force a version — let release-please own them. Correct the commit messages
  instead and let it recompute.
- Don't `git tag` manually — merging the release PR tags. (Re-running a build for
  an existing tag is `workflow_dispatch` with `tag_name`.)
- Don't put a crates.io token in CI or commit it anywhere.
