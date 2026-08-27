// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Saying "there is a world here" on the local network.
//!
//! **Reported from the window**: "I don't want kids to have to type in a LAN
//! server address. I want them to be able to detect LAN servers." A host that
//! has opened its world repeats a small datagram; a client that is looking
//! lists what it hears. The format, and why nothing in it is trusted, is
//! [`tiamot_core::discover`].
//!
//! # Never on by default
//!
//! Announcing tells every machine on the segment that this port is open, so it
//! happens only when somebody asks — the same decision, made in the same place,
//! as binding the world to more than loopback.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tiamot_core::discover::{Beacon, INTERVAL_MS, PORT};
use tracing::{debug, warn};

use crate::sim::Control;
use crate::transport::endpoint::Shared;

/// A running announcer. Dropping it stops the beacon.
pub struct Announcer {
    control: Control,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Announcer {
    /// Starts announcing `name` for a server listening on `port`.
    ///
    /// Returns `None` if no socket could be opened — a machine with the port in
    /// use, or a sandbox with no broadcast. That is not a reason to fail to
    /// host: the world is reachable by address either way, and the launcher
    /// says so.
    #[must_use]
    pub fn start(name: &str, port: u16, shared: &Arc<Shared>) -> Option<Self> {
        // Encoded ONCE, so a name the format refuses stops the announcer here
        // rather than failing silently on every tick of a thread nobody reads.
        let beacon = Beacon {
            protocol: tiamot_core::proto::PROTOCOL_VERSION,
            port,
            players: 0,
            max_players: u16::try_from(shared.max_players).unwrap_or(u16::MAX),
            name: name.to_owned(),
        };
        if let Err(err) = beacon.encode() {
            warn!(%err, "not announcing this world on the network");
            return None;
        }

        // Bound to a port of the operating system's choosing: this socket only
        // ever sends. Binding to `PORT` would fight the listener a client on
        // the same machine has open on it.
        let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .inspect_err(|err| warn!(%err, "could not open a socket to announce on"))
            .ok()?;
        socket
            .set_broadcast(true)
            .inspect_err(|err| warn!(%err, "could not broadcast; not announcing"))
            .ok()?;

        // Its own control, so stopping the announcer does not stop the world
        // and stopping the world does not have to know about the announcer.
        let control = Control::new();
        let stopping = control.clone();
        let shared = Arc::clone(shared);
        let thread = std::thread::Builder::new()
            .name("tiamot-announce".to_owned())
            .spawn(move || {
                let to = SocketAddrV4::new(Ipv4Addr::BROADCAST, PORT);
                while !stopping.stopping() {
                    let beacon = Beacon {
                        players: u16::try_from(shared.players.load(Ordering::Relaxed))
                            .unwrap_or(u16::MAX),
                        ..beacon.clone()
                    };
                    // Checked above, and the only field that changed since is a
                    // number. A failure here is nothing to say out loud once a
                    // second.
                    if let Ok(bytes) = beacon.encode()
                        && let Err(err) = socket.send_to(&bytes, to)
                    {
                        // Ordinary on a machine between networks. Said at debug
                        // because it recurs every interval and is not an error
                        // anybody can act on.
                        debug!(%err, "a beacon did not go out");
                    }
                    // Slept in short steps so stopping is quick: a second-long
                    // sleep would hold up closing a world by up to a second,
                    // which reads as the menu hanging.
                    for _ in 0..INTERVAL_MS / 50 {
                        if stopping.stopping() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            })
            .inspect_err(|err| warn!(%err, "could not start the announcer"))
            .ok()?;

        Some(Self {
            control,
            thread: Some(thread),
        })
    }

    /// Stops announcing and waits for the thread to finish.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.control.stop();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("the announcer thread panicked");
        }
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for Announcer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Announcer")
            .field("running", &self.thread.is_some())
            .finish()
    }
}
