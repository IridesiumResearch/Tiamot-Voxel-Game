// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Sounds a mod registers, and the events that ask for one to be played.
//!
//! # Why any of this is in `core`
//!
//! Charter rule 3 keeps every audio crate out of `crates/core`, and nothing
//! here decodes or plays anything. What lives here is the *description* of a
//! sound and the *event* that asks for it — both of which the simulation
//! produces and the protocol carries, so both have to exist where the server
//! can reach them. The client turns an event into a noise; the server never
//! knows that a speaker exists.
//!
//! # Determinism
//!
//! Charter rule 4 exempts audio explicitly, and this module stays on the right
//! side of that line by carrying no float the simulation reads back. A gain and
//! a pitch go out and nothing comes in: a mod cannot ask how loud something was,
//! so no simulation state can ever depend on it.

/// A sound a mod registered during the registration window.
#[derive(Debug, Clone, PartialEq)]
pub struct Sound {
    /// The qualified id, e.g. `"core_tools:break"`.
    pub id: String,
    /// The mod that registered it, and whose directory `file` is relative to.
    pub mod_id: String,
    /// The file inside that mod's directory, e.g. `"sounds/break.ogg"`.
    pub file: String,
    /// How loud, as a multiplier. `1.0` is the file's own level.
    pub gain: f32,
    /// How much to vary the pitch each time, as a fraction of the pitch.
    ///
    /// `0.0` plays it identically every time, which is what makes a repeated
    /// footstep sound like a machine. A small value is why a mod would set it.
    pub pitch_variance: f32,
}

/// A sound bound to a named event.
///
/// # Why binding is a separate step from registering
///
/// `register_sound` says a file exists and what it is called. This says WHEN it
/// plays. Keeping them apart is what makes the system a system rather than a
/// convention: the engine and every mod emit named cues, and a mod binds
/// whatever sound it likes to any of them without either side knowing about the
/// other. A mod that wants a noise on jumping does not have to find the jump
/// code — there is no jump code it could reach — and the engine does not have
/// to know that anybody wanted one.
///
/// It also means a mod can re-skin another mod's events, which is charter rule
/// 1 working the way it is supposed to: the engine carries the mechanism and
/// the content is somebody's Lua.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The event, e.g. `"engine:jump"` or `"core_doors:open"`.
    pub cue: String,
    /// The qualified sound id to play for it.
    pub sound: String,
    /// The mod that asked, for attribution in the settings screen.
    pub mod_id: String,
}

/// The cues the ENGINE emits, as opposed to the ones mods invent.
///
/// # Why a fixed list, and why it is short
///
/// A cue only needs to be here if the engine is the only thing that knows the
/// event happened. Everything a mod can already see — a block broken, a place,
/// a punch — it can cue itself from the hook it already has, and putting those
/// here as well would give a mod two ways to make one noise and no way to
/// choose between them.
///
/// What is left is the handful of moments only the client knows about, and it
/// knows about them because they must not wait for a round trip: your own jump,
/// your own landing, and your own click on a button. A sound of your own action
/// arriving 80 ms late does not read as latency, it reads as a different and
/// worse sound.
pub const ENGINE_CUES: [&str; 4] = [
    "engine:jump",
    "engine:land",
    "engine:ui_click",
    "engine:ui_close",
];

/// Whether a cue name is one the engine reserves.
///
/// Mods may BIND to these — that is the point of them — but may not emit them,
/// or a mod could make every other player's client believe they had jumped.
#[must_use]
pub fn is_engine_cue(cue: &str) -> bool {
    cue.starts_with("engine:")
}

/// A request to play a sound somewhere in the world.
///
/// **What a mod asks for, not what anybody hears.** Which players are close
/// enough is the server's business, and what it actually sounds like is the
/// client's.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayRequest {
    /// The qualified sound id, as registered.
    pub sound: String,
    /// Where it happens, in world blocks.
    ///
    /// Ignored when `entity` is set — a sound that follows something needs its
    /// position every frame, and only the client has one that fresh.
    pub pos: [f64; 3],
    /// How far away it can still be heard, in blocks.
    ///
    /// Also the culling radius: a player further than this is not sent the
    /// event at all, so a sound nobody can hear costs nothing but the check.
    pub radius: f32,
    /// How loud, multiplying the sound's registered gain.
    pub gain: f32,
    /// An entity to follow, if this sound should move.
    pub entity: Option<u64>,
}

/// A looping sound, and where it is heard.
///
/// # Ambience is a loop that follows you
///
/// `PlayRequest` is a thing that happens; this is a thing that is going ON. Day
/// and night, weather, the inside of a cave, a river ten blocks away — none of
/// them are events and none of them can be expressed by playing a clip over and
/// over, because a mod would have to guess the clip's length and the seams
/// would be audible.
///
/// `everywhere` is what makes ambience possible at all: a loop with no position
/// plays at full gain wherever the listener stands and does not pan. A mod
/// crossfading day into night starts two of these and stops one.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopRequest {
    /// The mod's own handle for it, qualified like every other id.
    ///
    /// **The mod names the loop, not the engine.** A server-allocated number
    /// would have to be given back to Lua and stored somewhere, and a mod that
    /// lost it could never stop the loop it started. A name a mod chose is a
    /// name it can always say again.
    pub id: String,
    /// The qualified sound id to loop.
    pub sound: String,
    /// Where it is, in world blocks. Ignored when `everywhere`.
    pub pos: [f64; 3],
    /// How far it carries. Ignored when `everywhere`.
    pub radius: f32,
    /// How loud, multiplying the sound's registered gain.
    pub gain: f32,
    /// Heard at full gain wherever the listener is, with no panning.
    pub everywhere: bool,
}

