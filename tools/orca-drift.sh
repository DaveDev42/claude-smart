#!/usr/bin/env bash
# tools/orca-drift.sh -- show what changed in the Orca files csm mirrors.
#
#   bash tools/orca-drift.sh [--repo <dir>] [--stat] [--tests] <old-tag> <new-tag>
#
# csm reads and writes Orca's account state without Orca's help when Orca is
# not running (src/orca/), so it carries a port of Orca's account code: the
# store layout, the stash format, the Keychain encoding, the runtime RPC
# methods and the runtime metadata file. Run this whenever a new Orca release
# lands, with the last verified tag (see README.md, "Verified Orca versions")
# and the new one, and read the diff before raising the verified range.
#
# The Orca repository is cloned once, blobless, into
# ${XDG_CACHE_HOME:-~/.cache}/csm/orca-src and reused after that; missing
# tags are fetched. --repo points at an existing clone instead (it must
# already hold both tags; the script does not fetch into a clone it did not
# create). --stat prints the per-file summary only. Test files are left out
# unless --tests is given.
#
# The script only reads: it never checks out a work tree and never touches
# csm's state, Orca's userData or the Keychain.
set -eu

REPO=""
STAT=0
TESTS=0
TAGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="$2"; shift 2 ;;
    --stat) STAT=1; shift ;;
    --tests) TESTS=1; shift ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    -*) echo "orca-drift: unknown flag $1" >&2; exit 2 ;;
    *) TAGS+=("$1"); shift ;;
  esac
done
if [ "${#TAGS[@]}" -ne 2 ]; then
  echo "usage: bash tools/orca-drift.sh [--repo <dir>] [--stat] [--tests] <old-tag> <new-tag>" >&2
  exit 2
fi
OLD=${TAGS[0]}
NEW=${TAGS[1]}
URL=https://github.com/stablyai/orca.git

# ─── the files csm mirrors ─────────────────────────────────────────────────────
# Keep this list in step with src/orca/. Each entry names what csm ports
# from it.
PATHS=(
  # Managed accounts: stash layout, selection, registration, the runtime
  # auth snapshot/restore/readback protocol, OAuth refresh, the live-PTY
  # gate (Orca's registry of running claude panes).
  src/main/claude-accounts/
  # Keychain encoding (service names, /usr/bin/security calls, timeouts).
  src/main/macos-keychain/generic-password.ts
  # The store: userData resolution, the profile index and profile paths,
  # orca-data.json load/write, the settings keys csm reads.
  src/main/persistence.ts
  src/main/persistence/loading-store/store.ts
  src/main/persistence/loading-store/user-data-path.ts
  src/main/persistence/profile-state/profile-state-active-location.ts
  src/main/orca-profiles/profile-index-store.ts
  src/main/orca-profiles/profile-storage-paths.ts
  src/shared/orca-profiles.ts
  src/shared/global-settings-types.ts
  src/shared/default-global-settings.ts
  # Runtime RPC: the accounts.* handlers and the controller behind them.
  src/main/runtime/rpc/methods/accounts.ts
  src/main/runtime/runtime-account-controller.ts
  src/main/startup/main-process-account-services.ts
  # Runtime metadata (orca-runtime.json) and the liveness signals csm reads.
  src/main/runtime/runtime-metadata.ts
  src/main/runtime/runtime-metadata-ownership-watch.ts
  src/main/runtime/runtime-rpc/runtime-rpc-lifecycle.ts
  src/main/startup/single-instance-lock.ts
)
EXCLUDE=()
if [ "$TESTS" = 0 ]; then
  EXCLUDE=(':(exclude,glob)**/*.test.ts' ':(exclude,glob)**/*-test-harness.ts' ':(exclude,glob)**/__tests__/**' ':(exclude,glob)**/__fixtures__/**')
fi

# ─── the clone ─────────────────────────────────────────────────────────────────

has_tag() { git -C "$REPO" rev-parse -q --verify "refs/tags/$1^{commit}" >/dev/null 2>&1; }

if [ -n "$REPO" ]; then
  git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1 || { echo "orca-drift: $REPO is not a git repository" >&2; exit 1; }
  for t in "$OLD" "$NEW"; do
    has_tag "$t" || { echo "orca-drift: $REPO has no tag $t (fetch it there, or drop --repo)" >&2; exit 1; }
  done
else
  REPO="${XDG_CACHE_HOME:-$HOME/.cache}/csm/orca-src"
  if [ ! -d "$REPO" ]; then
    mkdir -p "$(dirname "$REPO")"
    echo "orca-drift: cloning $URL into $REPO" >&2
    git clone -q --bare --filter=blob:none "$URL" "$REPO"
  fi
  for t in "$OLD" "$NEW"; do
    if ! has_tag "$t"; then
      echo "orca-drift: fetching $t" >&2
      git -C "$REPO" fetch -q --filter=blob:none origin "refs/tags/$t:refs/tags/$t"
    fi
    has_tag "$t" || { echo "orca-drift: no tag $t upstream" >&2; exit 1; }
  done
fi

# ─── report ────────────────────────────────────────────────────────────────────

echo "# Orca account-surface drift: $OLD -> $NEW"
# A path csm mirrors that is gone at the new tag was renamed or removed; the
# port needs a new source for it either way.
for p in "${PATHS[@]}"; do
  if ! git -C "$REPO" cat-file -e "$NEW:${p%/}" 2>/dev/null; then
    echo "# MISSING at $NEW: $p"
  fi
done
echo
if [ "$STAT" = 1 ]; then
  git -C "$REPO" --no-pager diff --stat=120 "$OLD" "$NEW" -- "${PATHS[@]}" "${EXCLUDE[@]}"
else
  git -C "$REPO" --no-pager diff --stat=120 --patch "$OLD" "$NEW" -- "${PATHS[@]}" "${EXCLUDE[@]}"
fi
