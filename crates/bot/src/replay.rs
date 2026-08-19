// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Recording and replaying bot sessions.
//!
//! A recording is a list of tick-stamped commands, one per line. Replaying it
//! reproduces the same sequence of edits against a server, which is what makes
//! the macro benchmark comparable between runs: the *input* is fixed, so any
//! change in tick time is the server's.
//!
//! # Why a line-oriented text format
//!
//! It diffs, it greps, and a failing benchmark can be bisected by deleting
//! lines. A binary format would be smaller and would cost a tool every time
//! someone wanted to know what a recording actually did. Recordings are
//! thousands of lines, not millions.
//!
//! # Ticks are advisory on replay
//!
//! A recording says *when* each command happened, and replay honours the gaps
//! so the server sees the same arrival pattern. It does not try to land each
//! command on the same absolute tick: a slower machine would fall behind and
//! then rush to catch up, which measures the replayer rather than the server.

use std::time::Duration;

use tiamot_core::{BlockPos, SubNodePos};

use crate::client::{Bot, BotError};
use crate::script::Command;

/// One recorded command and the tick it happened on.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    /// The tick the command was issued on, relative to the session start.
    pub tick: u64,
    /// What was issued.
    pub command: Command,
}

/// Parses a recording.
///
/// Format, one command per line: `<tick> <verb> [args...]`. Blank lines and
/// lines starting with `#` are ignored, so a recording can be annotated.
///
/// # Errors
///
/// A message naming the line number and what was wrong with it.
pub fn parse(text: &str) -> Result<Vec<Recorded>, String> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let mut parts = line.split_whitespace();

        let tick: u64 = parts
            .next()
            .ok_or_else(|| format!("line {number}: empty"))?
            .parse()
            .map_err(|_| format!("line {number}: the first field must be a tick number"))?;

        let verb = parts
            .next()
            .ok_or_else(|| format!("line {number}: missing a verb"))?;

        let rest: Vec<&str> = parts.collect();
        let command = match verb {
            "action" => {
                let id = rest
                    .first()
                    .ok_or_else(|| format!("line {number}: action needs an id and up/down"))?;
                let edge = rest.get(1).copied().unwrap_or("down");
                let pressed = match edge {
                    "down" => true,
                    "up" => false,
                    other => {
                        return Err(format!(
                            "line {number}: action takes `down` or `up`, not `{other}`"
                        ));
                    }
                };
                Command::Action((*id).to_owned(), pressed)
            }
            "dig_block" => Command::DigBlock(block_pos(&rest, number)?),
            "dig_subnode" => Command::DigSubNode(subnode_pos(&rest, number)?),
            "place" => {
                let pos = block_pos(&rest, number)?;
                let material = rest
                    .get(3)
                    .ok_or_else(|| format!("line {number}: place needs x y z material"))?
                    .parse()
                    .map_err(|_| format!("line {number}: material must be a number"))?;
                Command::Place(pos, material)
            }
            "place_subnode" => {
                let pos = subnode_pos(&rest, number)?;
                let material = rest
                    .get(3)
                    .ok_or_else(|| format!("line {number}: place_subnode needs x y z material"))?
                    .parse()
                    .map_err(|_| format!("line {number}: material must be a number"))?;
                Command::PlaceSubNode(pos, material)
            }
            "move_to" => {
                let coords = floats(&rest, number)?;
                Command::MoveTo(coords[0], coords[1], coords[2])
            }
            "chat" => Command::Chat(rest.join(" ")),
            other => {
                return Err(format!(
                    "line {number}: unknown verb `{other}`; expected dig_block, dig_subnode, \
                     place, place_subnode, move_to, or chat"
                ));
            }
        };

        out.push(Recorded { tick, command });
    }

    // Out-of-order ticks would make the replay's timing meaningless, and are
    // far more likely to be a hand-edit than an intention.
    for pair in out.windows(2) {
        if pair[1].tick < pair[0].tick {
            return Err(format!(
                "ticks go backwards: {} then {}. A recording must be in order.",
                pair[0].tick, pair[1].tick
            ));
        }
    }

    Ok(out)
}

