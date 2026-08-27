// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Listening for worlds on the local network.
//!
//! **Reported from the window**: "I don't want kids to have to type in a LAN
//! server address. I want them to be able to detect LAN servers." The host half
//! is `tiamot_server::announce`; the format, and why none of it is trusted, is
//! [`tiamot_core::discover`].
//!
//! # Why this holds a thread and not a future
//!
//! The front screen is drawn from the winit event loop, which has no executor.
//! One blocking socket on one thread, handing finished beacons back through a
//! mutex, is the whole mechanism — and a listener that cannot open its port
//! must not stop a player using the menu, so failing to start is a silence
//! rather than an error.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tiamot_core::discover::{Beacon, MAX_DATAGRAM, PORT, STALE_MS};

/// A world heard on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Where to connect, built from where the datagram came FROM and the port
    /// the beacon named. A host does not have to know its own address for this
    /// to be right, which is just as well, because behind a router it does not.
    pub address: SocketAddr,
    /// What the host calls it. Untrusted, and filtered by the decoder.
    pub name: String,
    /// Players on it now.
    pub players: u16,
    /// Players it will take.
    pub max_players: u16,
    /// Whether this client and that server speak the same protocol.
    ///
    /// Shown rather than hidden: "that world is a different version" is a
    /// better answer than a world that is missing from the list.
    pub compatible: bool,
}

/// How many worlds are kept at once.
///
/// A cap because the list is filled by whatever is on the network. Fifty is
/// more LAN worlds than a room has ever had, and it bounds what a machine
/// spraying beacons can make this client hold.
const MAX_FOUND: usize = 50;

/// A running listener. Dropping it stops the thread.
pub struct Discovery {
    found: Arc<Mutex<BTreeMap<SocketAddr, (Found, Instant)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Discovery {
    /// Starts listening, or returns `None` if the port cannot be opened.
    ///
    /// `None` is an ordinary outcome — another client on this machine may hold
    /// the port — and it means the network list stays empty, not that anything
    /// is broken.
    #[must_use]
    pub fn start() -> Option<Self> {
        let socket = bind().ok()?;
        // So the thread notices it has been stopped rather than parking in
        // `recv_from` until the next beacon, which on a quiet network is never.
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok()?;

        let found: Arc<Mutex<BTreeMap<SocketAddr, (Found, Instant)>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let writing = Arc::clone(&found);
        let stopping = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("tiamot-discovery".to_owned())
            .spawn(move || {
                // **Sized once, outside the loop.** A buffer allocated per
                // datagram would let anything on the network set this client's
                // allocation rate.
                let mut buffer = [0u8; MAX_DATAGRAM];
                while !stopping.load(Ordering::Relaxed) {
                    let Ok((read, from)) = socket.recv_from(&mut buffer) else {
                        continue;
                    };
                    let Some(beacon) = Beacon::decode(&buffer[..read.min(MAX_DATAGRAM)]) else {
                        continue;
                    };
                    let address = SocketAddr::new(from.ip(), beacon.port);
                    let Ok(mut found) = writing.lock() else {
                        return;
                    };
                    // The cap applies to NEW entries only, so a host already in
                    // the list keeps being refreshed rather than aging out
                    // because something else filled the table.
                    if found.len() >= MAX_FOUND && !found.contains_key(&address) {
                        continue;
                    }
                    found.insert(
                        address,
                        (
                            Found {
                                address,
                                name: beacon.name,
                                players: beacon.players,
                                max_players: beacon.max_players,
                                compatible: beacon.protocol == tiamot_core::proto::PROTOCOL_VERSION,
                            },
                            Instant::now(),
                        ),
                    );
                }
            })
            .ok()?;

        Some(Self {
            found,
            stop,
            thread: Some(thread),
        })
    }

    /// What has been heard recently, by name then address.
    ///
    /// Sorted so the list does not reorder itself between frames: a row that
    /// moves under the pointer is a row somebody clicks by mistake.
    #[must_use]
    pub fn worlds(&self) -> Vec<Found> {
        let Ok(found) = self.found.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut worlds: Vec<Found> = found
            .values()
            .filter(|(_, heard)| now.duration_since(*heard) < Duration::from_millis(STALE_MS))
            .map(|(world, _)| world.clone())
            .collect();
        worlds.sort_by(|a, b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
        worlds
    }

    /// Drops worlds nothing has been heard from for a while.
    ///
    /// [`Self::worlds`] already hides them; this is what stops the table
    /// growing for the life of the session on a network with a lot of comings
    /// and goings.
    pub fn forget_stale(&self) {
        let Ok(mut found) = self.found.lock() else {
            return;
        };
        let now = Instant::now();
        found.retain(|_, (_, heard)| {
            now.duration_since(*heard) < Duration::from_millis(STALE_MS * 4)
        });
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery")
            .field("worlds", &self.worlds().len())
            .finish()
    }
}

/// Opens the discovery port so that more than one client on a machine can hear.
///
/// `SO_REUSEADDR` before the bind: a player hosting a world and a second client
/// on the same machine both want this port, and without it the second one gets
/// nothing and shows an empty list.
fn bind() -> std::io::Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT).into())?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends one beacon to the loopback broadcast and returns what it said.
    fn announce(name: &str, port: u16) -> std::io::Result<()> {
        let beacon = Beacon {
            protocol: tiamot_core::proto::PROTOCOL_VERSION,
            port,
            players: 1,
            max_players: 8,
            name: name.to_owned(),
        };
        let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
        socket.set_broadcast(true)?;
        let bytes = beacon.encode().expect("a valid name");
        socket.send_to(&bytes, SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)))?;
        Ok(())
    }

    #[test]
    fn a_world_that_announces_itself_turns_up_in_the_list() {
        let Some(listening) = Discovery::start() else {
            // A sandbox with no UDP, or a port already held. Not a failure:
            // the whole feature degrades to an empty list.
            eprintln!("skipped: the discovery port could not be opened here");
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let heard = loop {
            if announce("Ada's world", 47811).is_err() {
                eprintln!("skipped: this machine cannot send a datagram");
                return;
            }
            let worlds = listening.worlds();
            if let Some(found) = worlds.into_iter().find(|w| w.name == "Ada's world") {
                break found;
            }
            assert!(
                Instant::now() < deadline,
                "a beacon was sent every 100ms for five seconds and none was heard"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(
            heard.address.port(),
            47811,
            "the address should carry the port the beacon named, not the one it came from"
        );
        assert!(heard.compatible, "the same build should read as compatible");
        assert_eq!(heard.players, 1);
    }

    #[test]
    fn a_datagram_that_is_not_a_beacon_adds_nothing() {
        let Some(listening) = Discovery::start() else {
            eprintln!("skipped: the discovery port could not be opened here");
            return;
        };
        let Ok(socket) = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))) else {
            eprintln!("skipped: this machine cannot send a datagram");
            return;
        };
        let before = listening.worlds().len();
        for junk in [&b"hello"[..], &[0xff; 400], &[]] {
            let _ = socket.send_to(junk, SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            listening.worlds().len(),
            before,
            "something that is not a beacon was listed as a world"
        );
    }
}
