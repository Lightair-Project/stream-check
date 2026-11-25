//! RTMP and RTMPS ingest: the server side of the handshake, the chunk stream and the publish
//! commands, FLV tags read into [`Media`] (H.264 and AAC; HEVC, AV1, VP9, VP8 and Opus the
//! Enhanced RTMP way; the older HEVC codec id some servers use), and the tags kept as an FLV file
//! as they came.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::amf::{self, Amf, obj, s};
use crate::analyze::Level;
use crate::hub::{Hub, Proto, Publisher};
use crate::media::{ACodec, Frames, Media};
use crate::nal::{self, VCodec};

const OUT_CHUNK: usize = 4096;

/// Writes messages as chunks (used by the server, and by test clients).
pub struct ChunkWriter {
    pub chunk: usize,
}

impl ChunkWriter {
    pub fn new() -> Self {
        ChunkWriter { chunk: 128 }
    }

    pub fn message(&self, csid: u8, typ: u8, stream: u32, ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        let ext = ts >= 0xff_ffff;
        out.push(csid & 0x3f);
        out.extend(&(if ext { 0xff_ffff } else { ts }).to_be_bytes()[1..]);
        out.extend(&(payload.len() as u32).to_be_bytes()[1..]);
        out.push(typ);
        out.extend(stream.to_le_bytes());
        if ext {
            out.extend(ts.to_be_bytes());
        }
        let mut first = true;
        for part in payload.chunks(self.chunk.max(1)) {
            if !first {
                out.push(0xc0 | (csid & 0x3f));
                if ext {
                    out.extend(ts.to_be_bytes());
                }
            }
            out.extend(part);
            first = false;
        }
        out
    }
}

impl Default for ChunkWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default, Clone)]
struct ChunkStream {
    ts: u32,
    delta: u32,
    len: usize,
    typ: u8,
    stream: u32,
    ext: bool,
    buf: Vec<u8>,
}

// Enhanced RTMP v2's connect flags ("Enhancing NetConnection connect Command")
const CAPS_RECONNECT: u32 = 0x01;
const CAPS_MULTITRACK: u32 = 0x02;
const CAPS_MODEX: u32 = 0x04;
const CAPS_NANO: u32 = 0x08;
const CAN_DECODE: f64 = 1.0;

/// The Enhanced RTMP fields of a `connect` command object, if it has any.
fn enhanced_caps(f: &[(String, Amf)]) -> Option<serde_json::Value> {
    let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v);
    let caps = get("capsEx").and_then(Amf::num);
    let list = get("fourCcList").map(Amf::to_json);
    let video = get("videoFourCcInfoMap").map(Amf::to_json);
    let audio = get("audioFourCcInfoMap").map(Amf::to_json);
    if caps.is_none() && list.is_none() && video.is_none() && audio.is_none() {
        return None;
    }
    let bits = caps.unwrap_or(0.0) as u32;
    Some(json!({
        "caps_ex": caps,
        "reconnect": bits & CAPS_RECONNECT != 0,
        "multitrack": bits & CAPS_MULTITRACK != 0,
        "mod_ex": bits & CAPS_MODEX != 0,
        "timestamp_nano_offset": bits & CAPS_NANO != 0,
        "four_cc_list": list,
        "video_four_cc_info_map": video,
        "audio_four_cc_info_map": audio,
    }))
}

fn describe_caps(c: &serde_json::Value) -> String {
    let flags: Vec<&str> =
        [("reconnect", "Reconnect"), ("multitrack", "Multitrack"), ("mod_ex", "ModEx"), ("timestamp_nano_offset", "TimestampNanoOffset")]
            .iter()
            .filter(|(k, _)| c[*k] == true)
            .map(|(_, n)| *n)
            .collect();
    let mut out = format!("capsEx {} ({})", c["caps_ex"], if flags.is_empty() { "none".to_string() } else { flags.join(", ") });
    for (k, n) in
        [("four_cc_list", "fourCcList"), ("video_four_cc_info_map", "videoFourCcInfoMap"), ("audio_four_cc_info_map", "audioFourCcInfoMap")]
    {
        if !c[k].is_null() {
            out.push_str(&format!(", {n} {}", c[k]));
        }
    }
    out
}

