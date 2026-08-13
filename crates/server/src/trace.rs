// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A per-tick record of what the server did to a player's body.
//!
//! # Why this exists
//!
//! The client can already write one line per frame and one per server message,
//! and together those established something the client alone cannot explain: the
//! two simulations part company by as much as eight cells, three times in a
//! thirteen-second session, **while the player is standing still** and the server
//! is loading chunks hard. A stationary client body and a moving server body
//! means the body that moved is the server's, and only this side can say what it
//! was standing on at the time.
//!
//! So: one line per player per tick, with the body before and after, the intent
//! applied, whether the collision consulted a chunk the server has not loaded,
//! and how many chunks it was holding. Lined up against the client's log by tick
//! number, that is both halves of the same moment.
//!
//! Off unless `TIAMOT_TRACE_SERVER` names a file, and bounded, for the reasons
//! the client's logs are.

use std::io::Write as _;

use tiamot_core::ChunkPos;
use tiamot_core::phys::{Body, Intent};

/// Lines one trace will write before it stops.
///
/// Twenty thousand ticks is nearly seventeen minutes at 20 Hz — far longer than
/// any session anyone will sit down to record, and small enough to open.
const MAX_LINES: u64 = 20_000;

/// One player's tick, as the trace records it.
///
/// A struct rather than eight arguments, which is both what clippy wants and
/// what stops two `bool`s at the end of a call being swapped in silence.
#[derive(Debug, Clone, Copy)]
pub struct Moment<'a> {
    /// The tick this was.
    pub tick: u64,
    /// The chunk the body's local position is measured from.
    pub origin: ChunkPos,
    /// The body as the tick found it.
    pub before: &'a Body,
    /// The body as the tick left it.
    pub after: &'a Body,
    /// What it was asked to do.
    pub intent: Intent,
    /// Whether the collision consulted a chunk the server has not loaded.
    pub touched_absent: bool,
    /// How many chunks the server was holding.
    pub chunks_cached: usize,
}

/// A per-tick server-side trace, if one was asked for.
#[derive(Debug)]
pub struct Trace {
    writer: std::sync::Mutex<std::io::BufWriter<std::fs::File>>,
    lines: std::sync::atomic::AtomicU64,
}

impl Trace {
    /// Opens a trace at the path `TIAMOT_TRACE_SERVER` names, if it names one.
    ///
    /// Returns `None` when the variable is unset, and when the file cannot be
    /// created: a diagnostic that refuses to start a server would be a poor one.
    #[must_use]
    pub fn from_environment() -> Option<Self> {
        let path = std::env::var_os("TIAMOT_TRACE_SERVER")?;
        let file = std::fs::File::create(&path).ok()?;
        let mut writer = std::io::BufWriter::new(file);
        writer
            .write_all(
                b"tick,origin_x,origin_y,origin_z,x,y,z,vx,vy,vz,on_ground,moved,\
                  walk_x,walk_z,jump,touched_absent,chunks_cached\n",
            )
            .ok()?;
        tracing::info!(path = ?path, "tracing the server's player bodies per tick");
        Some(Self {
            writer: std::sync::Mutex::new(writer),
            lines: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Records one player's tick.
    ///
    /// `moved` is the distance the body travelled, which is the column to sort
    /// by: a stationary player whose body moved cells is the whole question.
    pub fn tick(&self, moment: &Moment<'_>) {
        let Moment {
            tick,
            origin,
            before,
            after,
            intent,
            touched_absent,
            chunks_cached,
        } = *moment;
        use std::sync::atomic::Ordering;

        if self.lines.fetch_add(1, Ordering::Relaxed) >= MAX_LINES {
            return;
        }
        let Ok(mut writer) = self.writer.lock() else {
            return;
        };

        let [dx, dy, dz] = [
            after.position[0] - before.position[0],
            after.position[1] - before.position[1],
            after.position[2] - before.position[2],
        ];
        let moved = (dx * dx + dy * dy + dz * dz).sqrt();

        let _ = writeln!(
            writer,
            "{tick},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{moved:.4},{:.3},{:.3},{},{},\
             {chunks_cached}",
            origin.x,
            origin.y,
            origin.z,
            after.position[0],
            after.position[1],
            after.position[2],
            after.velocity[0],
            after.velocity[1],
            after.velocity[2],
            u8::from(after.on_ground),
            intent.walk[0],
            intent.walk[1],
            u8::from(intent.jump),
            u8::from(touched_absent),
        );
        let _ = writer.flush();
    }
}
