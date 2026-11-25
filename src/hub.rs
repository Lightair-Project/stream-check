//! The platform itself: who is publishing what, the sessions (a stream that comes back within the
//! grace period continues its session), faults, recordings and reports. Ingests (RTMP, SRT, WHIP)
//! call [`Hub::authorize`] and then feed the [`Publisher`] they get.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};

use crate::analyze::{Analyzer, Level, Levels, Note};
use crate::faults::{Effective, FaultRequest, Faults, merge};
use crate::media::{ACodec, Media};
use crate::mkv::{self, AudioTrack, Mkv, Offsets, VideoTrack};
use crate::nal::VCodec;
use crate::profiles::{self, Profile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Rtmp,
    Rtmps,
    Srt,
    Whip,
    /// A file read from disk (a recording, a replay clip).
    File,
    /// An RTSP server the lab plays from.
    Rtsp,
    /// An NDI source the lab receives.
    Ndi,
    /// A camera on this computer (a virtual camera, say).
    Camera,
}

impl Proto {
    /// What the lab records a connection's media as; none for files (the file is the
    /// recording) and for uncompressed sources (NDI, cameras), which get thumbnails only.
    pub fn recording(self) -> Option<&'static str> {
        match self {
            Proto::Rtmp | Proto::Rtmps => Some("flv"),
            Proto::Srt => Some("ts"),
            Proto::Whip | Proto::Rtsp => Some("mkv"),
            Proto::File | Proto::Ndi | Proto::Camera => None,
        }
    }
    pub fn container(self) -> &'static str {
        self.recording().unwrap_or("none")
    }
    pub fn name(self) -> &'static str {
        match self {
            Proto::Rtmp => "rtmp",
            Proto::Rtmps => "rtmps",
            Proto::Srt => "srt",
            Proto::Whip => "whip",
            Proto::File => "file",
            Proto::Rtsp => "rtsp",
            Proto::Ndi => "ndi",
            Proto::Camera => "camera",
        }
    }
    /// The lab reads from it, rather than a sender publishing to the lab.
    pub fn pulled(self) -> bool {
        matches!(self, Proto::Rtsp | Proto::Ndi | Proto::Camera)
    }
    /// "ingest", "pull" or "file": what kind of session this makes.
    pub fn kind(self) -> &'static str {
        match self {
            Proto::File => "file",
            p if p.pulled() => "pull",
            _ => "ingest",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub http_port: u16,
    pub rtmp_port: u16,
    pub rtmps_port: Option<u16>,
    pub srt_port: u16,
    pub srt_passphrase: Option<String>,
    pub whip_token: Option<String>,
    /// Accept any stream key, platform profiles or not.
    pub lenient_keys: bool,
    /// Only these keys, per platform, when given.
    pub accept_keys: HashMap<String, Vec<String>>,
    pub grace_override: Option<u32>,
    pub ffmpeg: bool,
    /// How many sessions keep their recordings (reports are kept always).
    pub keep_recordings: usize,
}