/// Renders commands back to the recording format.
///
/// Round-trips with [`parse`], which is what lets a recording be generated,
/// inspected, hand-edited, and replayed.
#[must_use]
pub fn render(recorded: &[Recorded]) -> String {
    let mut out = String::new();
    for entry in recorded {
        use std::fmt::Write as _;
        let _ = match &entry.command {
            Command::Action(id, pressed) => writeln!(
                out,
                "{} action {id} {}",
                entry.tick,
                if *pressed { "down" } else { "up" }
            ),
            Command::DigBlock(pos) => writeln!(
                out,
                "{} dig_block {} {} {}",
                entry.tick, pos.x, pos.y, pos.z
            ),
            Command::DigSubNode(pos) => writeln!(
                out,
                "{} dig_subnode {} {} {}",
                entry.tick, pos.x, pos.y, pos.z
            ),
            Command::Place(pos, material) => writeln!(
                out,
                "{} place {} {} {} {material}",
                entry.tick, pos.x, pos.y, pos.z
            ),
            Command::PlaceSubNode(pos, material) => writeln!(
                out,
                "{} place_subnode {} {} {} {material}",
                entry.tick, pos.x, pos.y, pos.z
            ),
            Command::MoveTo(x, y, z) => writeln!(out, "{} move_to {x} {y} {z}", entry.tick),
            Command::Chat(text) => writeln!(out, "{} chat {text}", entry.tick),
            // Only the recordable verbs round-trip. The rest are session
            // control, not world input, and replaying them would make a
            // recording depend on how it was captured.
            _ => Ok(()),
        };
    }
    out
}

/// Replays a recording against a connected bot.
///
/// Returns how many commands were applied.
///
/// # Errors
///
/// [`BotError`] if the connection fails partway.
pub async fn run(mut bot: Bot, recorded: &[Recorded], name: &str) -> Result<usize, BotError> {
    // The name is a parameter because display names are first-come and unique
    // (charter rule 13). Every replay bot joining as "replay" meant the first
    // one worked and the rest were refused — correct server behaviour, and a
    // benchmark that silently measured one bot instead of four.
    bot.join(name).await?;

    let mut applied = 0;
    let mut previous_tick = recorded.first().map_or(0, |entry| entry.tick);

    for entry in recorded {
        // Honour the GAP, not the absolute tick — see the module docs.
        let gap = entry.tick.saturating_sub(previous_tick);
        if gap > 0 {
            tokio::time::sleep(tiamot_core::tick::TICK_DURATION * u32::try_from(gap).unwrap_or(1))
                .await;
        }
        previous_tick = entry.tick;

        match &entry.command {
            Command::Action(id, pressed) => bot.action(id, *pressed).await?,
            Command::DigBlock(pos) => bot.dig_block(*pos).await?,
            Command::DigSubNode(pos) => bot.dig_subnode(*pos).await?,
            Command::Place(pos, material) => bot.place(*pos, *material).await?,
            Command::PlaceSubNode(pos, material) => bot.place_subnode(*pos, *material).await?,
            Command::MoveTo(x, y, z) => bot.move_to(*x, *y, *z).await?,
            Command::Chat(text) => bot.chat(text).await?,
            _ => continue,
        }
        applied += 1;
    }

    // Give the last commands a moment to be applied before disconnecting, or a
    // benchmark would stop measuring before the work finished.
    tokio::time::sleep(Duration::from_millis(500)).await;
    bot.disconnect().await;
    Ok(applied)
}

fn block_pos(parts: &[&str], line: usize) -> Result<BlockPos, String> {
    let coords = ints(parts, line)?;
    Ok(BlockPos::new(coords[0], coords[1], coords[2]))
}

fn subnode_pos(parts: &[&str], line: usize) -> Result<SubNodePos, String> {
    let coords = ints(parts, line)?;
    Ok(SubNodePos::new(coords[0], coords[1], coords[2]))
}

fn ints(parts: &[&str], line: usize) -> Result<[i32; 3], String> {
    if parts.len() < 3 {
        return Err(format!("line {line}: expected x y z"));
    }
    let mut out = [0i32; 3];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = parts[index]
            .parse()
            .map_err(|_| format!("line {line}: `{}` is not a coordinate", parts[index]))?;
    }
    Ok(out)
}

