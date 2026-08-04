#!/usr/bin/env bash
# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
#
# SPDX header check (charter rule 17).
#
# Enforcement hygiene, not bureaucracy: standing to enforce a licence depends on
# being able to show what is covered by it and who holds the copyright. Every
# source file says so in machine-readable form.
#
# Two rules:
#
#   1. Every source file must carry both an SPDX-FileCopyrightText and an
#      SPDX-License-Identifier header.
#   2. ANY file carrying an SPDX-License-Identifier must carry the right one for
#      where it lives: MIT under api/, GPL-3.0-only everywhere else. This rule
#      covers documentation too, so a GPL header cannot be copy-pasted into an
#      MIT file (or the reverse) without CI noticing.
#
# Usage: scripts/check-spdx.sh

set -euo pipefail

cd "$(dirname "$0")/.."

COPYRIGHT_HOLDER='Iridesium'

# Extensions that must carry headers. Markdown is intentionally absent — docs
# may carry a header and rule 2 validates it if present, but they are not
# required to.
REQUIRED_EXTENSIONS='\.(rs|lua|sh)$'

expected_license_for() {
    case "$1" in
        api/*) echo 'MIT' ;;
        *)     echo 'GPL-3.0-only' ;;
    esac
}

# Vendored third-party files, which carry SOMEBODY ELSE'S licence.
#
# Rule 2 would otherwise make it impossible to vendor anything correctly: a
# bundled BSD font's own LICENSE declares BSD-3-Clause, which is exactly right
# and exactly what the rule rejects. The convention is a `third-party/`
# directory component, so the exemption is visible in the path rather than
# hidden in this script as a list of blessed files.
#
# These files still need to be licence-compatible — that is `cargo deny`'s job
# for crates and a human's for vendored assets, and each such directory carries
# a README saying what it is and why it is allowed to be here.
is_third_party() {
    printf '%s' "$1" | grep -q '\(^\|/\)third-party/'
}

missing=()
wrong_license=()
missing_copyright=()

# Only check tracked files: build artefacts under target/ and vendored
# dependencies are not ours to annotate.
while IFS= read -r file; do
    [ -f "$file" ] || continue

    expected=$(expected_license_for "$file")
    declared=$(grep -m1 -o 'SPDX-License-Identifier:[[:space:]]*[A-Za-z0-9.+-]*' "$file" 2>/dev/null \
        | sed 's/.*SPDX-License-Identifier:[[:space:]]*//' || true)

    if ! is_third_party "$file" && printf '%s' "$file" | grep -qE "$REQUIRED_EXTENSIONS"; then
        if [ -z "$declared" ]; then
            missing+=("$file (expected $expected)")
            continue
        fi
        if ! grep -q "SPDX-FileCopyrightText:[[:space:]]*$COPYRIGHT_HOLDER" "$file"; then
            missing_copyright+=("$file")
        fi
    fi

    # Rule 2 applies to every tracked file, header-required or not — except
    # vendored ones, which declare the licence they actually came under.
    if is_third_party "$file"; then
        continue
    fi
    if [ -n "$declared" ] && [ "$declared" != "$expected" ]; then
        wrong_license+=("$file: declares '$declared', expected '$expected'")
    fi
done < <(git ls-files)

status=0

if [ "${#missing[@]}" -gt 0 ]; then
    status=1
    echo "MISSING SPDX HEADER (charter rule 17):"
    printf '  %s\n' "${missing[@]}"
    echo
fi

if [ "${#missing_copyright[@]}" -gt 0 ]; then
    status=1
    echo "MISSING SPDX COPYRIGHT LINE — expected 'SPDX-FileCopyrightText: $COPYRIGHT_HOLDER':"
    printf '  %s\n' "${missing_copyright[@]}"
    echo
fi

if [ "${#wrong_license[@]}" -gt 0 ]; then
    status=1
    echo "WRONG SPDX LICENCE FOR LOCATION:"
    printf '  %s\n' "${wrong_license[@]}"
    echo
    echo "  Files under api/ are MIT so mod authors can vendor them into"
    echo "  closed-source projects. Everything else is GPL-3.0-only."
    echo
fi

if [ "$status" -ne 0 ]; then
    cat <<'EOF'
  Add to the top of each file, using its comment syntax:

      // SPDX-FileCopyrightText: Iridesium
      // SPDX-License-Identifier: GPL-3.0-only

  (`--` for Lua, `#` for shell, `<!-- ... -->` for Markdown; MIT under api/.)
EOF
else
    echo "SPDX headers: OK"
fi

exit "$status"
