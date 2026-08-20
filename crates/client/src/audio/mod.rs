// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Audio: decoding what a server sends, and playing it.
//!
//! Charter rule 3 puts every audio crate in this crate and nowhere else. The
//! server never learns that a speaker exists — it says what happened and where
//! (`core::sound`), and everything about how that becomes a noise is here.

pub mod ingest;
pub mod mixer;
pub mod synth;

pub use ingest::{AudioError, Clip, Limits};
pub use mixer::{Bus, Mixer, Placement, Volumes, place};

/// Decodes a sound with panic isolation.
///
/// **Charter rule 14's "decoding on a worker with panic isolation".** A
/// poisoned asset disables that asset and nothing else: the caller gets an
/// error, the client shows a per-server warning, and the rest of the session
/// carries on. A panic inside a decoder is exactly the case this exists for —
/// [`ingest::decode`] is written not to panic, and this is what makes that a
/// property of the client rather than a promise about the code.
///
/// The fuzz target deliberately calls [`ingest::decode`] instead, because a
/// target that caught its own panics would find nothing.
///
/// # Errors
///
/// [`AudioError`] for a refusal, and [`AudioError::Malformed`] naming the panic
/// if one happened.
pub fn decode_isolated(bytes: &[u8], limits: Limits) -> Result<Clip, AudioError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ingest::decode(bytes, limits)
    }))
    .unwrap_or_else(|_| {
        Err(AudioError::Malformed(
            "the decoder panicked; this sound is disabled for the session".to_owned(),
        ))
    })
}

/// Sounds grouped by the mod that registered them, for the settings screen.
///
/// **The attribution criterion, the sound half.** Its twin is
/// [`crate::input::Actions::by_source`], and the two agree deliberately: groups
/// come out in first-appearance order, so mods appear in load order rather than
/// alphabetically, and the answer comes from what the server said rather than
/// from splitting a namespace back out of an id.
///
/// A borrowed view, because the screen only reads it — the table itself belongs
/// to the session and is replaced wholesale when a player joins somewhere else.
#[must_use]
pub fn by_mod(
    sounds: &[tiamot_core::proto::SoundDef],
) -> Vec<(&str, Vec<&tiamot_core::proto::SoundDef>)> {
    let mut groups: Vec<(&str, Vec<&tiamot_core::proto::SoundDef>)> = Vec::new();
    for sound in sounds {
        if let Some(group) = groups.iter_mut().find(|(id, _)| *id == sound.mod_id) {
            group.1.push(sound);
        } else {
            groups.push((&sound.mod_id, vec![sound]));
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use tiamot_core::proto::SoundDef;

    fn sound(id: &str, mod_id: &str) -> SoundDef {
        SoundDef {
            id: id.to_owned(),
            mod_id: mod_id.to_owned(),
            file: None,
            gain: 1.0,
            pitch_variance: 0.0,
        }
    }

    /// Every sound lands under the mod that registered it, and a mod that
    /// registered several appears once.
    #[test]
    fn sounds_group_under_the_mod_that_registered_them() {
        let sounds = [
            sound("core_tools:break", "core_tools"),
            sound("core_milk:splash", "core_milk"),
            sound("core_tools:place", "core_tools"),
        ];
        let groups = super::by_mod(&sounds);
        assert_eq!(groups.len(), 2, "two mods, two groups");
        assert_eq!(groups[0].0, "core_tools", "load order, not alphabetical");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "core_milk");
        assert_eq!(groups[1].1.len(), 1);
    }

    /// A server whose mods make no noise is the ordinary case, not an error.
    #[test]
    fn no_sounds_is_no_groups() {
        assert!(super::by_mod(&[]).is_empty());
    }
}
