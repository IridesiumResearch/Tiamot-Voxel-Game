// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The macro benchmark: a fixed workload, a tick-time distribution, and a gate.
//!
//! Starts a server on a fixed seed, replays a recorded four-bot session against
//! it, and reports how long each simulation tick took.
//!
//! # Why a replay rather than a live swarm
//!
//! A swarm's workload depends on timing, so two runs are never quite the same
//! and a 10% difference tells you nothing. A replay fixes the *input*: same
//! seed, same commands, same order. Anything that moves in the output is the
//! server.
//!
//! # Why p99 rather than the mean
//!
//! Charter rule 18 measures frame *pacing*, not average throughput, and the
//! same logic applies to ticks. A server whose mean tick is 2 ms and whose p99
//! is 60 ms stutters visibly every second; one at a flat 8 ms does not, despite
//! being four times slower on average. The gate watches p99 for that reason.
//!
//! # The gate is deliberately loose
//!
//! 2× on p99, per the plan. CI runners are shared and noisy, and a gate that
//! fires on scheduling jitter gets muted — after which it catches nothing at
//! all. It exists to catch an order-of-magnitude regression today; tightening
//! it needs a dedicated machine, not a smaller number.

use std::time::Duration;

/// A tick-time distribution, in microseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Bots that generated the load.
    pub bots: u32,
    /// Rounds of the standard session each bot replayed.
    pub rounds: u64,
    /// How many ticks were sampled.
    pub ticks: usize,
    /// Mean tick time.
    pub mean_us: u64,
    /// Median.
    pub p50_us: u64,
    /// 95th percentile.
    pub p95_us: u64,
    /// 99th percentile.
    pub p99_us: u64,
    /// Slowest tick.
    pub max_us: u64,
    /// Ticks that ran over the 50 ms budget.
    pub over_budget: u64,
    /// Ticks dropped to the catch-up cap.
    pub dropped: u64,
}

impl TickReport {
    /// Builds a report from raw samples.
    #[must_use]
    pub fn from_samples(
        samples: &[u32],
        over_budget: u64,
        dropped: u64,
        bots: u32,
        rounds: u64,
    ) -> Self {
        if samples.is_empty() {
            return Self {
                bots,
                rounds,
                ticks: 0,
                mean_us: 0,
                p50_us: 0,
                p95_us: 0,
                p99_us: 0,
                max_us: 0,
                over_budget,
                dropped,
            };
        }
        let mut sorted: Vec<u64> = samples.iter().map(|s| u64::from(*s)).collect();
        sorted.sort_unstable();
        let total: u64 = sorted.iter().sum();

        Self {
            bots,
            rounds,
            ticks: sorted.len(),
            mean_us: total / sorted.len() as u64,
            p50_us: percentile(&sorted, 50),
            p95_us: percentile(&sorted, 95),
            p99_us: percentile(&sorted, 99),
            max_us: *sorted.last().unwrap_or(&0),
            over_budget,
            dropped,
        }
    }

    /// The share of the 50 ms tick budget a duration takes, in **tenths of a
    /// percent**.
    ///
    /// Charter rule 18: report benchmarks as a share of the budget, never in
    /// isolation. "0.4 ms" says nothing; "0.4 ms, 0.8% of a tick" says
    /// something.
    ///
    /// Tenths rather than whole percent because whole percent rounds every
    /// healthy measurement to "0% of budget", which says as little as the bare
    /// microseconds did.
    #[must_use]
    pub fn budget_share_tenths(micros: u64) -> u64 {
        let budget_us = tiamot_core::tick::TICK_DURATION.as_micros() as u64;
        micros.saturating_mul(1000) / budget_us.max(1)
    }

    /// The budget share, formatted as a percentage with one decimal.
    #[must_use]
    pub fn budget_share(micros: u64) -> String {
        let tenths = Self::budget_share_tenths(micros);
        format!("{}.{}%", tenths / 10, tenths % 10)
    }

