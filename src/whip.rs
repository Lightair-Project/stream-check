//! WHIP ingest: the HTTP exchange (an SDP offer posted with a bearer token, the answer back with
//! a `Location`), a WebRTC peer that receives H.264, HEVC, AV1, VP9, VP8 and Opus, and RTP read
//! into [`Media`].
//! Sender time: WHIP carries no common clock without RTCP sender reports, so both tracks are
//! placed on the arrival timeline at their first packet and then follow their own RTP clocks.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS, MIME_TYPE_RTX, MIME_TYPE_VP8, MIME_TYPE_VP9, MediaEngine,
};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use tokio::sync::mpsc;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc::runtime::default_runtime;

use crate::analyze::Level;
use crate::hub::{Hub, Proto, Publisher};
use crate::media::{ACodec, AnnexB, Frames, Media, opus_toc};
use crate::nal::VCodec;
use crate::rtp::{AudioOrder, VideoAssembler};

struct Handler {
    gathered: mpsc::Sender<()>,
    state: mpsc::UnboundedSender<RTCPeerConnectionState>,
    tracks: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state.send(state);
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.tracks.send(track);
    }
}

/// The video codecs offered besides H.264, H.264 first: (MIME type, payload type, its RTX payload
/// type). No fmtp, so any profile of them matches.
const OTHER_VIDEO: [(&str, u8, u8); 4] =
    [(MIME_TYPE_HEVC, 49, 50), (MIME_TYPE_AV1, 45, 46), (MIME_TYPE_VP9, 98, 99), (MIME_TYPE_VP8, 96, 97)];

fn codecs() -> Vec<(RTCRtpCodecParameters, RtpCodecKind)> {
    let mut out = vec![];
    for (i, fmtp) in [
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f",
        "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f",
    ]
    .into_iter()
    .enumerate()
    {
        out.push((
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: fmtp.to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 102 + i as u8,
            },
            RtpCodecKind::Video,
        ));
    }
    // retransmissions the RFC 4588 way, one RTX payload type per H.264 one; the WebRTC stack
    // unwraps them (original sequence number back, primary SSRC) before they reach `read_track`
    for i in 0..5u8 {
        out.push((
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_RTX.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: format!("apt={}", 102 + i),
                    rtcp_feedback: vec![],
                },
                payload_type: 112 + i,
            },
            RtpCodecKind::Video,
        ));
    }
    for (mime, pt, rtx) in OTHER_VIDEO {
        for (mime, pt, fmtp) in [(mime, pt, String::new()), (MIME_TYPE_RTX, rtx, format!("apt={pt}"))] {
            out.push((
                RTCRtpCodecParameters {
                    rtp_codec: RTCRtpCodec {
                        mime_type: mime.to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: fmtp,
                        rtcp_feedback: vec![],
                    },
                    payload_type: pt,
                },
                RtpCodecKind::Video,
            ));
        }
    }
    out.push((
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
        },
        RtpCodecKind::Audio,
    ));
    out
}

/// H.264 RTP payloads into NAL units (single, STAP-A, FU-A).
#[derive(Default)]
pub struct H264Depay {
    fu: Vec<u8>,
    in_fu: bool,
    pub nals: Vec<Vec<u8>>,
}

impl H264Depay {
    pub fn push(&mut self, p: &[u8]) {
        let Some(&b0) = p.first() else { return };
        match b0 & 0x1f {
            1..=23 => self.nals.push(p.to_vec()),
            24 => {
                let mut i = 1;
                while i + 2 <= p.len() {
                    let n = u16::from_be_bytes([p[i], p[i + 1]]) as usize;
                    i += 2;
                    if i + n > p.len() {
                        break;
                    }
                    self.nals.push(p[i..i + n].to_vec());
                    i += n;
                }
            }
            28 if p.len() >= 2 => {
                let (start, end) = (p[1] & 0x80 != 0, p[1] & 0x40 != 0);
                if start {
                    self.fu.clear();
                    self.fu.push((b0 & 0xe0) | (p[1] & 0x1f));
                    self.in_fu = true;
                }
                if self.in_fu {
                    self.fu.extend(&p[2..]);
                    if end {
                        self.nals.push(std::mem::take(&mut self.fu));
                        self.in_fu = false;
                    }
                }
            }
            _ => {}
        }
    }

    /// A packet was lost: whatever fragment is half built is dropped.
    pub fn lost(&mut self) {
        self.fu.clear();
        self.in_fu = false;
    }
}

/// The lab's video codec for a negotiated MIME type.
fn video_codec(mime: &str) -> Option<VCodec> {
    let m = mime.to_ascii_lowercase();
    [
        (MIME_TYPE_H264, VCodec::H264),
        (MIME_TYPE_HEVC, VCodec::Hevc),
        (MIME_TYPE_AV1, VCodec::Av1),
        (MIME_TYPE_VP9, VCodec::Vp9),
        (MIME_TYPE_VP8, VCodec::Vp8),
    ]
    .into_iter()
    .find(|(t, _)| t.to_ascii_lowercase() == m)
    .map(|(_, c)| c)
}