/// The query of a published stream name: its parameter names and whether `clientConfigId` is
/// among them. Values are never kept (a key or a config id is the sender's secret).
fn key_query(full: &str) -> Option<serde_json::Value> {
    let (_, q) = full.split_once('?')?;
    let params: Vec<String> = q.split('&').filter(|p| !p.is_empty()).map(|p| p.split('=').next().unwrap_or("").to_string()).collect();
    let cc = params.iter().any(|p| p == "clientConfigId");
    Some(json!({ "params": params, "client_config_id": cc }))
}

/// What one video track has said about itself: its codec, and its NAL length size (H.264,
/// HEVC) or its frame reader (AV1, VP9, VP8).
struct VideoTrackState {
    codec: VCodec,
    nal_size: usize,
    frames: Option<Frames>,
}

/// Turns FLV tag bodies into [`Media`]: classic tags, and Enhanced RTMP v2's extended video and
/// audio (ModEx, command frames, metadata, and multitrack in all three layouts). Track 0 comes
/// out as plain media; every other track as [`Media::Track`].
#[derive(Default)]
pub struct FlvDemux {
    video_tracks: std::collections::HashMap<u8, VideoTrackState>,
    warned: Vec<String>,
}

/// The tracks a multitrack tag may carry that the lab keeps apart (more are ignored, once said).
const MAX_TRACKS: u8 = 16;

fn u16_at(d: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]))
}

fn u24_at(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes([0, *d.get(i)?, *d.get(i + 1)?, *d.get(i + 2)?]))
}

fn i24_at(d: &[u8], i: usize) -> Option<i32> {
    u24_at(d, i).map(|v| ((v << 8) as i32) >> 8)
}

/// Skips Enhanced RTMP's ModEx entries: (the packet type after them, where the body starts, the
/// nanosecond timestamp offset if one was given).
fn mod_ex(d: &[u8], mut pt: u8, mut i: usize) -> Option<(u8, usize, u32)> {
    let mut nano = 0;
    while pt == 7 {
        let mut size = *d.get(i)? as usize + 1;
        i += 1;
        if size == 256 {
            size = u16_at(d, i)? as usize + 1;
            i += 2;
        }
        let data = d.get(i..i + size)?;
        i += size;
        let b = *d.get(i)?;
        i += 1;
        // TimestampOffsetNano (0): at least 3 bytes, a UI24 of nanoseconds
        if b >> 4 == 0 && data.len() >= 3 {
            nano = u24_at(data, 0)?;
        }
        pt = b & 0x0f;
    }
    Some((pt, i, nano))
}

/// The tracks of a multitrack body. The FourCC is read once before the tracks, unless each track
/// names its own (ManyTracksManyCodecs, layout 2); OneTrack (0) has no track sizes.
/// One track of a multitrack message: its id, its FourCC, its body.
type TrackPart<'a> = (u8, &'a [u8], &'a [u8]);

fn tracks(d: &[u8], mut i: usize, layout: u8) -> Option<Vec<TrackPart<'_>>> {
    let mut out = vec![];
    let shared = if layout != 2 {
        let f = d.get(i..i + 4)?;
        i += 4;
        Some(f)
    } else {
        None
    };
    while i < d.len() {
        let fourcc = match shared {
            Some(f) => f,
            None => {
                let f = d.get(i..i + 4)?;
                i += 4;
                f
            }
        };
        let track = *d.get(i)?;
        i += 1;
        if layout == 0 {
            // OneTrack: the rest of the message is the track's
            out.push((track, fourcc, d.get(i..)?));
            break;
        }
        let size = u24_at(d, i)? as usize;
        i += 3;
        out.push((track, fourcc, d.get(i..i + size)?));
        i += size;
    }
    Some(out)
}

impl FlvDemux {
    pub fn new() -> Self {
        FlvDemux::default()
    }

    fn say(&mut self, what: String, warn: &mut Vec<String>) {
        if !self.warned.contains(&what) {
            warn.push(what.clone());
            self.warned.push(what);
        }
    }

    fn push(track: u8, m: Media, out: &mut Vec<Media>) {
        out.push(if track == 0 { m } else { Media::Track(track, Box::new(m)) });
    }

