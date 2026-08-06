// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The sky a mod described, and where it stands right now.
//!
//! # The engine interpolates; the mod decides what between
//!
//! Charter rule 1. Everything here is arithmetic over a list of colours the
//! client was handed: how long a day is, what colour dawn goes, whether there
//! is a day at all — none of it is known here. A world whose mods register no
//! sky gets [`Sky::none`], which never changes, and that is a legitimate world
//! rather than a missing feature.
//!
//! # Why the client interpolates rather than the server sending a colour
//!
//! The colour changes every frame and the tick runs at 20 Hz. Sending a colour
//! would either look stepped or cost a message per frame per player, and the
//! interpolation is four multiplications — presentation work, on the machine
//! doing the presenting. What the server owns is the *clock*, because two
//! players standing together must see the same sky.

use tiamot_core::proto::SkyFrame;

/// A sky's colours, and the clock that walks them.
#[derive(Debug, Clone, PartialEq)]
pub struct Sky {
    /// Ticks in a full day. Zero means no mod registered a sky.
    day_length_ticks: u32,
    /// Keyframes, sorted by time.
    keyframes: Vec<SkyFrame>,
    /// Where the day stands, `0.0..1.0`.
    time: f32,
}

/// What the sky looks like at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moment {
    /// The sky's own colour, which distance fog fades towards.
    pub sky: [f32; 3],
    /// The sun's colour.
    pub sun: [f32; 3],
    /// How strong the sun is, `0.0..=1.0`.
    pub intensity: f32,
}

impl Sky {
    /// A world with no sky mod: one fixed daylight moment, for ever.
    ///
    /// **Not an error case.** The engine registers no sky (charter rule 1), so
    /// this is what a world without one legitimately looks like — and it is
    /// exactly the lighting Task 08's scenes were built against, which is why
    /// their screenshots still hold.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            day_length_ticks: 0,
            keyframes: Vec::new(),
            time: 0.0,
        }
    }

    /// The sky a server described.
    #[must_use]
    pub fn new(day_length_ticks: u32, mut keyframes: Vec<SkyFrame>) -> Self {
        // Sorted defensively. The server sorts too, but this is data from a
        // peer and an out-of-order list would make the sky walk backwards
        // partway through the day rather than fail in any visible way.
        keyframes.sort_by(|a, b| a.time.total_cmp(&b.time));
        Self {
            day_length_ticks,
            keyframes,
            time: 0.0,
        }
    }

    /// Whether a mod gave this world a day.
    #[must_use]
    pub fn has_day(&self) -> bool {
        self.day_length_ticks > 0 && !self.keyframes.is_empty()
    }

    /// Moves the clock to where the server says it is.
    pub fn set_time(&mut self, time: f32) {
        // `rem_euclid` rather than a clamp: a peer sending 1.5 means the middle
        // of the next day, and clamping would stick the sky at midnight until
        // the next update rather than showing noon.
        self.time = if time.is_finite() {
            time.rem_euclid(1.0)
        } else {
            0.0
        };
    }

    /// Advances the clock by a frame's worth of time.
    ///
    /// **Between the server's updates, not instead of them.** The server sends
    /// the time once a second and this fills the gap so the sky moves smoothly;
    /// every update snaps it back to the truth. A client that only advanced
    /// locally would drift, which is why `set_time` overwrites rather than
    /// blends.
    pub fn advance(&mut self, seconds: f32) {
        if !self.has_day() {
            return;
        }
        let ticks_per_second = 1.0 / tiamot_core::tick::TICK_DURATION.as_secs_f32();
        let day_seconds = self.day_length_ticks as f32 / ticks_per_second;
        self.time = (self.time + seconds / day_seconds).rem_euclid(1.0);
    }

    /// Where the day stands, `0.0..1.0`.
    #[must_use]
    pub const fn time(&self) -> f32 {
        self.time
    }

    /// The sky at this moment.
    ///
    /// Interpolates between the two keyframes the clock sits between, wrapping
    /// from the last back to the first — a day is a circle, and a sky that cut
    /// hard at midnight would flicker once per day.
    #[must_use]
    pub fn moment(&self) -> Moment {
        let Some(first) = self.keyframes.first() else {
            // No sky: full daylight, unchanging. The same values Task 08 drew
            // with, so a world with no sky mod looks exactly as it did.
            return Moment {
                sky: crate::render::sky_colour(),
                sun: [1.0, 1.0, 1.0],
                intensity: 1.0,
            };
        };
        let last = self.keyframes.last().unwrap_or(first);

        // Before the first keyframe or after the last: between the last and the
        // first, across midnight.
        let (before, after, span) = if self.time < first.time {
            (last, first, first.time + (1.0 - last.time))
        } else {
            match self
                .keyframes
                .windows(2)
                .find(|pair| self.time >= pair[0].time && self.time < pair[1].time)
            {
                Some(pair) => (&pair[0], &pair[1], pair[1].time - pair[0].time),
                None => (last, first, first.time + (1.0 - last.time)),
            }
        };

        // How far between the two, guarding the case where they coincide: two
        // keyframes at the same time are a mod's mistake rather than a crash.
        let travelled = if self.time >= before.time {
            self.time - before.time
        } else {
            self.time + (1.0 - before.time)
        };
        let blend = if span > f32::EPSILON {
            (travelled / span).clamp(0.0, 1.0)
        } else {
            0.0
        };

        Moment {
            sky: mix(before.sky, after.sky, blend),
            sun: mix(before.sun, after.sun, blend),
            intensity: before.intensity + (after.intensity - before.intensity) * blend,
        }
    }
}