    /// Renders the machine-readable form.
    ///
    /// Hand-written rather than via a serialiser: it is eight integers, the
    /// shape is stable, and a baseline file a human can read and edit is worth
    /// more here than one a library owns.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"bots\": {},\n  \"rounds\": {},\n  \"ticks\": {},\n  \"mean_us\": {},\n  \"p50_us\": {},\n  \"p95_us\": {},\n  \
             \"p99_us\": {},\n  \"max_us\": {},\n  \"over_budget\": {},\n  \"dropped\": {}\n}}\n",
            self.bots,
            self.rounds,
            self.ticks,
            self.mean_us,
            self.p50_us,
            self.p95_us,
            self.p99_us,
            self.max_us,
            self.over_budget,
            self.dropped
        )
    }

    /// Parses the machine-readable form.
    ///
    /// # Errors
    ///
    /// A message if a required field is missing or unreadable.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let field = |name: &str| -> Result<u64, String> {
            let key = format!("\"{name}\"");
            let start = text
                .find(&key)
                .ok_or_else(|| format!("missing field `{name}`"))?
                + key.len();
            let rest = &text[start..];
            let colon = rest
                .find(':')
                .ok_or_else(|| format!("field `{name}` has no value"))?;
            rest[colon + 1..]
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .map_err(|_| format!("field `{name}` is not a number"))
        };

        Ok(Self {
            // Older baselines have no workload fields. Defaulting to 0 rather
            // than failing means `compare` reports "unknown workload" instead
            // of the gate breaking on an old file.
            bots: u32::try_from(field("bots").unwrap_or(0)).unwrap_or(0),
            rounds: field("rounds").unwrap_or(0),
            ticks: usize::try_from(field("ticks")?).unwrap_or(0),
            mean_us: field("mean_us")?,
            p50_us: field("p50_us")?,
            p95_us: field("p95_us")?,
            p99_us: field("p99_us")?,
            max_us: field("max_us")?,
            over_budget: field("over_budget")?,
            dropped: field("dropped")?,
        })
    }

    /// Renders the human-readable table.
    #[must_use]
    pub fn to_table(&self) -> String {
        let budget = tiamot_core::tick::TICK_DURATION;
        let mut out = format!("tick time over {} ticks (budget {budget:?}):\n", self.ticks);
        for (label, value) in [
            ("mean", self.mean_us),
            ("p50 ", self.p50_us),
            ("p95 ", self.p95_us),
            ("p99 ", self.p99_us),
            ("max ", self.max_us),
        ] {
            use std::fmt::Write as _;
            let _ = writeln!(
                out,
                "  {label}  {:>8} us  ({} of budget)",
                value,
                Self::budget_share(value)
            );
        }
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "  over budget: {}, dropped: {}",
            self.over_budget, self.dropped
        );
        out
    }
}

/// How a benchmark run compared against its baseline.
#[derive(Debug)]
pub struct Comparison {
    /// Whether the run is within tolerance.
    pub within_tolerance: bool,
    /// What went wrong, if anything.
    pub message: String,
}

/// Fails a run whose p99 is more than this multiple of the baseline's.
pub const REGRESSION_FACTOR: u64 = 2;

/// Compares a run against a baseline.
///
/// A run *faster* than the baseline is never a failure — that is the point of
/// the work — but it is reported, because a sudden 10× speedup usually means
/// the benchmark stopped doing what it used to.
#[must_use]
pub fn compare(baseline: &TickReport, run: &TickReport) -> Comparison {
    if run.ticks == 0 {
        return Comparison {
            within_tolerance: false,
            message: "the run recorded no ticks at all".to_owned(),
        };
    }

    // The workload must match, or the comparison is between two different
    // benchmarks. Fewer rounds lets startup ticks dominate a smaller sample and
    // the p99 rises for reasons that have nothing to do with the server — which
    // is a spurious failure, and spurious failures are how a gate gets muted.
    if baseline.bots != 0 && (baseline.bots != run.bots || baseline.rounds != run.rounds) {
        return Comparison {
            within_tolerance: false,
            message: format!(
                "workload mismatch: the baseline is {} bots x {} rounds but this run was {} x {}. \
                 Comparing them measures the parameters, not the server. Re-run with \
                 `--bots {} --rounds {}`, or record a new baseline.",
                baseline.bots,
                baseline.rounds,
                run.bots,
                run.rounds,
                baseline.bots,
                baseline.rounds
            ),
        };
    }

    // A baseline of 0 would make any run infinitely worse. Treat it as
    // "no useful baseline" rather than dividing by it.
    if baseline.p99_us == 0 {
        return Comparison {
            within_tolerance: true,
            message: format!(
                "no usable baseline (p99 was 0); this run's p99 is {} us",
                run.p99_us
            ),
        };
    }

    let limit = baseline.p99_us.saturating_mul(REGRESSION_FACTOR);
    if run.p99_us > limit {
        Comparison {
            within_tolerance: false,
            message: format!(
                "p99 regressed: {} us against a baseline of {} us, over the {REGRESSION_FACTOR}x \
                 limit of {limit} us ({} of the tick budget)",
                run.p99_us,
                baseline.p99_us,
                TickReport::budget_share(run.p99_us)
            ),
        }
    } else if run.p99_us.saturating_mul(4) < baseline.p99_us {
        Comparison {
            within_tolerance: true,
            message: format!(
                "p99 improved sharply: {} us against a baseline of {} us. Worth checking the \
                 benchmark still does what it used to before updating the baseline.",
                run.p99_us, baseline.p99_us
            ),
        }
    } else {
        Comparison {
            within_tolerance: true,
            message: format!(
                "p99 {} us against a baseline of {} us (limit {limit} us)",
                run.p99_us, baseline.p99_us
            ),
        }
    }
}

