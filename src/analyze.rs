//! What the lab measures about a stream, from the same events whatever the protocol: codec and
//! profile, size, frame rate, bitrate, keyframe interval, GOP, timestamps, stalls, A/V offset and
//! drift, audio format and levels; and from the video bitstream: SEI (broadcast performance
//! metrics, captions, user data), the sender's clock against arrival, colour tags, the level's
//! limits, IDRs and SPS changes.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::levels;
use crate::media::{ACodec, Media, opus_toc, read_asc};
use crate::nal::{self, HevcPps, PicType, Sps, VCodec};
use crate::sei::{self, BpmKind};

/// Something worth a line in the event log.
#[derive(Debug, Clone)]
pub struct Note {
    pub level: Level,
    pub kind: &'static str,
    pub msg: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warn,
    Error,
}

fn note(level: Level, kind: &'static str, msg: impl Into<String>) -> Note {
    Note { level, kind, msg: msg.into() }
}

/// Counts bytes per second over a sliding window.
#[derive(Debug, Default)]
struct Rate {
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
    fn add(&mut self, now: Instant, bytes: u64) {
        self.samples.push_back((now, bytes));
        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t).as_secs_f64() > 2.0) {
            self.samples.pop_front();
        }
    }
    fn kbps(&self, now: Instant) -> f64 {
        let bytes: u64 = self.samples.iter().filter(|(t, _)| now.duration_since(*t).as_secs_f64() <= 2.0).map(|s| s.1).sum();
        bytes as f64 * 8.0 / 2.0 / 1000.0
    }
}

/// Counts bytes per second over the last two seconds of the media's own timestamps.
#[derive(Debug, Default)]
struct MediaRate {
    samples: VecDeque<(i64, u64)>,
}

impl MediaRate {
    const WINDOW_US: i64 = 2_000_000;

    fn add(&mut self, t_us: i64, bytes: u64) {
        if self.samples.back().is_some_and(|(last, _)| t_us < last - Self::WINDOW_US) {
            // the timestamps started over
            self.samples.clear();
        }
        self.samples.push_back((t_us, bytes));
        let latest = self.samples.iter().map(|s| s.0).max().unwrap_or(t_us);
        while self.samples.front().is_some_and(|(t, _)| latest - *t >= Self::WINDOW_US) {
            self.samples.pop_front();
        }
    }

    /// The rate over the window, or over the media there is when it's shorter (a frame's worth
    /// of time added for the last one); none for less than a few packets.
    fn kbps(&self) -> Option<f64> {
        if self.samples.len() < 3 {
            return None;
        }
        let (lo, hi) = self.samples.iter().fold((i64::MAX, i64::MIN), |(lo, hi), s| (lo.min(s.0), hi.max(s.0)));
        let step = (hi - lo) / (self.samples.len() as i64 - 1);
        let span = (hi - lo + step).clamp(1, Self::WINDOW_US) as f64 / 1e6;
        let bytes: u64 = self.samples.iter().map(|s| s.1).sum();
        Some(bytes as f64 * 8.0 / span / 1000.0)
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct VideoStats {
    pub codec: Option<VCodec>,
    pub sps: Option<Sps>,
    pub frames: u64,
    pub bytes: u64,
    pub keyframes: u64,
    /// Measured from the stream's own timestamps.
    pub fps: f64,
    /// Measured from when the pictures arrived.
    pub fps_arrival: f64,
    pub kbps: f64,
    pub peak_kbps: f64,
    pub keyint_last_s: Option<f64>,
    pub keyint_min_s: Option<f64>,
    pub keyint_max_s: Option<f64>,
    pub keyint_avg_s: Option<f64>,
    pub gop_frames: Option<u32>,
    pub gop_pattern: String,
    pub b_frames: bool,
    pub max_consecutive_b: u32,
    pub reorder: bool,
    pub dts_backwards: u64,
    pub pts_duplicates: u64,
    pub gaps: u64,
    pub max_gap_ms: f64,
    pub largest_frame_bytes: u64,
    /// Seconds since the last keyframe, as of the latest picture.
    pub since_key_s: Option<f64>,
    /// What the bitstream itself says: IDRs, picture types, SPS changes, SEI, colour and level.
    pub bitstream: Bitstream,
    /// The broadcast performance metrics SEI (Twitch Enhanced Broadcasting, Amazon IVS).
    pub bpm: BpmStats,
    /// CEA-608 and CEA-708 captions (ATSC A/53 SEI).
    pub captions: sei::CaptionStats,
    /// Arrival time against the sender's own clock, when the stream carries one.
    pub latency: LatencyStats,
}

/// What the video bitstream says beyond size and timing.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Bitstream {
    /// Pictures holding an IDR (H.264 type 5, HEVC IDR_W_RADL or IDR_N_LP).
    pub idr: u64,
    /// Pictures the container marks as keyframes that hold no IDR (HEVC CRA or BLA, or an H.264
    /// I picture with a recovery point).
    pub keyframes_not_idr: u64,
    /// IDR pictures the container doesn't mark as keyframes.
    pub idr_not_marked: u64,
    /// Pictures by the type of their first slice.
    pub pictures_i: u64,
    pub pictures_p: u64,
    pub pictures_b: u64,
    /// Times the SPS changed mid-stream (a new sequence header, or an in-band SPS that differs).
    pub sps_changes: u64,
    /// Pictures carrying an SPS in-band, and how many of those differ from the one before.
    pub inband_sps: u64,
    pub inband_sps_differs: u64,
    /// Pictures whose length-prefixed NAL units don't add up to the picture's size.
    pub bad_nal_lengths: u64,
    /// SEI messages by payload type ("recovery point (6)": 12).
    pub sei: BTreeMap<String, u64>,
    /// User data unregistered messages that aren't BPM (encoders write their name and settings).
    pub user_data: Vec<sei::UserData>,
    /// Colour tags that are missing, contradict each other or are unusual for the picture.
    pub colour_warnings: Vec<String>,
    /// Where the stream goes past its level's limits (size, rate, bitrate, reference frames).
    pub level_warnings: Vec<String>,
}

/// Session metrics of a BPM SM message: frames since the track's previous keyframe.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct SessionCounts {
    pub rendered: u64,
    pub lagged: u64,
    pub dropped: u64,
    pub output: u64,
}

