// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Connecting a script to a live bot, and the swarm/replay behaviours.
//!
//! The script thread is synchronous — a Lua script reads top to bottom — while
//! the client is async. This module is the join between them: the script sends
//! [`Command`]s down a channel, an async task carries them out against a real
//! [`Bot`], and the reply unblocks the script.
//!
//! # Why a thread rather than an async Lua runtime
//!
//! Lua has coroutines, and mlua can drive them from async code. It would also
//! mean every scenario script had to be written in terms of them, and a script
//! that reads `dig(); assert(...)` is worth more than one that reads
//! `await dig(); assert(...)` — because the people writing scenarios are
//! testing a voxel engine, not learning an async runtime.

use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Duration;

use tiamot_core::identity::Identity;

use crate::client::{Bot, BotError};
use crate::script::{Command, Reply};

/// How long a command waits for the server before giving up.
///
/// Generous: a loaded server streaming chunks to twenty bots can take a moment.
/// Short enough that a wedged server fails the run rather than hanging CI,
/// which is the failure mode this number exists to prevent.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Drives one bot from a command channel until it closes.
///
/// Returns when the script side hangs up.
pub async fn drive(mut bot: Bot, commands: mpsc::Receiver<Command>, replies: mpsc::Sender<Reply>) {
    // `recv` blocks, so it runs on a blocking thread rather than stalling the
    // runtime. The channel is the only shared state.
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(command) = commands.recv() {
            if async_tx.send(command).is_err() {
                break;
            }
        }
    });

    while let Some(command) = async_rx.recv().await {
        let reply = match execute(&mut bot, &command).await {
            Ok(reply) => reply,
            Err(err) => Reply::Failed(format!("{command:?} failed: {err}")),
        };
        let closing = matches!(command, Command::Disconnect);
        if replies.send(reply).is_err() || closing {
            break;
        }
    }
}

async fn execute(bot: &mut Bot, command: &Command) -> Result<Reply, BotError> {
    match command {
        Command::Join(name) => {
            bot.join(name).await?;
            Ok(Reply::Done)
        }
        Command::DigBlock(pos) => {
            bot.dig_block(*pos).await?;
            Ok(Reply::Done)
        }
        Command::DigSubNode(pos) => {
            bot.dig_subnode(*pos).await?;
            Ok(Reply::Done)
        }
        Command::Place(pos, material) => {
            bot.place(*pos, *material).await?;
            Ok(Reply::Done)
        }
        Command::ExpectBlock(pos, material, timeout_ms) => {
            bot.expect_block(*pos, *material, Duration::from_millis(*timeout_ms))
                .await?;
            Ok(Reply::Done)
        }
        Command::MoveTo(x, y, z) => {
            bot.move_to(*x, *y, *z).await?;
            Ok(Reply::Done)
        }
        Command::Chat(text) => {
            bot.chat(text).await?;
            Ok(Reply::Done)
        }
        Command::SleepTicks(ticks) => {
            bot.sleep_ticks(*ticks).await;
            Ok(Reply::Done)
        }
        Command::ExpectUnits(material, units, timeout_ms) => {
            // Poll rather than sleep. A fixed sleep is a guess about how fast
            // the server is, and macOS CI proved the guess wrong: five digs,
            // sleep 200 ms, and only one had been applied.
            let deadline = tokio::time::Instant::now() + Duration::from_millis(*timeout_ms);
            loop {
                if bot.units_of(*material) >= *units {
                    return Ok(Reply::Done);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(Reply::Failed(format!(
                        "expected at least {units} units of material {material} within \
                         {timeout_ms} ms, saw {}",
                        bot.units_of(*material)
                    )));
                }
                bot.await_inventory(Duration::from_millis(50)).await?;
            }
        }
        Command::Inventory => {
            // Drain anything already queued so the answer is current. A short
            // window rather than none: an inventory read straight after a dig
            // would otherwise race the update it is waiting for.
            let stacks = bot.await_inventory(Duration::from_millis(300)).await?;
            Ok(Reply::Inventory(stacks))
        }
        Command::Disconnect => Ok(Reply::Done),
    }
}

/// What one bot did during a swarm run.
#[derive(Debug, Default, Clone)]
pub struct SwarmStats {
    /// Edits sent.
    pub edits: u64,
    /// Edits the server confirmed.
    pub confirmed: u64,
    /// Round-trip times for confirmed edits, in microseconds.
    pub latencies_us: Vec<u64>,
    /// Whether the bot finished without a transport failure.
    pub healthy: bool,
}