/// Nearest-rank percentile over a sorted slice.
fn percentile(sorted: &[u64], percentile: u32) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (u64::from(percentile) * sorted.len() as u64).div_ceil(100) as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Generates the standard benchmark session: four bots, fixed commands.
///
/// Deterministic by construction — no randomness, no wall-clock — so the
/// workload is identical on every machine and every run.
#[must_use]
pub fn standard_session(bots: u32, rounds: u64) -> Vec<crate::replay::Recorded> {
    use crate::script::Command;
    use tiamot_core::{BlockPos, SubNodePos};

    let mut out = Vec::new();
    for round in 0..rounds {
        for bot in 0..bots {
            // Spread the bots across chunks so the cache and the dirty set do
            // real work rather than hammering one hot chunk.
            let x = i32::try_from(bot).unwrap_or(0) * 37 + i32::try_from(round % 16).unwrap_or(0);
            let z = i32::try_from(bot).unwrap_or(0) * 53 - i32::try_from(round % 11).unwrap_or(0);
            let pos = BlockPos::new(x, 6, z);

            out.push(crate::replay::Recorded {
                tick: round,
                command: Command::Place(pos, 2),
            });
            // Every fourth round, chisel instead of placing whole blocks — the
            // sub-node path is the expensive one and the benchmark should feel
            // it.
            if round % 4 == 0 {
                out.push(crate::replay::Recorded {
                    tick: round,
                    command: Command::DigSubNode(SubNodePos::new(x * 3 + 1, 6 * 3 + 1, z * 3 + 1)),
                });
            }
            out.push(crate::replay::Recorded {
                tick: round,
                command: Command::DigBlock(pos),
            });
        }
    }
    out
}

