# Stream Check

A local, fake streaming platform for testing live-video senders. Point OBS, ffmpeg, a phone app or
your own encoder at it as if it were Twitch, YouTube or Kick, and see what actually arrives:
a live preview you can hear, every parameter measured, the rules a real platform would enforce,
and failures on demand. Everything is also a JSON API and a CLI, so a script (or an AI agent) can
run a streaming test and read the verdict without looking at a screen.

- **Ingest:** RTMP, RTMPS (self-signed certificate), SRT listener (optional passphrase, `streamid`),
  WHIP (HTTP endpoint, bearer token). H.264 and AAC; HEVC and Opus the Enhanced RTMP way; Opus over WHIP.
- **Several streams at once**, told apart by platform and stream key, each its own session.
- **Platform profiles** (Twitch-, YouTube- and Kick-like, plus an open one): they refuse wrong keys, flag
  streams that break the platform's rules (codec, bitrate, keyframe interval, resolution, audio) and keep
  a session alive for a reconnect grace period, as platforms do.
- **Fault injection** per stream, from the UI, the API and the CLI: drop the connection, refuse new
  ones for N seconds, freeze reading, throttle, add latency, lose packets.
- **See and hear it** in a browser (no plug-ins): live preview with sound and level meters, stats with
  graphs, a thumbnail timeline, an event log.
- **Evidence:** every session is recorded in the protocol's own container (FLV, MPEG-TS or Matroska)
  and ends with a JSON report, with ffprobe's verdict that the recording plays.

![Stream Check showing three live streams](docs/ui.jpg)

## Install and run

