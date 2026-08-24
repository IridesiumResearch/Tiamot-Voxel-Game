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

/// How far a run's tick count may drift from the baseline's before the two are
/// different benchmarks.
///
/// **This is the guard that was missing, and it cost six red nights across two
/// incidents.** The workload check below compares bots and rounds, which is
/// what a person changes on purpose — but what actually moved, twice, was how
/// long a ROUND takes. `04652ca` made chiselling cheaper and a 622-tick session
/// became 379; `09becc7` made a block come apart sub-node by sub-node and a
/// 626-tick session became 8,700. Both times the parameters matched, both times
/// p99 was suddenly measuring a different part of a different distribution, and
/// both times it was read as a regression for several nights running.
///
/// A quarter either way is loose enough for ordinary variation in how far four
/// bots get, and far tighter than any change that has ever moved this.
pub const TICK_DRIFT: f64 = 0.25;

/// A p99 at or below this is never a regression, whatever the ratio says.
///
/// 5% of the 50 ms tick — charter rule 18's unit, deliberately, because the
/// question the gate exists to answer is "does this still fit in a tick" and not
/// "is this the same number as last month".
///
/// The floor became necessary when the p99 fell below a millisecond. A ratio on
/// a 0.36 ms measurement is a scheduler detector: the CI runner is shared
/// silicon and was measured at 1.65× this project's reference machine on the
/// same workload, so a 2× limit over a sub-millisecond baseline fails on a busy
/// runner and teaches everyone to ignore the gate. Below 2.5 ms the server is
/// not the thing being measured.
///
/// A regression big enough to matter goes clean through this: it takes 7× to
/// reach the floor from where the benchmark sits today, and the numbers are in
/// the log and the uploaded artifact either way.
pub const NOISE_FLOOR_US: u64 = 2_500;

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

    // **And the session must be the same LENGTH**, which the parameters do not
    // guarantee: a change to how long a dig takes alters how many ticks the
    // same script occupies without touching bots or rounds. See `TICK_DRIFT`
    // for the two incidents this is here to name on sight.
    if baseline.ticks != 0 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "tick counts are thousands, exact in f64"
        )]
        let drift = (run.ticks as f64 - baseline.ticks as f64).abs() / baseline.ticks as f64;
        if drift > TICK_DRIFT {
            return Comparison {
                within_tolerance: false,
                message: format!(
                    "workload moved: the baseline ran {} ticks and this run ran {}. The                      parameters match, so something changed how long the script TAKES — a p99                      from these two is measuring different parts of different distributions,                      not the server. Find what changed the session length, then record a new                      baseline.",
                    baseline.ticks, run.ticks
                ),
            };
        }
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

    let limit = baseline
        .p99_us
        .saturating_mul(REGRESSION_FACTOR)
        .max(NOISE_FLOOR_US);
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

/// The block a bot's home sits on. The reference generator fills BELOW its
/// heightmap, so a surface of 0 puts the highest solid block at y = -1 and
/// digging at y = 0 would find air and never complete.
pub const GROUND: i32 = -1;

/// Digs one block and reports what it credited, then puts it back.
///
/// The benchmark has to name a material to build with, and it learns it here
/// rather than hard-coding an id — the id depends on the mod set's registration
/// order, so a constant would be a benchmark that measured a mod set. Runs
/// before the warmup, and restores the block it took, so it costs the
/// measurement nothing and leaves every run starting from the same world.
///
/// # Errors
///
/// A message naming what went wrong, including a dig that credited nothing —
/// which means the mod set has no drops and the workload cannot run at all.
pub async fn probe_material(addr: std::net::SocketAddr) -> Result<u16, String> {
    use tiamot_core::BlockPos;

    let identity = tiamot_core::identity::Identity::generate().map_err(|err| err.to_string())?;
    let mut bot = crate::Bot::connect_trusting(addr, identity)
        .await
        .map_err(|err| err.to_string())?;
    bot.join("bench-probe")
        .await
        .map_err(|err| err.to_string())?;

    // Beside spawn, and well clear of every bot's work area.
    let pos = BlockPos::new(2, GROUND, 0);
    bot.dig_block(pos).await.map_err(|err| err.to_string())?;
    let material = bot
        .await_inventory(Duration::from_millis(500))
        .await
        .map_err(|err| err.to_string())?
        .into_iter()
        .filter(|stack| stack.units > 0)
        .max_by_key(|stack| (stack.units, std::cmp::Reverse(stack.material)))
        .map(|stack| stack.material)
        .ok_or_else(|| {
            "the probe dig credited nothing; the mod set defines no drops, so there is \
             nothing to build with"
                .to_owned()
        })?;
    bot.place(pos, material)
        .await
        .map_err(|err| err.to_string())?;
    bot.disconnect().await;
    Ok(material)
}

