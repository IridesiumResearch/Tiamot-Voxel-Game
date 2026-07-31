// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The server-side connection state machine.
//!
//! One [`Session`] per connected peer, from `Hello` to in-world. It owns the
//! join flow, identity verification, and name binding — everything that decides
//! *whether* a peer may act, as opposed to *how* their bytes arrive.
//!
//! # Why this is separate from the transport
//!
//! Every rule worth testing lives here: version negotiation, challenge
//! verification, name-theft rejection, allowlist enforcement, and the ordering
//! constraint that **no world state flows before identity is proven**. None of
//! that involves sockets.
//!
//! Keeping it a pure state machine over messages means the identity suite runs
//! in-process, deterministically, in microseconds, and that adding QUIC later
//! cannot change any of the answers. The transport's job is to move bytes and
//! call [`Session::handle`].
//!
//! # The ordering constraint is the security property
//!
//! A peer in [`Phase::AwaitingHello`] or [`Phase::AwaitingAuth`] can reach
//! nothing but the handshake. Chunk data, block edits, chat, and key management
//! are all refused until [`Phase::InWorld`], and the check is the phase itself
//! rather than a flag someone can forget to test.

mod registry;

pub use registry::{IdentityRegistry, NameBinding, RegistryError};

use crate::identity::{
    Allowlist, AuthProvider, NONCE_BYTES, PlayerUuid, generate_nonce, public_key_from_bytes,
    signature_from_bytes,
};
use crate::proto::{
    ClientMessage, DisconnectReason, ModEntry, PROTOCOL_VERSION, ServerMessage,
    validate_client_message, version_compatible,
};

/// How far through the join flow a connection is.
///
/// The ordering here *is* the access control — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing received yet.
    AwaitingHello,
    /// Challenge sent; waiting for the client to prove it holds the key.
    AwaitingAuth,
    /// Identity proven. Client may fetch content and ask to join.
    Authenticated,
    /// In the world. Gameplay messages are accepted.
    InWorld,
    /// Closed. Nothing further is accepted.
    Closed,
}

/// What the transport should do after [`Session::handle`].
#[derive(Debug)]
pub struct Response {
    /// Messages to send, in order.
    pub send: Vec<ServerMessage>,
    /// Whether to close the connection after sending.
    pub close: bool,
}

impl Response {
    /// Send nothing, stay open.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            send: Vec::new(),
            close: false,
        }
    }

    /// Send one message, stay open.
    #[must_use]
    pub fn reply(message: ServerMessage) -> Self {
        Self {
            send: vec![message],
            close: false,
        }
    }

    /// Send a disconnect and close.
    #[must_use]
    pub fn disconnect(reason: DisconnectReason) -> Self {
        Self {
            send: vec![ServerMessage::Disconnect { reason }],
            close: true,
        }
    }
}

/// What the server needs to know to run a join flow.
pub struct JoinContext<'a> {
    /// `BLAKE3` of the server's self-signed certificate.
    ///
    /// Bound into the challenge signature, so a signature made for one server
    /// cannot be relayed to another.
    pub cert_fingerprint: &'a [u8; 32],
    /// The resolved mod set, from Task 05.
    pub mods: &'a [ModEntry],
    /// The mod set's fingerprint.
    pub mod_set_fingerprint: u64,
    /// Who is permitted to join.
    pub allowlist: &'a Allowlist,
    /// Maximum simultaneous players.
    pub max_players: u32,
    /// How many are currently connected.
    pub current_players: u32,
    /// Where a new player starts.
    pub spawn: crate::coords::BlockPos,
    /// The server's current tick.
    pub tick: u64,
}