    /// The video tag body; says what it couldn't handle through `warn`.
    pub fn video(&mut self, ts: u32, d: &[u8], out: &mut Vec<Media>, warn: &mut Vec<String>) {
        if d.is_empty() {
            return;
        }
        if d[0] & 0x80 == 0 {
            // the classic tag: frame type, codec id, then AVC packet type and composition time
            if d.len() < 5 {
                return;
            }
            let ft = d[0] >> 4;
            let codec = match d[0] & 0x0f {
                7 => VCodec::H264,
                12 => VCodec::Hevc,
                other => {
                    return self.say(format!("the video codec id {other} isn't supported by the lab (H.264 and HEVC are)"), warn);
                }
            };
            match d[1] {
                0 => self.config(0, codec, &d[5..], out),
                1 => self.frame(0, codec, ts, i24_at(d, 2).unwrap_or(0), 0, ft, &d[5..], out),
                _ => {}
            }
            return;
        }
        // Enhanced RTMP: ExHeader flag, frame type (3 bits), packet type (4 bits)
        let ft = (d[0] >> 4) & 7;
        let Some((pt, i, nano)) = mod_ex(d, d[0] & 0x0f, 1) else {
            return self.say("a video ModEx entry the lab couldn't read".into(), warn);
        };
        // a command frame (seek start or end) carries no picture; Metadata is colour info
        if ft == 5 || pt == 4 {
            return;
        }
        if pt == 6 {
            let Some(&b) = d.get(i) else { return };
            let (layout, inner) = (b >> 4, b & 0x0f);
            if layout > 2 || inner == 6 {
                return self.say(format!("a video multitrack layout the lab doesn't know ({layout}, packet type {inner})"), warn);
            }
            let Some(list) = tracks(d, i + 1, layout) else {
                return self.say("a video multitrack message the lab couldn't read to its end".into(), warn);
            };
            for (track, fourcc, body) in list {
                if track >= MAX_TRACKS {
                    self.say(format!("video track {track}: the lab keeps tracks 0 to {}", MAX_TRACKS - 1), warn);
                    continue;
                }
                self.video_track(track, fourcc, ft, inner, ts, nano, body, out, warn);
            }
            return;
        }
        let Some(fourcc) = d.get(i..i + 4) else { return };
        self.video_track(0, fourcc, ft, pt, ts, nano, &d[i + 4..], out, warn);
    }

    #[allow(clippy::too_many_arguments)]
    fn video_track(
        &mut self,
        track: u8,
        fourcc: &[u8],
        ft: u8,
        pt: u8,
        ts: u32,
        nano: u32,
        body: &[u8],
        out: &mut Vec<Media>,
        warn: &mut Vec<String>,
    ) {
        let codec = match fourcc {
            b"avc1" => VCodec::H264,
            b"hvc1" => VCodec::Hevc,
            b"av01" => VCodec::Av1,
            b"vp09" => VCodec::Vp9,
            b"vp08" => VCodec::Vp8,
            other => {
                return self.say(
                    format!(
                        "the video codec '{}' isn't supported by the lab (H.264, HEVC, AV1, VP9 and VP8 are)",
                        String::from_utf8_lossy(other)
                    ),
                    warn,
                );
            }
        };
        match pt {
            0 => self.config(track, codec, body, out),
            // a composition time only for H.264 and HEVC
            1 if codec.is_nal() && body.len() >= 3 => self.frame(track, codec, ts, i24_at(body, 0).unwrap_or(0), nano, ft, &body[3..], out),
            1 | 3 => self.frame(track, codec, ts, 0, nano, ft, body, out),
            _ => {}
        }
    }

