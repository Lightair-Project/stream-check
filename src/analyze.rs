//! What the lab measures about a stream, from the same events whatever the protocol: codec and
//! profile, size, frame rate, bitrate, keyframe interval, GOP, timestamps, stalls, A/V offset and
//! drift, audio format and levels.

use std::collections::VecDeque;
use std::time::Instant;

use serde::Serialize;

use crate::media::{ACodec, Media, opus_toc, read_asc};
use crate::nal::{self, HevcPps, PicType, Sps, VCodec};

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
}

impl Analyzer {
    pub fn new(t0: Instant) -> Self {
        Analyzer {
            t0,
            stats: Stats::default(),
            vrate: Rate::default(),
            arate: Rate::default(),
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
        }
    }

    fn elapsed_ms(&self, now: Instant) -> f64 {
        now.duration_since(self.t0).as_secs_f64() * 1000.0
    }

    /// A new connection continues the session: timestamps start over, the history stays.
    pub fn restarted(&mut self) {
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
                                "{} {} level {}, {}x{}{}",
                                codec.name(),
                                info.profile,
                                info.level,
                                info.width,
                                info.height,
                                if changed { " (the resolution changed mid-stream)" } else { "" }
                            ),
                        ));
                    }
                    self.stats.video.sps = Some(info);
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

        // GOP
        let nals = nal::split_lengths(data, 4);
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
        });
        if self.series.len() > 7200 {
            self.series.remove(0);
        }
        out
    }
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
