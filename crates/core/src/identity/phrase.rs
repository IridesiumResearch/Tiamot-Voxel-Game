// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! BIP-39 recovery phrases.
//!
//! The 24-word form encodes 256 bits of entropy plus an 8-bit checksum. Since
//! an Ed25519 secret key *is* 32 bytes of entropy, the phrase encodes the key
//! **directly** — no derivation path, and exactly one key per phrase forever.
//!
//! # The checksum earns its place
//!
//! Without it, a mistyped word would silently produce a *different valid seed*,
//! and the player would be handed a stranger's identity — a fresh, empty
//! account with someone else's UUID — with no indication anything went wrong.
//! With it, a single wrong word fails to parse. That is the difference between
//! "you typed it wrong, try again" and "your character is gone".

use bip39::Mnemonic;

use super::SEED_BYTES;

/// A recovery phrase could not be produced or read.
#[derive(Debug, thiserror::Error)]
pub enum PhraseError {
    /// The phrase is not valid BIP-39 — wrong word count, a word outside the
    /// wordlist, or a failed checksum.
    #[error(
        "not a valid recovery phrase: {reason}. Check the word count (24) and the spelling of \
         each word; a single wrong word is caught deliberately rather than silently producing a \
         different identity."
    )]
    Invalid {
        /// What the parser objected to.
        reason: String,
    },

    /// The phrase is valid BIP-39 but not the 24-word form.
    #[error("expected a 24-word recovery phrase, got {words} words")]
    WrongLength {
        /// How many words were supplied.
        words: usize,
    },
}

/// A 24-word BIP-39 recovery phrase.
///
/// Displayed once on first run and reproducible on demand. **This is the whole
/// backup** — a player with the phrase can rebuild their identity on any
/// machine, and a player without one who loses their key file needs an admin
/// rebind.
#[derive(Clone)]
pub struct RecoveryPhrase(Mnemonic);

impl RecoveryPhrase {
    /// Words in the phrase this engine uses.
    ///
    /// 24, not 12: 12 words is 128 bits, and an Ed25519 key is 256. A 12-word
    /// phrase could not encode the key without a derivation step, which is
    /// exactly the indirection this design avoids.
    pub const WORD_COUNT: usize = 24;

    /// Renders a seed as a phrase.
    ///
    /// # Errors
    ///
    /// [`PhraseError::Invalid`] if encoding fails, which cannot happen for a
    /// 32-byte input but is not worth an unwrap.
    pub fn from_seed(seed: &[u8; SEED_BYTES]) -> Result<Self, PhraseError> {
        Mnemonic::from_entropy(seed)
            .map(Self)
            .map_err(|err| PhraseError::Invalid {
                reason: err.to_string(),
            })
    }

    /// Parses a phrase back into a seed-bearing form.
    ///
    /// Whitespace-tolerant and case-insensitive, because this is typed by hand
    /// from a piece of paper, often badly, often while stressed.
    ///
    /// # Errors
    ///
    /// [`PhraseError`] if the phrase is not valid 24-word BIP-39.
    pub fn parse(text: &str) -> Result<Self, PhraseError> {
        let normalised = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let words = normalised.split_whitespace().count();
        if words != Self::WORD_COUNT {
            return Err(PhraseError::WrongLength { words });
        }

        Mnemonic::parse_normalized(&normalised.to_lowercase())
            .map(Self)
            .map_err(|err| PhraseError::Invalid {
                reason: err.to_string(),
            })
    }

    /// The seed this phrase encodes.
    ///
    /// # Errors
    ///
    /// [`PhraseError::Invalid`] if the entropy is not 32 bytes, which a
    /// 24-word phrase guarantees.
    pub fn seed(&self) -> Result<[u8; SEED_BYTES], PhraseError> {
        let (entropy, length) = self.0.to_entropy_array();
        entropy[..length]
            .try_into()
            .map_err(|_| PhraseError::Invalid {
                reason: format!("phrase encodes {length} bytes of entropy, expected {SEED_BYTES}"),
            })
    }

    /// The phrase as a space-separated string.
    #[must_use]
    pub fn to_words(&self) -> String {
        self.0.words().collect::<Vec<_>>().join(" ")
    }

    /// The words, for displaying in a numbered grid.
    #[must_use]
    pub fn words(&self) -> Vec<&'static str> {
        self.0.words().collect()
    }
}