    fn config(&mut self, track: u8, codec: VCodec, record: &[u8], out: &mut Vec<Media>) {
        let nal_size = match codec {
            VCodec::H264 => record.get(4).map_or(4, |b| (b & 3) as usize + 1),
            VCodec::Hevc => record.get(21).map_or(4, |b| (b & 3) as usize + 1),
            VCodec::Av1 | VCodec::Vp9 | VCodec::Vp8 => 4,
        };
        if codec.is_nal() {
            self.video_tracks.insert(track, VideoTrackState { codec, nal_size, frames: None });
            Self::push(track, Media::VideoConfig { codec, record: Arc::new(record.to_vec()) }, out);
            return;
        }
        // AV1, VP9, VP8: the record goes out once the bitstream has said enough (see Frames)
        let state = self.video_tracks.entry(track).or_insert_with(|| VideoTrackState { codec, nal_size, frames: None });
        if state.codec != codec || state.frames.is_none() {
            *state = VideoTrackState { codec, nal_size, frames: Some(Frames::new(codec)) };
        }
        let mut made = vec![];
        if let Some(f) = state.frames.as_mut() {
            f.config(record, &mut made);
        }
        for m in made {
            Self::push(track, m, out);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn frame(&mut self, track: u8, codec: VCodec, ts: u32, cts: i32, nano: u32, frame_type: u8, data: &[u8], out: &mut Vec<Media>) {
        let Some(state) = self.video_tracks.get_mut(&track) else { return };
        if state.codec != codec || data.is_empty() {
            return;
        }
        let dts = ts as i64 * 1000 + nano as i64 / 1000;
        if let Some(f) = state.frames.as_mut() {
            let mut made = vec![];
            f.frame(dts + cts as i64 * 1000, dts, frame_type == 1, data, &mut made);
            for m in made {
                Self::push(track, m, out);
            }
            return;
        }
        let data = if state.nal_size == 4 {
            data.to_vec()
        } else {
            let nals = nal::split_lengths(data, state.nal_size);
            nal::to_lengths(&nals)
        };
        Self::push(track, Media::Video { pts_us: dts + cts as i64 * 1000, dts_us: dts, key: frame_type == 1, data: Arc::new(data) }, out);
    }

    pub fn audio(&mut self, ts: u32, d: &[u8], out: &mut Vec<Media>, warn: &mut Vec<String>) {
        if d.len() < 2 {
            return;
        }
        let format = d[0] >> 4;
        match format {
            10 => self.audio_track(0, b"mp4a", d[1], ts, 0, &d[2..], out, warn),
            9 => {
                // Enhanced RTMP: the packet type in the low nibble, ModEx, then one or many tracks
                let Some((pt, i, nano)) = mod_ex(d, d[0] & 0x0f, 1) else {
                    return self.say("an audio ModEx entry the lab couldn't read".into(), warn);
                };
                if pt == 5 {
                    let Some(&b) = d.get(i) else { return };
                    let (layout, inner) = (b >> 4, b & 0x0f);
                    if layout > 2 || inner == 5 {
                        return self.say(format!("an audio multitrack layout the lab doesn't know ({layout}, packet type {inner})"), warn);
                    }
                    let Some(list) = tracks(d, i + 1, layout) else {
                        return self.say("an audio multitrack message the lab couldn't read to its end".into(), warn);
                    };
                    for (track, fourcc, body) in list {
                        if track < MAX_TRACKS {
                            self.audio_track(track, fourcc, inner, ts, nano, body, out, warn);
                        }
                    }
                    return;
                }
                let Some(fourcc) = d.get(i..i + 4) else { return };
                self.audio_track(0, fourcc, pt, ts, nano, &d[i + 4..], out, warn);
            }
            other => self.say(format!("the audio format {other} isn't supported by the lab (AAC and Opus are)"), warn),
        }
    }

    /// One audio track's packet: SequenceStart (0) or CodedFrames (1); the rest is skipped.
    #[allow(clippy::too_many_arguments)]
    fn audio_track(
        &mut self,
        track: u8,
        fourcc: &[u8],
        pt: u8,
        ts: u32,
        nano: u32,
        body: &[u8],
        out: &mut Vec<Media>,
        warn: &mut Vec<String>,
    ) {
        let pts_us = ts as i64 * 1000 + nano as i64 / 1000;
        match (fourcc, pt) {
            (b"mp4a", 0) => {
                let (rate, ch, _) = crate::media::read_asc(body).unwrap_or((44100, 2, 2));
                let m = Media::AudioConfig { codec: ACodec::Aac, config: Arc::new(body.to_vec()), sample_rate: rate, channels: ch };
                Self::push(track, m, out);
            }
            (b"Opus", 0) => {
                // the Opus ID header: the channel count is its 10th byte
                let ch = body.get(9).copied().unwrap_or(2);
                Self::push(
                    track,
                    Media::AudioConfig { codec: ACodec::Opus, config: Arc::new(vec![]), sample_rate: 48000, channels: ch },
                    out,
                );
            }
            (b"mp4a" | b"Opus", 1) => Self::push(track, Media::Audio { pts_us, data: Arc::new(body.to_vec()) }, out),
            (b"mp4a" | b"Opus", _) => {}
            (other, _) => self
                .say(format!("the audio codec '{}' isn't supported by the lab (AAC and Opus are)", String::from_utf8_lossy(other)), warn),
        }
    }
}

struct Conn<W> {
    w: W,
    cw: ChunkWriter,
    hub: Arc<Hub>,
}

impl<W: AsyncWrite + Unpin> Conn<W> {
    async fn send(&mut self, csid: u8, typ: u8, stream: u32, payload: &[u8], delay: bool) -> std::io::Result<()> {
        if delay {
            let ms = self.hub.global.faults.lock().latency_ms.current(std::time::Instant::now());
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms as u64)).await;
            }
        }
        let bytes = self.cw.message(csid, typ, stream, 0, payload);
        self.w.write_all(&bytes).await?;
        // over TLS nothing is sent until flushed
        self.w.flush().await
    }

    async fn control(&mut self, typ: u8, payload: &[u8]) -> std::io::Result<()> {
        self.send(2, typ, 0, payload, false).await
    }

    async fn command(&mut self, stream: u32, values: &[Amf], delay: bool) -> std::io::Result<()> {
        let mut p = vec![];
        for v in values {
            amf::write(&mut p, v);
        }
        self.send(3, 20, stream, &p, delay).await
    }

    async fn status(&mut self, level: &str, code: &str, desc: &str, delay: bool) -> std::io::Result<()> {
        self.command(
            1,
            &[s("onStatus"), Amf::Number(0.0), Amf::Null, obj(&[("level", s(level)), ("code", s(code)), ("description", s(desc))])],
            delay,
        )
        .await
    }
}

