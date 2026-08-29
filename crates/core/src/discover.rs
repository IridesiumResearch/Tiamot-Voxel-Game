// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Finding servers on the local network without typing an address.
//!
//! **Reported from the window**: "I don't want kids to have to type in a LAN
//! server address. I want them to be able to detect LAN servers." A host that
//! has opened its world says so on a UDP broadcast; a client that is looking
//! listens and lists what it hears.
//!
//! # This is a hint, not an authority
//!
//! A beacon says only "something at this address claims to be a Tiamot server
//! called this". Anything on the network can send one, so nothing here is
//! trusted: the address is a suggestion the player still has to accept, and the
//! name is a display string that goes through [`Beacon::decode`]'s filter
//! before anything shows it. Joining is the ordinary join, with the ordinary
//! certificate check.
//!
//! # A parser on an open port is hostile input (charter rule 14)
//!
//! Anyone on the network can send anything to [`PORT`]. So: a fixed magic that
//! costs nothing to reject, a hard cap on the datagram BEFORE it is parsed, a
//! name length checked against a cap and rejected rather than truncated, and no
//! allocation whose size the sender chooses. `fuzz_beacon` fuzzes this module.

/// The UDP port a beacon is sent to and listened for on.
///
/// One port for everybody, because a discovery port a player has to agree on
/// is an address they have to type — which is the thing this exists to remove.
pub const PORT: u16 = 47812;

/// The multicast group a host also sends to, so that a client on the SAME
/// machine hears it.
///
/// **A limited broadcast does not reliably come back to the machine that sent
/// it.** Linux and Windows hand it to local sockets listening on [`PORT`]; the
/// BSD under macOS does not, so a client on the hosting machine saw nothing at
/// all there. Multicast is the mechanism with the same answer everywhere:
/// `IP_MULTICAST_LOOP` is on by default on all three platforms and delivers a
/// copy to every socket on this machine that has joined the group.
///
/// It is sent IN ADDITION to the broadcast rather than instead of it. Broadcast
/// is what a home network delivers most dependably — consumer access points do
/// drop or rate-limit multicast — so the way other machines find a world is
/// left exactly as it was, and this covers only the case that was broken.
///
/// `239.255.0.0/16` is the administratively scoped IPv4 local scope of RFC
/// 2365: routable nowhere, which is the whole intent.
pub const GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 78, 12);

/// How often a host repeats itself.
///
/// Frequent enough that a client that has just opened the screen fills in
/// within a moment, and rare enough that fifty of them are nothing.
pub const INTERVAL_MS: u64 = 1_000;

/// How long a beacon is worth showing after it was last heard.
///
/// Three intervals: one missed datagram is ordinary on a busy network and
/// should not make a world flicker out of the list.
pub const STALE_MS: u64 = 3_500;

/// The largest datagram worth reading.
///
/// Checked before anything is parsed. A beacon is tens of bytes; this is room
/// to grow into and a hard stop for anything else.
pub const MAX_DATAGRAM: usize = 512;

/// The longest name a beacon may carry.
///
/// Refused rather than truncated: a truncated name is a different name, and
/// showing one as if the host had chosen it is a small lie in a list a player
/// picks from.
pub const MAX_NAME: usize = 48;

/// What every beacon starts with.
///
/// The trailing byte is the beacon format's own version, which is NOT the
/// protocol version — a client that cannot speak to a server should still be
/// able to see it and say so.
const MAGIC: [u8; 8] = *b"TIAMOTd\x01";

/// Bytes after the magic and before the name: protocol, port, players, maximum,
/// and the name's length.
const HEADER: usize = 4 + 2 + 2 + 2 + 1;

/// A host saying it is here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// The protocol version the host speaks.
    ///
    /// Carried so a client can show "this server is newer than you" rather
    /// than letting a player pick it and fail at the handshake.
    pub protocol: u32,
    /// The port to connect to.
    ///
    /// The ADDRESS is deliberately not in here: it is where the datagram came
    /// from, which cannot be got wrong by a host that does not know its own
    /// address — and every host behind a router does not.
    pub port: u16,
    /// How many players are on it.
    pub players: u16,
    /// How many it will take.
    pub max_players: u16,
    /// What to call it in the list.
    pub name: String,
}

impl Beacon {
    /// Encodes this beacon for the wire.
    ///
    /// # Errors
    ///
    /// [`BeaconError::Name`] if the name is too long or has characters a list
    /// cannot show — the same rule the decoder applies, so a host cannot send
    /// something its own reader would refuse.
    pub fn encode(&self) -> Result<Vec<u8>, BeaconError> {
        let name = check_name(&self.name)?;
        let mut out = Vec::with_capacity(MAGIC.len() + HEADER + name.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.protocol.to_le_bytes());
        out.extend_from_slice(&self.port.to_le_bytes());
        out.extend_from_slice(&self.players.to_le_bytes());
        out.extend_from_slice(&self.max_players.to_le_bytes());
        // One byte, and the cap is 48, so the length can never disagree with
        // what follows it.
        out.push(u8::try_from(name.len()).unwrap_or(0));
        out.extend_from_slice(name.as_bytes());
        Ok(out)
    }

