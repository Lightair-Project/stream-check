//! Pulling from an RTSP server (`rtsp://host:8554/live`), as a player would: OPTIONS, DESCRIBE,
//! SETUP of each track, PLAY, then RTP over the RTSP connection itself (interleaved, RFC 2326
//! §10.12). H.264 (RFC 6184) and HEVC (RFC 7798) pictures, AAC (RFC 3640, AAC-hbr and -lbr) and
//! Opus sound. What arrives is a session like a sender's: the same analysis, recording, preview
//! and report, with the RTP numbers (lost, duplicated) and the server's description beside them.
//! The session ends when the server stops sending, or at `end`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex as AsyncMutex;

use crate::analyze::Level;
use crate::hub::{Hub, Proto, Publisher, Session};
use crate::media::{ACodec, AnnexB, Media, opus_toc, read_asc};
use crate::nal::VCodec;
use crate::rtp::{AudioOrder, VideoAssembler};

/// Nothing from the server for this long ends the session.
const SILENT_END: Duration = Duration::from_secs(10);

/// The parts of an `rtsp://` URL.
#[derive(Debug, Clone)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub user: Option<(String, String)>,
    /// The URL without its user and password (what is sent and reported).
    pub clean: String,
}

pub fn parse_url(url: &str) -> Result<Url, String> {
    let rest = url.strip_prefix("rtsp://").ok_or("an RTSP address starts with rtsp://")?;
    let (auth, rest) = match rest.split_once('@') {
        Some((a, r)) if !a.contains('/') => (Some(a), r),
        _ => (None, rest),
    };
    let (hostport, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || hostport.starts_with('[') => (h, p.parse().map_err(|_| format!("'{p}' isn't a port"))?),
        _ => (hostport, 554),
    };
    if host.is_empty() {
        return Err("the RTSP address has no host".into());
    }
    let user = auth.map(|a| {
        let (u, p) = a.split_once(':').unwrap_or((a, ""));
        (u.to_string(), p.to_string())
    });
    Ok(Url { host: host.to_string(), port, user, clean: format!("rtsp://{hostport}/{path}") })
}

/// One track the server describes.
#[derive(Debug, Clone, Default)]
struct Track {
    media: String,
    payload_type: u8,
    encoding: String,
    clock: u32,
    channels: u8,
    fmtp: HashMap<String, String>,
    control: String,
}

impl Track {
    fn video_codec(&self) -> Option<VCodec> {
        match (self.media.as_str(), self.encoding.to_ascii_uppercase().as_str()) {
            ("video", "H264") => Some(VCodec::H264),
            ("video", "H265" | "HEVC") => Some(VCodec::Hevc),
            _ => None,
        }
    }
    fn audio_codec(&self) -> Option<ACodec> {
        match (self.media.as_str(), self.encoding.to_ascii_uppercase().as_str()) {
            ("audio", "MPEG4-GENERIC") if self.fmtp.get("mode").is_some_and(|m| m.to_ascii_lowercase().starts_with("aac")) => {
                Some(ACodec::Aac)
            }
            ("audio", "OPUS") => Some(ACodec::Opus),
            _ => None,
        }
    }
    fn describe(&self) -> Value {
        json!({
            "media": self.media,
            "payload_type": self.payload_type,
            "codec": self.encoding,
            "clock_rate": self.clock,
            "channels": self.channels,
            "control": self.control,
            "fmtp": self.fmtp,
        })
    }
}