/// Pictures into [`Media`]: parameter sets gathered for H.264 and HEVC, frames read for the others.
enum VideoOut {
    Nal(AnnexB),
    Frames(Frames),
}

struct Clock {
    first_ts: u32,
    base_us: i64,
    last: i64,
    rate: i64,
}

impl Clock {
    fn new(ts: u32, base_us: i64, rate: i64) -> Clock {
        Clock { first_ts: ts, base_us, last: 0, rate }
    }
    fn us(&mut self, ts: u32) -> i64 {
        // RTP timestamps wrap at 2^32; keep the running count
        let mut delta = ts.wrapping_sub(self.first_ts) as i64;
        if delta < self.last - (1 << 31) {
            delta += 1 << 32;
        }
        self.last = delta.max(self.last - (1 << 30));
        self.base_us + delta * 1_000_000 / self.rate
    }
}

/// Reads one track's RTP until it ends, feeding the publisher: packets are put in sequence order
/// first (late and retransmitted ones included), then pictures are built from them.
async fn read_track(track: Arc<dyn TrackRemote>, publisher: Arc<Publisher>, started: Instant) {
    enum Ev {
        Packet(u16, u32, bool, u32, Vec<u8>),
        Tick,
        End,
    }
    let kind = track.kind().await;
    let audio = kind == RtpCodecKind::Audio;
    // the video codec is known at the first packet (the negotiated one for its SSRC)
    let mut video: Option<(VideoAssembler, VideoOut)> = None;
    let mut opus = AudioOrder::new();
    let mut clock: Option<Clock> = None;
    let mut audio_cfg = false;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut last_stats = Instant::now();
    loop {
        let ev = tokio::select! {
            e = track.poll() => match e {
                Some(TrackRemoteEvent::OnRtpPacket(p)) => Ev::Packet(p.header.sequence_number, p.header.timestamp, p.header.marker, p.header.ssrc, p.payload.to_vec()),
                Some(_) => continue,
                None => Ev::End,
            },
            _ = tick.tick() => Ev::Tick,
        };
        let now = Instant::now();
        let end = matches!(ev, Ev::End);
        let mut pictures = vec![];
        let mut packets = vec![];
        match ev {
            Ev::Packet(seq, ts, marker, ssrc, payload) => {
                if audio {
                    packets = opus.push(seq, ts, &payload, now);
                } else {
                    if video.is_none() {
                        let mime = track.codec(ssrc).await.map(|c| c.mime_type).unwrap_or_default();
                        let codec = video_codec(&mime).unwrap_or_else(|| {
                            publisher.session.event(
                                Level::Warn,
                                "unsupported",
                                format!("WHIP video '{mime}' isn't one the lab reads; taken as H.264"),
                            );
                            VCodec::H264
                        });
                        publisher.session.event(Level::Info, "whip", format!("WHIP video is {}", codec.name()));
                        let out = if codec.is_nal() { VideoOut::Nal(AnnexB::new(codec)) } else { VideoOut::Frames(Frames::new(codec)) };
                        video = Some((VideoAssembler::with_codec(codec), out));
                    }
                    if let Some((v, _)) = video.as_mut() {
                        pictures = v.push(seq, ts, marker, &payload, now);
                    }
                }
            }
            Ev::Tick => {
                if audio {
                    packets = opus.tick(now);
                } else if let Some((v, _)) = video.as_mut() {
                    pictures = v.tick(now);
                }
            }
            Ev::End => {
                if let Some((v, _)) = video.as_mut() {
                    pictures = v.finish();
                }
            }
        }
        let rate = if audio { 48_000 } else { 90_000 };
        for p in packets {
            let c = clock.get_or_insert_with(|| Clock::new(p.ts, started.elapsed().as_micros() as i64, rate));
            let pts = c.us(p.ts);
            if !audio_cfg {
                audio_cfg = true;
                let ch = opus_toc(&p.payload).map_or(2, |t| t.1);
                publisher.media(Media::AudioConfig { codec: ACodec::Opus, config: Arc::new(vec![]), sample_rate: 48000, channels: ch });
            }
            publisher.media(Media::Audio { pts_us: pts, data: Arc::new(p.payload) });
        }
        for pic in pictures {
            let c = clock.get_or_insert_with(|| Clock::new(pic.ts, started.elapsed().as_micros() as i64, rate));
            let pts = c.us(pic.ts);
            let mut out = vec![];
            match video.as_mut().map(|v| &mut v.1) {
                Some(VideoOut::Nal(annexb)) => {
                    let refs: Vec<&[u8]> = pic.nals.iter().map(|n| n.as_slice()).collect();
                    annexb.picture(&refs, pts, pts, &mut out);
                }
                Some(VideoOut::Frames(frames)) => {
                    for f in &pic.nals {
                        frames.frame(pts, pts, false, f, &mut out);
                    }
                }
                None => {}
            }
            for m in out {
                publisher.media(m);
            }
        }
        if end || last_stats.elapsed() >= Duration::from_millis(500) {
            last_stats = Instant::now();
            publisher.rtp(audio, if audio { opus.stats() } else { video.as_ref().map(|v| v.0.stats()).unwrap_or_default() });
        }
        if end {
            return;
        }
    }
}

