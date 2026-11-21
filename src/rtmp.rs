//! RTMP and RTMPS ingest: the server side of the handshake, the chunk stream and the publish
//! commands, FLV tags read into [`Media`] (H.264 and AAC; HEVC and Opus the Enhanced RTMP way;
//! the older HEVC codec id some servers use), and the tags kept as an FLV file as they came.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::amf::{self, Amf, obj, s};
use crate::analyze::Level;
use crate::hub::{Hub, Proto, Publisher};
use crate::media::{ACodec, Media};
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

/// Turns FLV tag bodies into [`Media`].
#[derive(Default)]
pub struct FlvDemux {
    nal_size: usize,
    video_codec: Option<VCodec>,
    warned: Vec<String>,
}

impl FlvDemux {
    pub fn new() -> Self {
        FlvDemux { nal_size: 4, ..Default::default() }
    }

    /// The video tag body; says what it couldn't handle through `warn`.
    pub fn video(&mut self, ts: u32, d: &[u8], out: &mut Vec<Media>, warn: &mut Vec<String>) {
        let mut unsupported = |what: String, warned: &mut Vec<String>| {
            if !warned.contains(&what) {
                warn.push(format!("the video {what} isn't supported by the lab (H.264 and HEVC are)"));
                warned.push(what);
            }
        };
        let i24 = |b: &[u8]| (((b[0] as i32) << 16 | (b[1] as i32) << 8 | b[2] as i32) << 8) >> 8;
        if d.len() < 5 {
            return;
        }
        if d[0] & 0x80 != 0 {
            // Enhanced RTMP: packet type, frame type, FourCC
            let (pt, ft) = (d[0] & 0x0f, (d[0] >> 4) & 7);
            let codec = match &d[1..5] {
                b"hvc1" => VCodec::Hevc,
                b"avc1" => VCodec::H264,
                other => return unsupported(format!("codec '{}'", String::from_utf8_lossy(other)), &mut self.warned),
            };
            match pt {
                0 => self.config(codec, &d[5..], out),
                1 if d.len() >= 8 => self.frame(codec, ts, i24(&d[5..8]), ft, &d[8..], out),
                3 => self.frame(codec, ts, 0, ft, &d[5..], out),
                _ => {}
            }
            return;
        }
        // the classic tag: frame type, codec id, then AVC packet type and composition time
        let ft = d[0] >> 4;
        let codec = match d[0] & 0x0f {
            7 => VCodec::H264,
            12 => VCodec::Hevc,
            other => return unsupported(format!("codec id {other}"), &mut self.warned),
        };
        match d[1] {
            0 => self.config(codec, &d[5..], out),
            1 => self.frame(codec, ts, i24(&d[2..5]), ft, &d[5..], out),
            _ => {}
        }
    }

    fn config(&mut self, codec: VCodec, record: &[u8], out: &mut Vec<Media>) {
        self.video_codec = Some(codec);
        self.nal_size = match codec {
            VCodec::H264 => record.get(4).map_or(4, |b| (b & 3) as usize + 1),
            VCodec::Hevc => record.get(21).map_or(4, |b| (b & 3) as usize + 1),
        };
        out.push(Media::VideoConfig { codec, record: Arc::new(record.to_vec()) });
    }

    fn frame(&mut self, codec: VCodec, ts: u32, cts: i32, frame_type: u8, data: &[u8], out: &mut Vec<Media>) {
        if self.video_codec != Some(codec) || data.is_empty() {
            return;
        }
        let data = if self.nal_size == 4 {
            data.to_vec()
        } else {
            let nals = nal::split_lengths(data, self.nal_size);
            nal::to_lengths(&nals)
        };
        let dts = ts as i64 * 1000;
        out.push(Media::Video { pts_us: dts + cts as i64 * 1000, dts_us: dts, key: frame_type == 1, data: Arc::new(data) });
    }

    pub fn audio(&mut self, ts: u32, d: &[u8], out: &mut Vec<Media>, warn: &mut Vec<String>) {
        if d.len() < 2 {
            return;
        }
        let format = d[0] >> 4;
        match format {
            10 => match d[1] {
                0 => {
                    let (rate, ch, _) = crate::media::read_asc(&d[2..]).unwrap_or((44100, 2, 2));
                    out.push(Media::AudioConfig { codec: ACodec::Aac, config: Arc::new(d[2..].to_vec()), sample_rate: rate, channels: ch });
                }
                1 => out.push(Media::Audio { pts_us: ts as i64 * 1000, data: Arc::new(d[2..].to_vec()) }),
                _ => {}
            },
            9 if d.len() >= 5 && &d[1..5] == b"Opus" => match d[0] & 0x0f {
                0 => {
                    let ch = d.get(5 + 9).copied().unwrap_or(2);
                    out.push(Media::AudioConfig { codec: ACodec::Opus, config: Arc::new(vec![]), sample_rate: 48000, channels: ch });
                }
                1 => out.push(Media::Audio { pts_us: ts as i64 * 1000, data: Arc::new(d[5..].to_vec()) }),
                _ => {}
            },
            other => {
                let name = format!("audio format {other}");
                if !self.warned.contains(&name) {
                    warn.push(format!("the {name} isn't supported by the lab (AAC and Opus are)"));
                    self.warned.push(name);
                }
            }
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
                                        obj(&[("fmsVer", s("FMS/3,0,1,123")), ("capabilities", Amf::Number(31.0))]),
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
        d.video(0, &[0x90, b'a', b'v', b'0', b'1', 0], &mut out, &mut warn);
        d.video(0, &[0x90, b'a', b'v', b'0', b'1', 0], &mut out, &mut warn);
        assert_eq!(warn.len(), 1);
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