/// How far apart bots' work areas are, in blocks.
///
/// Wide enough that no two bots ever touch the same block — which matters now
/// that digging is real: a bot asking for a block another bot has already dug
/// waits out its patience for a delta the server has no reason to send.
/// Narrow enough that the whole grid stays inside a walk `Bot::move_to` can
/// actually finish, which is about forty blocks before it gives up.
const SPREAD: i32 = 24;

/// Where bot `index` of `bots` sets up, in blocks, relative to spawn.
///
/// A square grid centred on the spawn point: bots land in different chunks so
/// the chunk cache and the dirty set do real work, and no bot is farther from
/// spawn than it can walk. The spacing shrinks as the grid grows for that
/// second reason — the alternative is a fixed spacing that puts the outer bots
/// somewhere they never arrive, and a dig out of reach looks like a broken
/// reach check rather than a benchmark that staged itself wrong.
fn home(index: u32, bots: u32) -> (i32, i32) {
    // Integer side length, so the grid is the same on every machine. No
    // `sqrt` on a float, which would be a needless step outside the arithmetic
    // this file can promise is identical everywhere.
    let side = (1u32..).find(|s| s * s >= bots.max(1)).unwrap_or(1);
    let spacing = (SPREAD / i32::try_from(side).unwrap_or(1)).max(4);
    let column = i32::try_from(index % side).unwrap_or(0);
    let row = i32::try_from(index / side).unwrap_or(0);
    let centre = (i32::try_from(side).unwrap_or(1) - 1) * spacing / 2;
    (column * spacing - centre, row * spacing - centre)
}

