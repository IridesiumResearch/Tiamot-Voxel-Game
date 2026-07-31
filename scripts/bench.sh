#!/usr/bin/env bash
# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
#
# One entry point for every benchmark: the criterion micro-benches from
# Tasks 02-06, plus the macro benchmark from Task 07.
#
# Usage:
#   scripts/bench.sh            # everything
#   scripts/bench.sh micro      # criterion only
#   scripts/bench.sh macro      # the macro benchmark only
#   scripts/bench.sh gate       # macro, gated against the committed baseline
#
# Charter rule 18: report a benchmark as a share of the 50 ms tick budget,
# never in isolation. The macro benchmark does that itself; criterion output
# needs reading against `docs/performance-targets.md`.
set -euo pipefail

MODE="${1:-all}"
BASELINE="benches/macro-baseline.json"
OUT="${BENCH_OUT:-target/bench}"
mkdir -p "$OUT"

run_micro() {
  echo "=== criterion micro-benchmarks ==="
  cargo bench --workspace
}

run_macro() {
  echo "=== macro benchmark ==="
  cargo build --release -p bot
  ./target/release/bot bench --rounds 120 --json "$OUT/macro.json"
  echo "wrote $OUT/macro.json"
}

run_gate() {
  echo "=== macro benchmark, gated against $BASELINE ==="
  cargo build --release -p bot
  # The workload must match the baseline's, or the comparison measures the
  # parameters rather than the server. `bot bench` refuses a mismatch.
  ./target/release/bot bench --rounds 120 --json "$OUT/macro.json" --baseline "$BASELINE"
}

case "$MODE" in
  micro) run_micro ;;
  macro) run_macro ;;
  gate)  run_gate ;;
  all)   run_micro; run_macro ;;
  *) echo "unknown mode '$MODE'; expected micro, macro, gate, or all" >&2; exit 2 ;;
esac
