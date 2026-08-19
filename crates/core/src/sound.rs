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