impl Config {
    pub fn for_test(dir: PathBuf) -> Config {
        Config {
            data_dir: dir,
            http_port: 0,
            rtmp_port: 0,
            rtmps_port: None,
            srt_port: 0,
            srt_passphrase: None,
            whip_token: Some("secret".into()),
            lenient_keys: true,
            accept_keys: HashMap::new(),
            grace_override: None,
            ffmpeg: crate::helper::ffmpeg_available(),
            keep_recordings: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub t: f64,
    pub wall_ms: u64,
    pub level: Level,
    pub kind: String,
    pub msg: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rejection {
    pub wall_ms: u64,
    pub proto: Proto,
    pub platform: String,
    pub key: String,
    pub remote: String,
    pub code: &'static str,
    pub msg: String,
}

#[derive(Debug, Clone)]
pub struct Refusal {
    pub code: &'static str,
    pub msg: String,
}

pub struct Slot {
    pub faults: Mutex<Faults>,
    kill: watch::Sender<u64>,
}

impl Slot {
    fn new() -> Arc<Slot> {
        Arc::new(Slot { faults: Mutex::new(Faults::default()), kill: watch::channel(0).0 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Live,
    Grace,
    Ended,
}

#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    pub id: u64,
    pub proto: Proto,
    pub remote: String,
    pub started_s: f64,
    pub ended_s: Option<f64>,
    pub end_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentInfo {
    pub file: String,
    pub container: &'static str,
    pub connection: u64,
    pub bytes: u64,
    pub started_s: f64,
    pub probe: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViolationRecord {
    pub rule: String,
    pub level: Level,
    pub msg: String,
    pub first_s: f64,
    pub last_s: f64,
    pub active: bool,
    pub count: u32,
}

/// Where a connection's media goes besides the analyzer.
struct Pipeline {
    video: Option<(VCodec, Vec<u8>, u32, u32)>,
    audio: Option<(ACodec, Vec<u8>, u32, u8)>,
    pending: Vec<Media>,
    first: Option<Instant>,
    mkv: Option<Mkv>,
    started: bool,
    file: Option<(File, u64, Offsets)>,
    raw: Option<(File, u64)>,
    helper: Option<mpsc::Sender<Vec<u8>>>,
    ignored_late: bool,
    /// Nothing to record or decode (files, uncompressed sources).
    off: bool,
}

impl Pipeline {
    fn new() -> Pipeline {
        Pipeline {
            video: None,
            audio: None,
            pending: vec![],
            first: None,
            mkv: None,
            started: false,
            file: None,
            raw: None,
            helper: None,
            ignored_late: false,
            off: false,
        }
    }

    fn write_mkv(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Some((f, len, _)) = &mut self.file {
            if f.write_all(&bytes).is_ok() {
                *len += bytes.len() as u64;
            }
        }
        if let Some(h) = &self.helper {
            let _ = h.try_send(bytes);
        }
    }

    fn start(&mut self) {
        let video = self.video.take().map(|(codec, record, width, height)| VideoTrack { codec, record, width, height });
        let audio = self.audio.take().map(|(codec, config, rate, channels)| AudioTrack { codec, config, rate, channels });
        let (m, header) = Mkv::new(video, audio);
        if let Some((_, _, o)) = &mut self.file {
            *o = m.offsets;
        }
        self.mkv = Some(m);
        self.started = true;
        self.write_mkv(header);
        for m in std::mem::take(&mut self.pending) {
            self.write_media(&m);
        }
    }

    fn write_media(&mut self, m: &Media) {
        let Some(mkv) = &mut self.mkv else { return };
        let bytes = match m {
            Media::Video { pts_us, key, data, .. } => mkv.video(*pts_us, *key, &annexb_free(data)),
            Media::Audio { pts_us, data } => mkv.audio(*pts_us, data),
            _ => return,
        };
        self.write_mkv(bytes);
    }

    fn feed(&mut self, m: &Media, dims: (u32, u32)) -> Option<&'static str> {
        if self.off {
            return None;
        }
        let now = Instant::now();
        let mut note = None;
        match m {
            Media::VideoConfig { codec, record } => {
                if self.started {
                    if !self.ignored_late && self.video.is_none() && self.mkv.as_ref().is_some_and(|k| !k.has_video()) {
                        self.ignored_late = true;
                        note = Some("a video config came after the recording started; the recording has no picture");
                    }
                } else {
                    self.video = Some((*codec, record.to_vec(), dims.0, dims.1));
                }
            }
            Media::AudioConfig { codec, config, sample_rate, channels } => {
                if self.started {
                    if !self.ignored_late && self.mkv.as_ref().is_some_and(|k| !k.has_audio()) {
                        self.ignored_late = true;
                        note = Some("an audio config came after the recording started; the recording has no sound");
                    }
                } else {
                    self.audio = Some((*codec, config.to_vec(), *sample_rate, *channels));
                }
            }
            Media::Video { .. } | Media::Audio { .. } => {
                if !self.started {
                    self.first.get_or_insert(now);
                    self.pending.push(m.clone());
                    let waited = now.duration_since(self.first.unwrap()) > Duration::from_millis(1500);
                    if ((self.video.is_some() && self.audio.is_some()) || waited) && (self.video.is_some() || self.audio.is_some()) {
                        self.start();
                    }
                } else {
                    self.write_media(m);
                }
            }
            Media::Metadata(_) | Media::Track(..) => {}
        }
        note
    }

    /// Closes the files; the recording's bytes and the container's duration.
    fn finish(&mut self) -> Option<(u64, u64)> {
        if let Some(m) = &mut self.mkv {
            let tail = m.flush();
            self.write_mkv(tail);
        }
        self.helper = None;
        let dur = self.mkv.as_ref().map_or(0, |m| m.duration_ms());
        let mut size = None;
        if let Some((mut f, len, o)) = self.file.take() {
            if self.started {
                let _ = mkv::patch(&mut f, o, len, dur);
            }
            let _ = f.flush();
            size = Some((len, dur as u64));
        }
        if let Some((mut f, len)) = self.raw.take() {
            let _ = f.flush();
            size = Some((len, dur as u64));
        }
        size
    }
}

/// Length-prefixed pictures stay as they are in Matroska; this exists for symmetry with sources
/// that carry start codes.
fn annexb_free(d: &[u8]) -> Vec<u8> {
    d.to_vec()
}

pub struct Inner {
    pub state: State,
    pub grace_until: Option<Instant>,
    pub ended_wall_ms: Option<u64>,
    pub end_reason: Option<String>,
    pub conn: Option<(u64, Proto, SocketAddr)>,
    pub connections: Vec<Connection>,
    pub analyzer: Analyzer,
    pub events: Vec<Event>,
    pub violations: Vec<ViolationRecord>,
    pub segments: Vec<SegmentInfo>,
    pipeline: Pipeline,
    pub reconnects: u32,
    pub grace_uses: u32,
    pub protocols: Vec<Proto>,
    preview_init: Vec<Arc<Vec<u8>>>,
    preview_gop: Vec<Arc<Vec<u8>>>,
    pub report: Option<Value>,
    last_gone: Option<Instant>,
    epoch: u64,
    segment_no: u32,
    finalizing: bool,
    last_rtp_event: Option<Instant>,
    /// Enhanced RTMP multitrack: the tracks beyond the first.
    pub tracks: crate::tracks::Tracks,
    /// What the sender said about itself: its Enhanced RTMP capabilities and its key's query.
    pub client: Value,
    /// The latest onMetaData, when it carried track maps.
    pub track_maps: Value,
    /// Each other track's preview start: its config, and its pictures since the last keyframe.
    track_preview: BTreeMap<u8, PreviewStart>,
    /// What a file or pull session reads from, and what was learnt about it beyond the stats
    /// (the file's container and tracks, the RTSP server's description, the picture's colours).
    pub source: Value,
    /// Problems found about the source itself, for the report's verdict.
    pub source_problems: Vec<String>,
    /// The session's clock is the media's own (a file read faster than real time): the lab's
    /// once-a-second tick leaves it alone, and its reader ticks it.
    virtual_clock: bool,
    /// How long the media lasts, when that and not the wall clock is the session's duration.
    media_duration_s: Option<f64>,
    /// Why a file or pull session was told to stop (its reader ends with this).
    stop_reason: Option<String>,
}

/// A preview's start for a new viewer: its config messages, and its pictures since the last
/// keyframe.
type PreviewStart = (Vec<Arc<Vec<u8>>>, Vec<Arc<Vec<u8>>>);

pub struct Session {
    pub id: String,
    /// "ingest", "pull" or "file".
    pub kind: &'static str,
    pub platform: String,
    pub app: String,
    pub key: String,
    pub profile: &'static Profile,
    pub started_wall_ms: u64,
    pub t0: Instant,
    pub dir: PathBuf,
    pub slot: Arc<Slot>,
    pub inner: Mutex<Inner>,
    pub preview: broadcast::Sender<Arc<Vec<u8>>>,
    /// The other tracks' previews (Enhanced RTMP multitrack), each message with its track.
    pub track_preview: broadcast::Sender<(u8, Arc<Vec<u8>>)>,
}

pub struct Hub {
    pub cfg: Config,
    pub sessions: Mutex<Vec<Arc<Session>>>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    pub global: Arc<Slot>,
    pub rejections: Mutex<Vec<Rejection>>,
    pub started: Instant,
    next_conn: AtomicU64,
    handle: tokio::runtime::Handle,
    pub addresses: Mutex<Value>,
    /// The RTMP and RTMPS ports actually bound (GetClientConfiguration points a client at them).
    pub ingest_ports: Mutex<(u16, Option<u16>)>,
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// 20261121T153045Z
fn stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86400, secs % 86400);
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(24).collect()
}

/// A connection that publishes into a session.
pub struct Publisher {
    pub session: Arc<Session>,
    pub hub: Arc<Hub>,
    pub conn: u64,
    pub proto: Proto,
    killed: watch::Receiver<u64>,
    bucket: Instant,
    ended: bool,
    reason: String,
}

impl Publisher {
    pub fn media(&self, m: Media) {
        self.session.feed(self.conn, m);
    }

    /// Media whose moment is `now` rather than the wall clock's (a file read faster than real time).
    pub fn media_at(&self, m: Media, now: Instant) {
        self.session.feed_at(self.conn, m, now);
    }

    /// Whether the connection was told to stop (a fault, `end`, the lab closing).
    pub fn is_killed(&self) -> bool {
        self.killed.has_changed().unwrap_or(true)
    }

    /// Bytes of the container to keep as they came (FLV tags, TS packets).
    pub fn raw(&self, bytes: &[u8]) {
        self.session.raw(self.conn, bytes);
    }

    /// RTP statistics of a track (WHIP).
    pub fn rtp(&self, audio: bool, stats: crate::rtp::RtpStats) {
        self.session.set_rtp(self.conn, audio, stats);
    }

    pub fn effective(&self) -> Effective {
        let now = Instant::now();
        merge(&self.session.slot.faults.lock(), &self.hub.global.faults.lock(), now)
    }

    /// Resolves when the connection is to be dropped (a fault, or the lab closing).
    pub async fn killed(&self) {
        let mut rx = self.killed.clone();
        let _ = rx.changed().await;
    }

    /// Applies throttle and freeze to a read of `bytes`; false if the connection was killed.
    pub async fn gate(&mut self, bytes: usize) -> bool {
        loop {
            let e = self.effective();
            if *self.killed.borrow() != 0 && self.killed.has_changed().unwrap_or(false) {
                return false;
            }
            if e.frozen {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(50)) => continue,
                    _ = self.killed() => return false,
                }
            }
            if e.throttle_kbps > 0 && bytes > 0 {
                let cost = Duration::from_secs_f64(bytes as f64 * 8.0 / (e.throttle_kbps as f64 * 1000.0));
                let now = Instant::now();
                let start = self.bucket.max(now);
                self.bucket = start + cost;
                let wait = start.saturating_duration_since(now);
                if wait > Duration::from_millis(5) {
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = self.killed() => return false,
                    }
                }
            }
            return true;
        }
    }

    pub fn end(mut self, reason: &str) {
        self.reason = reason.to_string();
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if !self.ended {
            self.ended = true;
            let hub = self.hub.clone();
            let session = self.session.clone();
            let (conn, reason) = (self.conn, std::mem::take(&mut self.reason));
            let reason = if reason.is_empty() { "the sender closed the connection".to_string() } else { reason };
            self.hub.handle.spawn(async move {
                hub.connection_closed(session, conn, reason).await;
            });
        }
    }
}

impl Hub {
    pub fn new(cfg: Config) -> Arc<Hub> {
        let _ = std::fs::create_dir_all(cfg.data_dir.join("sessions"));
        let hub = Arc::new(Hub {
            cfg,
            sessions: Mutex::new(vec![]),
            slots: Mutex::new(HashMap::new()),
            global: Slot::new(),
            rejections: Mutex::new(vec![]),
            started: Instant::now(),
            next_conn: AtomicU64::new(1),
            handle: tokio::runtime::Handle::current(),
            addresses: Mutex::new(json!({})),
            ingest_ports: Mutex::new((1935, None)),
        });
        hub.load_old_reports();
        let ticker = hub.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(1));
            loop {
                every.tick().await;
                ticker.tick();
            }
        });
        hub
    }

    fn load_old_reports(&self) {
        let Ok(rd) = std::fs::read_dir(self.cfg.data_dir.join("sessions")) else { return };
        let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.join("report.json").exists()).collect();
        dirs.sort();
        // old sessions keep their reports; only the newest keep recordings
        let excess = dirs.len().saturating_sub(self.cfg.keep_recordings);
        for d in dirs.iter().take(excess) {
            if let Ok(rd) = std::fs::read_dir(d) {
                for f in rd.flatten() {
                    let p = f.path();
                    if p.is_dir() || p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("seg")) {
                        let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                    }
                }
            }
        }
    }

    pub fn slot(&self, key: &str) -> Arc<Slot> {
        if key == "*" {
            return self.global.clone();
        }
        self.slots.lock().entry(key.to_string()).or_insert_with(Slot::new).clone()
    }

    /// Whether new connections are being refused outright (the `*` fault).
    pub fn refusing_all(&self) -> bool {
        self.global.faults.lock().refusing(Instant::now())
    }

    pub fn find(&self, id_or_key: &str) -> Option<Arc<Session>> {
        let s = self.sessions.lock();
        if id_or_key == "latest" {
            return s.last().cloned();
        }
        s.iter()
            .rev()
            .find(|x| x.id == id_or_key)
            .or_else(|| s.iter().rev().find(|x| x.key == id_or_key))
            .or_else(|| s.iter().rev().find(|x| x.id.starts_with(id_or_key)))
            .cloned()
    }

    fn reject(&self, proto: Proto, platform: &str, key: &str, remote: SocketAddr, code: &'static str, msg: String) -> Refusal {
        let r = Rejection {
            wall_ms: now_ms(),
            proto,
            platform: platform.into(),
            key: key.into(),
            remote: remote.to_string(),
            code,
            msg: msg.clone(),
        };
        let mut g = self.rejections.lock();
        g.push(r);
        let n = g.len();
        if n > 200 {
            g.drain(..n - 200);
        }
        drop(g);
        // a stream with a session in grace hears about it there too
        if let Some(s) = self.sessions.lock().iter().rev().find(|s| s.key == key && s.inner.lock().state != State::Ended).cloned() {
            s.event(Level::Warn, "refused", format!("a {} connection was refused: {msg}", proto.name()));
        }
        Refusal { code, msg }
    }

    /// A sender asks to publish `app/key`. Says no for a wrong key, a refusing fault or a key already in use.
    pub fn authorize(self: &Arc<Self>, proto: Proto, app: &str, key: &str, remote: SocketAddr) -> Result<Publisher, Refusal> {
        self.open(proto, app, key, remote, true)
    }

    /// The lab starts reading a file or pulling from a source, measured against `profile` and
    /// known by `key`: a session like a sender's, without the stream key rules.
    pub fn authorize_pull(self: &Arc<Self>, proto: Proto, profile: &str, key: &str, remote: SocketAddr) -> Result<Publisher, Refusal> {
        self.open(proto, profile, key, remote, false)
    }

    fn open(self: &Arc<Self>, proto: Proto, app: &str, key: &str, remote: SocketAddr, check_key: bool) -> Result<Publisher, Refusal> {
        let profile = profiles::find(app).unwrap_or_else(|| profiles::find("open").unwrap());
        let platform = profile.name.to_string();
        let now = Instant::now();
        if key.is_empty() {
            return Err(self.reject(proto, &platform, key, remote, "no-key", "no stream key".into()));
        }
        if self.global.faults.lock().refusing(now) {
            return Err(self.reject(proto, &platform, key, remote, "refused-fault", "refusing every connection (injected)".into()));
        }
        let slot = self.slot(key);
        if slot.faults.lock().refusing(now) {
            return Err(self.reject(
                proto,
                &platform,
                key,
                remote,
                "refused-fault",
                "refusing this stream's connections (injected)".into(),
            ));
        }
        if !check_key {
        } else if let Some(keys) = self.cfg.accept_keys.get(&platform) {
            if !keys.iter().any(|k| k == key) {
                return Err(self.reject(
                    proto,
                    &platform,
                    key,
                    remote,
                    "bad-key",
                    format!("{} doesn't know that stream key", profile.label),
                ));
            }
        } else if !self.cfg.lenient_keys && !profile.key_ok(key) {
            return Err(self.reject(
                proto,
                &platform,
                key,
                remote,
                "bad-key",
                format!("{} doesn't accept that stream key (it looks like {})", profile.label, profile.key_hint),
            ));
        }

        let existing = self
            .sessions
            .lock()
            .iter()
            .rev()
            .find(|s| {
                s.key == key && s.platform == platform && {
                    let g = s.inner.lock();
                    g.state != State::Ended && !g.finalizing
                }
            })
            .cloned();
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let session = match existing {
            Some(s) => {
                let mut g = s.inner.lock();
                if g.state == State::Live {
                    drop(g);
                    return Err(self.reject(proto, &platform, key, remote, "in-use", "that stream key is already publishing".into()));
                }
                let away = g.last_gone.map_or(0.0, |t| t.elapsed().as_secs_f64());
                g.state = State::Live;
                g.grace_until = None;
                g.epoch += 1;
                g.reconnects += 1;
                g.grace_uses += 1;
                g.analyzer.restarted();
                g.tracks.restarted();
                drop(g);
                s.event(
                    Level::Info,
                    "reconnected",
                    format!("the sender came back after {away:.1} s, inside the {} s grace period: same session", self.grace_s(profile)),
                );
                s
            }
            None => self.new_session(profile, app, key, slot.clone(), proto.kind()),
        };
        let proto_known = session.inner.lock().protocols.contains(&proto);
        {
            let mut g = session.inner.lock();
            if !proto_known {
                g.protocols.push(proto);
            }
            g.conn = Some((conn, proto, remote));
            let started_s = session.t0.elapsed().as_secs_f64();
            g.connections.push(Connection { id: conn, proto, remote: remote.to_string(), started_s, ended_s: None, end_reason: None });
            g.segment_no += 1;
            let seg = g.segment_no;
            let mut pipeline = Pipeline::new();
            if let Some(ext) = proto.recording() {
                let file = format!("seg{seg:02}.{ext}");
                let path = session.dir.join(&file);
                match File::create(&path) {
                    Ok(f) => {
                        if ext == "mkv" {
                            pipeline.file = Some((f, 0, Offsets::default()));
                        } else {
                            pipeline.raw = Some((f, 0));
                        }
                    }
                    Err(e) => {
                        drop(g);
                        session.event(Level::Error, "recording", format!("couldn't create {file}: {e}"));
                        g = session.inner.lock();
                    }
                }
                if self.cfg.ffmpeg {
                    pipeline.helper = Some(crate::helper::spawn(session.clone(), seg, session.dir.clone()));
                }
                g.segments.push(SegmentInfo { file, container: ext, connection: conn, bytes: 0, started_s, probe: None });
            } else {
                pipeline.off = true;
            }
            g.pipeline = pipeline;
        }
        let who = match proto.kind() {
            "file" => format!("reading the file {key}"),
            "pull" => format!("{} source {key} ({remote})", proto.name().to_uppercase()),
            _ => format!("{} publisher from {remote}", proto.name().to_uppercase()),
        };
        session.event(Level::Info, "connected", who);
        let killed = session.slot.kill.subscribe();
        Ok(Publisher { session, hub: self.clone(), conn, proto, killed, bucket: Instant::now(), ended: false, reason: String::new() })
    }

    pub fn grace_s(&self, p: &Profile) -> u32 {
        self.cfg.grace_override.unwrap_or(p.grace_s)
    }

    fn new_session(&self, profile: &'static Profile, app: &str, key: &str, slot: Arc<Slot>, kind: &'static str) -> Arc<Session> {
        let wall = now_ms();
        let base = format!("{}-{}-{}", stamp(wall), profile.name, clean(key));
        let mut id = base.clone();
        let mut n = 1;
        while self.cfg.data_dir.join("sessions").join(&id).exists() {
            n += 1;
            id = format!("{base}-{n}");
        }
        let dir = self.cfg.data_dir.join("sessions").join(&id);
        let _ = std::fs::create_dir_all(dir.join("thumbs"));
        let t0 = Instant::now();
        let s = Arc::new(Session {
            id,
            kind,
            platform: profile.name.into(),
            app: app.into(),
            key: key.into(),
            profile,
            started_wall_ms: wall,
            t0,
            dir,
            slot,
            inner: Mutex::new(Inner {
                state: State::Live,
                grace_until: None,
                ended_wall_ms: None,
                end_reason: None,
                conn: None,
                connections: vec![],
                analyzer: Analyzer::new(t0),
                events: vec![],
                violations: vec![],
                segments: vec![],
                pipeline: Pipeline::new(),
                reconnects: 0,
                grace_uses: 0,
                protocols: vec![],
                preview_init: vec![],
                preview_gop: vec![],
                report: None,
                last_gone: None,
                epoch: 0,
                segment_no: 0,
                finalizing: false,
                last_rtp_event: None,
                tracks: Default::default(),
                client: Value::Null,
                track_maps: Value::Null,
                track_preview: BTreeMap::new(),
                source: Value::Null,
                source_problems: vec![],
                virtual_clock: false,
                media_duration_s: None,
                stop_reason: None,
            }),
            preview: broadcast::channel(256).0,
            track_preview: broadcast::channel(512).0,
        });
        self.sessions.lock().push(s.clone());
        s.event(Level::Info, "session", format!("new session {} on {} ({})", s.id, profile.label, profile.key_hint));
        s
    }

    async fn connection_closed(self: &Arc<Self>, s: Arc<Session>, conn: u64, reason: String) {
        let (grace, epoch, pulled) = {
            let mut g = s.inner.lock();
            if g.conn.map(|c| c.0) != Some(conn) {
                return;
            }
            let pulled = g.conn.is_some_and(|c| c.1.kind() != "ingest");
            g.conn = None;
            let t = s.t0.elapsed().as_secs_f64();
            if let Some(c) = g.connections.iter_mut().find(|c| c.id == conn) {
                c.ended_s = Some(t);
                c.end_reason = Some(reason.clone());
            }
            let seg = g.pipeline.finish();
            if let (Some((bytes, _)), Some(last)) =
                (seg, g.segments.iter_mut().rev().find(|x| x.connection == conn && !x.file.contains("-track")))
            {
                last.bytes = bytes;
            }
            for (file, bytes) in g.tracks.finish() {
                if let Some(x) = g.segments.iter_mut().find(|x| x.file == file) {
                    x.bytes = bytes;
                }
            }
            // a file or a pulled source is done when it stops: nothing comes back by itself
            let grace = if pulled { 0 } else { self.grace_s(s.profile) };
            g.last_gone = Some(Instant::now());
            if grace > 0 {
                g.state = State::Grace;
                g.grace_until = Some(Instant::now() + Duration::from_secs(grace as u64));
            }
            g.epoch += 1;
            (grace, g.epoch, pulled)
        };
        s.event(
            if pulled { Level::Info } else { Level::Warn },
            "disconnected",
            format!("{reason}{}", if grace > 0 { format!("; holding the session {grace} s for a reconnect") } else { String::new() }),
        );
        if grace == 0 {
            self.finalize(s, if pulled { reason } else { "the sender is gone".into() }).await;
            return;
        }
        let hub = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(grace as u64)).await;
            let still = {
                let g = s.inner.lock();
                g.state == State::Grace && g.epoch == epoch
            };
            if still {
                hub.finalize(s, format!("the sender didn't come back within {grace} s")).await;
            }
        });
    }

    /// Ends a session now: recordings closed, ffprobe run on them, report written.
    pub async fn finalize(self: &Arc<Self>, s: Arc<Session>, reason: String) {
        {
            let mut g = s.inner.lock();
            if g.state == State::Ended || g.finalizing {
                return;
            }
            g.finalizing = true;
            g.grace_until = None;
            g.ended_wall_ms = Some(now_ms());
            g.end_reason = Some(reason.clone());
            if g.conn.take().is_some() {
                let seg = g.pipeline.finish();
                if let (Some((bytes, _)), Some(last)) = (seg, g.segments.iter_mut().rev().find(|x| !x.file.contains("-track"))) {
                    last.bytes = bytes;
                }
                for (file, bytes) in g.tracks.finish() {
                    if let Some(x) = g.segments.iter_mut().find(|x| x.file == file) {
                        x.bytes = bytes;
                    }
                }
            }
        }
        s.event(Level::Info, "ended", reason);
        // let the audio helper deliver its last words
        tokio::time::sleep(Duration::from_millis(600)).await;
        let segs: Vec<(usize, String)> = s.inner.lock().segments.iter().enumerate().map(|(i, x)| (i, x.file.clone())).collect();
        for (i, file) in segs {
            let path = s.dir.join(&file);
            let probe = tokio::task::spawn_blocking(move || crate::helper::probe(&path)).await.ok().flatten();
            let size = std::fs::metadata(s.dir.join(&file)).map(|m| m.len()).unwrap_or(0);
            let mut g = s.inner.lock();
            g.segments[i].probe = probe;
            g.segments[i].bytes = size;
        }
        let report = s.build_report();
        let _ = std::fs::write(s.dir.join("report.json"), serde_json::to_vec_pretty(&report).unwrap_or_default());
        {
            let mut g = s.inner.lock();
            g.report = Some(report);
            g.state = State::Ended;
        }
        let n = self.sessions.lock().iter().filter(|x| x.inner.lock().state == State::Ended).count();
        if n > self.cfg.keep_recordings {
            self.load_old_reports();
        }
    }

    /// Applies a fault to a key (or `*`, or a session id) and says what happened.
    pub fn fault(&self, target: &str, r: &FaultRequest) -> Result<Vec<String>, String> {
        let key = if target == "*" {
            "*".to_string()
        } else {
            match self.find(target) {
                Some(s) => s.key.clone(),
                None => target.to_string(),
            }
        };
        let slot = self.slot(&key);
        let said = slot.faults.lock().apply(r, Instant::now());
        let drop_now = r.drop || r.refuse_s.is_some_and(|s| s > 0.0);
        if drop_now {
            slot.kill.send_modify(|n| *n += 1);
        }
        let sessions: Vec<_> =
            self.sessions.lock().iter().filter(|s| (key == "*" || s.key == key) && s.inner.lock().state != State::Ended).cloned().collect();
        for s in sessions {
            for m in &said {
                s.event(Level::Warn, "fault", format!("injected: {m}"));
            }
        }
        if drop_now && key == "*" {
            // every session's slot
            for slot in self.slots.lock().values() {
                slot.kill.send_modify(|n| *n += 1);
            }
        }
        Ok(said)
    }

    pub fn faults_json(&self) -> Value {
        let now = Instant::now();
        let mut m = serde_json::Map::new();
        m.insert("*".into(), self.global.faults.lock().describe(now));
        for (k, s) in self.slots.lock().iter() {
            m.insert(k.clone(), s.faults.lock().describe(now));
        }
        Value::Object(m)
    }

    fn tick(self: &Arc<Self>) {
        let sessions: Vec<_> = self.sessions.lock().iter().cloned().collect();
        for s in sessions {
            s.tick();
        }
    }

    pub fn snapshot(&self) -> Value {
        let sessions: Vec<Value> = self.sessions.lock().iter().rev().map(|s| s.summary(false)).collect();
        json!({
            "uptime_s": self.started.elapsed().as_secs_f64(),
            "sessions": sessions,
            "faults": self.faults_json(),
            "rejections": self.rejections.lock().iter().rev().take(30).cloned().collect::<Vec<_>>(),
            "addresses": self.addresses.lock().clone(),
        })
    }

    /// Drops every connection (shutting down).
    pub fn kill_all(&self) {
        self.global.kill.send_modify(|n| *n += 1);
        for s in self.slots.lock().values() {
            s.kill.send_modify(|n| *n += 1);
        }
    }
}