/// One peer's connection state.
pub struct Session {
    phase: Phase,
    nonce: Option<[u8; NONCE_BYTES]>,
    claimed_key: Option<ed25519_dalek::VerifyingKey>,
    claimed_name: Option<String>,
    uuid: Option<PlayerUuid>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// A fresh connection.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Phase::AwaitingHello,
            nonce: None,
            claimed_key: None,
            claimed_name: None,
            uuid: None,
        }
    }

    /// Current phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The verified identity, once there is one.
    ///
    /// `None` before authentication — which is the point: nothing downstream
    /// can accidentally act on a claimed-but-unproven identity, because there
    /// is nothing to act on.
    #[must_use]
    pub const fn uuid(&self) -> Option<PlayerUuid> {
        self.uuid
    }

    /// The bound display name, once in world.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.claimed_name.as_deref()
    }

    /// Handles one decoded client message.
    ///
    /// The message is validated against the protocol's field limits first, so
    /// nothing oversized reaches the logic below.
    pub fn handle(
        &mut self,
        message: &ClientMessage,
        context: &JoinContext<'_>,
        auth: &dyn AuthProvider,
        registry: &mut IdentityRegistry,
    ) -> Response {
        if self.phase == Phase::Closed {
            return Response::none();
        }

        if let Err(err) = validate_client_message(message) {
            return self.close_with(err.to_disconnect());
        }

        match (self.phase, message) {
            (Phase::AwaitingHello, ClientMessage::Hello { .. }) => {
                self.handle_hello(message, context, registry)
            }
            (Phase::AwaitingAuth, ClientMessage::AuthResponse { signature }) => {
                self.handle_auth(signature, context, auth, registry)
            }
            (Phase::Authenticated, ClientMessage::JoinWorld) => self.handle_join(context),
            (Phase::Authenticated | Phase::InWorld, ClientMessage::ContentRequest { .. }) => {
                // Content transfer is served by the transport layer, which owns
                // the file cache. Accepted here so the phase check is in one
                // place.
                Response::none()
            }
            (_, ClientMessage::Disconnect) => {
                self.phase = Phase::Closed;
                Response {
                    send: Vec::new(),
                    close: true,
                }
            }

            // Gameplay before the world: refused by phase, not by a flag.
            (_, message) => self.refuse_out_of_phase(message),
        }
    }

    fn handle_hello(
        &mut self,
        message: &ClientMessage,
        context: &JoinContext<'_>,
        registry: &IdentityRegistry,
    ) -> Response {
        let ClientMessage::Hello {
            protocol_version,
            public_key,
            display_name,
        } = message
        else {
            return self.refuse_out_of_phase(message);
        };

        // Version first. Everything below assumes both ends agree on what the
        // bytes mean, and a mismatch should say so rather than surfacing as a
        // decode error three messages later.
        if !version_compatible(*protocol_version) {
            return self.close_with(DisconnectReason::VersionMismatch {
                server: PROTOCOL_VERSION,
                client: *protocol_version,
            });
        }

        if context.current_players >= context.max_players {
            return self.close_with(DisconnectReason::ServerFull {
                max_players: context.max_players,
            });
        }

        let Ok(key) = public_key_from_bytes(public_key) else {
            return self.close_with(DisconnectReason::AuthFailed {
                detail: "malformed public key".to_owned(),
            });
        };

        // The name is checked here, BEFORE the challenge, so a client learns it
        // cannot have the name without doing the signing work. The check is
        // against the claimed key's identity, and it is re-checked after
        // verification — a claim is not a proof, and the pre-check is only a
        // courtesy.
        if let Some(holder) = registry.name_holder(display_name) {
            let claimed_identity = registry.identity_of_key(&key);
            if claimed_identity != Some(holder) {
                return self.close_with(DisconnectReason::NameTaken {
                    name: display_name.clone(),
                });
            }
        }

        let Ok(nonce) = generate_nonce() else {
            return self.close_with(DisconnectReason::AuthFailed {
                detail: "server could not generate a challenge".to_owned(),
            });
        };

        self.nonce = Some(nonce);
        self.claimed_key = Some(key);
        self.claimed_name = Some(display_name.clone());
        self.phase = Phase::AwaitingAuth;

        Response {
            send: vec![
                ServerMessage::HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                    cert_fingerprint: *context.cert_fingerprint,
                },
                ServerMessage::AuthChallenge { nonce },
            ],
            close: false,
        }
    }

    fn handle_auth(
        &mut self,
        signature: &crate::proto::WireSignature,
        context: &JoinContext<'_>,
        auth: &dyn AuthProvider,
        registry: &mut IdentityRegistry,
    ) -> Response {
        let (Some(nonce), Some(key)) = (self.nonce, self.claimed_key) else {
            return self.close_with(DisconnectReason::AuthFailed {
                detail: "no challenge outstanding".to_owned(),
            });
        };

        let Ok(signature) = signature_from_bytes(&signature.0) else {
            return self.close_with(DisconnectReason::AuthFailed {
                detail: "malformed signature".to_owned(),
            });
        };

        // The nonce is consumed whatever happens. A failed attempt must not
        // leave a live challenge for a second try — that would turn one
        // captured signature into unlimited retries.
        self.nonce = None;

        // Verify against the registry by immutable reborrow, THEN mutate it to
        // bind the name. Sequencing rather than holding both at once.
        // The error is discarded, not matched: EVERY failure must collapse to
        // the same message. Distinguishing "unknown key" from "bad signature"
        // would make this an oracle for probing which identities a server
        // knows, and discarding means a new error variant cannot accidentally
        // acquire a distinguishable message later.
        let Ok(uuid) = auth.verify(
            registry,
            &key,
            &nonce,
            context.cert_fingerprint,
            PROTOCOL_VERSION,
            &signature,
        ) else {
            return self.close_with(DisconnectReason::AuthFailed {
                detail: "authentication failed".to_owned(),
            });
        };

        if !context.allowlist.permits(&uuid) {
            return self.close_with(DisconnectReason::NotAllowlisted);
        }

        // Re-check the name against the PROVEN identity. The pre-check in
        // `handle_hello` used a claimed key; this one is authoritative.
        let name = self.claimed_name.clone().unwrap_or_default();
        match registry.bind_name(&name, uuid) {
            Ok(()) => {}
            Err(RegistryError::NameTaken { .. }) => {
                return self.close_with(DisconnectReason::NameTaken { name });
            }
            Err(err) => {
                return self.close_with(DisconnectReason::AuthFailed {
                    detail: err.to_string(),
                });
            }
        }

        self.uuid = Some(uuid);
        self.phase = Phase::Authenticated;

        Response::reply(ServerMessage::ModManifest {
            mods: context.mods.to_vec(),
            set_fingerprint: context.mod_set_fingerprint,
        })
    }

    fn handle_join(&mut self, context: &JoinContext<'_>) -> Response {
        let Some(uuid) = self.uuid else {
            return self.close_with(DisconnectReason::AuthFailed {
                detail: "not authenticated".to_owned(),
            });
        };
        self.phase = Phase::InWorld;
        Response::reply(ServerMessage::JoinWorld {
            player_uuid: *uuid.as_bytes(),
            spawn: context.spawn,
            tick: context.tick,
        })
    }

    /// Refuses a message that arrived in the wrong phase.
    fn refuse_out_of_phase(&mut self, message: &ClientMessage) -> Response {
        let what = match message {
            ClientMessage::Hello { .. } => "Hello",
            ClientMessage::AuthResponse { .. } => "AuthResponse",
            ClientMessage::ContentRequest { .. } => "ContentRequest",
            ClientMessage::JoinWorld => "JoinWorld",
            ClientMessage::PlayerInput { .. } => "PlayerInput",
            ClientMessage::BlockDelta { .. } => "BlockDelta",
            ClientMessage::Chat { .. } => "Chat",
            ClientMessage::AddKey { .. } => "AddKey",
            ClientMessage::RotateKey { .. } => "RotateKey",
            ClientMessage::Disconnect => "Disconnect",
        };
        self.close_with(DisconnectReason::ProtocolError {
            detail: format!("{what} is not valid in phase {:?}", self.phase),
        })
    }

    fn close_with(&mut self, reason: DisconnectReason) -> Response {
        self.phase = Phase::Closed;
        Response::disconnect(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::BlockPos;
    use crate::identity::{Identity, KeySet, SelfSovereign, challenge_payload};
    use crate::proto::WireSignature;

    const FINGERPRINT: [u8; 32] = [0xAB; 32];

    fn context<'a>(allowlist: &'a Allowlist, mods: &'a [ModEntry]) -> JoinContext<'a> {
        JoinContext {
            cert_fingerprint: &FINGERPRINT,
            mods,
            mod_set_fingerprint: 0xCAFE,
            allowlist,
            max_players: 50,
            current_players: 0,
            spawn: BlockPos::new(0, 1, 0),
            tick: 7,
        }
    }

    /// Drives a full successful join and returns the session.
    fn join(
        identity: &Identity,
        name: &str,
        registry: &mut IdentityRegistry,
        allowlist: &Allowlist,
    ) -> (Session, Vec<ServerMessage>) {
        let mods = Vec::new();
        let context = context(allowlist, &mods);
        let auth = SelfSovereign;
        let mut session = Session::new();
        let mut sent = Vec::new();

        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: *identity.public_key().as_bytes(),
            display_name: name.to_owned(),
        };
        let response = session.handle(&hello, &context, &auth, registry);
        sent.extend(response.send.iter().cloned());

        let Some(ServerMessage::AuthChallenge { nonce }) = response
            .send
            .iter()
            .find(|m| matches!(m, ServerMessage::AuthChallenge { .. }))
            .cloned()
        else {
            return (session, sent);
        };

        let signature = identity.sign(&challenge_payload(&nonce, &FINGERPRINT, PROTOCOL_VERSION));
        let auth_response = ClientMessage::AuthResponse {
            signature: WireSignature(signature.to_bytes()),
        };
        let response = session.handle(&auth_response, &context, &auth, registry);
        sent.extend(response.send.iter().cloned());

        if session.phase() == Phase::Authenticated {
            let response = session.handle(&ClientMessage::JoinWorld, &context, &auth, registry);
            sent.extend(response.send);
        }

        (session, sent)
    }

    fn registry_with(identity: &Identity) -> IdentityRegistry {
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(identity.public_key(), None, 0));
        registry
    }

    #[test]
    fn a_full_join_reaches_the_world() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();

        let (session, sent) = join(&alice, "Alice", &mut registry, &allowlist);

        assert_eq!(session.phase(), Phase::InWorld);
        assert_eq!(session.uuid(), Some(alice.uuid_as_root()));
        assert_eq!(session.display_name(), Some("Alice"));

        // The flow, in order, and nothing before it.
        assert!(matches!(sent[0], ServerMessage::HelloAck { .. }));
        assert!(matches!(sent[1], ServerMessage::AuthChallenge { .. }));
        assert!(matches!(sent[2], ServerMessage::ModManifest { .. }));
        assert!(matches!(sent[3], ServerMessage::JoinWorld { .. }));
    }

    #[test]
    fn no_world_state_flows_before_identity_is_proven() {
        // The ordering constraint, as an assertion. Everything a peer could ask
        // for before authenticating must be refused.
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let context = context(&allowlist, &mods);
        let auth = SelfSovereign;

        let premature = [
            ClientMessage::JoinWorld,
            ClientMessage::PlayerInput {
                tick: 0,
                movement: [0.0; 3],
                look: [0.0; 2],
                actions: 0,
            },
            ClientMessage::BlockDelta {
                edit: crate::proto::Edit::Block {
                    pos: BlockPos::new(0, 0, 0),
                    material: 2,
                },
            },
            ClientMessage::Chat {
                text: "hello".to_owned(),
            },
        ];

        for message in premature {
            let mut session = Session::new();
            let response = session.handle(&message, &context, &auth, &mut registry);
            assert!(
                response.close,
                "{message:?} should have closed the connection"
            );
            assert_eq!(session.phase(), Phase::Closed);
        }
    }

    #[test]
    fn a_version_mismatch_is_refused_with_both_versions_named() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let context = context(&allowlist, &mods);
        let auth = SelfSovereign;
        let mut session = Session::new();

        let response = session.handle(
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                public_key: *alice.public_key().as_bytes(),
                display_name: "Alice".to_owned(),
            },
            &context,
            &auth,
            &mut registry,
        );

        assert!(response.close);
        assert!(matches!(
            response.send.first(),
            Some(ServerMessage::Disconnect {
                reason: DisconnectReason::VersionMismatch { .. }
            })
        ));
    }

    #[test]
    fn a_replayed_signature_is_rejected() {
        // The nonce is fresh per connection, so a signature captured from one
        // session is useless in the next.
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let context = context(&allowlist, &mods);

        // First connection: capture the signature.
        let mut first = Session::new();
        let auth = SelfSovereign;
        let response = first.handle(
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                public_key: *alice.public_key().as_bytes(),
                display_name: "Alice".to_owned(),
            },
            &context,
            &auth,
            &mut registry,
        );
        let Some(ServerMessage::AuthChallenge { nonce }) = response.send.get(1).cloned() else {
            panic!("expected a challenge");
        };
        let captured = WireSignature(
            alice
                .sign(&challenge_payload(&nonce, &FINGERPRINT, PROTOCOL_VERSION))
                .to_bytes(),
        );

        // The signature is genuinely valid for the FIRST session — establish
        // that, or the replay rejection below could be hiding a bad signature.
        let accepted = first.handle(
            &ClientMessage::AuthResponse {
                signature: captured,
            },
            &context,
            &auth,
            &mut registry,
        );
        assert!(
            !accepted.close,
            "the captured signature must be valid where it was made"
        );
        assert_eq!(first.phase(), Phase::Authenticated);

        // Second connection: replay it against a fresh challenge. Same key, same
        // registry, same server — the ONLY thing that differs is the nonce.
        let mut second = Session::new();
        let _ = second.handle(
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                public_key: *alice.public_key().as_bytes(),
                display_name: "Alice".to_owned(),
            },
            &context,
            &auth,
            &mut registry,
        );
        let response = second.handle(
            &ClientMessage::AuthResponse {
                signature: captured,
            },
            &context,
            &auth,
            &mut registry,
        );

        assert!(response.close, "a replayed signature must be refused");
        assert_eq!(second.phase(), Phase::Closed);
    }

    #[test]
    fn a_failed_attempt_consumes_the_nonce() {
        // Otherwise one captured signature becomes unlimited retries against a
        // still-live challenge.
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let context = context(&allowlist, &mods);

        let mut session = Session::new();
        let auth = SelfSovereign;
        let _ = session.handle(
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                public_key: *alice.public_key().as_bytes(),
                display_name: "Alice".to_owned(),
            },
            &context,
            &auth,
            &mut registry,
        );

        let response = session.handle(
            &ClientMessage::AuthResponse {
                signature: WireSignature([0u8; 64]),
            },
            &context,
            &auth,
            &mut registry,
        );
        assert!(response.close);
        assert_eq!(session.phase(), Phase::Closed);
    }

    #[test]
    fn a_signature_for_a_different_server_is_rejected() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let context = context(&allowlist, &mods);

        let mut session = Session::new();
        let auth = SelfSovereign;
        let response = session.handle(
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                public_key: *alice.public_key().as_bytes(),
                display_name: "Alice".to_owned(),
            },
            &context,
            &auth,
            &mut registry,
        );
        let Some(ServerMessage::AuthChallenge { nonce }) = response.send.get(1).cloned() else {
            panic!("expected a challenge");
        };

        // Signed against another server's fingerprint — the MITM relay case.
        let wrong = alice.sign(&challenge_payload(
            &nonce,
            b"a-different-server",
            PROTOCOL_VERSION,
        ));
        let response = session.handle(
            &ClientMessage::AuthResponse {
                signature: WireSignature(wrong.to_bytes()),
            },
            &context,
            &auth,
            &mut registry,
        );

        assert!(
            response.close,
            "a signature bound to another server must be refused"
        );
    }

    #[test]
    fn a_second_identity_cannot_take_a_bound_name() {
        // The identity-theft case: Bob presents Alice's name with his own key.
        let alice = Identity::generate().expect("generate");
        let bob = Identity::generate().expect("generate");
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(alice.public_key(), None, 0));
        registry.insert(KeySet::new(bob.public_key(), None, 0));
        let allowlist = Allowlist::open();

        let (alice_session, _) = join(&alice, "Alice", &mut registry, &allowlist);
        assert_eq!(alice_session.phase(), Phase::InWorld);

        let (bob_session, sent) = join(&bob, "Alice", &mut registry, &allowlist);
        assert_eq!(bob_session.phase(), Phase::Closed);
        assert!(
            sent.iter().any(|m| matches!(
                m,
                ServerMessage::Disconnect {
                    reason: DisconnectReason::NameTaken { .. }
                }
            )),
            "Bob should be refused by name, got {sent:?}"
        );
    }

    #[test]
    fn the_same_identity_reclaims_its_own_name() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();

        let (first, _) = join(&alice, "Alice", &mut registry, &allowlist);
        assert_eq!(first.phase(), Phase::InWorld);

        // Reconnecting must work — the binding is hers.
        let (second, _) = join(&alice, "Alice", &mut registry, &allowlist);
        assert_eq!(second.phase(), Phase::InWorld);
        assert_eq!(second.uuid(), Some(alice.uuid_as_root()));
    }

    #[test]
    fn a_second_device_joins_as_the_same_identity_and_keeps_the_name() {
        // The whole point of key sets: a new device is a new key, not a new
        // player.
        let alice = Identity::generate().expect("generate");
        let laptop = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();

        let (_, _) = join(&alice, "Alice", &mut registry, &allowlist);

        // Authorise the laptop key.
        let uuid = alice.uuid_as_root();
        let payload = crate::identity::keyset::add_key_payload(&uuid, &laptop.public_key(), None);
        registry
            .add_key(
                &uuid,
                &alice.public_key(),
                laptop.public_key(),
                None,
                &alice.sign(&payload),
                1,
            )
            .expect("add key");

        let (session, _) = join(&laptop, "Alice", &mut registry, &allowlist);
        assert_eq!(session.phase(), Phase::InWorld);
        assert_eq!(
            session.uuid(),
            Some(uuid),
            "a second device must join as the SAME identity"
        );
    }

    #[test]
    fn an_identity_off_the_allowlist_is_refused() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::restricted([]);

        let (session, sent) = join(&alice, "Alice", &mut registry, &allowlist);
        assert_eq!(session.phase(), Phase::Closed);
        assert!(sent.iter().any(|m| matches!(
            m,
            ServerMessage::Disconnect {
                reason: DisconnectReason::NotAllowlisted
            }
        )));
    }

    #[test]
    fn an_allowlisted_identity_is_admitted() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::restricted([alice.uuid_as_root()]);

        let (session, _) = join(&alice, "Alice", &mut registry, &allowlist);
        assert_eq!(session.phase(), Phase::InWorld);
    }

    #[test]
    fn an_unknown_identity_is_refused_without_saying_why() {
        // The error must not distinguish "unknown key" from "bad signature", or
        // it becomes an oracle for probing which identities a server knows.
        let stranger = Identity::generate().expect("generate");
        let mut registry = IdentityRegistry::default();
        let allowlist = Allowlist::open();

        let (session, sent) = join(&stranger, "Nobody", &mut registry, &allowlist);
        assert_eq!(session.phase(), Phase::Closed);

        let Some(ServerMessage::Disconnect {
            reason: DisconnectReason::AuthFailed { detail },
        }) = sent.last()
        else {
            panic!("expected an auth failure, got {sent:?}");
        };
        assert_eq!(
            detail, "authentication failed",
            "the reason must not distinguish unknown-key from bad-signature"
        );
    }

    #[test]
    fn a_full_server_refuses_before_doing_crypto_work() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let mut context = context(&allowlist, &mods);
        context.current_players = context.max_players;

        let auth = SelfSovereign;
        let mut session = Session::new();
        let response = session.handle(
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                public_key: *alice.public_key().as_bytes(),
                display_name: "Alice".to_owned(),
            },
            &context,
            &auth,
            &mut registry,
        );
        assert!(matches!(
            response.send.first(),
            Some(ServerMessage::Disconnect {
                reason: DisconnectReason::ServerFull { .. }
            })
        ));
    }

    #[test]
    fn an_oversized_name_is_refused_by_the_protocol_limits() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let context = context(&allowlist, &mods);
        let auth = SelfSovereign;
        let mut session = Session::new();

        let response = session.handle(
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                public_key: *alice.public_key().as_bytes(),
                display_name: "x".repeat(1000),
            },
            &context,
            &auth,
            &mut registry,
        );
        assert!(response.close);
    }

    #[test]
    fn a_closed_session_ignores_everything_afterwards() {
        let alice = Identity::generate().expect("generate");
        let mut registry = registry_with(&alice);
        let allowlist = Allowlist::open();
        let mods = Vec::new();
        let context = context(&allowlist, &mods);
        let auth = SelfSovereign;
        let mut session = Session::new();

        let _ = session.handle(&ClientMessage::JoinWorld, &context, &auth, &mut registry);
        assert_eq!(session.phase(), Phase::Closed);

        let response = session.handle(
            &ClientMessage::Chat { text: "hi".into() },
            &context,
            &auth,
            &mut registry,
        );
        assert!(response.send.is_empty(), "a closed session must stay quiet");
    }
}
