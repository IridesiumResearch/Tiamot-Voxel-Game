// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Playing sounds: buses, volumes, and where a noise is coming from.
//!
//! # Silence is a valid outcome
//!
//! A machine with no sound device, a headless test, a CI runner — all of these
//! run the client and none of them can open a mixer. Every method here is a
//! no-op when there is no device, and [`Mixer::available`] says which happened.
//! That is deliberate: an engine that refused to start without a sound card
//! would be an engine nobody could test.
//!
//! # Spatialisation, and what it deliberately is not
//!
//! Distance attenuation and a stereo pan, and a gentle low-pass with distance.
//! **No HRTF**, which the task says in as many words: it is a large amount of
//! work for a difference most players hear as "muddier", and it costs per
//! source per frame.
//!
//! Charter rule 4 does not reach any of this — rule 4's scope note exempts
//! audio explicitly — so the arithmetic here is ordinary floating point with
//! `sin`, `sqrt` and anything else that helps. Nothing here is ever read back
//! into the simulation.

use std::collections::BTreeMap;

use crate::audio::Clip;

/// Which mixer bus a sound belongs to.
///
/// Volumes are per bus and persist in the client config, so a player can turn
/// music down without turning footsteps down. Named rather than numbered
/// because a config file that said `bus_2 = 0.4` would be unreadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bus {
    /// Everything the world does: blocks, fluid, entities.
    Effects,
    /// Looping background noise — wind, a river.
    Ambient,
    /// Music.
    Music,
    /// The interface: clicks, confirmations.
    Ui,
}

impl Bus {
    /// Every bus, for iterating settings.
    pub const ALL: [Self; 4] = [Self::Effects, Self::Ambient, Self::Music, Self::Ui];

    /// The name this bus has in the config file.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Effects => "effects",
            Self::Ambient => "ambient",
            Self::Music => "music",
            Self::Ui => "ui",
        }
    }
}

/// How loud each bus is, and the master over all of them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Volumes {
    /// Multiplies every bus.
    pub master: f32,
    /// Per bus, by [`Bus::name`].
    pub buses: BTreeMap<String, f32>,
}

impl Default for Volumes {
    fn default() -> Self {
        let mut buses = BTreeMap::new();
        for bus in Bus::ALL {
            // Not 1.0: a fresh install should be audible and not startling,
            // and every one of these is a slider a player can move.
            buses.insert(bus.name().to_owned(), 0.8);
        }
        Self { master: 0.7, buses }
    }
}

impl Volumes {
    /// The multiplier for one bus, master included.
    #[must_use]
    pub fn of(&self, bus: Bus) -> f32 {
        let own = self.buses.get(bus.name()).copied().unwrap_or(1.0);
        (self.master * own).clamp(0.0, 4.0)
    }
}

/// How a sound arrives at the listener: how loud, and from which side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Final gain, after distance and volumes.
    pub gain: f32,
    /// Stereo position, `-1.0` full left to `1.0` full right.
    pub pan: f32,
    /// How much of the high end survives, `0.0` to `1.0`.
    ///
    /// Distance eats treble, which is most of why something far away sounds
    /// far away rather than merely quiet.
    pub brightness: f32,
}