/// One RTMP connection, over TCP or TLS.
pub async fn serve<S>(stream: S, hub: Arc<Hub>, proto: Proto, remote: SocketAddr)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (r, w) = tokio::io::split(stream);
    let mut r = BufReader::with_capacity(64 * 1024, r);
    let mut c = Conn { w, cw: ChunkWriter::new(), hub: hub.clone() };
    let end = session(&mut r, &mut c, &hub, proto, remote).await;
    let _ = c.w.shutdown().await;
    if let Some((publisher, reason)) = end {
        publisher.end(&reason);
    }
}

async fn handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(r: &mut R, w: &mut W) -> std::io::Result<()> {
    let mut c0c1 = [0u8; 1537];
    r.read_exact(&mut c0c1).await?;
    let mut reply = vec![3u8];
    let mut s1 = vec![0u8; 1536];
    // time 0, zero field, then bytes that vary
    let seed = crate::hub::now_ms();
    for (i, b) in s1.iter_mut().enumerate().skip(8) {
        *b = (seed.wrapping_mul(i as u64 + 1) >> 3) as u8;
    }
    reply.extend(&s1);
    reply.extend(&c0c1[1..]);
    w.write_all(&reply).await?;
    w.flush().await?;
    let mut c2 = [0u8; 1536];
    r.read_exact(&mut c2).await?;
    Ok(())
}