/// The tracks of an SDP description, and the session's own control URL.
fn parse_sdp(sdp: &str) -> (Option<String>, Vec<Track>) {
    let mut session_control = None;
    let mut tracks: Vec<Track> = vec![];
    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=") {
            let parts: Vec<&str> = m.split_whitespace().collect();
            tracks.push(Track {
                media: parts.first().unwrap_or(&"").to_string(),
                payload_type: parts.get(3).and_then(|p| p.parse().ok()).unwrap_or(0),
                ..Default::default()
            });
        } else if let Some(a) = line.strip_prefix("a=") {
            let (k, v) = a.split_once(':').unwrap_or((a, ""));
            match (k, tracks.last_mut()) {
                ("control", None) => session_control = Some(v.to_string()),
                ("control", Some(t)) => t.control = v.to_string(),
                ("rtpmap", Some(t)) => {
                    // 96 H264/90000, 97 MPEG4-GENERIC/48000/2
                    let (_, enc) = v.split_once(' ').unwrap_or(("", v));
                    let mut p = enc.split('/');
                    t.encoding = p.next().unwrap_or("").to_string();
                    t.clock = p.next().and_then(|c| c.parse().ok()).unwrap_or(90_000);
                    t.channels = p.next().and_then(|c| c.parse().ok()).unwrap_or(1);
                }
                ("fmtp", Some(t)) => {
                    let (_, params) = v.split_once(' ').unwrap_or(("", v));
                    for kv in params.split(';') {
                        if let Some((k, v)) = kv.trim().split_once('=') {
                            t.fmtp.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // a payload type without an rtpmap: the static ones RTSP servers use
    for t in &mut tracks {
        if t.encoding.is_empty() && t.payload_type == 14 {
            t.encoding = "MPA".into();
        }
    }
    (session_control, tracks)
}

fn resolve(base: &str, control: &str) -> String {
    if control.is_empty() || control == "*" {
        return base.to_string();
    }
    if control.starts_with("rtsp://") {
        return control.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), control.trim_start_matches('/'))
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = vec![];
    let (mut acc, mut bits) = (0u32, 0);
    for v in s.bytes().filter_map(val) {
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

/// What the server sent: a response, RTP or RTCP on a channel, or a request of its own.
enum Item {
    Response { status: u16, headers: HashMap<String, String>, body: String },
    Data { channel: u8, data: Vec<u8> },
    Request,
}

async fn read_item(r: &mut BufReader<OwnedReadHalf>) -> std::io::Result<Item> {
    let first = {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf[0]
    };
    if first == b'$' {
        let mut h = [0u8; 4];
        r.read_exact(&mut h).await?;
        let len = u16::from_be_bytes([h[2], h[3]]) as usize;
        let mut data = vec![0u8; len];
        r.read_exact(&mut data).await?;
        return Ok(Item::Data { channel: h[1], data });
    }
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    let line = line.trim().to_string();
    if line.is_empty() {
        return Ok(Item::Request);
    }
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let h = h.trim();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len.min(1 << 20)];
    r.read_exact(&mut body).await?;
    if let Some(rest) = line.strip_prefix("RTSP/1.0 ") {
        let status = rest.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
        return Ok(Item::Response { status, headers, body: String::from_utf8_lossy(&body).into_owned() });
    }
    // a request from the server (an ANNOUNCE, a SET_PARAMETER): nothing the lab needs
    Ok(Item::Request)
}

/// The RTSP conversation before PLAY: requests and their answers, RTP that comes early dropped.
struct Talk {
    r: BufReader<OwnedReadHalf>,
    w: Arc<AsyncMutex<OwnedWriteHalf>>,
    cseq: u32,
    auth: Option<String>,
    session: Option<String>,
}

impl Talk {
    async fn request(&mut self, method: &str, url: &str, extra: &str) -> Result<(u16, HashMap<String, String>, String), String> {
        self.cseq += 1;
        let mut msg = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: Stream Check\r\n{extra}", self.cseq);
        if let Some(a) = &self.auth {
            msg.push_str(&format!("Authorization: {a}\r\n"));
        }
        if let Some(s) = &self.session {
            msg.push_str(&format!("Session: {s}\r\n"));
        }
        msg.push_str("\r\n");
        self.w.lock().await.write_all(msg.as_bytes()).await.map_err(|e| format!("{method}: {e}"))?;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), read_item(&mut self.r)).await {
                Err(_) => return Err(format!("{method}: the server didn't answer in 10 s")),
                Ok(Err(e)) => return Err(format!("{method}: the server closed the connection ({e})")),
                Ok(Ok(Item::Response { status, headers, body })) => return Ok((status, headers, body)),
                Ok(Ok(_)) => continue,
            }
        }
    }
}

/// What PLAY set up: the tracks read, each on its channel.
struct Playing {
    tracks: Vec<(Track, u8)>,
    rtp_info: HashMap<String, u32>,
    timeout_s: u64,
    describe: Value,
}

/// Connects, describes, sets up and plays; the session starts only once the server plays.
pub async fn start(hub: &Arc<Hub>, url: &str, key: Option<String>, profile: &str) -> Result<Arc<Session>, String> {
    let u = parse_url(url)?;
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((u.host.as_str(), u.port)))
        .await
        .map_err(|_| format!("{}:{} didn't answer in 5 s", u.host, u.port))?
        .map_err(|e| format!("can't connect to {}:{}: {e}", u.host, u.port))?;
    let _ = stream.set_nodelay(true);
    let remote = stream.peer_addr().unwrap_or(SocketAddr::from(([0, 0, 0, 0], u.port)));
    let (r, w) = stream.into_split();
    let w = Arc::new(AsyncMutex::new(w));
    let mut talk = Talk { r: BufReader::with_capacity(1 << 16, r), w: w.clone(), cseq: 0, auth: None, session: None };
    let playing = match handshake(&mut talk, &u).await {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    let name = key.unwrap_or_else(|| default_key(&u));
    let publisher = hub.authorize_pull(Proto::Rtsp, profile, &name, remote).map_err(|r| r.msg)?;
    let session = publisher.session.clone();
    session.set_source(playing.describe.clone());
    for (t, _) in &playing.tracks {
        session.event(Level::Info, "rtsp", format!("{} {} at {} Hz, control {}", t.media, t.encoding, t.clock, t.control));
    }
    let talk_session = talk.session.clone();
    let auth = talk.auth.clone();
    let cseq = talk.cseq;
    let clean = u.clean.clone();
    tokio::spawn(async move {
        let keep = keepalive(w.clone(), clean.clone(), talk_session.clone(), auth.clone(), cseq, playing.timeout_s);
        receive(talk.r, publisher, playing).await;
        keep.abort();
        // a polite goodbye; the server may already be gone
        let mut msg = format!("TEARDOWN {clean} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: Stream Check\r\n", cseq + 1000);
        if let Some(s) = &talk_session {
            msg.push_str(&format!("Session: {s}\r\n"));
        }
        if let Some(a) = &auth {
            msg.push_str(&format!("Authorization: {a}\r\n"));
        }
        msg.push_str("\r\n");
        let _ = tokio::time::timeout(Duration::from_secs(1), async { w.lock().await.write_all(msg.as_bytes()).await }).await;
    });
    Ok(session)
}

fn default_key(u: &Url) -> String {
    let path = u.clean.trim_start_matches("rtsp://").split_once('/').map_or("", |(_, p)| p);
    let last = path.split('?').next().unwrap_or("").trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if last.is_empty() { format!("rtsp-{}", u.host) } else { format!("rtsp-{last}") }
}

async fn handshake(talk: &mut Talk, u: &Url) -> Result<Playing, String> {
    let url = u.clean.clone();
    let (_, opts, _) = talk.request("OPTIONS", &url, "").await.unwrap_or((0, HashMap::new(), String::new()));
    let mut described = talk.request("DESCRIBE", &url, "Accept: application/sdp\r\n").await?;
    if described.0 == 401 {
        let how = described.1.get("www-authenticate").cloned().unwrap_or_default();
        if !how.to_ascii_lowercase().starts_with("basic") {
            return Err(format!(
                "DESCRIBE: the server wants authentication ({}) that Stream Check doesn't do (Basic only)",
                how.split_whitespace().next().unwrap_or("?")
            ));
        }
        let Some((user, pass)) = &u.user else {
            return Err("DESCRIBE: the server wants a user name and password (rtsp://user:pass@host/…)".into());
        };
        talk.auth = Some(format!("Basic {}", base64(format!("{user}:{pass}").as_bytes())));
        described = talk.request("DESCRIBE", &url, "Accept: application/sdp\r\n").await?;
    }
    let (status, headers, sdp) = described;
    if status != 200 {
        return Err(format!("DESCRIBE: the server said {status}"));
    }
    let base = headers.get("content-base").or_else(|| headers.get("content-location")).cloned().unwrap_or_else(|| url.clone());
    let (session_control, tracks) = parse_sdp(&sdp);
    let base = session_control.as_deref().filter(|c| c.starts_with("rtsp://")).map_or(base, str::to_string);
    let mut set_up: Vec<(Track, u8)> = vec![];
    let mut skipped = vec![];
    let mut timeout_s = 60;
    for t in &tracks {
        if t.video_codec().is_none() && t.audio_codec().is_none() {
            skipped.push(format!("{} {}", t.media, t.encoding));
            continue;
        }
        if set_up.iter().any(|(x, _)| x.media == t.media) {
            skipped.push(format!("a second {} track ({})", t.media, t.encoding));
            continue;
        }
        let ch = (set_up.len() * 2) as u8;
        let transport = format!("Transport: RTP/AVP/TCP;unicast;interleaved={}-{}\r\n", ch, ch + 1);
        let (status, h, _) = talk.request("SETUP", &resolve(&base, &t.control), &transport).await?;
        if status != 200 {
            return Err(format!("SETUP of the {} track: the server said {status} (Stream Check asks for RTP over TCP)", t.media));
        }
        if let Some(s) = h.get("session") {
            let mut parts = s.split(';');
            talk.session = parts.next().map(|x| x.trim().to_string());
            for p in parts {
                if let Some(t) = p.trim().strip_prefix("timeout=") {
                    timeout_s = t.parse().unwrap_or(60);
                }
            }
        }
        // the channel the server chose
        let given = h
            .get("transport")
            .and_then(|tr| tr.split(';').find_map(|p| p.trim().strip_prefix("interleaved=")))
            .and_then(|i| i.split('-').next())
            .and_then(|c| c.parse().ok())
            .unwrap_or(ch);
        set_up.push((t.clone(), given));
    }
    if set_up.is_empty() {
        return Err(format!("the server offers nothing Stream Check reads (H.264, HEVC, AAC, Opus): {}", skipped.join(", ")));
    }
    let (status, h, _) = talk.request("PLAY", &base, "Range: npt=0.000-\r\n").await?;
    if status != 200 {
        return Err(format!("PLAY: the server said {status}"));
    }
    // RTP-Info: each track's RTP time at the start, a common clock for picture and sound
    let mut rtp_info = HashMap::new();
    if let Some(info) = h.get("rtp-info") {
        for part in info.split(',') {
            let mut url = None;
            let mut time = None;
            for kv in part.split(';') {
                match kv.trim().split_once('=') {
                    Some(("url", v)) => url = Some(v.to_string()),
                    Some(("rtptime", v)) => time = v.parse().ok(),
                    _ => {}
                }
            }
            if let (Some(u), Some(t)) = (url, time) {
                rtp_info.insert(u, t);
            }
        }
    }
    let describe = json!({
        "kind": "rtsp",
        "url": u.clean,
        "server": headers.get("server").or_else(|| opts.get("server")),
        "methods": opts.get("public"),
        "sdp": sdp,
        "tracks": set_up.iter().map(|(t, ch)| { let mut v = t.describe(); v["channel"] = json!(ch); v }).collect::<Vec<_>>(),
        "skipped": skipped,
        "session_timeout_s": timeout_s,
        "transport": "RTP/AVP/TCP interleaved",
        "timing": if rtp_info.is_empty() { "each track from its first packet's arrival (no RTP-Info)" } else { "RTP-Info (a common start for every track)" },
    });
    let mut tracks_out = set_up;
    for (t, _) in &mut tracks_out {
        t.control = resolve(&base, &t.control);
    }
    Ok(Playing { tracks: tracks_out, rtp_info, timeout_s, describe })
}

/// OPTIONS now and then, so the server keeps the session.
fn keepalive(
    w: Arc<AsyncMutex<OwnedWriteHalf>>,
    url: String,
    session: Option<String>,
    auth: Option<String>,
    mut cseq: u32,
    timeout_s: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let every = Duration::from_secs((timeout_s / 2).clamp(5, 30));
        loop {
            tokio::time::sleep(every).await;
            cseq += 1;
            let mut msg = format!("OPTIONS {url} RTSP/1.0\r\nCSeq: {cseq}\r\nUser-Agent: Stream Check\r\n");
            if let Some(s) = &session {
                msg.push_str(&format!("Session: {s}\r\n"));
            }
            if let Some(a) = &auth {
                msg.push_str(&format!("Authorization: {a}\r\n"));
            }
            msg.push_str("\r\n");
            if w.lock().await.write_all(msg.as_bytes()).await.is_err() {
                return;
            }
        }
    })
}

