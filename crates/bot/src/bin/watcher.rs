// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A connector: a headless client that reports what it sees and takes
//! instructions, both as JSON lines.
//!
//! # Why this is a pipe and not a model
//!
//! The ask was "let an AI watch what I am doing, and for dev let it act". The
//! obvious build is an HTTP client to somebody's inference API, a key in a
//! config file, and a prompt loop. This is not that, for three reasons and all
//! of them hold whatever the model is:
//!
//! - **A game engine has no business making outbound requests to a vendor.**
//!   That is a dependency, a licence question (`cargo deny` gates every one),
//!   an egress path, and a place to keep somebody's API key — none of which the
//!   engine gains anything by owning.
//! - **The interesting harness is the one the user already has.** Claude Code,
//!   a Python script, a shell loop, a second engine: anything that can read and
//!   write lines can drive this. Picking a vendor would exclude all of them.
//! - **It stays honest about what it is.** This is an ordinary client speaking
//!   the ordinary protocol with its own identity. It sees what any player sees
//!   and can do what any player can do. There is no privileged channel, and
//!   nothing here is a back door into a server that would not let a person in.
//!
//! # Acting is opt-in, and local only
//!
//! Watching is always allowed: it is what a spectator does. Acting requires
//! `--allow-acting` **and** a loopback address, checked here rather than trusted
//! to a flag — a connector pointed at somebody else's server is a bot on their
//! world, and whether that is welcome is their decision and not this program's.
//! On a server you host yourself, you are the admin and it is yours to allow.
//!
//! # The protocol
//!
//! One JSON object per line, both ways, so a harness can read it with
//! `readline` and a person can read it with their eyes.
//!
//! Out:
//! ```text
//! {"event":"joined","name":"claude","address":"127.0.0.1:4433","acting":true}
//! {"event":"chat","text":"<Iridesium> look at this"}
//! {"event":"inventory","stacks":[{"material":2,"units":54,"shape":null}]}
//! {"event":"entities","list":[{"id":7,"model":"somemod:goat","name":null,"x":1.0,"y":2.0,"z":3.0}]}
//! {"event":"notice","text":"you cannot build there"}
//! {"event":"error","text":"..."}
//! ```
//!
//! In:
//! ```text
//! {"do":"say","text":"hello"}
//! {"do":"walk","x":1.0,"y":0.0,"z":0.0,"ticks":20}
//! {"do":"dig","x":1,"y":2,"z":3}
//! {"do":"place","x":1,"y":2,"z":3,"material":2}
//! {"do":"quit"}
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;

use bot::Bot;
use clap::Parser;
use tiamot_core::identity::Identity;
use tiamot_core::proto::ServerMessage;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "watcher",
    about = "Connect a program to a Tiamot server: sees the world as JSON lines, acts on JSON lines",
    version
)]
struct Cli {
    /// Server address.
    #[arg(long, value_name = "addr")]
    server: SocketAddr,

    /// Display name to join under.
    ///
    /// **It shows up in chat and on everybody's screen**, which is deliberate:
    /// a watcher nobody can see is a watcher nobody agreed to.
    #[arg(long, default_value = "watcher")]
    name: String,

    /// Let instructions act on the world, not only observe it.
    ///
    /// Refused unless the server is on this machine.
    #[arg(long)]
    allow_acting: bool,

    /// Where to keep this connector's identity. Created on first run.
    #[arg(long, value_name = "path")]
    identity: Option<PathBuf>,

    /// The server's certificate fingerprint, as 64 hex characters.
    ///
    /// **Required, and there is no accept-anything option.** The server prints
    /// it at startup ("server certificate ready — clients pin this on first
    /// connection"), so it is one line away, and a connector that skipped the
    /// check would be a second client in this tree that trusts whatever answers
    /// — which is exactly what charter rule 14 is about.
    #[arg(long, value_name = "hex")]
    fingerprint: String,
}

/// One instruction from whatever is driving this.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
enum Instruction {
    /// Say something in chat.
    Say { text: String },
    /// Walk in a direction for a number of ticks.
    Walk {
        x: f32,
        y: f32,
        z: f32,
        #[serde(default = "default_ticks")]
        ticks: u64,
    },
    /// Break a block.
    Dig { x: i32, y: i32, z: i32 },
    /// Put a block down.
    Place {
        x: i32,
        y: i32,
        z: i32,
        material: u16,
    },
    /// Disconnect and stop.
    Quit,
}

/// A second of walking, which is far enough to be worth asking for and short
/// enough that a wrong direction is not a journey.
const fn default_ticks() -> u64 {
    20
}

impl Instruction {
    /// Whether carrying this out changes the world.
    const fn acts(&self) -> bool {
        match self {
            // Chat is speech, and a watcher that cannot answer is not much of a
            // connector. It reaches other players either way, which is why it
            // is still refused when acting is off — see `refusal`.
            Self::Say { .. } | Self::Walk { .. } | Self::Dig { .. } | Self::Place { .. } => true,
            Self::Quit => false,
        }
    }
}