impl Session {
    pub fn event(&self, level: Level, kind: &str, msg: impl Into<String>) {
        let mut g = self.inner.lock();
        self.push_event(&mut g, level, kind, msg.into());
    }

    /// An event at a moment of the session's own clock (a file's media time).
    pub fn event_at(&self, now: Instant, level: Level, kind: &str, msg: impl Into<String>) {
        let mut g = self.inner.lock();
        self.push_event_at(&mut g, now, level, kind, msg.into());
    }

    fn push_event(&self, g: &mut Inner, level: Level, kind: &str, msg: String) {
        self.push_event_at(g, Instant::now(), level, kind, msg);
    }

    fn push_event_at(&self, g: &mut Inner, now: Instant, level: Level, kind: &str, msg: String) {
        let t = now.saturating_duration_since(self.t0).as_secs_f64();
        g.events.push(Event { t, wall_ms: now_ms(), level, kind: kind.into(), msg });
        if g.events.len() > 5000 {
            g.events.remove(0);
        }
    }

    fn push_notes(&self, g: &mut Inner, notes: Vec<Note>) {
        self.push_notes_at(g, notes, Instant::now());
    }

    fn push_notes_at(&self, g: &mut Inner, notes: Vec<Note>, now: Instant) {
        for n in notes {
            self.push_event_at(g, now, n.level, n.kind, n.msg);
        }
    }