/// Rendition metrics of a BPM ERM message: frames since the track's previous keyframe.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct RenditionCounts {
    pub input: u64,
    pub skipped: u64,
    pub output: u64,
}

/// The broadcast performance metrics a stream carries: three user data unregistered SEI
/// messages (timestamp, session metrics, encoded rendition metrics) in front of every IDR.
#[derive(Debug, Default, Clone, Serialize)]
pub struct BpmStats {
    /// Whether any BPM message arrived.
    pub present: bool,
    /// Keyframes (IDR, or marked key by the container) looked at.
    pub keyframes_checked: u64,
    /// Keyframes with all three messages before their first slice.
    pub keyframes_with_all: u64,
    /// Keyframes without one or more of them.
    pub keyframes_missing: u64,
    /// Whether every keyframe so far had all three (none until the first keyframe).
    pub every_keyframe: Option<bool>,
    /// What the latest keyframe lacked: "ts", "sm", "erm" (empty when it had all three).
    pub last_missing: Vec<String>,
    pub ts_messages: u64,
    pub sm_messages: u64,
    pub erm_messages: u64,
    /// BPM messages on pictures that aren't keyframes.
    pub off_keyframe: u64,
    /// BPM messages after the picture's first slice instead of before it.
    pub after_slice: u64,
    /// BPM messages that couldn't be read, or aren't in the form IVS reads (session and rendition
    /// metrics: one RFC 3339 time of event 4, four and three counters).
    pub malformed: u64,
    /// The latest timestamp message's time as sent, and its event (4: packet interleave).
    pub last_time: Option<String>,
    pub last_event: Option<u8>,
    /// Timestamp messages whose time went back from the one before.
    pub time_backwards: u64,
    /// The latest session metrics, and their sum over the stream.
    pub session: Option<SessionCounts>,
    pub session_total: SessionCounts,
    /// The latest rendition metrics, and their sum over the stream.
    pub rendition: Option<RenditionCounts>,
    pub rendition_total: RenditionCounts,
    /// Keyframes whose rendition "output" isn't the number of pictures that arrived since the
    /// keyframe before (pictures lost on the way, or counters that mean something else).
    pub output_mismatches: u64,
}