pub struct WhipState {
    pub hub: Arc<Hub>,
    sessions: Mutex<HashMap<String, Arc<Publisher>>>,
    next: std::sync::atomic::AtomicU64,
}

impl WhipState {
    pub fn new(hub: Arc<Hub>) -> Arc<WhipState> {
        Arc::new(WhipState { hub, sessions: Mutex::new(HashMap::new()), next: 1.into() })
    }

    /// Ends a WHIP session (the sender's DELETE).
    pub fn delete(&self, id: &str) -> bool {
        self.sessions.lock().remove(id).is_some()
    }

    /// The offer in, the answer out. `path` is `app/key` or `key`.
    pub async fn offer(
        self: &Arc<Self>,
        path: &str,
        bearer: Option<&str>,
        offer: String,
        remote: SocketAddr,
    ) -> Result<(String, String), (u16, String)> {
        if let Some(want) = &self.hub.cfg.whip_token {
            if bearer != Some(want.as_str()) {
                return Err((401, "the bearer token isn't the one this server wants".into()));
            }
        }
        let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
        let (app, key) = match parts.as_slice() {
            [] => return Err((404, "no stream key in the path".into())),
            [k] => ("", *k),
            [a, .., k] => (*a, *k),
        };
        let publisher = self.hub.authorize(Proto::Whip, app, key, remote).map_err(|r| {
            (
                if r.code == "in-use" {
                    409
                } else if r.code == "refused-fault" {
                    503
                } else {
                    403
                },
                r.msg,
            )
        })?;
        let publisher = Arc::new(publisher);
        let e = |e: rtc::shared::error::Error| (500u16, e.to_string());
        let mut media = MediaEngine::default();
        for (c, kind) in codecs() {
            media.register_codec(c, kind).map_err(e)?;
        }
        let registry = register_default_interceptors(Registry::new(), &mut media).map_err(e)?;
        let (gathered_tx, mut gathered) = mpsc::channel(1);
        let (state_tx, mut state) = mpsc::unbounded_channel();
        let (track_tx, mut tracks) = mpsc::unbounded_channel::<Arc<dyn TrackRemote>>();
        let runtime = default_runtime().ok_or((500, "no async runtime for WebRTC".to_string()))?;
        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler { gathered: gathered_tx, state: state_tx, tracks: track_tx }))
            .with_runtime(runtime)
            .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
            .build()
            .await
            .map_err(e)?;
        let offer = RTCSessionDescription::offer(offer).map_err(|x| (400u16, x.to_string()))?;
        pc.set_remote_description(offer).await.map_err(|x| (400u16, x.to_string()))?;
        let answer = pc.create_answer(None).await.map_err(e)?;
        pc.set_local_description(answer).await.map_err(e)?;
        let _ = tokio::time::timeout(Duration::from_secs(3), gathered.recv()).await;
        let sdp = pc.local_description().await.map(|d| d.sdp).ok_or((500u16, "no answer".to_string()))?;

        let id = format!("{}", self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        self.sessions.lock().insert(id.clone(), publisher.clone());
        let me = self.clone();
        let sid = id.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            let mut readers = vec![];
            let reason;
            loop {
                tokio::select! {
                    t = tracks.recv() => match t {
                        Some(t) => readers.push(tokio::spawn(read_track(t, publisher.clone(), started))),
                        None => { reason = "the peer connection closed".to_string(); break }
                    },
                    s = state.recv() => match s {
                        Some(RTCPeerConnectionState::Failed) => { reason = "the WebRTC connection failed".to_string(); break }
                        Some(RTCPeerConnectionState::Closed) => { reason = "the WebRTC connection closed".to_string(); break }
                        Some(RTCPeerConnectionState::Disconnected) => {
                            // give it a few seconds to come back on its own
                            let back = tokio::time::timeout(Duration::from_secs(5), recovered(&mut state)).await;
                            if !matches!(back, Ok(true)) { reason = "the WebRTC connection was lost".to_string(); break }
                        }
                        Some(_) => {}
                        None => { reason = "the peer connection closed".to_string(); break }
                    },
                    _ = publisher.killed() => { reason = "dropped by a fault (injected)".to_string(); break }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        if !me.sessions.lock().contains_key(&sid) { reason = "the sender ended the session (DELETE)".to_string(); break }
                    }
                }
            }
            let _ = pc.close().await;
            for r in readers {
                r.abort();
            }
            me.sessions.lock().remove(&sid);
            // the connection ends when the last clone of the publisher goes (the readers' too)
            publisher.session.event(Level::Info, "whip", reason);
        });
        Ok((sdp, id))
    }
}

/// Waits for a disconnected peer connection to come back (true) or fail for good (false).
async fn recovered(state: &mut mpsc::UnboundedReceiver<RTCPeerConnectionState>) -> bool {
    while let Some(s) = state.recv().await {
        match s {
            RTCPeerConnectionState::Connected => return true,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => return false,
            _ => {}
        }
    }
    false
}