/// Clamps a loop's numbers the way [`sanitise`] does a one-shot's.
#[must_use]
pub fn sanitise_loop(mut request: LoopRequest) -> LoopRequest {
    request.gain = clamp_finite(request.gain, 0.0, 8.0, 1.0);
    request.radius = clamp_finite(request.radius, 0.0, MAX_RADIUS, 16.0);
    for value in &mut request.pos {
        if !value.is_finite() {
            *value = 0.0;
        }
    }
    request
}

/// Where `game.play_sound` reaches.
///
/// The same seam shape as [`crate::storage::Access`] and [`crate::dig::Tools`],
/// and for the same reason: deciding who is close enough needs every connected
/// player, which lives above `core` (charter rule 3).
pub trait Access: Send + Sync {
    /// Asks for a sound to be played, returning how many players were told.
    ///
    /// The count is the mod's only feedback and is deliberately not a promise
    /// that anybody HEARD it: a client may have the sound muted, may still be
    /// fetching the file, or may have refused it as a poisoned asset.
    fn play(&self, request: &PlayRequest) -> u32;

    /// Starts a looping sound, returning how many players were told.
    ///
    /// Starting a loop that is already running REPLACES it, so a mod that calls
    /// this every tick does not end up with a tick's worth of overlapping
    /// copies — which is the mistake ambience invites, because "make sure the
    /// night loop is playing" is the natural thing to write.
    fn start_loop(&self, request: &LoopRequest) -> u32;

    /// Where the day stands, from 0 at midnight through 0.5 at noon.
    ///
    /// **On this trait rather than a new one**, because the only reason a mod
    /// needs the time on the SERVER is to decide what should be playing — the
    /// client already has its own copy for drawing the sky. A mod crossfading
    /// night into day is the case this exists for, and a second trait for one
    /// float would be a seam nothing else crosses.
    ///
    /// `0.0` in a world whose mods registered no sky, which is a world with no
    /// day rather than an error.
    fn time_of_day(&self) -> f32;

    /// Stops a looping sound by the id the mod gave it.
    ///
    /// Returns how many players were told. Stopping one that is not running is
    /// not an error: a mod tidying up on shutdown should not have to remember
    /// what it started.
    fn stop_loop(&self, id: &str) -> u32;
}

/// The largest radius a mod may ask for, in blocks.
///
/// A sound is broadcast to everyone inside it, so an unbounded radius is a mod
/// asking the server to send one message per player per call. This is well
/// beyond any view distance, so a mod that wants "everybody" gets it.
pub const MAX_RADIUS: f32 = 512.0;

/// Clamps a mod's numbers into ranges the client can act on.
///
/// **A mod is not hostile, but it is careless.** A `NaN` gain would reach the
/// wire and then a mixer; a negative radius would silently mean "nobody"; an
/// enormous one would mean "everybody, every tick". None of these deserve an
/// error — they deserve a sensible answer and a sound that still plays.
#[must_use]
pub fn sanitise(mut request: PlayRequest) -> PlayRequest {
    request.gain = clamp_finite(request.gain, 0.0, 8.0, 1.0);
    request.radius = clamp_finite(request.radius, 0.0, MAX_RADIUS, 16.0);
    for value in &mut request.pos {
        if !value.is_finite() {
            *value = 0.0;
        }
    }
    request
}

/// Clamps, substituting a default for anything that is not a number.
fn clamp_finite(value: f32, low: f32, high: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(low, high)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> PlayRequest {
        PlayRequest {
            sound: "test:thud".to_owned(),
            pos: [1.0, 2.0, 3.0],
            radius: 16.0,
            gain: 1.0,
            entity: None,
        }
    }

    #[test]
    fn a_careless_mod_gets_a_sound_rather_than_an_error() {
        // `0/0` in Lua is a quiet NaN and reaches here the same way it reaches
        // an entity patch. It must not reach a mixer, and refusing the whole
        // call would punish a mod for arithmetic that produced a usable sound
        // everywhere else.
        let cleaned = sanitise(PlayRequest {
            gain: f32::NAN,
            radius: f32::NAN,
            pos: [f64::NAN, 1.0, f64::INFINITY],
            ..request()
        });
        assert!((cleaned.gain - 1.0).abs() < f32::EPSILON);
        assert!((cleaned.radius - 16.0).abs() < f32::EPSILON);
        assert!(cleaned.pos.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn a_radius_is_bounded_in_both_directions() {
        // Negative would silently mean "nobody", which reads as a broken sound.
        let quiet = sanitise(PlayRequest {
            radius: -5.0,
            ..request()
        });
        assert!(quiet.radius >= 0.0);

        // And an enormous one is a mod asking for a message per player per
        // call, for ever.
        let loud = sanitise(PlayRequest {
            radius: 1.0e9,
            ..request()
        });
        assert!((loud.radius - MAX_RADIUS).abs() < f32::EPSILON);
    }

    #[test]
    fn an_ordinary_request_is_left_exactly_alone() {
        // The case that matters most: sanitising must not quietly retune a mod
        // that did nothing wrong.
        assert_eq!(sanitise(request()), request());
    }
}
