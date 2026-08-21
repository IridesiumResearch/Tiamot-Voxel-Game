#!/usr/bin/env bash
# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
#
# Only the VM implementations may name `mlua`.
#
# Charter rule 10: the VM sits behind a trait so it can be swapped, and the
# measurement in Task 05 chose the backend. That is worth nothing if simulation
# code binds to `mlua` types directly — the trait stops being a seam and becomes
# decoration.
#
# `crates/core/src/script/mlua_vm.rs` said in its own module docs that it was
# the only file that may name mlua, and that "CI greps for it". CI did not: the
# check did not exist, and the claim had already stopped being true. A comment
# asserting an invariant nobody enforces is worse than no comment, because it is
# read as evidence.
#
# Scoped to `crates/core`. The bot's scripted-client runner is a test harness
# that legitimately embeds Lua of its own.
#
# Usage: scripts/check-vm-containment.sh

set -euo pipefail

cd "$(dirname "$0")/.."

# The files that ARE the VM layer, and may therefore name it.
ALLOWED=(
    "crates/core/src/script/mlua_vm.rs"
    "crates/core/src/script/hud_vm.rs"
    "crates/core/src/script/budget.rs"
)

status=0
offenders=""

while IFS= read -r file; do
    allowed=0
    for permitted in "${ALLOWED[@]}"; do
        if [ "$file" = "$permitted" ]; then
            allowed=1
            break
        fi
    done
    if [ "$allowed" -eq 0 ]; then
        offenders="$offenders $file"
    fi
done < <(grep -rl '\bmlua::' crates/core/src --include='*.rs' || true)

if [ -n "$offenders" ]; then
    status=1
    echo "FILES OUTSIDE THE VM LAYER NAMING mlua:"
    for file in $offenders; do
        echo "  $file"
    done
    echo
    echo "  The VM sits behind ScriptVm so it can be swapped (charter rule 10)."
    echo "  Reach it through the trait, or add the file to ALLOWED here and say"
    echo "  in the commit why it belongs to the VM layer."
    echo
else
    echo "VM containment: OK (${#ALLOWED[@]} files may name mlua)"
fi

exit "$status"