impl std::fmt::Debug for RecoveryPhrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A phrase IS the private key. It must never appear in a log line or a
        // panic message just because something derived Debug upstream.
        f.write_str("RecoveryPhrase(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    #[test]
    fn a_phrase_round_trips_to_the_same_identity() {
        // THE recovery guarantee: write down 24 words, type them on a new
        // machine, get the same identity back.
        let original = Identity::generate().expect("generate");
        let phrase = original.recovery_phrase().expect("phrase");

        let restored = Identity::from_seed(
            &RecoveryPhrase::parse(&phrase.to_words())
                .expect("parse")
                .seed()
                .expect("seed"),
        );

        assert_eq!(original.public_key(), restored.public_key());
        assert_eq!(original.uuid_as_root(), restored.uuid_as_root());
    }

    #[test]
    fn a_phrase_is_twenty_four_words() {
        let phrase = Identity::generate()
            .expect("generate")
            .recovery_phrase()
            .expect("phrase");
        assert_eq!(phrase.words().len(), RecoveryPhrase::WORD_COUNT);
    }

    #[test]
    fn a_single_wrong_word_is_caught_rather_than_producing_a_stranger() {
        // The reason the checksum exists. Without it this would silently yield a
        // different valid identity — someone else's UUID, an empty account, and
        // no indication anything went wrong.
        let phrase = Identity::generate()
            .expect("generate")
            .recovery_phrase()
            .expect("phrase");
        let mut words = phrase.words();

        // Swap the last word for a different valid wordlist entry, so the only
        // thing wrong is the checksum.
        let replacement = if words[23] == "zoo" { "zone" } else { "zoo" };
        words[23] = replacement;

        let result = RecoveryPhrase::parse(&words.join(" "));
        assert!(
            result.is_err(),
            "a wrong word must fail rather than produce a different identity"
        );
        assert!(
            result.unwrap_err().to_string().contains("silently"),
            "the message should explain why this is caught"
        );
    }

    #[test]
    fn parsing_tolerates_the_way_people_actually_type() {
        let phrase = Identity::generate()
            .expect("generate")
            .recovery_phrase()
            .expect("phrase");
        let words = phrase.to_words();

        for variant in [
            words.to_uppercase(),
            format!("  {words}  "),
            words.replace(' ', "   "),
            words.replace(' ', "\n"),
        ] {
            let parsed = RecoveryPhrase::parse(&variant)
                .unwrap_or_else(|err| panic!("should tolerate this input: {err}"));
            assert_eq!(parsed.seed().expect("seed"), phrase.seed().expect("seed"));
        }
    }

    #[test]
    fn the_wrong_word_count_says_so_plainly() {
        let err = RecoveryPhrase::parse("word word word").expect_err("too short");
        assert!(matches!(err, PhraseError::WrongLength { words: 3 }));
        assert!(err.to_string().contains("24"), "{err}");
    }

    #[test]
    fn nonsense_is_rejected_without_panicking() {
        for text in ["", "   ", &"notaword ".repeat(24), &"\0".repeat(24)] {
            assert!(RecoveryPhrase::parse(text).is_err(), "`{text}` should fail");
        }
    }

    #[test]
    fn debug_never_prints_the_phrase() {
        // Deterministic seed, not a generated one. An earlier version of this
        // test generated a random phrase and asserted no individual word
        // appeared in the Debug output — which is flaky, because the BIP-39
        // wordlist contains `act` and `cover`, both substrings of
        // "RecoveryPhrase(<redacted>)". It failed roughly one run in fifty and
        // passed everywhere until CI drew an unlucky phrase.
        //
        // The property that actually matters is that the output is the fixed
        // redacted literal and contains none of the phrase, so that is what is
        // asserted — no per-word substring search, which was never the right
        // test.
        let phrase = Identity::from_seed(&[0x42; SEED_BYTES])
            .recovery_phrase()
            .expect("phrase");
        let printed = format!("{phrase:?}");

        assert_eq!(printed, "RecoveryPhrase(<redacted>)");
        assert!(
            !printed.contains(&phrase.to_words()),
            "the phrase leaked into Debug"
        );
    }

    #[test]
    fn different_identities_get_different_phrases() {
        let a = Identity::generate()
            .expect("generate")
            .recovery_phrase()
            .expect("phrase");
        let b = Identity::generate()
            .expect("generate")
            .recovery_phrase()
            .expect("phrase");
        assert_ne!(a.to_words(), b.to_words());
    }
}
