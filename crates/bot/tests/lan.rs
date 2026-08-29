// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A world opened to the network is reachable from it.
//!
//! **Reported from the window**: "I need a way to open a LAN server from the
//! menu before I join the game, so people can host LAN servers at home."
//!
//! The client's front screen ticks a box and the world it starts binds to every
//! interface instead of loopback. What this file checks is the half that can
//! silently be wrong: that binding is what actually decides reachability, and
//! that the default has not quietly become "open".

use std::path::{Path, PathBuf};

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-lan").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
        .join("game")
}

/// Where to dial a server that may be listening on everything.
///
/// **The unspecified address is somewhere to listen, not somewhere to connect**
/// — QUIC refuses it outright. The client hit exactly this the first time a
/// world was opened to the LAN: it could not join its own world. See
/// `own_address` in the client.
fn dial(listening: std::net::SocketAddr) -> std::net::SocketAddr {
    if listening.ip().is_unspecified() {
        std::net::SocketAddr::from(([127, 0, 0, 1], listening.port()))
    } else {
        listening
    }
}

/// The discovery port, opened the way a client opens it.
///
/// `SO_REUSEADDR` before the bind, because the port is shared by construction
/// and every client test that builds a front screen has one open. A broadcast
/// is delivered to every socket bound to the port, so sharing costs this test
/// nothing.
fn listen_on_the_discovery_port() -> std::net::UdpSocket {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .expect("a UDP socket");
    socket
        .set_reuse_address(true)
        .expect("the discovery port is shared by construction");
    socket
        .bind(&std::net::SocketAddr::from(([0, 0, 0, 0], tiamot_core::discover::PORT)).into())
        .expect("the discovery port");
    // **The copy a world hosted on this machine sends.** A limited broadcast is
    // not handed back to the sender on every platform — macOS does not — so
    // this is the only datagram guaranteed to arrive here, and a listener that
    // did not join the group would be testing broadcast loopback rather than
    // announcing. Joined here exactly as `client::discovery::bind` joins it.
    socket
        .join_multicast_v4(
            &tiamot_core::discover::GROUP,
            &std::net::Ipv4Addr::UNSPECIFIED,
        )
        .expect("a world hosted here should be audible here");
    socket.into()
}

/// A world bound the way the front screen binds one.
fn start(name: &str, bind: &str, max_players: u32) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: bind.parse().expect("an address"),
        world_path: scratch(name),
        max_players,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(3),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

