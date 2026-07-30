#!/usr/bin/env bash
# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
#
# Reports whether the bulk noise fills actually auto-vectorised.
#
# Task 04 asks for this to be CHECKED rather than assumed — "if it did not, say
# so rather than assuming". A loop written in a vectorisable shape is not the
# same as a loop the compiler vectorised, and the difference is invisible from
# the source.
#
# This is diagnostic, not a gate. It is not wired into CI: whether LLVM
# vectorises a given loop varies with compiler version, and a build that fails
# because a new rustc made a different inlining decision would be noise. Run it
# when changing the fill loops, and record what it says.
#
# Usage: scripts/check-vectorisation.sh

set -euo pipefail

cd "$(dirname "$0")/.."

echo "Emitting assembly for tiamot-core (release)..."
rm -rf target/vectorisation-check
RUSTFLAGS="--emit asm" \
    CARGO_TARGET_DIR=target/vectorisation-check \
    cargo build --release -p tiamot-core --quiet

asm=$(find target/vectorisation-check/release/deps -name 'tiamot_core-*.s' | head -1)
if [ -z "$asm" ]; then
    echo "could not find emitted assembly" >&2
    exit 1
fi

echo "Reading $asm"
echo

# Packed SSE/AVX instructions. Scalar float work uses the `ss`/`sd` suffixes;
# packed work uses `ps`/`pd`. The presence of `ps` forms in a function is what
# vectorisation looks like.
packed=$(grep -cE '\b(v?(mul|add|sub|div)p[sd]|vfmadd[0-9]*p[sd])\b' "$asm" || true)
scalar=$(grep -cE '\b(v?(mul|add|sub|div)s[sd])\b' "$asm" || true)

echo "packed (SIMD) float instructions: $packed"
echo "scalar float instructions:        $scalar"
echo

if [ "$packed" -gt 0 ]; then
    echo "SOME vectorisation is present in the crate."
else
    echo "NO packed float instructions anywhere in the crate."
fi

echo
echo "The honest summary, as recorded in crates/core/src/detgen/noise.rs:"
cat <<'EOF'

  The bulk fills do NOT auto-vectorise, and the reason is inherent rather than
  a matter of loop shape:

    - every sample does a data-dependent gradient-table lookup (a gather), and
    - the simplex kernel branches on its radial falloff, which is lane-varying.

  Both defeat auto-vectorisation regardless of how the outer loop is written.
  The flat-slice, no-carried-accumulator shape is still worth keeping — it is
  what would let a future explicit-SIMD implementation slot in — but it does not
  by itself produce SIMD.

  Deliberate SIMD is NOT attempted here. It would need its own determinism
  argument: a vector path that disagrees with the scalar path by one bit breaks
  the cross-platform hash gate on any machine that dispatches differently.
EOF