    /// What a file or pull session reads from (replaces what was said before).
    pub fn set_source(&self, v: Value) {
        self.inner.lock().source = v;
    }

    /// Changes what is said about the source.
    pub fn update_source(&self, f: impl FnOnce(&mut Value)) {
        f(&mut self.inner.lock().source);
    }

    /// A problem with the source itself, for the report's verdict.
    pub fn add_problem(&self, p: impl Into<String>) {
        let p = p.into();
        let mut g = self.inner.lock();
        if !g.source_problems.contains(&p) {
            g.source_problems.push(p);
        }
    }

    /// Sets numbers the analyzer can't see by itself (an uncompressed picture's size, raw sound).
    pub fn with_stats(&self, f: impl FnOnce(&mut crate::analyze::Stats)) {
        f(&mut self.inner.lock().analyzer.stats);
    }

    /// The session runs on its media's clock: [`Session::tick_at`] is called by its reader, not
    /// by the lab's once-a-second tick.
    pub fn set_virtual_clock(&self, on: bool) {
        self.inner.lock().virtual_clock = on;
    }

    /// How long the media lasts, which is then the session's duration.
    pub fn set_media_duration(&self, s: f64) {
        self.inner.lock().media_duration_s = Some(s);
    }

    /// What the sender said about itself (Enhanced RTMP capabilities, its key's query).
    pub fn set_client(&self, v: Value) {
        self.inner.lock().client = v;
    }

