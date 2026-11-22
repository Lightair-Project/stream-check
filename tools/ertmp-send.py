#!/usr/bin/env python3
"""Publish an FLV to an RTMP server as Enhanced RTMP v2 multitrack, for testing a receiver.

Takes an FLV with two video streams and AAC sound, as ffmpeg 8 writes one (track 0 as classic
AVC tags, track 1 as Multitrack/OneTrack), and sends it in real time with the tracks packed the
way you ask, so every layout of the spec can be tried against Stream Check:

  python3 tools/ertmp-send.py in.flv rtmp://127.0.0.1:1935/twitch-dual/live_123456789_labtestkey \
      --layout many [--modex] [--audio-multitrack] [--track-maps] [--query clientConfigId=abc]

  --layout one    every track in its own Multitrack/OneTrack message (track 0 too)
  --layout many   both tracks in one ManyTracks message per timestamp (one FourCC, track sizes)
  --layout codecs both tracks in one ManyTracksManyCodecs message (a FourCC per track)
  --layout ffmpeg pass the tags through as they are (classic track 0, OneTrack track 1)
  --modex         put a ModEx TimestampOffsetNano entry (0 ns) before each video packet type
  --audio-multitrack  send the AAC as Enhanced audio, Multitrack/OneTrack track 0
  --track-maps    add a videoTrackIdInfoMap for track 1 to onMetaData
  --query         a query for the stream name (Twitch Dual Format's clientConfigId)

Make the input with:
  ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=30 -f lavfi -i testsrc2=size=1080x1920:rate=30 \
      -f lavfi -i sine=frequency=440:sample_rate=48000 -t 20 -map 0:v -map 1:v -map 2:a \
      -c:v libx264 -preset ultrafast -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 -c:a aac -f flv in.flv

Written for Stream Check from the Enhanced RTMP v2 specification (veovera/enhanced-rtmp); it is a
test tool, not a full RTMP client (no server messages are acted on, they are read and dropped).
"""

import argparse
import os
import socket
import struct
import sys
import threading
import time
from urllib.parse import urlparse

# --- AMF0 -------------------------------------------------------------------------------------


def amf(v):
    if v is None:
        return b"\x05"
    if isinstance(v, bool):
        return b"\x01" + (b"\x01" if v else b"\x00")
    if isinstance(v, (int, float)):
        return b"\x00" + struct.pack(">d", float(v))
    if isinstance(v, str):
        b = v.encode()
        return b"\x02" + struct.pack(">H", len(b)) + b
    if isinstance(v, list):
        return b"\x0a" + struct.pack(">I", len(v)) + b"".join(amf(x) for x in v)
    if isinstance(v, dict):
        out = b"\x03"
        for k, x in v.items():
            kb = str(k).encode()
            out += struct.pack(">H", len(kb)) + kb + amf(x)
        return out + b"\x00\x00\x09"
    raise TypeError(v)


# --- RTMP -------------------------------------------------------------------------------------

CHUNK = 4096


class Rtmp:
    def __init__(self, host, port):
        self.s = socket.create_connection((host, port))
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        # the simple handshake: C0 C1, read S0 S1 S2, C2 echoes S1
        c1 = struct.pack(">II", 0, 0) + os.urandom(1528)
        self.s.sendall(b"\x03" + c1)
        s = self.read(1 + 1536 + 1536)
        self.s.sendall(s[1 : 1 + 1536])
        threading.Thread(target=self.drain, daemon=True).start()
        self.send(2, 1, 0, 0, struct.pack(">I", CHUNK))

    def read(self, n):
        b = b""
        while len(b) < n:
            x = self.s.recv(n - len(b))
            if not x:
                raise ConnectionError("closed")
            b += x
        return b

    def drain(self):
        try:
            while self.s.recv(65536):
                pass
        except OSError:
            pass

    def send(self, csid, typ, msid, ts, payload):
        ext = ts >= 0xFFFFFF
        t = 0xFFFFFF if ext else ts
        h = bytes([csid & 0x3F]) + t.to_bytes(3, "big") + len(payload).to_bytes(3, "big") + bytes([typ]) + struct.pack("<I", msid)
        if ext:
            h += struct.pack(">I", ts)
        out = bytearray(h)
        for i in range(0, max(len(payload), 1), CHUNK):
            if i:
                out.append(0xC0 | (csid & 0x3F))
                if ext:
                    out += struct.pack(">I", ts)
            out += payload[i : i + CHUNK]
        self.s.sendall(bytes(out))

    def command(self, msid, *values):
        self.send(3, 20, msid, 0, b"".join(amf(v) for v in values))


# --- FLV --------------------------------------------------------------------------------------


def flv_tags(path):
    b = open(path, "rb").read()
    i = 13
    while i + 11 <= len(b):
        typ = b[i]
        size = int.from_bytes(b[i + 1 : i + 4], "big")
        ts = int.from_bytes(b[i + 4 : i + 7], "big") | (b[i + 7] << 24)
        yield typ, ts, b[i + 11 : i + 11 + size]
        i += 11 + size + 4