/// How long to let the server settle before sampling.
///
/// The first ticks after startup include mod loading and the first chunk
/// generation, which are real but are not what a steady-state benchmark is
/// measuring.
pub const WARMUP: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    fn report(p99: u64) -> TickReport {
        TickReport {
            bots: 4,
            rounds: 120,
            ticks: 100,
            mean_us: p99 / 2,
            p50_us: p99 / 2,
            p95_us: p99,
            p99_us: p99,
            max_us: p99 * 2,
            over_budget: 0,
            dropped: 0,
        }
    }

    #[test]
    fn a_report_round_trips_through_json() {
        // The baseline lives in the repo as JSON. If it cannot be read back,
        // the gate silently compares against nothing.
        let original = TickReport::from_samples(&[100, 200, 300, 400, 500], 1, 2, 4, 120);
        let parsed = TickReport::from_json(&original.to_json()).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let report =
            TickReport::from_samples(&[10, 20, 30, 40, 50, 60, 70, 80, 90, 100], 0, 0, 4, 120);
        assert_eq!(report.p50_us, 50);
        assert_eq!(report.p95_us, 100);
        assert_eq!(report.p99_us, 100);
        assert_eq!(report.max_us, 100);
        assert_eq!(report.mean_us, 55);
    }

    #[test]
    fn an_empty_sample_reports_zeros_rather_than_panicking() {
        let report = TickReport::from_samples(&[], 0, 0, 4, 120);
        assert_eq!(report.ticks, 0);
        assert_eq!(report.p99_us, 0);
    }

    #[test]
    fn the_budget_share_is_reported_with_useful_precision() {
        // Charter rule 18: never report a benchmark in isolation. And whole
        // percent is not enough precision — it rounds every healthy tick to
        // "0% of budget", which says as little as the bare microseconds did.
        assert_eq!(TickReport::budget_share(50_000), "100.0%", "a full tick");
        assert_eq!(TickReport::budget_share(25_000), "50.0%");
        assert_eq!(TickReport::budget_share(500), "1.0%");
        assert_eq!(
            TickReport::budget_share(150),
            "0.3%",
            "a healthy tick must not round to zero"
        );
        assert!(
            TickReport::from_samples(&[150], 0, 0, 4, 120)
                .to_table()
                .contains("0.3% of budget"),
            "the table must show the share"
        );
    }

    #[test]
    fn a_doubled_p99_is_within_tolerance_and_more_is_not() {
        // The gate's exact boundary, both sides. A gate whose threshold is off
        // by one is a gate nobody trusts.
        let baseline = report(1000);

        assert!(
            compare(&baseline, &report(2000)).within_tolerance,
            "exactly 2x must pass"
        );
        assert!(
            !compare(&baseline, &report(2001)).within_tolerance,
            "just over 2x must fail"
        );
        assert!(compare(&baseline, &report(1000)).within_tolerance);
        assert!(
            compare(&baseline, &report(500)).within_tolerance,
            "faster is fine"
        );
    }

    #[test]
    fn a_regression_message_says_what_regressed_and_by_how_much() {
        let comparison = compare(&report(1000), &report(9000));
        assert!(!comparison.within_tolerance);
        assert!(
            comparison.message.contains("9000"),
            "{}",
            comparison.message
        );
        assert!(
            comparison.message.contains("1000"),
            "{}",
            comparison.message
        );
        assert!(
            comparison.message.contains("budget"),
            "and its share of the budget: {}",
            comparison.message
        );
    }

    #[test]
    fn a_different_workload_is_refused_rather_than_compared() {
        // Found by running the same code at `--rounds 60` against a baseline
        // recorded at 120 and watching it "regress": fewer rounds lets startup
        // ticks dominate a smaller sample, so the p99 rises for reasons that
        // have nothing to do with the server. A gate that fires spuriously is
        // a gate that gets muted.
        let baseline = report(1000);
        let mut different = report(1000);
        different.rounds = 60;

        let comparison = compare(&baseline, &different);
        assert!(!comparison.within_tolerance);
        assert!(
            comparison.message.contains("workload mismatch"),
            "{}",
            comparison.message
        );
        assert!(
            comparison.message.contains("--rounds 120"),
            "the message should say how to fix it: {}",
            comparison.message
        );
    }

    #[test]
    fn a_baseline_without_workload_fields_still_gates() {
        // Older baselines predate the fields. Failing on them would break the
        // gate rather than protect it.
        let mut old_baseline = report(1000);
        old_baseline.bots = 0;
        old_baseline.rounds = 0;

        assert!(compare(&old_baseline, &report(1500)).within_tolerance);
        assert!(!compare(&old_baseline, &report(9000)).within_tolerance);
    }

    #[test]
    fn the_workload_survives_a_json_round_trip() {
        let original = TickReport::from_samples(&[100, 200], 0, 0, 7, 42);
        let parsed = TickReport::from_json(&original.to_json()).expect("parse");
        assert_eq!(parsed.bots, 7);
        assert_eq!(parsed.rounds, 42);
        assert_eq!(parsed, original);
    }

    #[test]
    fn a_run_with_no_ticks_fails_rather_than_passing_vacuously() {
        // A benchmark that measured nothing must not report success. This is
        // the failure mode where a broken harness looks like a fast server.
        let empty = TickReport::from_samples(&[], 0, 0, 4, 120);
        assert!(!compare(&report(1000), &empty).within_tolerance);
    }

    #[test]
    fn a_zero_baseline_does_not_divide_by_zero() {
        let comparison = compare(&report(0), &report(5000));
        assert!(comparison.within_tolerance);
        assert!(comparison.message.contains("no usable baseline"));
    }

    #[test]
    fn a_sharp_improvement_is_reported_but_not_failed() {
        // A 10x speedup is more often a benchmark that stopped working than a
        // breakthrough, and it is worth saying so without failing the run.
        let comparison = compare(&report(10_000), &report(100));
        assert!(comparison.within_tolerance);
        assert!(
            comparison.message.contains("improved sharply"),
            "{}",
            comparison.message
        );
    }

    #[test]
    fn the_standard_session_is_deterministic() {
        // The whole point of a replay benchmark: same input every run, so any
        // change in the output is the server's.
        assert_eq!(standard_session(4, 50), standard_session(4, 50));
        assert!(!standard_session(4, 50).is_empty());
    }

    #[test]
    fn the_standard_session_exercises_the_subnode_path() {
        // The expensive path. A benchmark that only placed whole blocks would
        // miss the engine's defining feature entirely.
        use crate::script::Command;
        let session = standard_session(4, 8);
        assert!(
            session
                .iter()
                .any(|entry| matches!(entry.command, Command::DigSubNode(_))),
            "the benchmark must chisel, not only place"
        );
    }

    #[test]
    fn the_standard_session_renders_and_parses() {
        let session = standard_session(2, 4);
        let rendered = crate::replay::render(&session);
        let parsed = crate::replay::parse(&rendered).expect("parse");
        assert_eq!(parsed, session);
    }
}