    /// Reads a beacon out of a datagram, or `None` if it is not one.
    ///
    /// **Every failure is the same answer.** A caller cannot act differently on
    /// "wrong magic" and "bad name", and a listener that logged the difference
    /// would be a way for anything on the network to write to a log.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_DATAGRAM || bytes.len() < MAGIC.len() + HEADER {
            return None;
        }
        if bytes[..MAGIC.len()] != MAGIC {
            return None;
        }
        let body = &bytes[MAGIC.len()..];
        let field = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
        let length = body[HEADER - 1] as usize;
        // **The length must account for the whole datagram.** Allowing extra
        // after the name would let two different datagrams mean the same
        // beacon, which is a difference nothing looks at and everything has to
        // carry.
        if body.len() != HEADER + length {
            return None;
        }
        let name = std::str::from_utf8(&body[HEADER..]).ok()?;
        let checked = check_name(name).ok()?;
        // **A name the sender padded is a SECOND spelling of this beacon.**
        // `check_name` trims, which is right for a host naming its own world
        // and wrong here: accepting `" Home "` and reporting `"Home"` means two
        // datagrams mean one beacon, and the second is one this crate could
        // never have produced.
        //
        // Found by `beacon_decode` on its first run — the input was a name
        // beginning with a newline, which trims away and takes the length with
        // it.
        if checked.len() != name.len() {
            return None;
        }
        Some(Self {
            protocol: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            port: field(4),
            players: field(6),
            max_players: field(8),
            name: checked.to_owned(),
        })
    }
}

/// Why a beacon could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BeaconError {
    /// The name is too long, empty, or has characters a list cannot show.
    #[error("a server name must be 1 to {MAX_NAME} printable characters")]
    Name,
}

/// Checks a name against the one rule both ends apply.
///
/// Control characters are refused rather than stripped. A name is chosen by
/// somebody else's machine and drawn in this player's list, so it must not be
/// able to carry a newline, a terminal escape, or a direction override into it.
fn check_name(name: &str) -> Result<&str, BeaconError> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(BeaconError::Name);
    }
    if name.chars().any(|c| c.is_control() || is_bidi(c)) {
        return Err(BeaconError::Name);
    }
    Ok(name)
}

/// Whether a character can reorder the text around it.
///
/// These are not control characters as far as `char::is_control` is concerned,
/// and they can make a name in a list read as an entirely different one.
const fn is_bidi(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beacon(name: &str) -> Beacon {
        Beacon {
            protocol: 33,
            port: 47811,
            players: 2,
            max_players: 8,
            name: name.to_owned(),
        }
    }

    #[test]
    fn a_beacon_survives_the_wire() {
        let sent = beacon("Ada's world");
        let bytes = sent.encode().expect("encode");
        assert_eq!(Beacon::decode(&bytes), Some(sent));
    }

    #[test]
    fn anything_that_is_not_a_beacon_is_not_one() {
        assert_eq!(Beacon::decode(&[]), None, "an empty datagram");
        assert_eq!(Beacon::decode(b"hello there"), None, "somebody else's chat");
        assert_eq!(
            Beacon::decode(&vec![0u8; MAX_DATAGRAM + 1]),
            None,
            "an oversized datagram was parsed rather than dropped"
        );
        let good = beacon("Home").encode().expect("encode");
        for cut in 0..good.len() {
            assert_eq!(
                Beacon::decode(&good[..cut]),
                None,
                "a beacon cut at {cut} bytes decoded anyway"
            );
        }
        let mut trailing = good.clone();
        trailing.push(b'!');
        assert_eq!(
            Beacon::decode(&trailing),
            None,
            "trailing bytes were ignored, so one beacon has two spellings"
        );
        let mut wrong = good;
        wrong[3] = b'X';
        assert_eq!(Beacon::decode(&wrong), None, "the magic was not checked");
    }

    #[test]
    fn a_name_that_could_rewrite_the_list_is_refused_at_both_ends() {
        // The name is drawn in a list this player picks from, and it was
        // chosen by somebody else's machine. Refused rather than stripped: a
        // stripped name is a different name shown as if it were theirs.
        for bad in [
            "Home\nnot really",
            "Home\u{1b}[2J",
            "\u{202e}drowssap",
            "",
            "   ",
        ] {
            assert!(beacon(bad).encode().is_err(), "`{bad}` encoded");
        }
        let long = "n".repeat(MAX_NAME + 1);
        assert!(beacon(&long).encode().is_err(), "an over-long name encoded");

        // And the decoder does not rely on the encoder having refused: a
        // hostile sender writes the bytes itself.
        let mut hand_rolled = Vec::from(MAGIC);
        hand_rolled.extend_from_slice(&[33, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        hand_rolled.push(5);
        hand_rolled.extend_from_slice(b"a\nb\tc");
        assert_eq!(
            Beacon::decode(&hand_rolled),
            None,
            "a hand-written datagram got a control character into the list"
        );
    }

    #[test]
    fn a_name_is_trimmed_by_the_sender_and_never_by_the_reader() {
        // The host trims, so a world called `"  Home  "` goes out as `"Home"`.
        let bytes = beacon("  Home  ").encode().expect("encode");
        assert_eq!(
            Beacon::decode(&bytes).map(|found| found.name),
            Some("Home".to_owned())
        );

        // The reader does NOT, because trimming on the way in gives one beacon
        // two spellings — and the second is one this crate could not have
        // sent. **Found by the fuzzer on its first run**, as a name beginning
        // with a newline.
        let mut padded = Vec::from(MAGIC);
        padded.extend_from_slice(&[0; HEADER - 1]);
        padded.push(5);
        padded.extend_from_slice(b"\nHome");
        assert_eq!(
            Beacon::decode(&padded),
            None,
            "a padded name decoded, so one beacon has two spellings on the wire"
        );
    }
}