/// RTP time to microseconds of the session, counting wrap-arounds.
struct Clock {
    prev: u32,
    ticks: i64,
    base_us: i64,
    rate: i64,
}

impl Clock {
    fn new(first_ts: u32, base_us: i64, rate: u32) -> Clock {
        Clock { prev: first_ts, ticks: 0, base_us, rate: rate.max(1) as i64 }
    }
    fn us(&mut self, ts: u32) -> i64 {
        // each step as a signed distance, so a wrap-around or a small step back stays in line
        self.ticks += ts.wrapping_sub(self.prev) as i32 as i64;
        self.prev = ts;
        self.base_us + self.ticks * 1_000_000 / self.rate
    }
}

/// One track being received.
enum Rx {
    Video { codec: VCodec, asm: Box<VideoAssembler>, annexb: AnnexB },
    Aac { order: AudioOrder, size_bits: u32, index_bits: u32, delta_bits: u32 },
    Opus { order: AudioOrder },
}

/// The RTP headers' fields the lab uses: (sequence, timestamp, marker, payload).
fn rtp_packet(d: &[u8]) -> Option<(u16, u32, bool, &[u8])> {
    if d.len() < 12 || d[0] >> 6 != 2 {
        return None;
    }
    let cc = (d[0] & 0x0f) as usize;
    let mut at = 12 + 4 * cc;
    if d[0] & 0x10 != 0 {
        let len = u16::from_be_bytes([*d.get(at + 2)?, *d.get(at + 3)?]) as usize;
        at += 4 + 4 * len;
    }
    let mut end = d.len();
    if d[0] & 0x20 != 0 {
        end = end.checked_sub(*d.last()? as usize)?;
    }
    let payload = d.get(at..end)?;
    Some((u16::from_be_bytes([d[2], d[3]]), u32::from_be_bytes([d[4], d[5], d[6], d[7]]), d[1] & 0x80 != 0, payload))
}