fn floats(parts: &[&str], line: usize) -> Result<[f32; 3], String> {
    if parts.len() < 3 {
        return Err(format!("line {line}: expected x y z"));
    }
    let mut out = [0f32; 3];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = parts[index]
            .parse()
            .map_err(|_| format!("line {line}: `{}` is not a number", parts[index]))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_round_trips() {
        // Generate, render, parse: a recording that cannot be read back is a
        // recording nobody can inspect or hand-edit.
        let original = vec![
            Recorded {
                tick: 0,
                command: Command::Place(BlockPos::new(1, 2, 3), 7),
            },
            Recorded {
                tick: 5,
                command: Command::DigBlock(BlockPos::new(1, 2, 3)),
            },
            // Both edges of a mod's action: a recording that rendered only the
            // press would replay a key held down for ever.
            Recorded {
                tick: 6,
                command: Command::Action("core_tools:chisel_mode".to_owned(), true),
            },
            Recorded {
                tick: 7,
                command: Command::Action("core_tools:chisel_mode".to_owned(), false),
            },
            Recorded {
                tick: 9,
                command: Command::DigSubNode(SubNodePos::new(-4, 5, 6)),
            },
            Recorded {
                tick: 10,
                // Negative on every axis: a sub-node placement names a CELL,
                // and a renderer that dropped the sign would round-trip every
                // recording made east and above the origin and none made west
                // of it.
                command: Command::PlaceSubNode(SubNodePos::new(-4, -5, -6), 7),
            },
            Recorded {
                tick: 12,
                command: Command::MoveTo(1.5, 2.0, -3.25),
            },
        ];

        let rendered = render(&original);
        let parsed = parse(&rendered).expect("the rendering must parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        // A recording someone can annotate is a recording someone will read.
        let text =
            "# a session worth keeping\n\n0 place 1 2 3 7\n\n# and the dig\n5 dig_block 1 2 3\n";
        let parsed = parse(text).expect("parse");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn an_unknown_verb_names_the_line_and_what_was_expected() {
        let err = parse("0 teleport 1 2 3").expect_err("must fail");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("teleport"), "{err}");
        assert!(
            err.contains("dig_block"),
            "the message should list the verbs: {err}"
        );
    }

    #[test]
    fn a_malformed_coordinate_is_an_error_not_a_zero() {
        // Defaulting to 0 would silently replay a different session, and the
        // benchmark would compare two different workloads.
        let err = parse("0 place x 2 3 7").expect_err("must fail");
        assert!(err.contains("not a coordinate"), "{err}");
    }

    #[test]
    fn a_missing_argument_is_an_error() {
        assert!(parse("0 place 1 2").is_err(), "place needs x y z material");
        assert!(parse("0 place 1 2 3").is_err(), "place needs a material");
        assert!(parse("0 dig_block 1 2").is_err());
        assert!(parse("0").is_err(), "a verb is required");
    }

    #[test]
    fn ticks_going_backwards_is_an_error() {
        // Out-of-order ticks make the replay timing meaningless, and are far
        // more likely to be a bad hand-edit than an intention.
        let err =
            parse("0 place 1 2 3 7\n10 dig_block 1 2 3\n5 dig_block 1 2 3").expect_err("must fail");
        assert!(err.contains("backwards"), "{err}");
    }

    #[test]
    fn an_empty_recording_is_valid() {
        assert!(parse("").expect("parse").is_empty());
        assert!(
            parse("# nothing but a comment\n")
                .expect("parse")
                .is_empty()
        );
    }

    #[test]
    fn chat_keeps_its_spaces() {
        let parsed = parse("3 chat hello there world").expect("parse");
        assert_eq!(
            parsed[0].command,
            Command::Chat("hello there world".to_owned())
        );
    }

    #[test]
    fn session_control_commands_are_not_recorded() {
        // A recording is world INPUT. Replaying a `join` or a `disconnect`
        // would make it depend on how it was captured rather than on what the
        // player did.
        let rendered = render(&[
            Recorded {
                tick: 0,
                command: Command::Join("Alice".to_owned()),
            },
            Recorded {
                tick: 1,
                command: Command::Place(BlockPos::new(0, 0, 0), 2),
            },
            Recorded {
                tick: 2,
                command: Command::Disconnect,
            },
        ]);
        let parsed = parse(&rendered).expect("parse");
        assert_eq!(parsed.len(), 1, "only the world input survives");
        assert!(matches!(parsed[0].command, Command::Place(_, _)));
    }
}