def video_parts(d):
    """A video tag as (track, frame type, packet type, fourcc, body with SI24 composition time
    for CodedFrames). Classic AVC becomes track 0; ffmpeg's OneTrack stays its track."""
    if d[0] & 0x80 == 0:
        ft, codec = d[0] >> 4, d[0] & 0x0F
        if codec != 7:
            return None
        if d[1] == 0:
            return (0, ft, 0, b"avc1", d[5:])
        if d[1] == 1:
            return (0, ft, 1, b"avc1", d[2:5] + d[5:])
        return None
    ft, pt = (d[0] >> 4) & 7, d[0] & 0x0F
    if pt == 6:
        layout, inner = d[1] >> 4, d[1] & 0x0F
        if layout != 0:
            return None
        fourcc, track, body = d[2:6], d[6], d[7:]
    else:
        inner, fourcc, track, body = pt, d[1:5], 0, d[5:]
    if inner == 3:  # CodedFramesX: make it CodedFrames with a zero composition time
        inner, body = 1, b"\x00\x00\x00" + body
    return (track, ft, inner, fourcc, body)


def modex_prefix(pt):
    # one ModEx entry: size-1 = 2 (3 bytes), TimestampOffsetNano = 0 ns, then the real type
    return bytes([2]) + b"\x00\x00\x00" + bytes([(0 << 4) | pt])


def ex_header(ft, pt, modex):
    if modex:
        return bytes([0x80 | (ft << 4) | 7]) + modex_prefix(pt)
    return bytes([0x80 | (ft << 4) | pt])


def multitrack(parts, layout, modex):
    """parts: [(track, ft, pt, fourcc, body)], same ft and pt. One Multitrack message."""
    ft, pt = parts[0][1], parts[0][2]
    code = {"one": 0, "many": 1, "codecs": 2}[layout]
    out = ex_header(ft, 6, modex) + bytes([(code << 4) | pt])
    if code != 2:
        out += parts[0][3]
    for track, _, _, fourcc, body in parts:
        if code == 2:
            out += fourcc
        out += bytes([track])
        if code != 0:
            out += len(body).to_bytes(3, "big")
        out += body
    return out


def audio_multitrack(d):
    # classic AAC (format 10): d[1] 0 = config, 1 = raw. As Enhanced audio Multitrack/OneTrack.
    if d[0] >> 4 != 10:
        return d
    pt = d[1]
    return bytes([(9 << 4) | 5, (0 << 4) | pt]) + b"mp4a" + bytes([0]) + d[2:]


def main():
    a = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    a.add_argument("flv")
    a.add_argument("url")
    a.add_argument("--layout", choices=["one", "many", "codecs", "ffmpeg"], default="many")
    a.add_argument("--modex", action="store_true")
    a.add_argument("--audio-multitrack", action="store_true")
    a.add_argument("--track-maps", action="store_true")
    a.add_argument("--query", default="")
    o = a.parse_args()

    u = urlparse(o.url)
    path = u.path.lstrip("/")
    app, name = path.rsplit("/", 1)
    if o.query:
        name += "?" + o.query
    r = Rtmp(u.hostname, u.port or 1935)
    tc = f"rtmp://{u.hostname}:{u.port or 1935}/{app}"
    r.command(0, "connect", 1, {
        "app": app, "type": "nonprivate", "flashVer": "FMLE/3.0 (compatible; ertmp-send)", "tcUrl": tc,
        "capsEx": 0x02 | 0x04, "fourCcList": ["avc1", "hvc1", "mp4a"],
        "videoFourCcInfoMap": {"avc1": 0x02, "hvc1": 0x02}, "audioFourCcInfoMap": {"mp4a": 0x02},
    })
    r.command(0, "releaseStream", 2, None, name)
    r.command(0, "FCPublish", 3, None, name)
    r.command(0, "createStream", 4, None)
    time.sleep(0.5)
    r.command(1, "publish", 5, None, name, "live")
    time.sleep(0.5)

    tags = list(flv_tags(o.flv))
    start = time.monotonic()
    pending = []  # video parts at the current timestamp (for the many/codecs layouts)
    pending_ts = None

    def flush():
        nonlocal pending
        if not pending:
            return
        # parts that share frame and packet type go together; the rest on their own
        groups = {}
        for p in pending:
            groups.setdefault((p[1], p[2]), []).append(p)
        for g in groups.values():
            r.send(6, 9, 1, pending_ts, multitrack(sorted(g), o.layout, o.modex))
        pending = []

    sent = 0
    for typ, ts, d in tags:
        wait = start + ts / 1000 - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        if typ == 18:
            r.send(5, 18, 1, ts, d)
            if o.track_maps:
                maps = {"videoTrackIdInfoMap": {"1": {"width": 1080, "height": 1920, "videocodecid": 1635148593}}}
                r.send(5, 18, 1, ts, amf("onMetaData") + amf(maps))
        elif typ == 8:
            r.send(4, 8, 1, ts, audio_multitrack(d) if o.audio_multitrack else d)
        elif typ == 9:
            if o.layout == "ffmpeg":
                r.send(6, 9, 1, ts, d)
                continue
            p = video_parts(d)
            if p is None:
                continue
            if o.layout == "one":
                r.send(6, 9, 1, ts, multitrack([p], "one", o.modex))
                continue
            if pending_ts is not None and ts != pending_ts:
                flush()
            pending_ts = ts
            pending.append(p)
        sent += 1
    flush()
    r.command(1, "FCUnpublish", 6, None, name)
    r.command(1, "deleteStream", 7, None, 1)
    time.sleep(0.5)
    print(f"sent {sent} tags as layout '{o.layout}'" + (" with ModEx" if o.modex else ""), file=sys.stderr)


if __name__ == "__main__":
    main()