/// The access units of an RFC 3640 payload (AU headers, then the units).
fn aac_units(p: &[u8], size_bits: u32, index_bits: u32, delta_bits: u32) -> Vec<&[u8]> {
    let mut out = vec![];
    if size_bits == 0 || p.len() < 2 {
        return out;
    }
    let header_bits = u16::from_be_bytes([p[0], p[1]]) as usize;
    let header_bytes = header_bits.div_ceil(8);
    let Some(headers) = p.get(2..2 + header_bytes) else { return out };
    let mut b = crate::nal::Bits::new(headers);
    let mut sizes = vec![];
    let mut read = 0;
    let mut first = true;
    while read + (size_bits as usize) <= header_bits {
        let Some(size) = b.u(size_bits as usize) else { break };
        let idx_bits = if first { index_bits } else { delta_bits } as usize;
        if b.u(idx_bits).is_none() && idx_bits > 0 {
            break;
        }
        read += size_bits as usize + idx_bits;
        sizes.push(size as usize);
        first = false;
    }
    let mut at = 2 + header_bytes;
    for s in sizes {
        let Some(unit) = p.get(at..at + s) else { break };
        out.push(unit);
        at += s;
    }
    out
}

/// Reads RTP until the server stops or the session is told to end; says why it ended.
async fn receive(mut r: BufReader<OwnedReadHalf>, mut p: Publisher, playing: Playing) -> String {
    let s = p.session.clone();
    let mut rx: HashMap<u8, (usize, Rx, Option<Clock>)> = HashMap::new();
    for (i, (t, ch)) in playing.tracks.iter().enumerate() {
        let num = |k: &str| t.fmtp.get(k).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
        let r = if let Some(codec) = t.video_codec() {
            let mut annexb = AnnexB::new(codec);
            // the parameter sets the description carries, for a server that sends them only there
            let mut sets: Vec<Vec<u8>> = vec![];
            for k in ["sprop-vps", "sprop-sps", "sprop-pps", "sprop-parameter-sets"] {
                if let Some(v) = t.fmtp.get(k) {
                    sets.extend(v.split(',').map(unbase64).filter(|x| !x.is_empty()));
                }
            }
            if !sets.is_empty() {
                let refs: Vec<&[u8]> = sets.iter().map(Vec::as_slice).collect();
                let mut out = vec![];
                annexb.picture(&refs, 0, 0, &mut out);
                for m in out {
                    p.media(m);
                }
            }
            Rx::Video { codec, asm: Box::new(VideoAssembler::with_codec(codec)), annexb }
        } else if t.audio_codec() == Some(ACodec::Aac) {
            let asc = unhex(t.fmtp.get("config").map_or("", String::as_str));
            let (rate, ch, _) = read_asc(&asc).unwrap_or((t.clock, t.channels, 2));
            p.media(Media::AudioConfig { codec: ACodec::Aac, config: Arc::new(asc), sample_rate: rate, channels: ch });
            Rx::Aac {
                order: AudioOrder::new(),
                size_bits: num("sizelength"),
                index_bits: num("indexlength"),
                delta_bits: num("indexdeltalength"),
            }
        } else {
            let ch = if t.channels >= 2 { 2 } else { 1 };
            p.media(Media::AudioConfig { codec: ACodec::Opus, config: Arc::new(vec![]), sample_rate: 48000, channels: ch });
            Rx::Opus { order: AudioOrder::new() }
        };
        rx.insert(*ch, (i, r, None));
    }
    let mut rtcp = 0u64;
    let mut last_stats = Instant::now();
    let play_us = s.t0.elapsed().as_micros() as i64;
    let reason = loop {
        let item = tokio::select! {
            _ = p.killed() => break s.stop_reason(),
            i = tokio::time::timeout(SILENT_END, read_item(&mut r)) => i,
        };
        let (channel, data) = match item {
            Err(_) => break format!("nothing came from the server for {} s", SILENT_END.as_secs()),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break "the server closed the connection".into(),
            Ok(Err(e)) => break format!("the connection failed: {e}"),
            Ok(Ok(Item::Data { channel, data })) => (channel, data),
            Ok(Ok(Item::Response { status, .. })) => {
                if status >= 400 {
                    s.event(Level::Warn, "rtsp", format!("the server answered a keep-alive with {status}"));
                }
                continue;
            }
            Ok(Ok(Item::Request)) => continue,
        };
        if !p.gate(data.len() + 4).await {
            break "the connection was dropped (a fault)".into();
        }
        let now = Instant::now();
        let Some((idx, track, clock)) = rx.get_mut(&channel) else {
            if channel % 2 == 1 {
                rtcp += 1;
            }
            continue;
        };
        let Some((seq, ts, marker, payload)) = rtp_packet(&data) else { continue };
        let t = &playing.tracks[*idx].0;
        let clock = clock.get_or_insert_with(|| match playing.rtp_info.get(&t.control) {
            Some(&start) => Clock::new(start, play_us, t.clock),
            None => Clock::new(ts, s.t0.elapsed().as_micros() as i64, t.clock),
        });
        match track {
            Rx::Video { codec, asm, annexb } => {
                let mut pictures = asm.push(seq, ts, marker, payload, now);
                pictures.extend(asm.tick(now));
                for pic in pictures {
                    let pts = clock.us(pic.ts);
                    let refs: Vec<&[u8]> = pic.nals.iter().map(Vec::as_slice).collect();
                    let mut out = vec![];
                    if codec.is_nal() {
                        annexb.picture(&refs, pts, pts, &mut out);
                    }
                    for m in out {
                        p.media(m);
                    }
                }
            }
            Rx::Aac { order, size_bits, index_bits, delta_bits } => {
                for pk in order.push(seq, ts, payload, now) {
                    let first = clock.us(pk.ts);
                    for (k, unit) in aac_units(&pk.payload, *size_bits, *index_bits, *delta_bits).into_iter().enumerate() {
                        // each unit after the first is 1024 samples later
                        let pts = first + k as i64 * 1024 * 1_000_000 / clock.rate;
                        p.media(Media::Audio { pts_us: pts, data: Arc::new(unit.to_vec()) });
                    }
                }
            }
            Rx::Opus { order } => {
                for pk in order.push(seq, ts, payload, now) {
                    if opus_toc(&pk.payload).is_some() {
                        p.media(Media::Audio { pts_us: clock.us(pk.ts), data: Arc::new(pk.payload) });
                    }
                }
            }
        }
        if last_stats.elapsed() >= Duration::from_millis(500) {
            last_stats = now;
            report_rtp(&p, &rx);
        }
    };
    // what was still being put together
    for (_, track, clock) in rx.values_mut() {
        if let (Rx::Video { codec, asm, annexb }, Some(clock)) = (track, clock) {
            for pic in asm.finish() {
                let pts = clock.us(pic.ts);
                let refs: Vec<&[u8]> = pic.nals.iter().map(Vec::as_slice).collect();
                let mut out = vec![];
                if codec.is_nal() {
                    annexb.picture(&refs, pts, pts, &mut out);
                }
                for m in out {
                    p.media(m);
                }
            }
        }
    }
    report_rtp(&p, &rx);
    s.update_source(|v| v["rtcp_packets"] = json!(rtcp));
    p.end(&reason);
    reason
}

fn report_rtp(p: &Publisher, rx: &HashMap<u8, (usize, Rx, Option<Clock>)>) {
    for (_, track, _) in rx.values() {
        match track {
            Rx::Video { asm, .. } => p.rtp(false, asm.stats()),
            Rx::Aac { order, .. } | Rx::Opus { order } => p.rtp(true, order.stats()),
        }
    }
}