    pub fn set_levels(&self, l: Levels) {
        self.set_levels_at(l, Instant::now());
    }

    pub fn set_levels_at(&self, l: Levels, now: Instant) {
        let mut g = self.inner.lock();
        if g.state == State::Ended {
            return;
        }
        let notes = g.analyzer.set_levels(l, now);
        self.push_notes_at(&mut g, notes, now);
    }

    pub fn set_rtp(&self, conn: u64, audio: bool, stats: crate::rtp::RtpStats) {
        let mut g = self.inner.lock();
        if g.conn.map(|c| c.0) != Some(conn) {
            return;
        }
        let now = Instant::now();
        let slot = if audio { &g.analyzer.stats.rtp.audio } else { &g.analyzer.stats.rtp.video };
        let worse = (stats.lost + stats.late + stats.pictures_dropped) > (slot.lost + slot.late + slot.pictures_dropped);
        let better = stats.recovered > slot.recovered;
        let quiet = g.last_rtp_event.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2));
        if (worse || better) && quiet {
            g.last_rtp_event = Some(now);
            let which = if audio { "audio" } else { "video" };
            let msg = format!(
                "RTP {which}: {} lost, {} recovered by retransmission or reordering, {} too late (dropped){}",
                stats.lost,
                stats.recovered,
                stats.late,
                if audio { String::new() } else { format!(", {} picture(s) dropped for missing packets", stats.pictures_dropped) }
            );
            self.push_event(&mut g, if worse { Level::Warn } else { Level::Info }, "rtp", msg);
        }
        g.analyzer.stats.rtp.active = true;
        if audio {
            g.analyzer.stats.rtp.audio = stats;
        } else {
            g.analyzer.stats.rtp.video = stats;
        }
    }

    pub fn raw(&self, conn: u64, bytes: &[u8]) {
        let mut g = self.inner.lock();
        if g.conn.map(|c| c.0) != Some(conn) {
            return;
        }
        if let Some((f, len)) = &mut g.pipeline.raw {
            if f.write_all(bytes).is_ok() {
                *len += bytes.len() as u64;
            }
        }
    }

    pub fn feed(&self, conn: u64, m: Media) {
        self.feed_at(conn, m, Instant::now());
    }

    pub fn feed_at(&self, conn: u64, m: Media, now: Instant) {
        let mut g = self.inner.lock();
        if g.conn.map(|c| c.0) != Some(conn) {
            return;
        }
        // a track beyond the first (Enhanced RTMP multitrack): analysed and recorded on its own
        let m = match m {
            Media::Track(id, inner) => {
                let seg = g.segment_no;
                let copy = (*inner).clone();
                let (notes, started) = g.tracks.feed(id, *inner, now, self.t0, &self.dir, seg);
                // the track's own preview, as the main one is made
                let msg = g.tracks.list.get(&id).and_then(|t| preview_message(&copy, &t.analyzer)).map(Arc::new);
                if let Some(msg) = msg {
                    let (init, gop) = g.track_preview.entry(id).or_default();
                    match &copy {
                        Media::VideoConfig { .. } => {
                            init.retain(|x| x[0] != 1);
                            init.push(msg.clone());
                            gop.clear();
                        }
                        Media::Video { key, .. } => {
                            if *key {
                                gop.clear();
                            }
                            if gop.len() < 600 {
                                gop.push(msg.clone());
                            }
                        }
                        _ => {}
                    }
                    if matches!(copy, Media::VideoConfig { .. } | Media::Video { .. }) {
                        let _ = self.track_preview.send((id, msg));
                    }
                }
                self.push_notes(&mut g, notes);
                if let Some(file) = started {
                    let started_s = now.duration_since(self.t0).as_secs_f64();
                    g.segments.push(SegmentInfo { file, container: "mkv", connection: conn, bytes: 0, started_s, probe: None });
                }
                return;
            }
            other => other,
        };
        g.tracks.on_track0(&m);
        if let Media::Metadata(v) = &m {
            self.push_event_at(&mut g, now, Level::Info, "metadata", format!("the sender's own description: {v}"));
            // Enhanced RTMP's per-track descriptions (trackId 1 and up; 0 is the fields above)
            let maps = json!({ "video": v.get("videoTrackIdInfoMap"), "audio": v.get("audioTrackIdInfoMap") });
            if !maps["video"].is_null() || !maps["audio"].is_null() {
                self.push_event(
                    &mut g,
                    Level::Info,
                    "metadata-tracks",
                    format!("track maps: video {}, audio {}", maps["video"], maps["audio"]),
                );
                g.track_maps = maps;
            }
        }
        let notes = g.analyzer.on_media(&m, now);
        self.push_notes_at(&mut g, notes, now);
        let dims = g.analyzer.stats.video.sps.as_ref().map_or((0, 0), |s| (s.width, s.height));
        if let Some(n) = g.pipeline.feed(&m, dims) {
            self.push_event(&mut g, Level::Warn, "recording", n.into());
        }
        // the browser preview (an uncompressed source's pictures are only counted: nothing to show)
        if matches!(&m, Media::Video { data, .. } | Media::Audio { data, .. } if data.is_empty()) {
            return;
        }
        let msg = preview_message(&m, &g.analyzer);
        if let Some(msg) = msg {
            let msg = Arc::new(msg);
            match &m {
                Media::VideoConfig { .. } | Media::AudioConfig { .. } => {
                    let kind = msg[0];
                    g.preview_init.retain(|x| x[0] != kind);
                    g.preview_init.push(msg.clone());
                    g.preview_gop.clear();
                }
                Media::Video { key, .. } => {
                    if *key {
                        g.preview_gop.clear();
                    }
                    if g.preview_gop.len() < 600 {
                        g.preview_gop.push(msg.clone());
                    }
                }
                _ => {}
            }
            let _ = self.preview.send(msg);
        }
    }

    /// What a new preview viewer needs first: the configs and the pictures since the last keyframe.
    /// What a new viewer of track `id`'s preview gets first.
    pub fn track_preview_start(&self, id: u8) -> Vec<Arc<Vec<u8>>> {
        let g = self.inner.lock();
        g.track_preview.get(&id).map_or(vec![], |(init, gop)| init.iter().chain(gop.iter()).cloned().collect())
    }

    pub fn preview_start(&self) -> Vec<Arc<Vec<u8>>> {
        let g = self.inner.lock();
        g.preview_init.iter().chain(g.preview_gop.iter()).cloned().collect()
    }

    pub fn kill(&self) {
        self.slot.kill.send_modify(|n| *n += 1);
    }

    /// Tells a file or pull session's reader to stop, and why.
    pub fn stop_with(&self, reason: impl Into<String>) {
        self.inner.lock().stop_reason = Some(reason.into());
        self.kill();
    }

    /// Why the session was told to stop: `end`, the time asked for, or else a fault's drop.
    pub fn stop_reason(&self) -> String {
        self.inner.lock().stop_reason.clone().unwrap_or_else(|| "dropped (a fault)".into())
    }

    fn tick(&self) {
        if self.inner.lock().virtual_clock {
            return;
        }
        self.tick_at(Instant::now());
    }

    /// The once-a-second work (derived numbers, graphs, platform rules) as of `now`.
    pub fn tick_at(&self, now: Instant) {
        let mut g = self.inner.lock();
        if g.state == State::Ended {
            return;
        }
        if g.state == State::Live {
            let notes = g.analyzer.tick(now);
            self.push_notes_at(&mut g, notes, now);
            let fps0 = g.analyzer.stats.video.fps;
            let notes = g.tracks.tick(now, fps0);
            self.push_notes_at(&mut g, notes, now);
            let seconds = now.duration_since(self.t0).as_secs_f64();
            let mut found = profiles::check(self.profile, &g.analyzer.stats, seconds);
            found.extend(profiles::check_tracks(self.profile, &g.analyzer.stats, &g.tracks.stats(), seconds));
            let t = seconds;
            let mut active: HashSet<String> = HashSet::new();
            for v in found {
                active.insert(v.rule.to_string());
                match g.violations.iter_mut().find(|x| x.rule == v.rule) {
                    Some(rec) => {
                        rec.last_s = t;
                        rec.msg = v.msg.clone();
                        if !rec.active {
                            rec.active = true;
                            rec.count += 1;
                            let m = v.msg.clone();
                            self.push_event_at(&mut g, now, v.level, "violation", format!("{}: {m}", v.rule));
                        }
                    }
                    None => {
                        g.violations.push(ViolationRecord {
                            rule: v.rule.into(),
                            level: v.level,
                            msg: v.msg.clone(),
                            first_s: t,
                            last_s: t,
                            active: true,
                            count: 1,
                        });
                        self.push_event_at(&mut g, now, v.level, "violation", format!("{}: {}", v.rule, v.msg));
                    }
                }
            }
            let mut cleared = vec![];
            for rec in g.violations.iter_mut() {
                if rec.active && !active.contains(&rec.rule) {
                    rec.active = false;
                    cleared.push(rec.rule.clone());
                }
            }
            for r in cleared {
                self.push_event_at(&mut g, now, Level::Info, "violation-cleared", format!("{r}: back within the rules"));
            }
        }
    }

    pub fn summary(&self, detail: bool) -> Value {
        let g = self.inner.lock();
        let now = Instant::now();
        let dur = match (g.state, g.ended_wall_ms, g.media_duration_s) {
            (_, _, Some(d)) => d,
            (State::Ended, Some(e), _) => (e - self.started_wall_ms) as f64 / 1000.0,
            _ => now.duration_since(self.t0).as_secs_f64(),
        };
        let conn = g.conn.map(|(id, p, r)| json!({"id": id, "protocol": p, "remote": r.to_string()}));
        // the bytes of the recording being written now
        let mut segments = g.segments.clone();
        if g.conn.is_some() {
            if let (Some(last), Some(n)) = (
                segments.iter_mut().rev().find(|x| !x.file.contains("-track")),
                g.pipeline.raw.as_ref().map(|r| r.1).or(g.pipeline.file.as_ref().map(|f| f.1)),
            ) {
                last.bytes = n;
            }
            for (file, n) in g.tracks.recording_bytes() {
                if let Some(x) = segments.iter_mut().find(|x| x.file == file) {
                    x.bytes = n;
                }
            }
        }
        let mut v = json!({
            "id": self.id,
            "kind": self.kind,
            "source": g.source,
            "platform": self.platform,
            "profile": self.profile,
            "app": self.app,
            "key": self.key,
            "state": g.state,
            "started_ms": self.started_wall_ms,
            "ended_ms": g.ended_wall_ms,
            "duration_s": dur,
            "end_reason": g.end_reason,
            "connection": conn,
            "protocols": g.protocols,
            "reconnects": g.reconnects,
            "grace_uses": g.grace_uses,
            "grace_s": self.profile.grace_s,
            "grace_left_s": g.grace_until.map(|u| u.saturating_duration_since(now).as_secs_f64()),
            "stats": g.analyzer.stats,
            "tracks": g.tracks.summary(&g.analyzer.stats),
            "client": g.client,
            "track_maps": g.track_maps,
            "violations": g.violations,
            "events_total": g.events.len(),
            "faults": self.slot.faults.lock().describe(now),
            "segments": segments,
            "has_report": g.report.is_some(),
        });
        if detail {
            let series: Vec<_> = g.analyzer.series.iter().rev().take(900).rev().cloned().collect();
            v["series"] = json!(series);
            v["events"] = json!(g.events.iter().rev().take(300).rev().collect::<Vec<_>>());
            v["thumbs"] = json!(self.thumbs(&g));
        } else {
            let series: Vec<_> = g.analyzer.series.iter().rev().take(120).rev().cloned().collect();
            v["series"] = json!(series);
            v["events"] = json!(g.events.iter().rev().take(40).rev().collect::<Vec<_>>());
            v["thumbs"] = json!(self.thumbs(&g).into_iter().rev().take(40).rev().collect::<Vec<_>>());
        }
        v
    }

    /// Thumbnails taken so far: file name and the second of the session they show (about).
    fn thumbs(&self, g: &Inner) -> Vec<Value> {
        let Ok(rd) = std::fs::read_dir(self.dir.join("thumbs")) else { return vec![] };
        let mut names: Vec<String> =
            rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".jpg")).collect();
        names.sort();
        names
            .into_iter()
            .map(|n| {
                // s<segment>_<index>.jpg, one every 2 s of that segment
                let seg: usize = n.get(1..3).and_then(|x| x.parse().ok()).unwrap_or(1);
                let idx: f64 = n.get(4..9).and_then(|x| x.parse().ok()).unwrap_or(0.0);
                let start = g.segments.get(seg.saturating_sub(1)).map_or(0.0, |s| s.started_s);
                json!({"file": n, "t": start + idx * crate::helper::THUMB_EVERY_S})
            })
            .collect()
    }

    pub fn build_report(&self) -> Value {
        let g = self.inner.lock();
        let st = &g.analyzer.stats;
        let dur = g
            .media_duration_s
            .unwrap_or_else(|| g.ended_wall_ms.map_or(self.t0.elapsed().as_secs_f64(), |e| (e - self.started_wall_ms) as f64 / 1000.0));
        let mut problems: Vec<String> = g.source_problems.clone();
        for v in &g.violations {
            if v.level == Level::Error {
                problems.push(format!("rule broken: {} ({})", v.rule, v.msg));
            }
        }
        if st.video.dts_backwards > 0 {
            problems.push(format!("{} picture timestamps went backwards", st.video.dts_backwards));
        }
        if st.audio.pts_backwards > 0 {
            problems.push(format!("{} sound timestamps went backwards", st.audio.pts_backwards));
        }
        if !st.has_video && !st.has_audio {
            problems.push("no media ever arrived".into());
        }
        problems.extend(g.tracks.problems());
        for s in &g.segments {
            if s.bytes > 0 && s.probe.is_none() && self.dir.join(&s.file).exists() && crate::helper::ffmpeg_available() {
                problems.push(format!("{} doesn't play (ffprobe could not read it)", s.file));
            }
        }
        let thumbs = self.thumbs(&g);
        let series: Vec<_> = g.analyzer.series.iter().step_by(((g.analyzer.series.len() / 600) + 1).max(1)).cloned().collect();
        json!({
            "id": self.id,
            "kind": self.kind,
            "source": g.source,
            "ok": problems.is_empty(),
            "problems": problems,
            "platform": self.platform,
            "app": self.app,
            "key": self.key,
            "started_ms": self.started_wall_ms,
            "ended_ms": g.ended_wall_ms,
            "duration_s": dur,
            "end_reason": g.end_reason,
            "protocols": g.protocols,
            "connections": g.connections,
            "reconnects": g.reconnects,
            "grace_uses": g.grace_uses,
            "profile": self.profile,
            "stats": st,
            "tracks": g.tracks.summary(st),
            "client": g.client,
            "track_maps": g.track_maps,
            "violations": g.violations,
            "segments": g.segments,
            "thumbs": thumbs,
            "series": series,
            "events": g.events,
            "directory": self.dir,
        })
    }
}