/// Arrival time minus the sender's clock carried in the stream, in ms. Both clocks must agree
/// (the same machine, or both synchronised) for it to mean the trip from the sender.
#[derive(Debug, Default, Clone, Serialize)]
pub struct LatencyStats {
    /// Where the sender's clock came from: "bpm" (the BPM timestamp message).
    pub source: Option<String>,
    pub samples: u64,
    pub last_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct AudioStats {
    pub codec: Option<ACodec>,
    pub sample_rate: u32,
    pub channels: u8,
    pub aac_object: Option<u8>,
    pub frames: u64,
    pub bytes: u64,
    pub kbps: f64,
    pub frame_ms: f64,
    pub pts_backwards: u64,
    pub gaps: u64,
    pub max_gap_ms: f64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Levels {
    pub available: bool,
    pub momentary_lufs: Option<f64>,
    pub short_term_lufs: Option<f64>,
    pub integrated_lufs: Option<f64>,
    pub peak_l_db: Option<f64>,
    pub peak_r_db: Option<f64>,
    /// The loudest sample peak since the stream began.
    pub max_peak_db: Option<f64>,
    pub rms_db: Option<f64>,
    pub silent: bool,
    pub silence_events: u32,
    pub silence_total_s: f64,
    pub clipped_blocks: u32,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct AvStats {
    /// Video ahead of audio in the stream's own timestamps, ms (median of the last half minute).
    pub offset_ms: Option<f64>,
    /// How far that offset moved since the first seconds, ms.
    pub drift_ms: Option<f64>,
    pub drift_ms_per_min: Option<f64>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct StallStats {
    pub count: u32,
    pub total_ms: f64,
    pub longest_ms: f64,
    pub stalled_now: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub t: f64,
    pub video_kbps: f64,
    pub audio_kbps: f64,
    pub fps: f64,
    pub lufs: Option<f64>,
    pub peak_db: Option<f64>,
    pub av_offset_ms: Option<f64>,
    /// The latest arrival latency against the sender's clock (BPM), ms.
    pub latency_ms: Option<f64>,
}

/// RTP receive statistics per track (WHIP only).
#[derive(Debug, Default, Clone, Serialize)]
pub struct RtpPair {
    pub active: bool,
    pub video: crate::rtp::RtpStats,
    pub audio: crate::rtp::RtpStats,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    pub rtp: RtpPair,
    pub video: VideoStats,
    pub audio: AudioStats,
    pub levels: Levels,
    pub av: AvStats,
    pub stalls: StallStats,
    pub total_kbps: f64,
    pub has_video: bool,
    pub has_audio: bool,
}

pub struct Analyzer {
    t0: Instant,
    pub stats: Stats,
    vrate: Rate,
    arate: Rate,
    /// The same by the media's timestamps, for the rates a finished connection keeps.
    vmedia: MediaRate,
    amedia: MediaRate,
    frame_arrivals: VecDeque<Instant>,
    frame_dts: VecDeque<i64>,
    prev_dts: Option<i64>,
    max_pts: Option<i64>,
    last_pts: Option<i64>,
    recent_deltas: VecDeque<i64>,
    last_key_dts: Option<i64>,
    keyints: Vec<f64>,
    gop: Vec<char>,
    gop_frames: u32,
    consec_b: u32,
    pps: Vec<(u64, HevcPps)>,
    prev_audio_pts: Option<i64>,
    last_audio_arrival: Option<Instant>,
    last_video_arrival: Option<Instant>,
    last_any: Option<Instant>,
    in_stall: bool,
    vpts: Option<i64>,
    apts: Option<i64>,
    offsets: VecDeque<f64>,
    clock_v: Option<f64>,
    clock_a: Option<f64>,
    baseline: Option<(f64, f64)>,
    pub series: Vec<Sample>,
    silence_since: Option<f64>,
    last_levels_at: Option<Instant>,
    /// The wall clock (ms since 1970) at `inst0`, for arrival times.
    wall0_ms: f64,
    inst0: Instant,
    captions: sei::Captions,
    /// The SPS in force, as bytes.
    sps_raw: Option<Vec<u8>>,
    last_bpm_ms: Option<f64>,
    latency_sum: f64,
    /// BPM misses said in the event log so far.
    bpm_said: u64,
    /// Kinds of note said once per session.
    said: Vec<&'static str>,
    /// The kinds of level excess found at the last tick.
    level_kinds: Vec<&'static str>,
}

impl Analyzer {
    pub fn new(t0: Instant) -> Self {
        Analyzer {
            t0,
            stats: Stats::default(),
            vrate: Rate::default(),
            arate: Rate::default(),
            vmedia: MediaRate::default(),
            amedia: MediaRate::default(),
            frame_arrivals: VecDeque::new(),
            frame_dts: VecDeque::new(),
            prev_dts: None,
            max_pts: None,
            last_pts: None,
            recent_deltas: VecDeque::new(),
            last_key_dts: None,
            keyints: vec![],
            gop: vec![],
            gop_frames: 0,
            consec_b: 0,
            pps: vec![],
            prev_audio_pts: None,
            last_audio_arrival: None,
            last_video_arrival: None,
            last_any: None,
            in_stall: false,
            vpts: None,
            apts: None,
            offsets: VecDeque::new(),
            clock_v: None,
            clock_a: None,
            baseline: None,
            series: vec![],
            silence_since: None,
            last_levels_at: None,
            wall0_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0),
            inst0: Instant::now(),
            captions: sei::Captions::default(),
            sps_raw: None,
            last_bpm_ms: None,
            latency_sum: 0.0,
            bpm_said: 0,
            said: vec![],
            level_kinds: vec![],
        }
    }

    /// The wall clock at `now`, ms since 1970.
    fn wall_ms(&self, now: Instant) -> f64 {
        match now.checked_duration_since(self.inst0) {
            Some(d) => self.wall0_ms + d.as_secs_f64() * 1000.0,
            None => self.wall0_ms - self.inst0.duration_since(now).as_secs_f64() * 1000.0,
        }
    }

    /// A note said only the first time its kind comes up.
    fn once(&mut self, out: &mut Vec<Note>, level: Level, kind: &'static str, msg: impl Into<String>) {
        if !self.said.contains(&kind) {
            self.said.push(kind);
            out.push(note(level, kind, msg));
        }
    }

    fn elapsed_ms(&self, now: Instant) -> f64 {
        now.duration_since(self.t0).as_secs_f64() * 1000.0
    }

    /// A new connection continues the session: timestamps start over, the history stays.
    pub fn restarted(&mut self) {
        self.vmedia = MediaRate::default();
        self.amedia = MediaRate::default();
        self.prev_dts = None;
        self.max_pts = None;
        self.last_pts = None;
        self.recent_deltas.clear();
        self.last_key_dts = None;
        self.gop.clear();
        self.prev_audio_pts = None;
        self.frame_arrivals.clear();
        self.frame_dts.clear();
        self.clock_v = None;
        self.clock_a = None;
        self.baseline = None;
        self.offsets.clear();
        self.vpts = None;
        self.apts = None;
        self.last_any = None;
        self.last_audio_arrival = None;
        self.last_video_arrival = None;
        self.in_stall = false;
        self.last_bpm_ms = None;
    }

    /// The connection ended: the bitrates become those of the last two seconds of media, by the
    /// media's own timestamps. A sender can go quiet some seconds before its connection is known
    /// to be gone (an SRT caller that stops sending is noticed by the peer's idle timeout), and
    /// the once-a-second rates, which go by arrival, would be left at zero by those seconds.
    pub fn connection_ended(&mut self) {
        if let Some(k) = self.vmedia.kbps() {
            self.stats.video.kbps = k;
        }
        if let Some(k) = self.amedia.kbps() {
            self.stats.audio.kbps = k;
        }
        self.stats.total_kbps = self.stats.video.kbps + self.stats.audio.kbps;
    }

    fn stall_check(&mut self, now: Instant, out: &mut Vec<Note>) {
        if let Some(last) = self.last_any {
            let gap = now.duration_since(last).as_secs_f64() * 1000.0;
            if gap > 1000.0 {
                let s = &mut self.stats.stalls;
                s.count += 1;
                s.total_ms += gap;
                s.longest_ms = s.longest_ms.max(gap);
                out.push(note(Level::Warn, "stall", format!("nothing arrived for {gap:.0} ms")));
            }
        }
        self.last_any = Some(now);
        self.in_stall = false;
        self.stats.stalls.stalled_now = false;
    }

    pub fn on_media(&mut self, m: &Media, now: Instant) -> Vec<Note> {
        let mut out = vec![];
        match m {
            Media::VideoConfig { codec, record } => {
                let sets = match codec {
                    VCodec::H264 => nal::read_avcc(record),
                    VCodec::Hevc => nal::read_hvcc(record),
                    _ => None,
                };
                self.stats.video.codec = Some(*codec);
                self.stats.has_video = true;
                let info = match sets {
                    Some((_, sps, pps)) => {
                        if *codec == VCodec::Hevc {
                            self.pps = pps.iter().filter_map(|p| nal::parse_hevc_pps(p)).collect();
                        }
                        self.sps_raw = sps.first().cloned();
                        sps.first().and_then(|s| nal::parse_sps(*codec, s))
                    }
                    // AV1, VP9, VP8: the codec record says it
                    None => crate::media::video_info(*codec, record),
                };
                if let Some(info) = info {
                    let changed = self.stats.video.sps.as_ref().is_some_and(|o| (o.width, o.height) != (info.width, info.height));
                    if self.stats.video.sps.is_none() || changed {
                        out.push(note(
                            if changed { Level::Warn } else { Level::Info },
                            "video-config",
                            format!(
                                "{} {} level {}, {}x{}, {}{}",
                                codec.name(),
                                info.profile,
                                info.level,
                                info.width,
                                info.height,
                                colour_summary(&info),
                                if changed { " (the resolution changed mid-stream)" } else { "" }
                            ),
                        ));
                    }
                    self.new_sps(*codec, info, "sequence header", &mut out);
                }
            }
            Media::AudioConfig { codec, config, sample_rate, channels } => {
                let a = &mut self.stats.audio;
                a.codec = Some(*codec);
                a.sample_rate = *sample_rate;
                a.channels = *channels;
                if *codec == ACodec::Aac {
                    if let Some((rate, ch, obj)) = read_asc(config) {
                        a.sample_rate = rate;
                        a.channels = ch;
                        a.aac_object = Some(obj);
                        a.frame_ms = 1024.0 * 1000.0 / rate as f64;
                    }
                }
                self.stats.has_audio = true;
                out.push(note(Level::Info, "audio-config", format!("{} {} Hz, {} channel(s)", codec.name(), a.sample_rate, a.channels)));
            }
            Media::Video { pts_us, dts_us, key, data } => self.on_video(*pts_us, *dts_us, *key, data, now, &mut out),
            Media::Audio { pts_us, data } => self.on_audio(*pts_us, data, now, &mut out),
            Media::Metadata(_) | Media::Track(..) => {}
        }
        out
    }

    fn on_video(&mut self, pts: i64, dts: i64, key: bool, data: &[u8], now: Instant, out: &mut Vec<Note>) {
        self.stall_check(now, out);
        let codec = self.stats.video.codec.unwrap_or(VCodec::H264);
        let v = &mut self.stats.video;
        v.frames += 1;
        v.bytes += data.len() as u64;
        v.largest_frame_bytes = v.largest_frame_bytes.max(data.len() as u64);
        self.vrate.add(now, data.len() as u64);
        self.vmedia.add(dts, data.len() as u64);
        self.frame_arrivals.push_back(now);
        self.frame_dts.push_back(dts);
        if self.frame_arrivals.len() > 120 {
            self.frame_arrivals.pop_front();
            self.frame_dts.pop_front();
        }
        self.last_video_arrival = Some(now);

        // timestamps
        if let Some(prev) = self.prev_dts {
            let delta = dts - prev;
            if delta < 0 {
                v.dts_backwards += 1;
                out.push(note(
                    Level::Error,
                    "dts-backwards",
                    format!("a picture's decode time went back by {:.1} ms", -delta as f64 / 1000.0),
                ));
            } else if delta > 0 {
                let median = {
                    let mut d: Vec<i64> = self.recent_deltas.iter().copied().collect();
                    d.sort_unstable();
                    d.get(d.len() / 2).copied()
                };
                if let Some(m) = median {
                    if self.recent_deltas.len() >= 10 && delta > (m * 5 / 2).max(100_000) {
                        v.gaps += 1;
                        v.max_gap_ms = v.max_gap_ms.max(delta as f64 / 1000.0);
                        out.push(note(
                            Level::Warn,
                            "video-gap",
                            format!(
                                "a {:.0} ms gap in the picture's timestamps (usual spacing {:.0} ms)",
                                delta as f64 / 1000.0,
                                m as f64 / 1000.0
                            ),
                        ));
                    }
                }
                self.recent_deltas.push_back(delta);
                if self.recent_deltas.len() > 60 {
                    self.recent_deltas.pop_front();
                }
            }
        }
        self.prev_dts = Some(dts);
        if self.last_pts == Some(pts) {
            v.pts_duplicates += 1;
        }
        self.last_pts = Some(pts);
        if pts != dts {
            v.reorder = true;
        }
        self.max_pts = Some(self.max_pts.map_or(pts, |m| m.max(pts)));
        self.vpts = Some(pts);
        let c = pts as f64 / 1000.0 - now.duration_since(self.t0).as_secs_f64() * 1000.0;
        self.clock_v = Some(self.clock_v.map_or(c, |o| o * 0.95 + c * 0.05));

        // the bitstream: NAL lengths, SEI, IDRs, in-band SPS
        let nals = if codec.is_nal() { nal::split_lengths(data, 4) } else { vec![] };
        if codec.is_nal() {
            if nals.iter().map(|n| n.len() + 4).sum::<usize>() != data.len() {
                self.stats.video.bitstream.bad_nal_lengths += 1;
                self.once(
                    out,
                    Level::Error,
                    "nal-lengths",
                    "a picture's NAL unit lengths don't add up to its size (start codes where lengths belong, or a wrong length size)",
                );
            }
            self.scan_nals(codec, &nals, key, now, out);
        }
        let v = &mut self.stats.video;

        // GOP
        let mut ty = if key { Some(PicType::I) } else { None };
        if ty.is_none() {
            for n in &nals {
                if nal::is_slice(codec, nal::nal_type(codec, n)) {
                    ty = nal::slice_type(codec, n, &self.pps);
                    break;
                }
            }
        }
        let ty = ty.unwrap_or(PicType::P);
        match ty {
            PicType::I => v.bitstream.pictures_i += 1,
            PicType::P => v.bitstream.pictures_p += 1,
            PicType::B => v.bitstream.pictures_b += 1,
        }
        if ty == PicType::B {
            v.b_frames = true;
            self.consec_b += 1;
            v.max_consecutive_b = v.max_consecutive_b.max(self.consec_b);
        } else {
            self.consec_b = 0;
        }
        if key {
            v.keyframes += 1;
            if !self.gop.is_empty() {
                v.gop_frames = Some(self.gop_frames);
                v.gop_pattern = rle(&self.gop);
            }
            if let Some(prev) = self.last_key_dts {
                let s = (dts - prev) as f64 / 1e6;
                self.keyints.push(s);
                v.keyint_last_s = Some(s);
                v.keyint_min_s = Some(self.keyints.iter().cloned().fold(f64::MAX, f64::min));
                v.keyint_max_s = Some(self.keyints.iter().cloned().fold(0.0, f64::max));
                v.keyint_avg_s = Some(self.keyints.iter().sum::<f64>() / self.keyints.len() as f64);
            }
            self.last_key_dts = Some(dts);
            self.gop.clear();
            self.gop_frames = 0;
        }
        if self.gop.len() < 400 {
            self.gop.push(match ty {
                PicType::I => 'I',
                PicType::P => 'P',
                PicType::B => 'B',
            });
        }
        self.gop_frames += 1;
        v.since_key_s = self.last_key_dts.map(|k| (dts - k) as f64 / 1e6);
        if let Some(sps) = &v.sps {
            if sps.max_reorder.is_some_and(|r| r > 0) {
                v.reorder = true;
            }
        }
    }

    fn on_audio(&mut self, pts: i64, data: &[u8], now: Instant, out: &mut Vec<Note>) {
        self.stall_check(now, out);
        let a = &mut self.stats.audio;
        a.frames += 1;
        a.bytes += data.len() as u64;
        let since_start = now.duration_since(self.t0).as_secs_f64() * 1000.0;
        self.arate.add(now, data.len() as u64);
        self.amedia.add(pts, data.len() as u64);
        self.last_audio_arrival = Some(now);
        let mut frame_us = if a.frame_ms > 0.0 { (a.frame_ms * 1000.0) as i64 } else { 0 };
        if a.codec == Some(ACodec::Opus) {
            if let Some((us, _)) = opus_toc(data) {
                frame_us = us as i64;
                a.frame_ms = us as f64 / 1000.0;
            }
        }
        if let Some(prev) = self.prev_audio_pts {
            let delta = pts - prev;
            if delta < 0 {
                a.pts_backwards += 1;
                out.push(note(
                    Level::Error,
                    "audio-backwards",
                    format!("a sound packet's time went back by {:.1} ms", -delta as f64 / 1000.0),
                ));
            } else if frame_us > 0 && delta > frame_us * 3 / 2 + 5_000 {
                a.gaps += 1;
                a.max_gap_ms = a.max_gap_ms.max(delta as f64 / 1000.0);
                out.push(note(
                    Level::Warn,
                    "audio-gap",
                    format!(
                        "a {:.0} ms gap in the sound's timestamps (a packet is {:.1} ms)",
                        delta as f64 / 1000.0,
                        frame_us as f64 / 1000.0
                    ),
                ));
            }
        }
        self.prev_audio_pts = Some(pts);
        self.apts = Some(pts);
        let c = pts as f64 / 1000.0 - since_start;
        self.clock_a = Some(self.clock_a.map_or(c, |o| o * 0.95 + c * 0.05));
        if let (Some(v), Some(au)) = (self.vpts, self.apts) {
            self.offsets.push_back((v - au) as f64 / 1000.0);
            if self.offsets.len() > 600 {
                self.offsets.pop_front();
            }
        }
    }

    /// Levels decoded by the audio helper.
    pub fn set_levels(&mut self, l: Levels, now: Instant) -> Vec<Note> {
        let mut out = vec![];
        let t = self.elapsed_ms(now) / 1000.0;
        let was = self.stats.levels.silent;
        let mut l = l;
        l.silence_events = self.stats.levels.silence_events;
        l.silence_total_s = self.stats.levels.silence_total_s;
        l.clipped_blocks = self.stats.levels.clipped_blocks;
        l.max_peak_db = match (
            self.stats.levels.max_peak_db,
            l.peak_l_db.into_iter().chain(l.peak_r_db).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b)))),
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if let Some(prev) = self.last_levels_at {
            if was {
                l.silence_total_s += now.duration_since(prev).as_secs_f64();
            }
        }
        self.last_levels_at = Some(now);
        if l.peak_l_db.into_iter().chain(l.peak_r_db).any(|p| p >= -0.1) {
            l.clipped_blocks += 1;
        }
        if l.silent && !was {
            l.silence_events += 1;
            self.silence_since = Some(t);
            out.push(note(Level::Warn, "silence", "the sound went silent"));
        } else if !l.silent && was {
            out.push(note(
                Level::Info,
                "silence-ended",
                format!("sound came back after {:.1} s of silence", t - self.silence_since.unwrap_or(t)),
            ));
        }
        l.available = true;
        self.stats.levels = l;
        out
    }

