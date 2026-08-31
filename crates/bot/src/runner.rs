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
        Command::Action(id, pressed) => {
            bot.action(id, *pressed).await?;
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
        Command::PlaceSubNode(pos, material) => {
            bot.place_subnode(*pos, *material).await?;
            Ok(Reply::Done)
        }
        Command::ExpectPartial(pos, material, cells, timeout_ms) => {
            bot.expect_partial(*pos, *material, *cells, Duration::from_millis(*timeout_ms))
                .await?;
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

/// The material a bot is holding most of, if it is holding anything.
///
/// Learned rather than assumed, for the reason `churn.lua` gives: a hard-coded
/// id is a scenario coupled to one mod set, and the reference mods are a test
/// fixture rather than the game (charter's scope discipline).
///
/// Waits briefly for an update rather than reading what is already known: the
/// credit for a dig arrives on its own message, which can land a tick after the
/// block delta the dig was waiting for.
///
/// Ties break toward the lower id so two bots in the same state choose the same
/// material — a swarm is easier to reason about when its bots are not each
/// making a different arbitrary choice.
async fn carrying(bot: &mut Bot) -> Option<u16> {
    // **Two seconds, not two hundred milliseconds.** The wait is only paid when
    // no inventory update arrives at all, and under twenty bots that is exactly
    // when it matters: a credit that lands late reads as "carrying nothing", the
    // place-back is skipped, and the hole is still there when the wander comes
    // back round. Paying a bounded wait once beats leaving a trap in the world.
    let stacks = bot.await_inventory(Duration::from_secs(2)).await.ok()?;
    stacks
        .into_iter()
        .filter(|stack| stack.units > 0)
        .max_by_key(|stack| (stack.units, std::cmp::Reverse(stack.material)))
        .map(|stack| stack.material)
}

/// Runs one wandering bot for `duration`, editing as it goes.
///
/// The `wander` behaviour: move somewhere, dig a block out, put it straight
/// back. Deliberately self-cleaning — a swarm that only dug would eat the world
/// and one that only built would grow it without bound, and either measures the
/// disk rather than the server.
///
/// # Every bot gets its own strip
///
/// `index` is what keeps twenty bots out of each other's way: bot *n* wanders
/// a column of the world `STRIP` blocks wide and edits only inside it. Sharing
/// one area looks more like a real server and is not: two bots digging the same
/// block means one of them asks for a block that is *already* air, and a dig
/// that can never complete is a hang rather than a failure — the server has
/// nothing to broadcast, so the bot waits out its patience for a delta that
/// will never come.
pub async fn wander(
    addr: SocketAddr,
    name: String,
    index: u32,
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

    /// How wide a strip each bot owns, in blocks. Wide enough that a bot has
    /// somewhere to go, narrow enough that twenty of them still span several
    /// chunks and give the cache and the dirty set real work.
    const STRIP: u32 = 8;

    let home = i32::try_from(index).unwrap_or(0) * i32::try_from(STRIP).unwrap_or(8);

    while tokio::time::Instant::now() < deadline {
        let roll = next();
        let x = home + i32::try_from(roll % u64::from(STRIP)).unwrap_or(0);
        let z = i32::try_from((roll >> 8) % 64).unwrap_or(0) - 32;
        // Walk somewhere, then dig **where the bot actually ended up** rather
        // than where it was aiming. `move_to` is a straight line with a jump
        // heuristic and makes no promise of arriving; the server bounds digging
        // by `phys::REACH`, so a bot that dug at its intended destination
        // instead of its real position would be refused every time it fell
        // short — and the failure would look like the reach check being broken.
        bot.move_to(x as f32, 0.0, z as f32).await?;
        // **Settled, not merely arrived.** `move_to` jumps at anything that
        // stalls it and returns as soon as it is close enough, so the position
        // one tick later can be the top of a jump — a block higher than the
        // ground. Everything below is computed from where the feet are, and a
        // block chosen from a mid-air sample is air, which a dig then waits out
        // its whole patience for. That was a red nightly, and the coordinate it
        // reported was thirty seconds of a bot aiming at the sky.
        let here = bot.settle().await?.block();
        // BESIDE the bot, never underneath it. Digging the block you are
        // standing on drops you into the hole, and putting it back is then
        // refused for being inside a player — the rule working, and the
        // scenario staging it wrong. `churn.lua` learned this first.
        let pos = tiamot_core::BlockPos::new(here.x + 1, here.y - 1, here.z);

        // **Dig first, build second.** This used to place and then dig, which
        // needed the bot to be carrying something before it had mined anything.
        // A client cannot conjure material any more, so the loop runs the way a
        // player's does: take it out, put it back — which also keeps the world
        // from growing without bound, which was the original reason for the
        // pairing.
        // **Never dig a hole that is already a hole.** A dig at an empty block
        // has nothing to broadcast, so the bot waits out its whole patience for
        // a delta that will never come and the run fails thirty seconds later
        // with "nothing broke at all". The bot gets there honestly: a previous
        // round dug this block and the place-back did not happen — because the
        // credit had not arrived yet — and the wander brought it back to the
        // same spot. Four consecutive red nightlies, and the message was
        // telling the truth every time.
        if bot.block_is_empty(pos) {
            // Fill it in if this bot is carrying anything, so the strip does
            // not slowly turn into holes; otherwise leave it and move on.
            if let Some(material) = carrying(&mut bot).await
                && bot.place(pos, material).await.is_ok()
            {
                stats.edits += 1;
                stats.confirmed += 1;
            }
            bot.sleep_ticks(2).await;
            continue;
        }

        let started = tokio::time::Instant::now();
        bot.dig_block(pos).await?;
        stats.edits += 1;
        stats.confirmed += 1;
        stats
            .latencies_us
            .push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));

        // Put it back, so the next round has something to dig — with whatever
        // the dig actually credited. A hard-coded material id would couple the
        // swarm to one mod set, and would spend the whole run being refused for
        // carrying none of it. `Bot::place` waits ten seconds before reporting a
        // refusal, so asking only when there is something to place is the
        // difference between a load test and a bot asleep in a queue.
        if let Some(material) = carrying(&mut bot).await
            && bot.place(pos, material).await.is_ok()
        {
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