#[test]
fn a_world_opened_to_the_network_takes_more_than_one_player() {
    // The "Open to LAN" tick box binds every interface and raises the player
    // cap: one is right for a world nobody else can reach and a baffling
    // refusal for one they can.
    let server = start("open", "0.0.0.0:0", 8);
    block_on(async {
        let mut first = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        first.join("First").await.expect("join");

        let mut second = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("a second machine should be able to connect");
        second
            .join("Second")
            .await
            .expect("a world open to the network takes a second player");

        first.disconnect().await;
        second.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_world_kept_to_this_machine_admits_one_player() {
    // The default. A second player is refused rather than crashing anything —
    // which is what makes the tick box mean something.
    let server = start("closed", "127.0.0.1:0", 1);
    block_on(async {
        let mut first = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        first.join("First").await.expect("join");

        let mut second = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        assert!(
            second.join("Second").await.is_err(),
            "a world kept to one machine let a second player in"
        );

        first.disconnect().await;
    });
    server.stop();
}

#[test]
fn an_open_world_says_it_is_here_on_the_network() {
    // **The other half of the report**: "I don't want kids to have to type in
    // a LAN server address. I want them to be able to detect LAN servers."
    // Binding to every interface makes a world reachable; this is what makes
    // it findable.
    //
    // Listened for with a plain socket and decoded with the engine's own
    // parser, so what is checked is the datagram that actually goes out and
    // not a round trip through the code that sent it.
    let server = start("announced", "127.0.0.1:0", 8);
    // **`SO_REUSEADDR`, the way the client binds it.** A plain bind here loses
    // to whichever other test binary `cargo` is running that already holds the
    // discovery port — and `client::front` opens one in every test that builds
    // a front screen — so this test used to skip itself instead of running.
    // It skipped on macOS for six CI runs while reporting nothing at all, and
    // the moment it stopped skipping it failed: a limited broadcast is not
    // handed back to the machine that sent it on a BSD. Skipping is not a
    // neutral outcome, and a test that can skip on a whole platform is a test
    // that is not run there.
    let listening = listen_on_the_discovery_port();
    listening
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .expect("a read timeout");

    // A name nothing else uses. **The discovery port is shared by
    // construction** — that is what it is for — so this listener hears every
    // other test binary `cargo` is running at the same time, and a beacon is
    // only this test's if it says so.
    let announcing = server
        .announce("an announced world under test")
        .expect("a world should be able to announce itself");

    let mut buffer = [0u8; tiamot_core::discover::MAX_DATAGRAM];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let heard = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "an announcing world sent nothing in ten seconds"
        );
        let Ok((read, _)) = listening.recv_from(&mut buffer) else {
            continue;
        };
        if let Some(beacon) = tiamot_core::discover::Beacon::decode(&buffer[..read])
            && beacon.name == "an announced world under test"
        {
            break beacon;
        }
    };

    assert_eq!(heard.name, "an announced world under test");
    assert_eq!(
        heard.port,
        server.local_addr().port(),
        "the beacon named a port nothing is listening on"
    );
    assert_eq!(heard.max_players, 8);
    assert_eq!(
        heard.protocol,
        tiamot_core::proto::PROTOCOL_VERSION,
        "a beacon that did not say which protocol it speaks"
    );

    // **And it stops.** A beacon outliving its server advertises an address
    // nothing answers on, which is a worse experience than not being listed:
    // the player picks it and waits for a timeout.
    announcing.stop();
    let quiet_from = std::time::Instant::now();
    let mut last_heard = None;
    while std::time::Instant::now() - quiet_from < std::time::Duration::from_secs(3) {
        let Ok((read, _)) = listening.recv_from(&mut buffer) else {
            continue;
        };
        if tiamot_core::discover::Beacon::decode(&buffer[..read])
            .is_some_and(|beacon| beacon.name == "an announced world under test")
        {
            last_heard = Some(std::time::Instant::now());
        }
    }
    if let Some(last) = last_heard {
        assert!(
            last - quiet_from < std::time::Duration::from_millis(1_500),
            "the world was still announcing itself more than a second after being stopped"
        );
    }

    server.stop();
}

#[test]
fn a_beacon_sent_to_the_group_is_heard_on_the_machine_that_sent_it() {
    // **The property macOS needs, tested on its own.**
    // `an_open_world_says_it_is_here_on_the_network` drives the real announcer,
    // but on Linux it can be satisfied by the broadcast copy alone — so on the
    // one platform where the group copy is the ONLY one that arrives, the
    // mechanism would be untested until CI failed.
    //
    // This asks the narrow question directly: does a socket that has joined
    // `discover::GROUP` hear a datagram this machine sent to it? That is what
    // `IP_MULTICAST_LOOP` promises on all three platforms, and it is the whole
    // reason a host announces to a group as well as to the broadcast address.
    //
    // A private port, because the shared one carries every other test binary's
    // traffic and this assertion is about one socket.
    const PORT: u16 = 47_899;

    let listening = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .expect("a UDP socket");
    listening.set_reuse_address(true).expect("a shared port");
    listening
        .bind(&std::net::SocketAddr::from(([0, 0, 0, 0], PORT)).into())
        .expect("a private port");
    listening
        .join_multicast_v4(
            &tiamot_core::discover::GROUP,
            &std::net::Ipv4Addr::UNSPECIFIED,
        )
        .expect("joining the group");
    let listening: std::net::UdpSocket = listening.into();
    listening
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("a read timeout");

    let sending = std::net::UdpSocket::bind(std::net::SocketAddr::from((
        std::net::Ipv4Addr::UNSPECIFIED,
        0,
    )))
    .expect("a sending socket");
    sending.set_multicast_loop_v4(true).expect("loop it back");
    sending.set_multicast_ttl_v4(1).expect("one hop");

    let beacon = tiamot_core::discover::Beacon {
        protocol: tiamot_core::proto::PROTOCOL_VERSION,
        port: 47_811,
        players: 1,
        max_players: 8,
        name: "a world on this very machine".to_owned(),
    };
    let bytes = beacon.encode().expect("a valid name");
    sending
        .send_to(
            &bytes,
            std::net::SocketAddr::from((tiamot_core::discover::GROUP, PORT)),
        )
        .expect("sending to the group");

    let mut buffer = [0u8; tiamot_core::discover::MAX_DATAGRAM];
    let (read, _) = listening.recv_from(&mut buffer).expect(
        "a datagram this machine sent to the group came back to nobody on it, so a client \
         could not see a world hosted beside it",
    );
    assert_eq!(
        tiamot_core::discover::Beacon::decode(&buffer[..read]),
        Some(beacon),
        "what came back through the group was not what went out"
    );
}