    /// Once a second: derived numbers and the graph samples.
    pub fn tick(&mut self, now: Instant) -> Vec<Note> {
        let mut out = vec![];
        let t = self.elapsed_ms(now) / 1000.0;
        let v = &mut self.stats.video;
        v.kbps = self.vrate.kbps(now);
        v.peak_kbps = v.peak_kbps.max(v.kbps);
        self.stats.audio.kbps = self.arate.kbps(now);
        self.stats.total_kbps = self.stats.video.kbps + self.stats.audio.kbps;
        let v = &mut self.stats.video;
        if self.frame_arrivals.len() >= 10 {
            let span = now.min(*self.frame_arrivals.back().unwrap()).duration_since(*self.frame_arrivals.front().unwrap()).as_secs_f64();
            if span > 0.0 {
                v.fps_arrival = (self.frame_arrivals.len() - 1) as f64 / span;
            }
            let dspan = (*self.frame_dts.back().unwrap() - *self.frame_dts.front().unwrap()) as f64 / 1e6;
            if dspan > 0.0 {
                v.fps = (self.frame_dts.len() - 1) as f64 / dspan;
            }
        }
        // the level's limits against the measured rate and bitrate
        let v = &self.stats.video;
        if let (Some(codec), Some(sps)) = (v.codec, &v.sps) {
            let fps = if v.fps > 0.0 { v.fps } else { sps.declared_fps.unwrap_or(0.0) };
            let found = levels::level_problems(codec, sps, fps, v.peak_kbps);
            // said when a kind of excess first shows (the numbers move from tick to tick)
            for (kind, w) in &found {
                if !self.level_kinds.contains(kind) {
                    out.push(note(Level::Warn, "video-level", w.clone()));
                }
            }
            self.level_kinds = found.iter().map(|f| f.0).collect();
            self.stats.video.bitstream.level_warnings = found.into_iter().map(|f| f.1).collect();
        }
        // A/V offset and drift
        if !self.offsets.is_empty() {
            let mut s: Vec<f64> = self.offsets.iter().copied().collect();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            self.stats.av.offset_ms = Some(s[s.len() / 2]);
        }
        if let (Some(cv), Some(ca)) = (self.clock_v, self.clock_a) {
            let diff = cv - ca;
            match self.baseline {
                None if t > 3.0 => self.baseline = Some((diff, t)),
                Some((b, t0)) => {
                    let drift = diff - b;
                    self.stats.av.drift_ms = Some(drift);
                    if t - t0 > 60.0 {
                        self.stats.av.drift_ms_per_min = Some(drift / ((t - t0) / 60.0));
                    }
                }
                _ => {}
            }
        }
        // stalled right now?
        if let Some(last) = self.last_any {
            let gap = now.duration_since(last).as_secs_f64() * 1000.0;
            let s = &mut self.stats.stalls;
            if gap > 1000.0 {
                if !self.in_stall {
                    self.in_stall = true;
                    out.push(note(Level::Warn, "stalled", "no media for over a second"));
                }
                s.stalled_now = true;
            }
        }
        self.series.push(Sample {
            t,
            video_kbps: self.stats.video.kbps,
            audio_kbps: self.stats.audio.kbps,
            fps: if self.stats.video.fps > 0.0 { self.stats.video.fps_arrival } else { 0.0 },
            lufs: self.stats.levels.momentary_lufs,
            peak_db: self
                .stats
                .levels
                .peak_l_db
                .into_iter()
                .chain(self.stats.levels.peak_r_db)
                .fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b)))),
            av_offset_ms: self.stats.av.offset_ms,
            latency_ms: self.stats.video.latency.last_ms,
        });
        if self.series.len() > 7200 {
            self.series.remove(0);
        }
        out
    }
}

