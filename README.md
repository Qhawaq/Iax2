# iax2

A native Rust implementation of the **Inter-Asterisk eXchange v2** protocol
([RFC 5456](https://datatracker.ietf.org/doc/html/rfc5456)) — the protocol
Asterisk and FreePBX use to trunk and register endpoints over a single UDP port.

The crate is built around a **sans-io core**: all protocol logic (framing,
information elements, the registration/call state machine, reliability,
jitter buffering) is pure and synchronous, driven by `poll`/`handle` calls.
You bring your own I/O. Optional features add an async UDP driver and
cross-platform audio so you can go from "parse a frame" to "make a phone call"
without leaving the crate.

> **Status: experimental (0.x).** The protocol core has been validated against a
> live FreePBX/Asterisk instance — registration, inbound/outbound calls,
> qualify, per-call reliability, multi-PBX operation and the jitter buffer all
> work against real hardware. The public API is not yet stable and the codec
> support is intentionally narrow (see [Scope](#scope-and-limitations)).

## What works

- **Registration** with call-token pre-authentication and MD5 challenge/response
- **Outbound and inbound calls** (full call setup: NEW / AUTH / ACCEPT / ANSWER / HANGUP)
- **Qualify** (POKE/PONG) that keeps the peer reachable, with a rotating call
  number so Asterisk stops logging spurious notices
- **Per-call reliability**: real sliding-window retransmission with cumulative
  ACK — each call leg has its own independent window
- **Multi-call / multi-PBX**: N parallel registrations (one account per PBX),
  call waiting across PBXes, hold/unhold, call switching
- **Adaptive jitter buffer**: reorders by timestamp, adapts depth to measured
  network jitter (RFC 3550 estimate), and reports gaps for packet-loss concealment
- **G.711 µ-law** codec, with optional polyphase resampling to/from the device rate
- **LAGRQ/LAGRP** lag measurement and **comfort-noise** handling
- **DTMF** send and receive (RFC 5456 DTMF frames). Sending the feature codes
  your PBX expects lets you reach server-side features such as call transfer.

## Architecture

The core is sans-io and pulls in no heavy dependencies — if you only need the
protocol, you don't drag in tokio, cpal or rubato:

| module                 | feature   | contents                                            |
|------------------------|-----------|-----------------------------------------------------|
| `frame`, `ie`, `consts`| always    | frame / information-element encode & decode         |
| `g711`                 | always    | G.711 µ-law codec                                   |
| `client`               | always    | sans-io `PbxClient` state machine (one per account) |
| `jitter`               | always    | adaptive jitter buffer                              |
| `resample`             | `dsp`     | polyphase sinc anti-alias resampling (rubato)       |
| `audio`                | `audio`   | microphone / speaker via cpal (cross-platform)      |

Feature flags: `net` (async UDP driver, tokio), `dsp` (resampling),
`audio` (= `dsp` + `net` + cpal). The default feature set is empty.

## Library usage

The `PbxClient` is a sans-io state machine. You own the socket and the clock;
the client tells you what to send, what happened, and when to wake it next:

```rust
use iax2::{PbxClient, Config, Command, Event};
use std::time::Instant;

let mut client = PbxClient::new(Config::new("Office", "1001", "secret"));

loop {
    // 1) drain outgoing datagrams to your UDP socket
    while let Some(datagram) = client.poll_transmit() {
        // socket.send(&datagram)?;
    }

    // 2) react to protocol events
    while let Some(event) = client.poll_event() {
        match event {
            Event::Registered => println!("registered"),
            Event::Incoming { call, from, .. } => {
                println!("incoming call from {from}");
                client.handle_command(Command::Answer { call }, Instant::now());
            }
            Event::Voice { call, ts, ulaw } => {
                // decode µ-law and feed it to iax2::JitterBuffer keyed by `ts`
            }
            _ => {}
        }
    }

    // 3) feed inbound datagrams and tick the clock
    // client.handle_input(&buf[..n], Instant::now());
    // client.handle_timeout(Instant::now());
    // then sleep until client.poll_timeout()
}
```

The jitter buffer is equally I/O-free — push received frames keyed by their
timestamp, pull one 20 ms frame per playout tick:

```rust
use iax2::{JitterBuffer, Pull};
use std::time::Instant;

let mut jb = JitterBuffer::new();
jb.push(timestamp_ms, ulaw_payload, Instant::now());

match jb.pull() {
    Pull::Play(ulaw) => { /* decode and play */ }
    Pull::Conceal    => { /* packet lost: do PLC */ }
    Pull::Silence    => { /* buffer priming / underrun */ }
}
```

## Example binaries

Built with `--features audio` (or `net` for the diagnostic-only one):

| binary      | feature | what it does                                          |
|-------------|---------|-------------------------------------------------------|
| `regtest`   | `net`   | registration handshake only (diagnostics)             |
| `calltest`  | `audio` | outbound call to the `*43` echo test, with audio      |
| `regdaemon` | `audio` | persistent registration + answers a single inbound call |
| `softphone` | `audio` | multi-line / multi-PBX softphone (keyboard-driven)    |

The `softphone` registers against one or more PBXes (one account each), rings on
inbound calls, supports call waiting across PBXes, hold/unhold, call switching
and DTMF, and runs the adaptive jitter buffer on the active call.

```text
softphone <host> <user> <secret> [port] [refresh] [name]   # single account
softphone accounts.conf                                     # multiple PBXes
```

See `accounts.conf.example` for the multi-PBX configuration format.

## Building

```text
cargo build --release --features audio    # all binaries
cargo build --release --features net      # regtest only
cargo build                               # protocol core, no I/O
```

- **Linux** needs ALSA development headers for cpal: `libasound2-dev` and
  `pkg-config`.
- **Windows** uses WASAPI — no system dependencies.
- `rubato` is pinned to `=0.15.0` (the resampler API is not yet stable across
  minor versions).

Minimum supported Rust version: **1.75**.

## FreePBX / Asterisk setup

Create an **IAX2** extension (`type=friend`, a `secret`, `host=dynamic`),
`allow=ulaw`, `context=from-internal`. Call-token authentication
(`requirecalltoken=yes`) is supported and expected.

To receive calls, the peer must be **registered** (qualify OK). The client
answers POKE keepalives, so once registered Asterisk routes calls to it.

## Scope and limitations

This crate is a focused, honest implementation — not a full IAX2 stack:

- **Codec**: G.711 µ-law only. No A-law, GSM, G.729 or other formats yet.
- **No** video, text messages, or trunked (meta) frames.
- **Call transfer** is done through PBX feature codes (sent as DTMF), not via the
  IAX2 native transfer (TXREQ/TXMEDIA) media-path optimization, which is not
  implemented.
- **No** acoustic echo cancellation — use a headset, or wire your own AEC into
  the audio layer (the playout reference and capture frames are both exposed).
- Validated with `requirecalltoken=yes`, `ulaw`, `from-internal`. Other
  configurations may work but are untested.

Contributions and field reports against other PBX setups are welcome.

## Protocol notes

A few hard-won details, in case they save you the packet captures they cost us:

- Call-token pre-auth means **resending the original request unchanged** with the
  token added — not learning sequence numbers from the token frame.
- The qualify **PONG must echo the POKE's timestamp**, not your own uptime, or
  Asterisk computes an absurd round-trip time and marks the peer unreachable.
- The same echo rule applies to **LAGRP** replying to a **LAGRQ**.
- Frame body parsing depends on the frame type: IAX/CONTROL bodies are
  information elements; VOICE/VIDEO bodies are raw media.
- A POKE keepalive holds the NAT pinhole open; for a stable deployment a
  point-to-point overlay (e.g. WireGuard) is the real fix.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.