impl Mkv {
    pub fn has_video(&self) -> bool {
        self.track_info().0
    }
    pub fn has_audio(&self) -> bool {
        self.track_info().1
    }
}

/// The preview wire format: 1 video config, 2 video picture, 3 audio config, 4 audio packet.
fn preview_message(m: &Media, a: &Analyzer) -> Option<Vec<u8>> {
    let mut out = vec![];
    match m {
        Media::VideoConfig { codec, record } => {
            out.push(1);
            out.push((*codec == VCodec::Hevc) as u8);
            let cs = a.stats.video.sps.as_ref().map_or(String::new(), |s| s.codec_string.clone());
            out.push(cs.len() as u8);
            out.extend(cs.as_bytes());
            let (w, h) = a.stats.video.sps.as_ref().map_or((0, 0), |s| (s.width, s.height));
            out.extend((w as u16).to_be_bytes());
            out.extend((h as u16).to_be_bytes());
            out.extend(record.iter());
        }
        Media::Video { pts_us, key, data, .. } => {
            out.push(2);
            out.push(*key as u8);
            out.extend(pts_us.to_be_bytes());
            out.extend(data.iter());
        }
        Media::AudioConfig { codec, config, sample_rate, channels } => {
            out.push(3);
            out.push((*codec == ACodec::Opus) as u8);
            out.extend(a.stats.audio.sample_rate.max(*sample_rate).to_be_bytes());
            out.push(*channels);
            out.extend(config.iter());
        }
        Media::Audio { pts_us, data } => {
            out.push(4);
            out.extend(pts_us.to_be_bytes());
            out.extend(data.iter());
        }
        Media::Metadata(_) | Media::Track(..) => return None,
    }
    Some(out)
}
