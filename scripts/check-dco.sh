#!/usr/bin/env bash
# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
#
# Developer Certificate of Origin check (charter rule 17).
#
# Provenance is standing: the project can only enforce its licence, and can only
# amend the §7 exception, if it can show where every line came from. Every
# commit carries a Signed-off-by trailer matching its author.
#
# Scoped to the commits under review rather than all of history, deliberately.
# A repository's root commit predates the policy, and retroactively failing it
# would mean the check could never pass.
#
# Usage:
#   scripts/check-dco.sh              # commits on HEAD not on origin/main
#   scripts/check-dco.sh <base>..<head>

set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$#" -ge 1 ] && [ -n "$1" ]; then
    range="$1"
elif git rev-parse --verify --quiet origin/main >/dev/null; then
    range="origin/main..HEAD"
else
    range="HEAD~1..HEAD"
fi

commits=$(git rev-list --no-merges "$range" 2>/dev/null || true)

if [ -z "$commits" ]; then
    echo "DCO: OK (no non-merge commits in range '$range')"
    exit 0
fi

status=0
count=0

for sha in $commits; do
    count=$((count + 1))
    author_name=$(git show -s --format='%an' "$sha")
    author_email=$(git show -s --format='%ae' "$sha")
    expected="Signed-off-by: ${author_name} <${author_email}>"

    # Case-insensitive on the address: git preserves the case a contributor
    # typed, but email addresses are not case-sensitive in the part that
    # matters, and failing a build over `Name@host` vs `name@host` is noise.
    if git show -s --format='%B' "$sha" \
        | grep -qiF "$expected"; then
        continue
    fi

    status=1
    echo "MISSING OR MISMATCHED DCO SIGN-OFF"
    echo "  commit:   $(git show -s --format='%h %s' "$sha")"
    echo "  author:   ${author_name} <${author_email}>"
    echo "  expected: $expected"
    found=$(git show -s --format='%B' "$sha" | grep -i '^Signed-off-by:' || true)
    if [ -n "$found" ]; then
        echo "  found:    $found"
        echo "            (the trailer must match the commit author exactly)"
    else
        echo "  found:    no Signed-off-by trailer"
    fi
    echo
done

if [ "$status" -ne 0 ]; then
    cat <<'EOF'
  Fix by amending or rebasing with sign-off:

      git commit --amend -s --no-edit          # last commit
      git rebase --signoff origin/main         # a whole branch

  To get sign-off automatically on every commit, run once after cloning:

      git config core.hooksPath .githooks

  See CONTRIBUTING.md for the DCO text you are certifying to.
EOF
else
    echo "DCO: OK ($count commit(s) in range '$range' signed off)"
fi

exit "$status"
