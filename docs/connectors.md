<!--
SPDX-FileCopyrightText: Iridesium
SPDX-License-Identifier: GPL-3.0-only
-->

# Connectors: driving a client from another program

`watcher` is a headless client that reports what it sees as JSON lines and takes
instructions the same way. It exists so a program — an AI harness, a script, a
test rig, another engine — can watch a session and, on a server you run
yourself, take part in one.

```
cargo run --bin watcher -- \
    --server 127.0.0.1:4433 \
    --fingerprint <64 hex characters> \
    --name claude \
    --allow-acting
```

The fingerprint is on the server's own first log line: *"server certificate
ready — clients pin this on first connection"*.

## What it is, and what it deliberately is not

It is **an ordinary client speaking the ordinary protocol with its own
identity**. It sees what any player sees, it can do what any player can do, and
it appears in chat and on everybody's screen under the name you give it. There
is no privileged channel and no back door: a connector cannot reach a server
that would not let a person in.

It is **not** a client for anybody's inference API. There is no HTTP, no API
key, and no vendor. Three reasons, and all of them hold whatever the model is:

- A game engine has no business making outbound requests to a vendor. That is a
  dependency, a licence question (`cargo deny` gates every one), an egress path,
  and a place to keep somebody's secret — none of which the engine gains
  anything by owning.
- The interesting harness is the one you already have. Anything that can read
  and write lines can drive this; picking a vendor would exclude all of them.
- It stays honest about what it is. A pipe is inspectable. You can watch every
  line go past, and run the thing by hand with `cat`.

## Acting is opt-in, and local only

Watching any server is fine — that is what a spectator does. **Acting requires
`--allow-acting` and a loopback address**, and the address is checked rather
than taken on trust: a connector pointed at somebody else's server is a bot on
their world, and whether that is welcome is their decision. On a server you host
yourself you are the admin, and it is yours to allow.

A refused instruction says so on stdout rather than vanishing. A harness whose
instructions disappear will keep sending them, and the person running it will
conclude the connector is broken rather than that it is behaving.

## The protocol

One JSON object per line, both directions, flushed per line.

### Out

| Line | When |
| --- | --- |
| `{"event":"joined","name":…,"address":…,"acting":bool}` | once, on the way in |
| `{"event":"chat","text":…}` | somebody said something |
| `{"event":"inventory","stacks":[{"material":…,"units":…,"shape":…}]}` | what it is carrying changed |
| `{"event":"entities","list":[{"id":…,"model":…,"name":…,"x":…,"y":…,"z":…}]}` | something came into view |
| `{"event":"refused","text":…}` | an instruction needed `--allow-acting` |
| `{"event":"error","text":…}` | a bad instruction, or something went wrong |
| `{"event":"closed","text":…}` | the connection ended |

Positions are in **blocks**, as a floating-point world coordinate. The
chunk-and-cell split of charter rule 7 is the engine's problem, not something to
make a model do arithmetic on.

Chunks, lighting and fluid are deliberately **not** reported. A connector that
emitted every chunk would drown its own chat lines in terrain, and terrain is
not what somebody watching a session is watching.

### In

| Line | Acts? |
| --- | --- |
| `{"do":"say","text":"hello"}` | yes |
| `{"do":"walk","x":1.0,"y":0.0,"z":0.0,"ticks":20}` | yes |
| `{"do":"dig","x":1,"y":2,"z":3}` | yes |
| `{"do":"place","x":1,"y":2,"z":3,"material":2}` | yes |
| `{"do":"quit"}` | no |

`ticks` defaults to 20 — one second — because a harness asking to walk usually
means "a bit" rather than a number it has thought about. An unknown verb is an
error rather than something guessed at.

Closing the pipe stops the connector. A watcher with nobody to watch for is a
process nobody remembered to kill.

## Driving it by hand

```
mkfifo /tmp/w
cargo run --bin watcher -- --server 127.0.0.1:4433 --fingerprint … < /tmp/w &
echo '{"do":"say","text":"hello"}' > /tmp/w
```

## What is deliberately missing

- **No raycast.** The connector cannot ask what is under a crosshair, because it
  has no crosshair. Give it coordinates.
- **No world reads.** It receives chunks like any client and does not expose
  them. A harness that needs to know what a block is should ask the server
  through a mod, which is the API that exists for asking questions about a world
  (charter rule 1).
- **No privileged commands.** Teleporting, spawning and banning are RCON's, and
  RCON already exists and is already authenticated.
