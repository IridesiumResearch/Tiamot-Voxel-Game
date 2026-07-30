#!/usr/bin/env bash
# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
#
# Dependency firewall (charter rule 3).
#
# `core` and `server` must never depend on a render, window, input, audio, or
# UI crate, directly or transitively. The point is that the headless server
# builds and runs on a machine with no display server, no GPU, and no audio
# device — and that the single simulation code path stays free of presentation
# concerns.
#
# This is checked with `--target all` on purpose. A platform-specific
# dependency is exactly how something like winit sneaks in: a crate that pulls
# it only on, say, macOS would pass a check run on a Linux runner and break the
# build for everyone else. Checking every target closes that hole and means the
# check only needs to run once rather than on all three matrix legs.
#
# Usage: scripts/check-dep-firewall.sh

set -euo pipefail

cd "$(dirname "$0")/.."

# Crates that must stay clean.
GUARDED_PACKAGES=(tiamot-core server)

# Forbidden crate families. Matched as a prefix followed by end-of-name or a
# separator, so `wgpu` also catches `wgpu-core` and `wgpu-hal`, and `egui`
# catches `egui-winit` — the transitive pieces are just as disqualifying as the
# façade crate, and a check that only looked for the façade would miss them.
BANNED_PREFIXES=(wgpu winit kira egui)

banned_regex="^($(IFS='|'; echo "${BANNED_PREFIXES[*]}"))(-[A-Za-z0-9_-]+)? v"

status=0

for package in "${GUARDED_PACKAGES[@]}"; do
    # --edges normal,build: what actually ends up in the built artefact.
    # dev-dependencies are excluded deliberately — a test harness may
    # legitimately need more than the shipped binary does.
    tree=$(cargo tree \
        --package "$package" \
        --target all \
        --edges normal,build \
        --prefix none \
        --format '{p}' \
        --no-dedupe)

    if hits=$(printf '%s\n' "$tree" | grep -E "$banned_regex" | sort -u) && [ -n "$hits" ]; then
        status=1
        echo "DEPENDENCY FIREWALL VIOLATION in '$package' (charter rule 3)"
        echo
        printf '%s\n' "$hits" | while IFS= read -r hit; do
            crate_name=${hit%% *}
            echo "  forbidden: $crate_name"
            # Show the chain that pulled it in, so the fix is obvious rather
            # than a scavenger hunt.
            cargo tree \
                --package "$package" \
                --target all \
                --edges normal,build \
                --invert "$crate_name" \
                --format '{p}' 2>/dev/null | sed 's/^/      /' || true
            echo
        done
        echo "  '$package' must not depend on render, window, input, audio, or UI"
        echo "  crates. These belong to 'client' alone. If a shared type is pulling"
        echo "  one in, move the type into 'tiamot-core' rather than the dependency."
        echo
    fi
done

if [ "$status" -eq 0 ]; then
    echo "dependency firewall: OK (${GUARDED_PACKAGES[*]} clean of ${BANNED_PREFIXES[*]} across all targets)"
fi

exit "$status"