async fn session<R, W>(r: &mut R, c: &mut Conn<W>, hub: &Arc<Hub>, proto: Proto, remote: SocketAddr) -> Option<(Publisher, String)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if hub.refusing_all() {
        return None;
    }
    if tokio::time::timeout(Duration::from_secs(10), handshake(r, &mut c.w)).await.ok()?.is_err() {
        return None;
    }
    let mut in_chunk = 128usize;
    let mut streams: std::collections::HashMap<u32, ChunkStream> = Default::default();
    let mut app = String::new();
    // the client's Enhanced RTMP capabilities from `connect`, told to the session once it publishes
    let mut client_caps: Option<serde_json::Value> = None;
    let mut publisher: Option<Publisher> = None;
    let mut demux = FlvDemux::new();
    let mut total: u64 = 0;
    let mut acked: u64 = 0;
    let mut window: u64 = 2_500_000;
    let mut header_written = false;

    loop {
        // read one chunk
        let step = async {
            let mut b0 = [0u8; 1];
            r.read_exact(&mut b0).await?;
            let fmt = b0[0] >> 6;
            let mut csid = (b0[0] & 0x3f) as u32;
            let mut used = 1u64;
            if csid == 0 {
                let mut b = [0u8; 1];
                r.read_exact(&mut b).await?;
                csid = 64 + b[0] as u32;
                used += 1;
            } else if csid == 1 {
                let mut b = [0u8; 2];
                r.read_exact(&mut b).await?;
                csid = 64 + b[0] as u32 + b[1] as u32 * 256;
                used += 2;
            }
            let st = streams.entry(csid).or_default();
            let hdr_len = [11, 7, 3, 0][fmt as usize];
            let mut h = [0u8; 11];
            r.read_exact(&mut h[..hdr_len]).await?;
            used += hdr_len as u64;
            let mut value = 0u32;
            if fmt <= 2 {
                value = (h[0] as u32) << 16 | (h[1] as u32) << 8 | h[2] as u32;
            }
            if fmt <= 1 {
                st.len = ((h[3] as usize) << 16) | ((h[4] as usize) << 8) | h[5] as usize;
                st.typ = h[6];
            }
            if fmt == 0 {
                st.stream = u32::from_le_bytes([h[7], h[8], h[9], h[10]]);
            }
            let fresh = st.buf.is_empty();
            if fmt <= 2 {
                st.ext = value == 0xff_ffff;
            }
            if st.ext {
                let mut e = [0u8; 4];
                r.read_exact(&mut e).await?;
                used += 4;
                value = u32::from_be_bytes(e);
            }
            if fresh {
                match fmt {
                    0 => st.ts = value,
                    1 | 2 => {
                        st.delta = value;
                        st.ts = st.ts.wrapping_add(value);
                    }
                    _ => st.ts = st.ts.wrapping_add(st.delta),
                }
            }
            let want = (st.len - st.buf.len()).min(in_chunk);
            let start = st.buf.len();
            st.buf.resize(start + want, 0);
            r.read_exact(&mut st.buf[start..]).await?;
            used += want as u64;
            let done = st.buf.len() >= st.len;
            let msg = done.then(|| (st.typ, st.ts, st.stream, std::mem::take(&mut st.buf)));
            Ok::<_, std::io::Error>((used, msg))
        };
        let result = match &publisher {
            Some(p) => tokio::select! {
                _ = p.killed() => return publisher.map(|p| (p, "dropped by a fault (injected)".to_string())),
                r = step => r,
            },
            None => match tokio::time::timeout(Duration::from_secs(30), step).await {
                Ok(r) => r,
                Err(_) => return None,
            },
        };
        let (used, msg) = match result {
            Ok(x) => x,
            Err(e) => {
                let reason = if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    "the sender closed the connection".to_string()
                } else {
                    format!("read error: {e}")
                };
                return publisher.map(|p| (p, reason));
            }
        };
        total += used;
        if let Some(p) = publisher.as_mut() {
            if !p.gate(used as usize).await {
                return publisher.map(|p| (p, "dropped by a fault (injected)".to_string()));
            }
        }
        // Acknowledge often over TLS: some clients (ffmpeg's with Apple's TLS) block reading the
        // server between writes and stall for good unless the server says something now and then.
        let ack_every = if proto == Proto::Rtmps { 4096 } else { window };
        if total - acked >= ack_every {
            acked = total;
            if c.control(3, &(total as u32).to_be_bytes()).await.is_err() {
                return publisher.map(|p| (p, "write error".to_string()));
            }
        }
        let Some((typ, ts, msid, payload)) = msg else { continue };
        let latency_on = publisher.is_some();
        match typ {
            1 if payload.len() >= 4 => {
                in_chunk = (u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) & 0x7fff_ffff).max(1) as usize
            }
            5 if payload.len() >= 4 => window = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]).max(1) as u64,
            8 | 9 | 18 | 15 => {
                let Some(p) = publisher.as_ref() else { continue };
                if !header_written {
                    header_written = true;
                    p.raw(&[b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0]);
                }
                let mut tag = vec![typ];
                tag.extend(&(payload.len() as u32).to_be_bytes()[1..]);
                tag.extend(&ts.to_be_bytes()[1..]);
                tag.push((ts >> 24) as u8);
                tag.extend([0, 0, 0]);
                tag.extend(&payload);
                tag.extend(((payload.len() + 11) as u32).to_be_bytes());
                p.raw(&tag);
                let mut media = vec![];
                let mut warn = vec![];
                match typ {
                    9 => demux.video(ts, &payload, &mut media, &mut warn),
                    8 => demux.audio(ts, &payload, &mut media, &mut warn),
                    _ => {
                        let vals = amf::read_all(&payload);
                        if vals.first().and_then(|v| v.str()) == Some("@setDataFrame")
                            || vals.first().and_then(|v| v.str()) == Some("onMetaData")
                        {
                            if let Some(meta) = vals.iter().rev().find(|v| matches!(v, Amf::Object(_))) {
                                media.push(Media::Metadata(meta.to_json()));
                            }
                        }
                    }
                }
                for w in warn {
                    p.session.event(Level::Warn, "unsupported", w);
                }
                for m in media {
                    p.media(m);
                }
            }
            20 | 17 => {
                let body = if typ == 17 { payload.get(1..).unwrap_or(&[]) } else { &payload[..] };
                let vals = amf::read_all(body);
                let name = vals.first().and_then(|v| v.str()).unwrap_or("").to_string();
                let txn = vals.get(1).and_then(|v| v.num()).unwrap_or(0.0);
                let io = match name.as_str() {
                    "connect" => {
                        if let Some(Amf::Object(f)) = vals.get(2) {
                            app = f.iter().find(|(k, _)| k == "app").and_then(|(_, v)| v.str()).unwrap_or("").to_string();
                            client_caps = enhanced_caps(f);
                        }
                        let mut res = c.control(5, &2_500_000u32.to_be_bytes()).await;
                        if res.is_ok() {
                            res = c.control(6, &[0x00, 0x26, 0x25, 0xa0, 2]).await;
                        }
                        if res.is_ok() {
                            res = c.control(4, &[0, 0, 0, 0, 0, 0]).await;
                        }
                        if res.is_ok() {
                            res = c.control(1, &(OUT_CHUNK as u32).to_be_bytes()).await;
                            c.cw.chunk = OUT_CHUNK;
                        }
                        if res.is_ok() {
                            res = c
                                .command(
                                    0,
                                    &[
                                        s("_result"),
                                        Amf::Number(txn),
                                        // Enhanced RTMP v2: multitrack and ModEx, and the codecs the lab reads
                                        obj(&[
                                            ("fmsVer", s("FMS/3,0,1,123")),
                                            ("capabilities", Amf::Number(31.0)),
                                            ("capsEx", Amf::Number((CAPS_MULTITRACK | CAPS_MODEX) as f64)),
                                            (
                                                "videoFourCcInfoMap",
                                                obj(&[
                                                    ("avc1", Amf::Number(CAN_DECODE)),
                                                    ("hvc1", Amf::Number(CAN_DECODE)),
                                                    ("av01", Amf::Number(CAN_DECODE)),
                                                    ("vp09", Amf::Number(CAN_DECODE)),
                                                    ("vp08", Amf::Number(CAN_DECODE)),
                                                ]),
                                            ),
                                            (
                                                "audioFourCcInfoMap",
                                                obj(&[("mp4a", Amf::Number(CAN_DECODE)), ("Opus", Amf::Number(CAN_DECODE))]),
                                            ),
                                        ]),
                                        obj(&[
                                            ("level", s("status")),
                                            ("code", s("NetConnection.Connect.Success")),
                                            ("description", s("Connection succeeded.")),
                                            ("objectEncoding", Amf::Number(0.0)),
                                        ]),
                                    ],
                                    false,
                                )
                                .await;
                        }
                        res
                    }
                    "releaseStream" | "FCPublish" | "FCUnpublish" | "getStreamLength" | "_checkbw" => {
                        c.command(0, &[s("_result"), Amf::Number(txn), Amf::Null, Amf::Null], latency_on).await
                    }
                    "createStream" => c.command(0, &[s("_result"), Amf::Number(txn), Amf::Null, Amf::Number(1.0)], false).await,
                    "publish" => {
                        let full = vals.get(3).and_then(|v| v.str()).unwrap_or("").to_string();
                        let key = full.split('?').next().unwrap_or("").trim_matches('/').to_string();
                        let platform = app.split('/').next().unwrap_or("").to_string();
                        if publisher.is_some() {
                            Ok(())
                        } else {
                            match hub.authorize(proto, &platform, &key, remote) {
                                Ok(p) => {
                                    p.session.event(Level::Info, "rtmp", format!("publish app '{app}' key '{key}'"));
                                    // the key's query (Twitch Dual Format's clientConfigId): names only
                                    let query = key_query(&full);
                                    if let Some(q) = &query {
                                        let names: Vec<&str> =
                                            q["params"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
                                        let cc =
                                            if q["client_config_id"] == true { "clientConfigId is there" } else { "no clientConfigId" };
                                        p.session.event(
                                            Level::Info,
                                            "key-query",
                                            format!("the key has a query: {} ({cc})", names.join(", ")),
                                        );
                                    }
                                    if let Some(c) = &client_caps {
                                        p.session.event(
                                            Level::Info,
                                            "enhanced-rtmp",
                                            format!("the sender's Enhanced RTMP capabilities: {}", describe_caps(c)),
                                        );
                                    }
                                    p.session.set_client(json!({ "enhanced_rtmp": client_caps, "key_query": query }));
                                    publisher = Some(p);
                                    let r = c.control(4, &[0, 0, 0, 0, 0, 1]).await;
                                    if r.is_ok() {
                                        c.status("status", "NetStream.Publish.Start", &format!("{key} is now published."), true).await
                                    } else {
                                        r
                                    }
                                }
                                Err(refusal) => {
                                    let code = if refusal.code == "bad-key" || refusal.code == "no-key" {
                                        "NetStream.Publish.BadName"
                                    } else {
                                        "NetStream.Publish.Failed"
                                    };
                                    let _ = c.status("error", code, &refusal.msg, false).await;
                                    let _ = c.w.flush().await;
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                    return None;
                                }
                            }
                        }
                    }
                    "deleteStream" | "closeStream" => {
                        return publisher.map(|p| (p, "the sender ended the stream".to_string()));
                    }
                    _ => Ok(()),
                };
                let _ = msid;
                if io.is_err() {
                    return publisher.map(|p| (p, "write error".to_string()));
                }
            }
            _ => {}
        }
        let _ = json!(null);
    }
}