/// Linear blend between two colours.
fn mix(from: [f32; 3], to: [f32; 3], blend: f32) -> [f32; 3] {
    [
        from[0] + (to[0] - from[0]) * blend,
        from[1] + (to[1] - from[1]) * blend,
        from[2] + (to[2] - from[2]) * blend,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(time: f32, value: f32, intensity: f32) -> SkyFrame {
        SkyFrame {
            time,
            sky: [value; 3],
            sun: [value; 3],
            intensity,
        }
    }

    #[test]
    fn a_world_with_no_sky_mod_is_permanently_daylight() {
        // Charter rule 1: no sky registered is a world without a day, not a
        // broken one — and it must look exactly like Task 08's scenes, whose
        // screenshot hashes still have to hold.
        let sky = Sky::none();
        assert!(!sky.has_day());
        let moment = sky.moment();
        assert!((moment.intensity - 1.0).abs() < 1e-6);
        assert!(
            moment
                .sun
                .iter()
                .all(|channel| (channel - 1.0).abs() < 1e-6),
            "a world with no sky should be lit by a white sun: {:?}",
            moment.sun
        );
    }

    #[test]
    fn advancing_a_world_with_no_day_does_nothing() {
        let mut sky = Sky::none();
        sky.advance(1_000.0);
        assert!((sky.time() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn the_clock_lands_between_the_keyframes_it_sits_between() {
        let mut sky = Sky::new(100, vec![frame(0.0, 0.0, 0.0), frame(1.0, 1.0, 1.0)]);
        sky.set_time(0.25);
        let moment = sky.moment();
        assert!(
            (moment.intensity - 0.25).abs() < 1e-5,
            "a quarter of the way should be a quarter lit, got {}",
            moment.intensity
        );
        assert!((moment.sky[0] - 0.25).abs() < 1e-5);
    }

    #[test]
    fn the_day_wraps_at_midnight_rather_than_cutting() {
        // **The bug a naive lookup produces**: a sky that snaps from the last
        // keyframe to the first once a day. Between the last keyframe and 1.0,
        // the sky is on its way back to the first one.
        let mut sky = Sky::new(100, vec![frame(0.0, 0.0, 0.0), frame(0.5, 1.0, 1.0)]);
        sky.set_time(0.75);
        let midway = sky.moment();
        assert!(
            midway.intensity > 0.0 && midway.intensity < 1.0,
            "three quarters through should be between the two, got {}",
            midway.intensity
        );

        // And just before midnight it is nearly back to the first keyframe.
        sky.set_time(0.99);
        let nearly = sky.moment();
        assert!(
            nearly.intensity < midway.intensity,
            "the sky should still be darkening towards midnight: {} then {}",
            midway.intensity,
            nearly.intensity
        );
    }

    #[test]
    fn keyframes_out_of_order_are_sorted_rather_than_trusted() {
        // Data from a peer. An unsorted list would make the sky walk backwards
        // partway through the day rather than fail in any way anyone could see.
        let mut sky = Sky::new(100, vec![frame(1.0, 1.0, 1.0), frame(0.0, 0.0, 0.0)]);
        sky.set_time(0.25);
        assert!((sky.moment().intensity - 0.25).abs() < 1e-5);
    }

    #[test]
    fn a_time_outside_the_day_wraps_into_it() {
        // A peer sending 1.5 means the middle of the next day. Clamping would
        // hold the sky at midnight until the next update.
        let mut sky = Sky::new(100, vec![frame(0.0, 0.0, 0.0), frame(1.0, 1.0, 1.0)]);
        sky.set_time(1.5);
        assert!((sky.time() - 0.5).abs() < 1e-6);
        // And a non-finite value is a peer sending nonsense, which must not
        // become a NaN colour.
        sky.set_time(f32::NAN);
        assert!(sky.time().is_finite());
    }

    #[test]
    fn two_keyframes_at_the_same_moment_do_not_divide_by_zero() {
        // A mod's mistake, not a crash.
        let mut sky = Sky::new(100, vec![frame(0.5, 0.0, 0.0), frame(0.5, 1.0, 1.0)]);
        sky.set_time(0.5);
        assert!(sky.moment().intensity.is_finite());
    }

    #[test]
    fn advancing_covers_the_whole_day_in_the_length_the_mod_set() {
        // 100 ticks at 20 Hz is five seconds, so five seconds of advancing
        // should return the clock to where it started.
        let mut sky = Sky::new(100, vec![frame(0.0, 0.0, 0.0), frame(1.0, 1.0, 1.0)]);
        sky.advance(2.5);
        assert!(
            (sky.time() - 0.5).abs() < 1e-4,
            "half a day should be halfway, got {}",
            sky.time()
        );
        sky.advance(2.5);
        assert!(
            sky.time() < 1e-4 || sky.time() > 1.0 - 1e-4,
            "the day did not wrap"
        );
    }
}