/// Works out how a sound at `source` reaches a listener.
///
/// `forward` and `right` are the listener's own axes — unit vectors — so this
/// needs no camera type and can be tested with numbers.
///
/// **Attenuation is linear in the distance ratio, not inverse-square.** An
/// inverse-square law is correct for a point source in free air and sounds
/// wrong in a game: it drops to near-nothing within a few blocks and then has a
/// very long inaudible tail. Falling linearly to zero at the radius means the
/// radius the mod asked for is the distance the sound actually stops carrying,
/// which is the thing a mod author can reason about.
#[must_use]
pub fn place(
    source: [f64; 3],
    listener: [f64; 3],
    forward: [f32; 3],
    right: [f32; 3],
    radius: f32,
    gain: f32,
) -> Placement {
    let offset = [
        (source[0] - listener[0]) as f32,
        (source[1] - listener[1]) as f32,
        (source[2] - listener[2]) as f32,
    ];
    let distance = (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();

    // At or past the radius there is nothing to hear. Guarding zero radius as
    // well: a mod may ask for one, and a division by it would be a NaN in
    // somebody's ears.
    if radius <= 0.0 || distance >= radius {
        return Placement {
            gain: 0.0,
            pan: 0.0,
            brightness: 1.0,
        };
    }
    let nearness = 1.0 - (distance / radius);

    // **Standing inside the sound is not a direction.** Within half a block
    // the pan is meaningless and the arithmetic below is unstable, so it
    // collapses to centred rather than swinging wildly as a player turns.
    let (pan, brightness) = if distance < 0.5 {
        (0.0, 1.0)
    } else {
        let inverse = 1.0 / distance;
        let unit = [
            offset[0] * inverse,
            offset[1] * inverse,
            offset[2] * inverse,
        ];
        let sideways = unit[0] * right[0] + unit[1] * right[1] + unit[2] * right[2];
        // A little behind the head is not the same as beside it, but the
        // difference a stereo pair can express is small; `forward` is used to
        // keep a sound from swapping sides as a player turns past it.
        let ahead = unit[0] * forward[0] + unit[1] * forward[1] + unit[2] * forward[2];
        let pan = (sideways * (1.0 - 0.3 * ahead.abs())).clamp(-1.0, 1.0);
        // Treble falls faster than volume does, which is what distance sounds
        // like. Never below a quarter, or a far sound becomes a rumble.
        (pan, 0.25 + 0.75 * nearness)
    };

    Placement {
        gain: (gain * nearness).clamp(0.0, 4.0),
        pan,
        brightness,
    }
}

/// The audio backend, or nothing if this machine has no sound device.
pub struct Mixer {
    manager: Option<kira::AudioManager>,
    /// Decoded sounds, by qualified id.
    clips: BTreeMap<String, Clip>,
    /// How loud each bus is.
    volumes: Volumes,
}

impl std::fmt::Debug for Mixer {
    /// Hand-written because `kira::AudioManager` is not `Debug`, and because
    /// the useful facts about a mixer are how many sounds it holds and whether
    /// it has anywhere to play them — not the contents of every buffer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mixer")
            .field("available", &self.available())
            .field("clips", &self.clips.len())
            .field("volumes", &self.volumes)
            .finish_non_exhaustive()
    }
}

impl Mixer {
    /// Opens the sound device, or gives back a silent mixer if there is none.
    ///
    /// **Never fails.** A machine with no audio should run the game without
    /// sound rather than not run it, and every headless test is such a machine.
    #[must_use]
    pub fn open(volumes: Volumes) -> Self {
        let manager = kira::AudioManager::new(kira::AudioManagerSettings::default()).ok();
        if manager.is_none() {
            tracing::info!("no audio device; the client will run silently");
        }
        Self {
            manager,
            clips: BTreeMap::new(),
            volumes,
        }
    }

    /// Whether there is a device to play through.
    #[must_use]
    pub fn available(&self) -> bool {
        self.manager.is_some()
    }

    /// How loud each bus is.
    #[must_use]
    pub fn volumes(&self) -> &Volumes {
        &self.volumes
    }

    /// Replaces the volumes, as the settings screen does.
    pub fn set_volumes(&mut self, volumes: Volumes) {
        self.volumes = volumes;
    }

    /// Takes a decoded sound, ready to be played by id.
    ///
    /// Held even with no device: whether a sound decoded is a property of the
    /// asset and the test suite asks about it on machines with no speakers.
    pub fn insert(&mut self, id: String, clip: Clip) {
        self.clips.insert(id, clip);
    }

    /// Whether this sound has been decoded and is ready.
    #[must_use]
    pub fn holds(&self, id: &str) -> bool {
        self.clips.contains_key(id)
    }

    /// How many sounds are loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// Whether no sounds are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// Plays a loaded sound, placed for the listener.
    ///
    /// Returns whether anything was started. `false` covers all the ordinary
    /// reasons — no device, a sound still being fetched, a placement out of
    /// earshot — none of which is an error worth reporting to a player.
    pub fn play(&mut self, id: &str, bus: Bus, placement: Placement) -> bool {
        let volume = self.volumes.of(bus) * placement.gain;
        if volume <= 0.0 {
            return false;
        }
        let Some(clip) = self.clips.get(id) else {
            return false;
        };
        let Some(manager) = self.manager.as_mut() else {
            return false;
        };

        // kira wants its own frame type, and this is where the decoded samples
        // become one. Built per play rather than cached because a `StaticSound`
        // carries its own settings and the pan differs every time.
        let frames: Vec<kira::Frame> = match clip.channels {
            1 => clip
                .samples
                .iter()
                .map(|sample| kira::Frame::from_mono(*sample))
                .collect(),
            _ => clip
                .samples
                .chunks_exact(clip.channels as usize)
                .map(|frame| kira::Frame::new(frame[0], frame[1]))
                .collect(),
        };
        if frames.is_empty() {
            return false;
        }

        let sound = kira::sound::static_sound::StaticSoundData {
            sample_rate: clip.sample_rate,
            frames: frames.into(),
            settings: kira::sound::static_sound::StaticSoundSettings::new()
                .volume(kira::Decibels(amplitude_to_db(volume)))
                .panning(kira::Panning(placement.pan)),
            slice: None,
        };
        manager.play(sound).is_ok()
    }
}