impl Analyzer {
    /// A parameter set came (sequence header or in-band): counts a change, says what changed and
    /// checks the colour tags.
    fn new_sps(&mut self, codec: VCodec, info: Sps, from: &str, out: &mut Vec<Note>) {
        if let Some(old) = &self.stats.video.sps {
            if *old == info {
                return;
            }
            self.stats.video.bitstream.sps_changes += 1;
            let what = sps_diff(old, &info);
            out.push(note(
                Level::Warn,
                "sps-changed",
                format!("the SPS changed mid-stream ({from}): {}", if what.is_empty() { "other fields".into() } else { what.join(", ") }),
            ));
        }
        if codec.is_nal() || info.vui {
            let found = levels::colour_problems(&info);
            for w in &found {
                if !self.stats.video.bitstream.colour_warnings.contains(w) {
                    out.push(note(Level::Warn, "video-colour", w.clone()));
                }
            }
            self.stats.video.bitstream.colour_warnings = found;
        }
        self.stats.video.sps = Some(info);
    }

    /// One picture's NAL units: SEI (BPM, captions, user data), IDRs, in-band SPS.
    fn scan_nals(&mut self, codec: VCodec, nals: &[&[u8]], key: bool, now: Instant, out: &mut Vec<Note>) {
        let t = now.duration_since(self.t0).as_secs_f64();
        let (mut idr, mut slice_seen, mut bpm_any) = (false, false, false);
        // timestamp, session metrics, rendition metrics before the first slice
        let mut before = [false; 3];
        let mut erm_output = None;
        for n in nals {
            if n.is_empty() {
                continue;
            }
            let ty = nal::nal_type(codec, n);
            if nal::is_slice(codec, ty) {
                slice_seen = true;
                idr |= matches!((codec, ty), (VCodec::H264, 5) | (VCodec::Hevc, 19 | 20));
                continue;
            }
            if matches!((codec, ty), (VCodec::H264, 7) | (VCodec::Hevc, 33)) {
                self.stats.video.bitstream.inband_sps += 1;
                if self.sps_raw.as_deref() != Some(*n) {
                    if self.sps_raw.is_some() {
                        self.stats.video.bitstream.inband_sps_differs += 1;
                    }
                    self.sps_raw = Some(n.to_vec());
                    if let Some(info) = nal::parse_sps(codec, n) {
                        self.new_sps(codec, info, "in-band", out);
                    }
                }
                continue;
            }
            if !sei::is_sei(codec, ty) {
                continue;
            }
            for m in sei::messages(codec, n) {
                *self.stats.video.bitstream.sei.entry(sei::type_name(codec, m.kind)).or_default() += 1;
                match m.kind {
                    sei::SEI_UNREGISTERED => match sei::bpm_kind(&m.payload) {
                        Some(kind) => {
                            bpm_any = true;
                            if slice_seen {
                                self.stats.video.bpm.after_slice += 1;
                            } else {
                                before[kind as usize] = true;
                            }
                            if let Some(o) = self.on_bpm(&m.payload, now, out) {
                                erm_output = Some(o);
                            }
                        }
                        None => sei::note_user_data(&mut self.stats.video.bitstream.user_data, &m.payload),
                    },
                    sei::SEI_REGISTERED => {
                        if let Some(cc) = sei::a53_cc(&m.payload) {
                            if !self.captions.stats.present {
                                out.push(note(Level::Info, "captions", "captions in the picture (CEA-608/708 in A/53 SEI)"));
                            }
                            self.captions.feed(&cc, t);
                            self.stats.video.captions = self.captions.stats.clone();
                        }
                    }
                    _ => {}
                }
            }
        }
        if idr {
            self.stats.video.bitstream.idr += 1;
        }
        if key && !idr {
            self.stats.video.bitstream.keyframes_not_idr += 1;
            self.once(
                out,
                Level::Info,
                "keyframe-not-idr",
                "a keyframe that isn't an IDR (HEVC CRA, or an I picture with a recovery point)",
            );
        }
        if idr && !key {
            self.stats.video.bitstream.idr_not_marked += 1;
            self.once(out, Level::Warn, "idr-not-marked", "an IDR picture the container doesn't mark as a keyframe");
        }
        let keyframe = key || idr;
        let bpm = &mut self.stats.video.bpm;
        if bpm_any && !keyframe {
            bpm.off_keyframe += 1;
        }
        if !keyframe {
            return;
        }
        bpm.keyframes_checked += 1;
        bpm.last_missing = ["ts", "sm", "erm"].iter().zip(before).filter(|(_, had)| !had).map(|(n, _)| n.to_string()).collect();
        if bpm.last_missing.is_empty() {
            bpm.keyframes_with_all += 1;
        } else {
            bpm.keyframes_missing += 1;
            // said when the sender writes BPM at all (a stream without any is checked by the profile)
            if bpm.present && (self.bpm_said < 5 || bpm.keyframes_missing % 100 == 0) {
                self.bpm_said += 1;
                out.push(note(
                    Level::Warn,
                    "bpm-missing",
                    format!(
                        "a keyframe at {t:.1} s without the BPM {} SEI in front of it ({} so far)",
                        bpm.last_missing.join(", "),
                        bpm.keyframes_missing
                    ),
                ));
            }
        }
        bpm.every_keyframe = Some(bpm.keyframes_missing == 0);
        // the rendition's frames since the keyframe before, against the pictures that arrived
        if let (Some(o), Some(_)) = (erm_output, self.last_key_dts) {
            if o != self.gop_frames as u64 {
                bpm.output_mismatches += 1;
                if bpm.output_mismatches <= 3 {
                    out.push(note(
                        Level::Info,
                        "bpm-counters",
                        format!(
                            "the BPM rendition metrics say {o} frames were output since the last keyframe; {} arrived",
                            self.gop_frames
                        ),
                    ));
                }
            }
        }
    }

