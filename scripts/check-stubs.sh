#!/usr/bin/env bash
# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
#
# Every `game.*` function the engine registers must appear in the API stubs.
#
# The stubs are what a mod author's editor reads. A function that exists but is
# undocumented is invisible to them; one documented but removed is worse, since
# they will write code against it. Both drift silently, because neither breaks a
# build — which is exactly why this check exists.
#
# Deliberately one-directional in strictness: an engine function missing from
# the stubs is an ERROR, a stub entry with no engine function is a WARNING.
# Method stubs (`ChunkBuffer:fill_all`) and type declarations legitimately have
# no `game.set("...")` call behind them.
#
# Usage: scripts/check-stubs.sh

set -euo pipefail

cd "$(dirname "$0")/.."

STUBS="api/stubs/game.lua"
SOURCE="crates/core/src/script/mlua_vm.rs"

if [ ! -f "$STUBS" ]; then
    echo "missing $STUBS" >&2
    exit 1
fi

# Everything the engine puts on the `game` table: `game.set("name", ...)`.
#
# Classified by what the STUBS say, not by the name's letter case. An earlier
# version guessed "lowercase means function, uppercase means constant" and
# flagged `game.mod_id` — a lowercase field — as an undocumented function. What
# matters is that each name is documented SOMEHOW, either as a function or as a
# field.
registered=$(
    grep -oE 'game\.set\("[A-Za-z_]+"' "$SOURCE" \
        | sed 's/game\.set("//; s/"$//' \
        | sort -u
)

status=0
missing=""
documented_count=0

for name in $registered; do
    if grep -qE "^function game\.${name}\b" "$STUBS" \
        || grep -qE "^---@field ${name}\b" "$STUBS"; then
        documented_count=$((documented_count + 1))
    else
        missing="$missing $name"
    fi
done

if [ -n "$missing" ]; then
    status=1
    echo "ENGINE API MISSING FROM $STUBS:"
    for name in $missing; do
        echo "  game.$name"
    done
    echo
    echo "  A mod author's editor reads these stubs. Anything undocumented is"
    echo "  invisible to them. Add a ---@param-annotated function declaration,"
    echo "  or a ---@field entry if it is a constant."
    echo
fi

# The other direction is a warning, not an error: method stubs
# (ChunkBuffer:fill_all) and type declarations legitimately have no
# `game.set(...)` behind them.
for documented in $(grep -oE '^function game\.[A-Za-z_]+' "$STUBS" | sed 's/^function game\.//'); do
    if ! echo "$registered" | grep -qx "$documented"; then
        echo "WARNING: $STUBS documents game.$documented, which the engine does not register."
        echo "  (a removed function still in the stubs is worse than an undocumented one —"
        echo "   mod authors will write code against it)"
    fi
done

if [ "$status" -eq 0 ]; then
    echo "API stubs: OK ($documented_count entries documented)"
fi

exit "$status"