/// An RTMP server on a listener (plain, or wrapped in TLS when `tls` is given).
pub async fn listen(hub: Arc<Hub>, listener: tokio::net::TcpListener, proto: Proto, tls: Option<tokio_rustls::TlsAcceptor>) {
    loop {
        let Ok((sock, remote)) = listener.accept().await else { continue };
        let _ = sock.set_nodelay(true);
        let hub = hub.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            if hub.refusing_all() {
                return;
            }
            match tls {
                Some(acceptor) => {
                    if let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(sock)).await {
                        serve(stream, hub, proto, remote).await;
                    }
                }
                None => serve(sock, hub, proto, remote).await,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_and_enhanced_tags_become_media() {
        let mut d = FlvDemux::new();
        let (mut out, mut warn) = (vec![], vec![]);
        // avcC with 4-byte lengths, one SPS and PPS
        let avcc = vec![1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x1f, 1, 0, 2, 0x68, 0xeb];
        let mut tag = vec![0x17, 0, 0, 0, 0];
        tag.extend(&avcc);
        d.video(0, &tag, &mut out, &mut warn);
        assert!(matches!(out[0], Media::VideoConfig { codec: VCodec::H264, .. }));
        // a keyframe with composition time 66 ms at 1 s
        let mut tag = vec![0x17, 1, 0, 0, 66];
        tag.extend([0, 0, 0, 3, 0x65, 1, 2]);
        d.video(1000, &tag, &mut out, &mut warn);
        let Media::Video { pts_us, dts_us, key, .. } = out[1] else { panic!() };
        assert_eq!((pts_us, dts_us, key), (1_066_000, 1_000_000, true));
        // enhanced: HEVC, coded frames without composition time
        let mut hv = vec![0u8; 23];
        hv[21] = 3;
        let mut tag = vec![0x90, b'h', b'v', b'c', b'1'];
        tag.extend(&hv);
        d.video(0, &tag, &mut out, &mut warn);
        assert!(matches!(out[2], Media::VideoConfig { codec: VCodec::Hevc, .. }));
        let mut tag = vec![0x93, b'h', b'v', b'c', b'1'];
        tag.extend([0, 0, 0, 2, 0x26, 1]);
        d.video(40, &tag, &mut out, &mut warn);
        assert!(matches!(out[3], Media::Video { key: true, .. }));
        // an unknown codec is said so once
        d.video(0, &[0x90, b'x', b'x', b'x', b'x', 0], &mut out, &mut warn);
        d.video(0, &[0x90, b'x', b'x', b'x', b'x', 0], &mut out, &mut warn);
        assert_eq!(warn.len(), 1, "{warn:?}");
    }

    #[test]
    fn chunks_are_split_and_continued() {
        let mut w = ChunkWriter::new();
        w.chunk = 4;
        let out = w.message(3, 20, 1, 7, &[1, 2, 3, 4, 5, 6]);
        // header (12 bytes) + 4 payload bytes, then a type 3 header and the last 2
        assert_eq!(out.len(), 12 + 4 + 1 + 2);
        assert_eq!(out[16], 0xc3);
    }

    #[test]
    fn aac_and_opus_tags_become_media() {
        let mut d = FlvDemux::new();
        let (mut out, mut warn) = (vec![], vec![]);
        d.audio(0, &[0xaf, 0, 0x11, 0x90], &mut out, &mut warn);
        d.audio(23, &[0xaf, 1, 9, 9, 9], &mut out, &mut warn);
        assert!(matches!(&out[0], Media::AudioConfig { sample_rate: 48000, channels: 2, .. }));
        assert!(matches!(&out[1], Media::Audio { pts_us: 23_000, .. }));
        d.audio(0, &[0x20, 0, 0], &mut out, &mut warn);
        assert_eq!(warn.len(), 1);
    }
}