/// Whether the server is on this machine.
///
/// **Checked rather than asked.** `--allow-acting` says what the operator
/// wants; this says what is true. A connector aimed at somebody else's server
/// is a bot on their world, and that is their decision to make.
fn is_local(address: SocketAddr) -> bool {
    address.ip().is_loopback()
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        // **The log goes to stderr, because stdout is the protocol.** A tracing
        // line on stdout would be a line the harness tries to parse.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let acting = cli.allow_acting && is_local(cli.server);
    if cli.allow_acting && !acting {
        emit(&serde_json::json!({
            "event": "error",
            "text": format!(
                "`--allow-acting` refused: {} is not on this machine. A connector may watch any \
                 server and may act only on one you are running yourself.",
                cli.server
            ),
        }));
        return std::process::ExitCode::FAILURE;
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            emit(&serde_json::json!({"event": "error", "text": err.to_string()}));
            return std::process::ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(&cli, acting)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            emit(&serde_json::json!({"event": "error", "text": err}));
            std::process::ExitCode::FAILURE
        }
    }
}

/// Writes one line of the protocol.
fn emit(value: &serde_json::Value) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    // Flushed per line: a harness reading this is waiting on it, and a buffered
    // observation is one that arrives after the moment it was about.
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Connects, then pumps the server and stdin until one of them ends.
async fn run(cli: &Cli, acting: bool) -> Result<(), String> {
    let path = cli
        .identity
        .clone()
        .unwrap_or_else(|| PathBuf::from(".tiamot-watcher").join(format!("{}.key", cli.name)));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create `{}`: {err}", parent.display()))?;
    }
    let identity = Identity::load_or_create(&path)
        .map_err(|err| format!("could not read `{}`: {err}", path.display()))?;

    let fingerprint = parse_fingerprint(&cli.fingerprint)?;
    let mut bot = Bot::connect(cli.server, identity, fingerprint)
        .await
        .map_err(|err| format!("could not connect to {}: {err}", cli.server))?;
    bot.join(&cli.name)
        .await
        .map_err(|err| format!("could not join: {err}"))?;

    emit(&serde_json::json!({
        "event": "joined",
        "name": cli.name,
        "address": cli.server.to_string(),
        "acting": acting,
    }));

    // stdin on its own thread, because reading it is blocking and the server
    // must not stop being read while a harness thinks.
    let (lines, mut instructions) = tokio::sync::mpsc::channel::<String>(64);
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if lines.blocking_send(line).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            message = bot.recv() => match message {
                Ok(message) => report(&message),
                Err(err) => {
                    emit(&serde_json::json!({"event": "closed", "text": err.to_string()}));
                    return Ok(());
                }
            },
            line = instructions.recv() => match line {
                Some(line) if line.trim().is_empty() => {}
                Some(line) => {
                    if !obey(&mut bot, &line, acting).await {
                        return Ok(());
                    }
                }
                // The harness closed the pipe. Watching without anybody to
                // watch for is a process nobody stopped, so this stops.
                None => return Ok(()),
            },
        }
    }
}

/// Reports one message from the server, if it is one worth reporting.
///
/// **Deliberately not everything.** A connector that emitted every chunk would
/// drown its own chat lines in terrain, and terrain is not what somebody
/// watching a session is watching.
fn report(message: &ServerMessage) {
    match message {
        ServerMessage::Chat { text, .. } => {
            emit(&serde_json::json!({"event": "chat", "text": text}));
        }
        ServerMessage::InventoryUpdate { stacks } => {
            let stacks: Vec<serde_json::Value> = stacks
                .iter()
                .map(|stack| {
                    serde_json::json!({
                        "material": stack.material,
                        "units": stack.units,
                        "shape": (stack.shape != 0).then_some(stack.shape),
                    })
                })
                .collect();
            emit(&serde_json::json!({"event": "inventory", "stacks": stacks}));
        }
        ServerMessage::EntitySpawn { entities } => {
            // Where, in blocks, because a harness reasoning about a world
            // thinks in blocks — the chunk-and-cell split of charter rule 7 is
            // the engine's problem and not something to make a model do.
            let list: Vec<serde_json::Value> = entities
                .iter()
                .map(|entity| {
                    let block = |axis: usize, chunk: i32| {
                        f64::from(chunk) * f64::from(tiamot_core::CHUNK_BLOCKS)
                            + f64::from(entity.local[axis])
                                / f64::from(tiamot_core::SUBNODES_PER_AXIS)
                    };
                    serde_json::json!({
                        "id": entity.id,
                        "model": entity.model,
                        "name": entity.nametag,
                        "x": block(0, entity.chunk.x),
                        "y": block(1, entity.chunk.y),
                        "z": block(2, entity.chunk.z),
                    })
                })
                .collect();
            emit(&serde_json::json!({"event": "entities", "list": list}));
        }
        ServerMessage::Disconnect { reason } => {
            emit(&serde_json::json!({"event": "closed", "text": format!("{reason:?}")}));
        }
        _ => {}
    }
}