    /// One BPM message: its counters and times, and the latency from its timestamp. Gives an
    /// encoded rendition metrics message's "output" count.
    fn on_bpm(&mut self, payload: &[u8], now: Instant, out: &mut Vec<Note>) -> Option<u64> {
        let arrival = self.wall_ms(now);
        let bpm = &mut self.stats.video.bpm;
        if !bpm.present {
            bpm.present = true;
            out.push(note(Level::Info, "bpm", "broadcast performance metrics (BPM) SEI in the picture"));
        }
        let Some(m) = sei::bpm(payload) else {
            bpm.malformed += 1;
            self.once(out, Level::Warn, "bpm-malformed", "a BPM SEI message that can't be read");
            return None;
        };
        let counter = |tag: u8| m.counters.iter().find(|c| c.0 == tag).map_or(0, |c| c.1 as u64);
        let first = m.times.first();
        let ivs_form = m.times.len() == 1
            && first.is_some_and(|t| t.kind == 1 && t.event == 4)
            && match m.kind {
                BpmKind::Ts => true,
                BpmKind::Sm => m.counters.len() == 4,
                BpmKind::Erm => m.counters.len() == 3,
            };
        if !ivs_form && m.kind != BpmKind::Ts {
            bpm.malformed += 1;
            self.once(
                out,
                Level::Warn,
                "bpm-form",
                "a BPM metrics message not in the form IVS reads (one RFC 3339 time of event 4; four session or three rendition counters)",
            );
        }
        let bpm = &mut self.stats.video.bpm;
        let mut output = None;
        match m.kind {
            BpmKind::Ts => {
                bpm.ts_messages += 1;
                let t = m.times.iter().find(|t| t.event == 4 && t.unix_ms.is_some()).or(m.times.iter().find(|t| t.unix_ms.is_some()));
                if let Some(t) = t {
                    bpm.last_time = t.text.clone().or_else(|| t.unix_ms.map(|ms| format!("{ms:.0} ms since 1970")));
                    bpm.last_event = Some(t.event);
                    let sent = t.unix_ms.unwrap_or(0.0);
                    if self.last_bpm_ms.is_some_and(|l| sent < l) {
                        bpm.time_backwards += 1;
                    }
                    self.last_bpm_ms = Some(sent);
                    let ms = arrival - sent;
                    // a clock a day away is no clock to measure against
                    if ms.abs() < 86_400_000.0 {
                        let l = &mut self.stats.video.latency;
                        l.source = Some("bpm".into());
                        l.samples += 1;
                        l.last_ms = Some(ms);
                        l.min_ms = Some(l.min_ms.map_or(ms, |m| m.min(ms)));
                        l.max_ms = Some(l.max_ms.map_or(ms, |m| m.max(ms)));
                        self.latency_sum += ms;
                        l.avg_ms = Some(self.latency_sum / l.samples as f64);
                    }
                }
            }
            BpmKind::Sm => {
                bpm.sm_messages += 1;
                let c = SessionCounts { rendered: counter(1), lagged: counter(2), dropped: counter(3), output: counter(4) };
                let s = &mut bpm.session_total;
                s.rendered += c.rendered;
                s.lagged += c.lagged;
                s.dropped += c.dropped;
                s.output += c.output;
                bpm.session = Some(c);
            }
            BpmKind::Erm => {
                bpm.erm_messages += 1;
                let c = RenditionCounts { input: counter(1), skipped: counter(2), output: counter(3) };
                let r = &mut bpm.rendition_total;
                r.input += c.input;
                r.skipped += c.skipped;
                r.output += c.output;
                bpm.rendition = Some(c);
                output = Some(c.output);
            }
        }
        output
    }
}