impl SwarmStats {
    /// Percentile of the recorded latencies, in microseconds.
    ///
    /// Nearest-rank, which is the definition that does not invent a value that
    /// never happened — a p99 someone quotes should be a latency that actually
    /// occurred.
    ///
    /// Integer arithmetic throughout. Not because this is simulation — it is
    /// not, and the float-determinism rules do not reach a latency report — but
    /// because `ceil(p/100 * N)` in integers is exact, and the workspace's
    /// `disallowed-methods` lint bans `f64::ceil` anyway. Nothing was lost:
    /// percentiles worth reporting are whole numbers.
    ///
    /// Returns 0 for an empty sample.
    #[must_use]
    pub fn percentile_us(&self, percentile: u32) -> u64 {
        if self.latencies_us.is_empty() {
            return 0;
        }
        let mut sorted = self.latencies_us.clone();
        sorted.sort_unstable();
        // Nearest-rank: ceil(p * N / 100), clamped into the slice.
        let rank = (u64::from(percentile) * sorted.len() as u64).div_ceil(100) as usize;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    }

    /// Mean latency in microseconds.
    #[must_use]
    pub fn mean_us(&self) -> u64 {
        if self.latencies_us.is_empty() {
            return 0;
        }
        let total: u64 = self.latencies_us.iter().sum();
        total / self.latencies_us.len() as u64
    }
}

/// Runs one wandering bot for `duration`, editing as it goes.
///
/// The `wander` behaviour: move somewhere, place a block, dig it back out.
/// Deliberately self-cleaning — a swarm that only placed would grow the world
/// without bound and measure disk rather than the server.
pub async fn wander(
    addr: SocketAddr,
    name: String,
    material: u16,
    duration: Duration,
    seed: u64,
) -> Result<SwarmStats, BotError> {
    let mut bot = Bot::connect_trusting(
        addr,
        Identity::generate().map_err(|_| BotError::Connect {
            addr,
            reason: "could not generate an identity".to_owned(),
        })?,
    )
    .await?;
    bot.join(&name).await?;

    let mut stats = SwarmStats::default();
    let deadline = tokio::time::Instant::now() + duration;
    // A cheap deterministic sequence, seeded per bot. Not for simulation, so it
    // is outside charter rule 4 — but reproducible so a failing swarm run can
    // be re-run with the same movement.
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    while tokio::time::Instant::now() < deadline {
        let roll = next();
        let x = i32::try_from(roll % 64).unwrap_or(0) - 32;
        let z = i32::try_from((roll >> 8) % 64).unwrap_or(0) - 32;
        // The top solid layer: the reference worldgen fills BELOW its heightmap,
        // so this is the highest block that actually exists.
        let pos = tiamot_core::BlockPos::new(x, -1, z);

        bot.move_to(x as f32, 0.0, z as f32).await?;

        // **Dig first, build second.** This used to place and then dig, which
        // needed the bot to be carrying something before it had mined anything.
        // A client cannot conjure material any more, so the loop runs the way a
        // player's does: take it out, put it back — which also keeps the world
        // from growing without bound, which was the original reason for the
        // pairing.
        let started = tokio::time::Instant::now();
        bot.dig_block(pos).await?;
        stats.edits += 1;
        stats.confirmed += 1;
        stats
            .latencies_us
            .push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));

        // Put it back, so the next round has something to dig.
        if bot.place(pos, material).await.is_ok() {
            stats.edits += 1;
            stats.confirmed += 1;
        }

        bot.sleep_ticks(2).await;
    }

    stats.healthy = true;
    bot.disconnect().await;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        // Nearest-rank never invents a value that did not occur, which matters
        // for a latency report someone will quote.
        let stats = SwarmStats {
            latencies_us: vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100],
            ..SwarmStats::default()
        };

        assert_eq!(stats.percentile_us(50), 50);
        assert_eq!(stats.percentile_us(90), 90);
        assert_eq!(stats.percentile_us(99), 100);
        assert_eq!(stats.percentile_us(100), 100);
        assert!(
            stats.latencies_us.contains(&stats.percentile_us(95)),
            "a percentile must be a value that actually occurred"
        );
    }

    #[test]
    fn percentiles_of_an_empty_sample_are_zero_rather_than_a_panic() {
        let stats = SwarmStats::default();
        assert_eq!(stats.percentile_us(99), 0);
        assert_eq!(stats.mean_us(), 0);
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let stats = SwarmStats {
            latencies_us: vec![42],
            ..SwarmStats::default()
        };
        for percentile in [0, 50, 99, 100] {
            assert_eq!(stats.percentile_us(percentile), 42, "at p{percentile}");
        }
    }

    #[test]
    fn the_mean_is_the_mean() {
        let stats = SwarmStats {
            latencies_us: vec![10, 20, 30],
            ..SwarmStats::default()
        };
        assert_eq!(stats.mean_us(), 20);
    }

    #[test]
    fn percentiles_do_not_depend_on_input_order() {
        // The sample arrives in completion order, which is not sorted.
        let ordered = SwarmStats {
            latencies_us: vec![1, 2, 3, 4, 5],
            ..SwarmStats::default()
        };
        let shuffled = SwarmStats {
            latencies_us: vec![4, 1, 5, 3, 2],
            ..SwarmStats::default()
        };
        assert_eq!(ordered.percentile_us(90), shuffled.percentile_us(90));
        assert_eq!(ordered.mean_us(), shuffled.mean_us());
    }
}
