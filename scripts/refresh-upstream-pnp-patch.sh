#!/usr/bin/env bash
set -euo pipefail

# Regenerates patches/upstream-pnp.patch from microsoft/TypeScript#63919
# (Yarn PnP support, authored upstream by GGomez99).
#
# We do not carry this code as our own — we vendor the PR mechanically so
# that if it ever merges, this patch simply disappears, and if it moves,
# refetching it is a script run rather than a hand-rebase.
#
# The base commit for the diff is the merge-base of upstream `main` and the
# PR's head, i.e. the commit the PR actually targets — NOT main's tip. A PR
# branched N commits ago diffs cleanly only against the commit it branched
# from; diffing against main's current tip would pull in every main commit
# the PR's own branch doesn't have, which git would present as spurious
# conflict-shaped hunks in exactly the files upstream and the PR both touch.
# Those hunks are not something we want to own, which is the whole reason
# this patch is generated instead of hand-maintained. So the base is derived
# fresh each run, never hard-coded: hard-coding it would silently drift the
# moment the PR is rebased and turn "the base" into folklore nobody re-checks.
#
# Usage: scripts/refresh-upstream-pnp-patch.sh

readonly PR_NUMBER=63919
readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly SUBMODULE_DIR="${REPO_ROOT}/vendor/typescript"
readonly PATCH_FILE="${REPO_ROOT}/patches/upstream-pnp.patch"
readonly TEMP_REF="refresh-upstream-pnp-tmp"

err() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

[ -d "${SUBMODULE_DIR}/.git" ] || [ -f "${SUBMODULE_DIR}/.git" ] \
    || err "vendor/typescript is not a submodule checkout (run 'git submodule update --init' first)."

sub() {
    git -C "${SUBMODULE_DIR}" "$@"
}

# Refuse to touch a dirty submodule worktree: we're about to move HEAD around
# to verify the patch, and a dirty tree makes "restore what was there"
# ambiguous — better to stop than to guess what the caller wanted kept.
if [ -n "$(sub status --porcelain)" ]; then
    err "vendor/typescript has uncommitted changes. Commit, discard, or stash them (outside this repo's normal workflow, which avoids 'git stash') before refreshing the patch."
fi

original_head="$(sub rev-parse HEAD)"

# Leftover from a previous run that died before cleanup — safe to discard,
# it only ever points at a fetched PR head we can refetch.
sub branch -D "${TEMP_REF}" >/dev/null 2>&1 || true

cleanup() {
    sub checkout --quiet --detach "${original_head}" >/dev/null 2>&1 || true
    sub branch -D "${TEMP_REF}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "Fetching PR #${PR_NUMBER} head..." >&2
sub fetch --depth 200 origin "+refs/pull/${PR_NUMBER}/head:${TEMP_REF}"

# Find the merge-base of origin/main and the PR head. Both sides start out
# shallow (this submodule is deliberately shallow — it's a large repository),
# so the common ancestor may sit outside what's fetched yet. Deepen both
# sides together and retry until it resolves, rather than guessing a depth
# up front.
depth=200
base=""
while [ -z "${base}" ]; do
    sub fetch --depth "${depth}" origin main
    sub fetch --depth "${depth}" origin "+refs/pull/${PR_NUMBER}/head:${TEMP_REF}"
    if base="$(sub merge-base origin/main "${TEMP_REF}" 2>/dev/null)"; then
        break
    fi
    base=""
    depth=$((depth * 2))
    if [ "${depth}" -gt 12800 ]; then
        err "could not find a merge-base between origin/main and PR #${PR_NUMBER} within ${depth} commits of history. Investigate manually rather than trusting a deeper guess."
    fi
    echo "No common ancestor within current history yet; deepening to ${depth} commits..." >&2
done

echo "Generating diff ${base}..${TEMP_REF}..." >&2
sub diff "${base}..${TEMP_REF}" > "${PATCH_FILE}"

# Verify the patch applies with zero conflicts at the commit it was diffed
# from. This requires checking that commit out; we restore original_head
# afterward via the trap regardless of the outcome below.
sub checkout --quiet --detach "${base}"
sub clean -fdq
if sub apply --check "${PATCH_FILE}"; then
    apply_result="applies cleanly"
else
    apply_result="DOES NOT APPLY CLEANLY"
fi
sub checkout --quiet .
sub clean -fdq

cat <<EOF >&2

================================================================
Derived base commit: ${base}
  (merge-base of origin/main and PR #${PR_NUMBER}'s head)
Patch verification: ${apply_result}
================================================================

If this base differs from vendor/typescript's current pin
(currently ${original_head}), the submodule pin must move to match:

  git -C vendor/typescript fetch --depth 200 origin ${base}
  git -C vendor/typescript checkout --detach ${base}
  git add vendor/typescript

That is a human decision, not something this script does for you —
moving the pin can shift what patches/luchta.patch needs to apply on
top of, and that deserves a look before it's committed.
EOF

if [ "${apply_result}" != "applies cleanly" ]; then
    err "Generated patch does not apply cleanly at ${base} -- do not trust it. This usually means the base commit above is stale relative to what's already checked out; move the pin (see above) and rerun."
fi