/// Generates the standard benchmark session for ONE bot.
///
/// Deterministic by construction — no randomness, no wall-clock — so the
/// workload is identical on every machine and every run.
///
/// # Why the session is per-bot rather than one recording replayed by all
///
/// It used to be one list every bot replayed, which worked while a client could
/// write blocks straight into the world: four bots writing the same block four
/// times is wasteful but harmless. It stopped working the moment digging became
/// real (Task 09). Now the first bot to arrive digs the block and the other
/// three ask for one that is already air, get nothing, carry nothing, and fail
/// on the placement that follows. The workload has to give each bot its own
/// ground, the way fifty real players have their own.
///
/// # What one round is
///
/// Walk home once, then per round: dig the block beside you, put it back,
/// and every fourth round chisel one sub-node out and back. Digging first is
/// not a style choice — a player cannot conjure material, so the only thing a
/// bot has to build with is what it just mined. `material` is what the ground
/// turned out to be, learned by digging rather than assumed, because a
/// hard-coded id is a benchmark coupled to one mod set.
///
/// Beside the bot rather than under it: digging the block you are standing on
/// drops you into the hole, and the placement that follows is refused for being
/// inside a player.
#[must_use]
pub fn standard_session(
    index: u32,
    bots: u32,
    rounds: u64,
    material: u16,
) -> Vec<crate::replay::Recorded> {
    use crate::script::Command;
    use tiamot_core::{BlockPos, SubNodePos};

    let (home_x, home_z) = home(index, bots);
    let mut out = vec![crate::replay::Recorded {
        tick: 0,
        command: Command::MoveTo(home_x as f32, 0.0, home_z as f32),
    }];

    for round in 0..rounds {
        // A small ring around home, so consecutive rounds are not the same
        // block and every one of them is inside `phys::REACH`.
        let x = home_x + 1 + i32::try_from(round % 3).unwrap_or(0);
        let z = home_z + i32::try_from((round / 3) % 3).unwrap_or(0) - 1;
        let pos = BlockPos::new(x, GROUND, z);

        out.push(crate::replay::Recorded {
            tick: round,
            command: Command::DigBlock(pos),
        });
        out.push(crate::replay::Recorded {
            tick: round,
            command: Command::Place(pos, material),
        });
        // Every fourth round, chisel as well — the sub-node path is the
        // expensive one and the benchmark should feel it. Out and straight
        // back, so the world ends each round the way it started.
        if round % 4 == 0 {
            let cell = SubNodePos::new(x * 3 + 1, GROUND * 3 + 1, z * 3 + 1);
            out.push(crate::replay::Recorded {
                tick: round,
                command: Command::DigSubNode(cell),
            });
            out.push(crate::replay::Recorded {
                tick: round,
                command: Command::PlaceSubNode(cell, material),
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
    fn a_session_that_got_longer_is_named_as_such_and_not_as_a_regression() {
        // **The failure this exists to stop.** Twice a change to how long an
        // action takes altered the session length at identical parameters, and
        // twice the gate reported a p99 regression that nobody could find,
        // because p99 was measuring a different part of a different
        // distribution. The parameters match here; only the tick count moved.
        let mut baseline = report(500);
        baseline.ticks = 626;
        let mut run = report(3338);
        run.ticks = 8854;

        let verdict = compare(&baseline, &run);
        assert!(!verdict.within_tolerance);
        assert!(
            verdict.message.contains("workload moved"),
            "a moved workload must be named, not reported as a regression: {}",
            verdict.message
        );
        assert!(
            verdict.message.contains("626") && verdict.message.contains("8854"),
            "the message must carry both tick counts, or it says nothing actionable: {}",
            verdict.message
        );
    }

    #[test]
    fn ordinary_variation_in_session_length_is_not_a_moved_workload() {
        // Four bots do not get exactly as far every run. A guard that fired on
        // that would be a guard everybody turns off.
        let mut baseline = report(500);
        baseline.ticks = 8700;
        for ticks in [8700, 7000, 10_000] {
            let mut run = report(600);
            run.ticks = ticks;
            assert!(
                compare(&baseline, &run).within_tolerance,
                "{ticks} ticks against 8700 should be the same benchmark"
            );
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
        //
        // The baseline is above `NOISE_FLOOR_US` so this measures the ratio.
        // Below the floor the floor is the limit, which the next test covers.
        let baseline = report(NOISE_FLOOR_US * 2);

        assert!(
            compare(&baseline, &report(NOISE_FLOOR_US * 4)).within_tolerance,
            "exactly 2x must pass"
        );
        assert!(
            !compare(&baseline, &report(NOISE_FLOOR_US * 4 + 1)).within_tolerance,
            "just over 2x must fail"
        );
        assert!(compare(&baseline, &report(NOISE_FLOOR_US * 2)).within_tolerance);
        assert!(
            compare(&baseline, &report(500)).within_tolerance,
            "faster is fine"
        );
    }

    #[test]
    fn a_tiny_baseline_is_gated_by_the_floor_rather_than_by_the_ratio() {
        // The case the floor exists for: a p99 well under a millisecond, where
        // twice the baseline is inside the runner's scheduling noise. Ten times
        // a 0.3 ms p99 is still 3 ms, and a server that spends 3 ms on a tick
        // it used to spend 0.3 ms on has not stopped fitting in a tick.
        let baseline = report(300);

        assert!(
            compare(&baseline, &report(700)).within_tolerance,
            "over 2x but far under the floor: this must not fail a build"
        );
        assert!(
            compare(&baseline, &report(NOISE_FLOOR_US)).within_tolerance,
            "exactly at the floor must pass"
        );
        assert!(
            !compare(&baseline, &report(NOISE_FLOOR_US + 1)).within_tolerance,
            "past the floor must still fail, or the gate catches nothing"
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
        assert_eq!(standard_session(1, 4, 50, 7), standard_session(1, 4, 50, 7));
        assert!(!standard_session(1, 4, 50, 7).is_empty());
    }

    #[test]
    fn the_standard_session_exercises_the_subnode_path() {
        // The expensive path. A benchmark that only placed whole blocks would
        // miss the engine's defining feature entirely.
        use crate::script::Command;
        let session = standard_session(0, 4, 8, 7);
        assert!(
            session
                .iter()
                .any(|entry| matches!(entry.command, Command::DigSubNode(_))),
            "the benchmark must chisel, not only place"
        );
        assert!(
            session
                .iter()
                .any(|entry| matches!(entry.command, Command::PlaceSubNode(..))),
            "and put the cell back, or the world erodes over a long run"
        );
    }

    #[test]
    fn the_standard_session_renders_and_parses() {
        let session = standard_session(2, 4, 4, 7);
        let rendered = crate::replay::render(&session);
        let parsed = crate::replay::parse(&rendered).expect("parse");
        assert_eq!(parsed, session);
    }

    #[test]
    fn every_dig_is_paid_for_before_it_is_spent() {
        // The rule the old session broke: a bot cannot conjure material, so
        // every placement must follow a dig that credited it. Checked as an
        // ordering property rather than by reading the generator, because the
        // generator is exactly what would be wrong.
        use crate::script::Command;
        let session = standard_session(0, 4, 12, 7);
        let mut dug = 0i32;
        for entry in &session {
            match entry.command {
                Command::DigBlock(_) | Command::DigSubNode(_) => dug += 1,
                Command::Place(..) | Command::PlaceSubNode(..) => {
                    dug -= 1;
                    assert!(
                        dug >= 0,
                        "a placement at tick {} has nothing behind it",
                        entry.tick
                    );
                }
                _ => {}
            }
        }
        assert_eq!(dug, 0, "the world must end the run the way it started");
    }

    #[test]
    fn no_two_bots_are_given_the_same_ground() {
        // Two bots on one block is not a busier benchmark, it is a hang: the
        // second one asks for a block that is already air and waits out its
        // patience for a delta the server has no reason to send.
        use crate::script::Command;
        use std::collections::BTreeSet;

        for bots in [1u32, 4, 9, 20] {
            // A bot revisits its own blocks round after round, which is the
            // point; what must not happen is two bots sharing one. So the
            // comparison is between whole per-bot SETS, not between visits.
            let mut claimed: BTreeSet<(i32, i32, i32)> = BTreeSet::new();
            for index in 0..bots {
                let mine: BTreeSet<(i32, i32, i32)> = standard_session(index, bots, 12, 7)
                    .iter()
                    .filter_map(|entry| match entry.command {
                        Command::DigBlock(pos) => Some((pos.x, pos.y, pos.z)),
                        _ => None,
                    })
                    .collect();
                for block in mine {
                    assert!(
                        claimed.insert(block),
                        "{bots} bots: two of them were sent to {block:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_bot_can_walk_to_its_home() {
        // `Bot::move_to` gives up after forty legs, which is about forty blocks
        // of walking. A grid that puts the outer bots past that leaves them
        // digging out of reach, and the failure reads as a broken reach check
        // rather than a benchmark that staged itself wrong.
        for bots in [1u32, 4, 9, 20, 50] {
            for index in 0..bots {
                let (x, z) = home(index, bots);
                assert!(
                    x * x + z * z <= 40 * 40,
                    "{bots} bots: bot {index} is sent to ({x}, {z}), too far to walk"
                );
            }
        }
    }
}