/// Range and colour tags in a few words: "limited range, BT.709".
fn colour_summary(s: &Sps) -> String {
    let c = &s.colour;
    let range = match c.full_range {
        Some(true) => "full range",
        Some(false) => "limited range",
        None => "range not tagged",
    };
    let colours = match (&c.primaries_name, &c.transfer_name, &c.matrix_name) {
        (Some(p), Some(t), Some(m)) if p == t && t == m => p.clone(),
        (Some(p), Some(t), Some(m)) => format!("primaries {p}, transfer {t}, matrix {m}"),
        _ => "colours not tagged".into(),
    };
    format!("{range}, {colours}, {} bit", s.bit_depth)
}

/// What differs between two SPS, in words.
fn sps_diff(a: &Sps, b: &Sps) -> Vec<String> {
    let mut out = vec![];
    let mut d = |name: &str, x: String, y: String| {
        if x != y {
            out.push(format!("{name} {x} → {y}"));
        }
    };
    d("size", format!("{}x{}", a.width, a.height), format!("{}x{}", b.width, b.height));
    d("profile", a.profile.clone(), b.profile.clone());
    d("level", a.level.clone(), b.level.clone());
    d("bit depth", a.bit_depth.to_string(), b.bit_depth.to_string());
    d("chroma format", a.chroma_format.to_string(), b.chroma_format.to_string());
    d("full range", format!("{:?}", a.colour.full_range), format!("{:?}", b.colour.full_range));
    d("primaries", format!("{:?}", a.colour.primaries), format!("{:?}", b.colour.primaries));
    d("transfer", format!("{:?}", a.colour.transfer), format!("{:?}", b.colour.transfer));
    d("matrix", format!("{:?}", a.colour.matrix), format!("{:?}", b.colour.matrix));
    d("declared fps", format!("{:?}", a.declared_fps), format!("{:?}", b.declared_fps));
    d("reordering", format!("{:?}", a.max_reorder), format!("{:?}", b.max_reorder));
    d("reference frames", format!("{:?}", a.ref_frames), format!("{:?}", b.ref_frames));
    d("interlaced", a.interlaced.to_string(), b.interlaced.to_string());
    out
}