/// Amplitude to decibels, for a backend that thinks in decibels.
///
/// Silence is not `-inf` here but a floor: kira treats a very negative gain the
/// same way and a `-inf` would propagate through anything that later averaged
/// it.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "audio is presentation; float-determinism.md Scope"
)]
fn amplitude_to_db(amplitude: f32) -> f32 {
    if amplitude <= 0.000_01 {
        return -80.0;
    }
    20.0 * amplitude.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORWARD: [f32; 3] = [0.0, 0.0, -1.0];
    const RIGHT: [f32; 3] = [1.0, 0.0, 0.0];

    #[test]
    fn a_sound_gets_quieter_with_distance_and_stops_at_its_radius() {
        let near = place([0.0, 0.0, -1.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        let far = place([0.0, 0.0, -12.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(
            near.gain > far.gain,
            "distance did not attenuate: {near:?} against {far:?}"
        );

        // **At the radius it is silent, not merely quiet.** The radius a mod
        // asks for is the distance the sound stops carrying — which is the
        // whole reason attenuation here is linear rather than inverse-square.
        let edge = place([0.0, 0.0, -16.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(edge.gain.abs() < f32::EPSILON, "{edge:?}");
        let beyond = place([0.0, 0.0, -100.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(beyond.gain.abs() < f32::EPSILON, "{beyond:?}");
    }

    #[test]
    fn a_sound_on_the_right_pans_right_and_one_underfoot_pans_nowhere() {
        let right = place([5.0, 0.0, 0.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(
            right.pan > 0.5,
            "a sound to the right did not pan right: {right:?}"
        );

        let left = place([-5.0, 0.0, 0.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(
            left.pan < -0.5,
            "a sound to the left did not pan left: {left:?}"
        );

        // **Standing inside it is not a direction.** Within half a block the
        // arithmetic is unstable and the answer is meaningless, so it collapses
        // to centred rather than swinging as a player turns on the spot.
        let underfoot = place([0.05, 0.0, 0.05], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(underfoot.pan.abs() < f32::EPSILON, "{underfoot:?}");
    }

    #[test]
    fn distance_eats_treble_but_never_all_of_it() {
        let near = place([0.0, 0.0, -1.0], [0.0; 3], FORWARD, RIGHT, 32.0, 1.0);
        let far = place([0.0, 0.0, -30.0], [0.0; 3], FORWARD, RIGHT, 32.0, 1.0);
        assert!(
            far.brightness < near.brightness,
            "distance did not dull the sound"
        );
        assert!(
            far.brightness >= 0.25,
            "a distant sound became a rumble: {far:?}"
        );
    }

    #[test]
    fn a_zero_radius_is_silence_rather_than_a_division() {
        // A mod may ask for one, and dividing by it would put a NaN in
        // somebody's ears.
        let placement = place([1.0, 0.0, 0.0], [0.0; 3], FORWARD, RIGHT, 0.0, 1.0);
        assert!(placement.gain.abs() < f32::EPSILON);
        assert!(placement.pan.is_finite() && placement.brightness.is_finite());
    }

    #[test]
    fn volumes_multiply_the_master_by_the_bus() {
        let mut volumes = Volumes {
            master: 0.5,
            ..Volumes::default()
        };
        volumes.buses.insert("music".to_owned(), 0.5);
        assert!((volumes.of(Bus::Music) - 0.25).abs() < 0.001);

        // A bus nobody configured is at full, so a mod adding one does not
        // arrive silent.
        volumes.buses.remove("music");
        assert!((volumes.of(Bus::Music) - 0.5).abs() < 0.001);
    }

    #[test]
    fn a_mixer_without_a_device_is_silent_rather_than_broken() {
        // The property every headless test depends on, and the reason `open`
        // cannot fail: a machine with no sound card should run the game.
        let mut mixer = Mixer::open(Volumes::default());
        mixer.insert(
            "test:thud".to_owned(),
            Clip {
                samples: vec![0.5; 32],
                channels: 1,
                sample_rate: 48_000,
            },
        );
        assert!(mixer.holds("test:thud"));
        assert_eq!(mixer.len(), 1);

        // Playing is allowed to do nothing; it must not panic either way.
        let placement = Placement {
            gain: 1.0,
            pan: 0.0,
            brightness: 1.0,
        };
        let _ = mixer.play("test:thud", Bus::Effects, placement);
        // And a sound nobody loaded is simply not played.
        assert!(!mixer.play("test:missing", Bus::Effects, placement));
    }
}