/// Carries out one instruction. Returns whether to keep going.
async fn obey(bot: &mut Bot, line: &str, acting: bool) -> bool {
    let instruction: Instruction = match serde_json::from_str(line) {
        Ok(instruction) => instruction,
        Err(err) => {
            emit(&serde_json::json!({
                "event": "error",
                "text": format!("could not read that instruction: {err}"),
            }));
            return true;
        }
    };

    if instruction.acts() && !acting {
        refusal();
        return true;
    }

    let outcome = match instruction {
        Instruction::Quit => return false,
        Instruction::Say { text } => bot.chat(&text).await.err(),
        Instruction::Walk { x, y, z, ticks } => bot.walk([x, y, z], 0, ticks).await.err(),
        Instruction::Dig { x, y, z } => bot
            .dig_block(tiamot_core::BlockPos::new(x, y, z))
            .await
            .err(),
        Instruction::Place { x, y, z, material } => bot
            .place(tiamot_core::BlockPos::new(x, y, z), material)
            .await
            .err(),
    };
    if let Some(err) = outcome {
        emit(&serde_json::json!({"event": "error", "text": err.to_string()}));
    }
    true
}

/// Says why an instruction was not carried out.
///
/// A message rather than silence: a harness whose instructions vanish will keep
/// sending them, and the person running it will think the connector is broken
/// rather than that it is behaving.
fn refusal() {
    emit(&serde_json::json!({
        "event": "refused",
        "text": "this connector is watching only. Start it with `--allow-acting`, against a \
                 server on this machine.",
    }));
}

/// Reads a certificate fingerprint written as hex.
fn parse_fingerprint(hex: &str) -> Result<[u8; 32], String> {
    let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.len() != 64 {
        return Err(format!(
            "a fingerprint is 64 hex characters; got {}",
            cleaned.len()
        ));
    }
    let mut bytes = [0u8; 32];
    for (index, pair) in cleaned.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).map_err(|_| "not hex".to_owned())?;
        bytes[index] = u8::from_str_radix(text, 16).map_err(|_| format!("`{text}` is not hex"))?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acting_is_refused_against_anything_that_is_not_this_machine() {
        // **The rule the whole design rests on.** `--allow-acting` says what the
        // operator wants; this says what is true, and a flag cannot talk it
        // into letting a bot loose on somebody else's world.
        assert!(is_local("127.0.0.1:4433".parse().expect("addr")));
        assert!(is_local("[::1]:4433".parse().expect("addr")));
        assert!(!is_local("10.0.0.4:4433".parse().expect("addr")));
        assert!(!is_local("203.0.113.9:4433".parse().expect("addr")));
    }

    #[test]
    fn every_instruction_that_touches_the_world_is_marked_as_one() {
        // Quitting is the only thing a watcher may do without permission, and
        // it only stops the watcher.
        assert!(!Instruction::Quit.acts());
        assert!(
            Instruction::Say {
                text: String::new()
            }
            .acts()
        );
        assert!(
            Instruction::Walk {
                x: 1.0,
                y: 0.0,
                z: 0.0,
                ticks: 1
            }
            .acts(),
            "a body moving is something other players can see"
        );
        assert!(Instruction::Dig { x: 0, y: 0, z: 0 }.acts());
        assert!(
            Instruction::Place {
                x: 0,
                y: 0,
                z: 0,
                material: 1
            }
            .acts()
        );
    }

    #[test]
    fn an_instruction_reads_the_way_it_is_documented() {
        let dig: Instruction =
            serde_json::from_str(r#"{"do":"dig","x":1,"y":2,"z":3}"#).expect("parse");
        assert!(matches!(dig, Instruction::Dig { x: 1, y: 2, z: 3 }));

        // `ticks` is optional, because a harness asking to walk usually means
        // "a bit" rather than a number it has thought about.
        let walk: Instruction =
            serde_json::from_str(r#"{"do":"walk","x":1.0,"y":0.0,"z":0.0}"#).expect("parse");
        let Instruction::Walk { ticks, .. } = walk else {
            panic!("walk did not parse as a walk");
        };
        assert_eq!(ticks, default_ticks());

        // And an unknown verb is an error rather than something guessed at.
        assert!(serde_json::from_str::<Instruction>(r#"{"do":"detonate"}"#).is_err());
    }

    #[test]
    fn a_fingerprint_is_read_or_refused_with_a_reason() {
        let hex = "a".repeat(64);
        assert_eq!(parse_fingerprint(&hex).expect("parse"), [0xAA; 32]);
        assert!(parse_fingerprint("abc").is_err(), "too short");
        assert!(parse_fingerprint(&"z".repeat(64)).is_err(), "not hex");
    }
}