/// "I P×29" for a frame-type run.
fn rle(g: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < g.len() {
        let c = g[i];
        let mut n = 1;
        while i + n < g.len() && g[i + n] == c {
            n += 1;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push(c);
        if n > 1 {
            out.push_str(&format!("×{n}"));
        }
        i += n;
        if out.len() > 120 {
            out.push('…');
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    fn video(a: &mut Analyzer, at: Instant, i: i64, key: bool) -> Vec<Note> {
        let nal: &[u8] = if key { &[0x65, 0x88] } else { &[0x41, 0x9a] };
        let data = Arc::new(nal::to_lengths(&[nal]));
        a.on_media(&Media::Video { pts_us: i * 33_333, dts_us: i * 33_333, key, data }, at)
    }

    #[test]
    fn fps_keyframe_interval_and_gop_are_measured() {
        let t0 = Instant::now();
        let mut a = Analyzer::new(t0);
        for i in 0..90 {
            video(&mut a, t0 + Duration::from_micros(i as u64 * 33_333), i, i % 30 == 0);
        }
        a.tick(t0 + Duration::from_secs(3));
        let v = &a.stats.video;
        assert!((v.fps - 30.0).abs() < 0.5, "{}", v.fps);
        assert!((v.keyint_last_s.unwrap() - 1.0).abs() < 0.01);
        assert_eq!(v.gop_frames, Some(30));
        assert_eq!(v.gop_pattern, "I P×29");
        assert!(!v.b_frames);
        assert_eq!(v.keyframes, 3);
    }

    #[test]
    fn a_backwards_timestamp_and_a_gap_are_noticed() {
        let t0 = Instant::now();
        let mut a = Analyzer::new(t0);
        for i in 0..20 {
            video(&mut a, t0 + Duration::from_millis(i as u64 * 33), i, i == 0);
        }
        let notes = video(&mut a, t0 + Duration::from_millis(700), 40, false);
        assert!(notes.iter().any(|n| n.kind == "video-gap"), "{notes:?}");
        let notes = video(&mut a, t0 + Duration::from_millis(733), 5, false);
        assert!(notes.iter().any(|n| n.kind == "dts-backwards"));
        assert_eq!(a.stats.video.dts_backwards, 1);
    }

    #[test]
    fn a_silence_of_input_counts_as_a_stall() {
        let t0 = Instant::now();
        let mut a = Analyzer::new(t0);
        video(&mut a, t0, 0, true);
        let notes = video(&mut a, t0 + Duration::from_millis(2500), 1, false);
        assert!(notes.iter().any(|n| n.kind == "stall"));
        assert_eq!(a.stats.stalls.count, 1);
    }

    #[test]
    fn the_av_offset_is_the_difference_of_timestamps() {
        let t0 = Instant::now();
        let mut a = Analyzer::new(t0);
        a.on_media(&Media::AudioConfig { codec: ACodec::Aac, config: Arc::new(vec![0x11, 0x90]), sample_rate: 48000, channels: 2 }, t0);
        for i in 0..100i64 {
            let at = t0 + Duration::from_millis(i as u64 * 21);
            video(&mut a, at, i * 21_333 / 33_333, i % 30 == 0);
            // audio runs 80 ms behind the picture
            a.on_media(&Media::Audio { pts_us: i * 21_333 - 80_000, data: Arc::new(vec![1; 10]) }, at);
        }
        a.tick(t0 + Duration::from_secs(2));
        let off = a.stats.av.offset_ms.unwrap();
        assert!((off - 80.0).abs() < 20.0, "{off}");
    }
}
