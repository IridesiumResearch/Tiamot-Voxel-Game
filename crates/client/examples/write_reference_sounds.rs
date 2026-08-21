// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only
//
//! Regenerates the sounds shipped by the reference mods in `game/`.
//!
//! The same argument as `write_reference_textures`, and the same one that made
//! `engine:humanoid` a table of measurements rather than a committed `.glb`:
//! **a binary blob in a repository is not source.** These noises are *defined*
//! in Rust — [`client::audio::synth::Recipe`] — and written out here, so a
//! reader can see that a footstep is a short noisy scuff at 320 Hz decaying
//! fast, rather than seeing four kilobytes of samples and taking it on trust.
//!
//! They are **fixtures, not content.** The mods in `game/` are reference
//! implementations and test fixtures (see `game/README.md`), and so are their
//! sounds. A real game ships recordings, made by whoever makes that game.
//!
//! WAV rather than Ogg because there is no Vorbis encoder in this repository
//! and a WAV is a header and some samples. The client decodes both through the
//! same capped, isolated path.
//!
//! Usage: `cargo run -p client --example write_reference_sounds -- game`

use client::audio::synth::{Recipe, wav};

fn main() {
    let root =
        std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "game".to_owned()));

    // Digging and building, which is most of what a player does.
    write(&root.join("core_tools/sounds/break.wav"), Recipe::thud());
    write(&root.join("core_tools/sounds/place.wav"), Recipe::thud());
    // A footstep, played by the client from its own movement.
    write(&root.join("core_blocks/sounds/step.wav"), Recipe::step());
    // Milk: poured, and swum in.
    write(&root.join("core_milk/sounds/pour.wav"), Recipe::splash());

    // The cue system's own reference sounds. `core_sky` owns the day, so it
    // owns what the day sounds like; the click and the movement noises belong
    // to the mod that binds the engine's cues.
    write(&root.join("core_sky/sounds/day.wav"), Recipe::day());
    write(&root.join("core_sky/sounds/night.wav"), Recipe::night());
    write(&root.join("core_ui/sounds/click.wav"), Recipe::click());
    write(&root.join("core_blocks/sounds/jump.wav"), Recipe::step());
    write(&root.join("core_blocks/sounds/land.wav"), Recipe::thud());

    // **And seeds for `fuzz/ogg_ingest`.** A fuzzer starting from noise spends
    // its whole budget failing the container check and never reaches the
    // decoder; starting from a real file it mutates a valid header into an
    // invalid one, which is where the interesting answers are.
    //
    // WAV rather than Ogg for the reason the sounds are: no encoder here. A
    // real `.ogg` dropped into the same directory is strictly better and the
    // fuzz target says so.
    let corpus = std::path::Path::new("fuzz/corpus/ogg_ingest");
    write(&corpus.join("click.wav"), Recipe::click());
    write(&corpus.join("thud.wav"), Recipe::thud());
}

/// Renders a recipe and writes it, creating parent directories.
fn write(path: &std::path::Path, recipe: Recipe) {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("could not create {}: {err}", parent.display());
        std::process::exit(1);
    }
    let bytes = wav(recipe);
    match std::fs::write(path, &bytes) {
        Ok(()) => println!("wrote {} ({} bytes)", path.display(), bytes.len()),
        Err(err) => {
            eprintln!("could not write {}: {err}", path.display());
            std::process::exit(1);
        }
    }
}