You need a Rust toolchain ([rustup.rs](https://rustup.rs)). ffmpeg is optional but recommended: with it
on your `PATH` you get thumbnails, loudness (LUFS) and peak meters, and recordings are checked with ffprobe.

```sh
git clone https://github.com/MoonGoblinDev/stream-check
cd stream-check
cargo run --release
```

It prints the addresses to use, with this machine's LAN address so phones can reach it. Open
<http://localhost:8890> for the UI. Ports (all changeable, see `--help`):

| What | Default |
|---|---|
| Web UI, API, WHIP | `8890` (TCP) |
| RTMP | `1935` (TCP) |
| RTMPS | `1936` (TCP), certificate in `<data>/tls/cert.pem` |
| SRT | `9000` (UDP) |

The Android emulator reaches this machine as `10.0.2.2`; the UI lists that address too, with QR codes
for the LAN addresses ("Connect a sender").

### Sending to it

The first part of the path is the platform (`twitch`, `youtube`, `kick`, or anything else: `open`):

```sh
# RTMP: server rtmp://<host>:1935/<platform>, key <key>
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i sine=frequency=440 \
  -c:v libx264 -g 60 -bf 0 -pix_fmt yuv420p -c:a aac -ar 48000 -f flv rtmp://localhost:1935/open/mytest

# SRT (needs an ffmpeg built with libsrt; OBS and most hardware encoders have it)
ffmpeg -re ... -f mpegts "srt://localhost:9000?streamid=open/mytest"

# WHIP (any WHIP client): POST an SDP offer to
http://localhost:8890/whip/open/mytest
```

In OBS: *Settings → Stream → Custom*, server `rtmp://<host>:1935/twitch`, key `live_123456789_anything`.

Stream keys are checked like the platform would: a Twitch-like platform wants `live_…`, a YouTube-like one
`xxxx-xxxx-xxxx-xxxx-xxxx`, a Kick-like one `sk_…`; `open` takes anything. Start with `--any-key` to accept
any key on every platform, or `--accept-key twitch=live_mykey` to accept only certain keys.

## The UI

One card per stream: a live preview (decoded in the browser with WebCodecs from the NAL units the lab
forwards: Chrome, Edge and Safari; the preview needs HEVC support in the browser for HEVC streams, and falls
back to thumbnails when it can't), an unmute button, level meters, and stats: codec, profile and level,
resolution, measured frame rate, bitrate, keyframe interval, GOP structure, B-frames, timestamp
monotonicity and gaps, A/V offset and drift, stalls, audio format, loudness (momentary, short-term, integrated
LUFS), peaks, silence, reconnects and grace-period use, and every platform rule broken. Under the card:
graphs, fault buttons, a thumbnail timeline and the event log.

## Platform profiles

| Profile | Video | Bitrate | Keyframes | Audio | Grace |
|---|---|---|---|---|---|
| `twitch` | H.264, no B-frames | 8500 kbps | every 2 s | AAC 44.1/48 kHz | 30 s |
| `youtube` | H.264, HEVC | 51000 kbps | every 4 s | AAC, Opus | 10 s |
| `kick` | H.264, no B-frames | 8000 kbps | every 2 s | AAC 44.1/48 kHz | 15 s |
| `open` | anything | no limit | any | anything | 10 s |

These are approximations of the platforms' published limits, for testing; they aren't a promise about the
platforms. A stream that disconnects stays a *session* for the grace period (shown as "waiting"); if the same
key publishes again within it, the session continues (new recording segment, `reconnects` and `grace_uses`
go up); otherwise the session ends and its report is written. `--grace <s>` overrides the profiles.

## Fault injection

Per stream key (the same key on any platform), or `*` for every stream; from the UI buttons, the API or the CLI.

| Fault | Effect |
|---|---|
| `drop` | closes the connection now (sender reconnects, or doesn't) |
| `refuse <s>` | closes it and refuses new connections for N seconds (RTMP: `NetStream.Publish.Failed`, WHIP: 503) |
| `freeze <s>` | stops reading for N seconds: a dead network that never says so (TCP back-pressure builds) |
| `throttle <kbps>` | paces reading to this rate; the sender's own send queue fills, as on a slow link |
| `latency <ms>` | delays the lab's replies to the sender (RTMP command responses and acks) |
| `loss <pct>` | drops this share of received SRT datagrams *after* SRT's own recovery, as if unrecoverable |

`throttle`, `latency` and `loss` accept `--for <s>` to expire by themselves. The lab sits at the receiving
end, so these are approximations of network trouble, not a network emulator; for packet-level loss and delay
use a tool like `dnctl`/`pfctl` (macOS) or `tc netem` (Linux) in front of it.

```sh
stream-check fault mytest drop
stream-check fault mytest throttle 800 --for 20
stream-check fault '*' refuse 10
curl -X POST localhost:8890/api/faults/mytest -d '{"throttle_kbps":800,"for_s":20}' -H 'Content-Type: application/json'
```

## For scripts and agents

```sh
stream-check wait-for mytest --live --timeout 30     # exit 0 when media is flowing (prints a summary), 1 on timeout
stream-check wait-for mytest --ended --timeout 120   # exit 0 when the session has ended
stream-check report latest --summary                 # a few lines of text; exit 3 if problems were found
stream-check report mytest                           # the whole JSON report
stream-check events mytest --kind violation          # event log lines
stream-check list
```

`--url` (or `$STREAM_CHECK_URL`) says where it runs; `report` reads the files in `--data` when it isn't running.

### JSON API

| | |
|---|---|
| `GET /api/status` | uptime, addresses, all sessions, faults, refused connections |
| `GET /api/sessions` | sessions (live and ended) with live stats; `{id}` may be an id, a prefix, a stream key or `latest` |
| `GET /api/sessions/{id}` | one session with graph series, events and thumbnails |
| `GET /api/sessions/{id}/events?since=<s>&kind=<kind>` | the event log |
| `GET /api/sessions/{id}/report` | the report (a live session gets a report of the moment); earlier runs are read from disk |
| `POST /api/faults/{key or *}` · `POST /api/sessions/{id}/fault` | `{"drop":true}`, `{"refuse_s":10}`, `{"freeze_s":10}`, `{"throttle_kbps":500,"for_s":30}`, `{"latency_ms":300}`, `{"loss_pct":5}`, `{"clear":true}` |
| `POST /api/sessions/{id}/end` | end a session now and write its report |
| `GET /api/faults`, `/api/rejections`, `/api/profiles`, `/api/past` | faults in force, refused connections, profiles, earlier reports |
| `GET /media/{id}/{file}` | recordings and thumbnails |
| `ws:///ws/state`, `ws:///ws/preview/{id}` | the UI's live feeds |

### The report

`<data>/sessions/<id>/report.json` next to the recording(s) `seg01.flv`, `seg02.flv`… and `thumbs/`:
`ok` and `problems` (a verdict: broken platform rules, timestamps going backwards, recordings that don't
play), the final stats, the series for graphs, every violation with when it began and how often, every
connection (protocol, remote address, why it ended), the segments with ffprobe's description of each,
thumbnails with their times, and the event log. Stalls, gaps and reconnects are reported but are not
"problems" by themselves, since faults cause them on purpose.

## How it works, and what it can't tell you

- Each ingest turns what arrives into the same events (parameter sets, pictures, audio packets), which
  feed the analyzer, the recorder, the browser preview and an ffmpeg helper (Matroska on its stdin) that
  decodes sound and pictures for loudness and thumbnails.
- Timing numbers (fps, bitrate, keyframe interval) come from the stream's own timestamps and from arrival
  times, both shown where they differ. WHIP carries no common clock for picture and sound without RTCP
  sender reports, so the A/V offset of a WHIP stream is only as good as the first packets' arrival.
- WHIP video and audio go through a small jitter buffer that restores sequence order: a packet that arrives late or as a
  retransmission (NACK, or RFC 4588 RTX, which the WebRTC stack unwraps) in time is slotted into its place; one that arrives
  after the buffer gave up is dropped and counted, never taken for a new picture. A picture with missing packets is dropped
  (counted as such). The report's `stats.rtp` has `lost`, `recovered`, `late`, `duplicates` and pictures dropped per track.
- RTMP recordings are the FLV tags as sent; SRT recordings the transport stream as sent; WHIP recordings are
  written as Matroska.

## Development

```sh
cargo test          # unit tests, and end-to-end tests with ffmpeg (skipped without it)
cargo clippy --all-targets -- -D warnings
```

Permissive dependencies only. The code is organised as: `rtmp`, `srt`/`ts`, `whip` (ingests), `hub` (sessions,
grace, faults, recordings, reports), `analyze`, `profiles`, `faults`, `http` and `ui.html`, `cli`.

Stream Check was built to test Lightair's streaming, and is used for that, but has no ties to it.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT) at
your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 licence, shall be dual licensed as above, without any
additional terms or conditions.
