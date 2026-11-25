//! Enhanced RTMP multitrack (Twitch Dual Format: one connection with a landscape and a vertical
//! picture). Track 0 is the session's own media; every other track is analysed and recorded on
//! its own here, and the tracks are checked against each other: keyframes at the same times, one
//! timestamp base, timestamps that rise.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use serde_json::{Value, json};

use crate::analyze::{Analyzer, Level, Note, Stats};
use crate::media::Media;
use crate::mkv::{self, AudioTrack, Mkv, Offsets, VideoTrack};

/// A track's start more than this from track 0's is a second timestamp base.
const BASE_TOLERANCE_US: i64 = 1_000_000;
/// Keyframes are compared once both tracks have media this far past them.
const SETTLE_US: i64 = 500_000;
/// How many misaligned keyframes are said one by one per track (the rest are counted).
const SAY_MISALIGNED: u64 = 5;

/// A track's recording: one Matroska file of its picture (or of its sound), playable on its own.
struct Rec {
    name: String,
    file: File,
    len: u64,
    offsets: Offsets,
    mkv: Mkv,
}

impl Rec {
    fn create(dir: &Path, name: String, video: Option<VideoTrack>, audio: Option<AudioTrack>) -> std::io::Result<(Rec, Vec<u8>)> {
        let mut file = File::create(dir.join(&name))?;
        let (m, header) = Mkv::new(video, audio);
        file.write_all(&header)?;
        Ok((Rec { name, file, len: header.len() as u64, offsets: m.offsets, mkv: m }, header))
    }

    fn write(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() && self.file.write_all(bytes).is_ok() {
            self.len += bytes.len() as u64;
        }
    }

    /// Writes the rest, patches the header; (file, bytes).
    fn close(mut self) -> (String, u64) {
        let tail = self.mkv.flush();
        self.write(&tail);
        let _ = mkv::patch(&mut self.file, self.offsets, self.len, self.mkv.duration_ms());
        let _ = self.file.flush();
        (self.name, self.len)
    }
}

/// A track's sound (Enhanced RTMP audio track N, Twitch's VOD track say): measured on its own
/// like the main sound (bitrate, timestamps, levels and silence through ffmpeg), and recorded as
/// `segNN-trackN-audio.mkv`.
pub struct AudioPart {
    pub analyzer: Analyzer,
    rec: Option<Rec>,
    /// The recording's header, until a decoder for the levels is attached.
    header: Option<Vec<u8>>,
    /// The decoder the levels come from (ffmpeg), fed the same Matroska as the recording.
    meter: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// The latest recording's name (kept after it is closed).
    recording: Option<String>,
}

impl AudioPart {
    fn send_meter(&mut self, bytes: &[u8]) {
        if let (Some(m), false) = (&self.meter, bytes.is_empty()) {
            let _ = m.try_send(bytes.to_vec());
        }
    }
}

impl Track {
    /// A message of the track's sound: its notes, and the name of a recording it started.
    fn audio_media(&mut self, id: u8, m: &Media, now: Instant, t0: Instant, dir: &Path, segment: u32) -> (Vec<Note>, Option<String>) {
        let a = self.audio.get_or_insert_with(|| AudioPart {
            analyzer: Analyzer::new(t0),
            rec: None,
            header: None,
            meter: None,
            recording: None,
        });
        let mut notes = audio_prefixed(id, a.analyzer.on_media(m, now));
        let mut started = None;
        match m {
            Media::AudioConfig { codec, config, .. } if a.rec.is_none() => {
                let st = &a.analyzer.stats.audio;
                let name = format!("seg{segment:02}-track{id}-audio.mkv");
                let audio = AudioTrack { codec: *codec, config: config.to_vec(), rate: st.sample_rate, channels: st.channels };
                match Rec::create(dir, name.clone(), None, Some(audio)) {
                    Ok((rec, header)) => {
                        a.rec = Some(rec);
                        a.header = Some(header);
                        a.recording = Some(name.clone());
                        started = Some(name);
                    }
                    Err(e) => notes.push(Note {
                        level: Level::Error,
                        kind: "recording",
                        msg: format!("track {id} audio: couldn't create {name}: {e}"),
                    }),
                }
            }
            Media::Audio { pts_us, data } => {
                if let Some(r) = &mut a.rec {
                    let bytes = r.mkv.audio(*pts_us, data);
                    r.write(&bytes);
                    a.send_meter(&bytes);
                }
            }
            _ => {}
        }
        (notes, started)
    }
}

pub struct Track {
    /// The picture's analysis (and, without an [`AudioPart`], nothing else).
    pub analyzer: Analyzer,
    /// The track's sound, when it has one.
    pub audio: Option<AudioPart>,
    rec: Option<Rec>,
    first_dts: Option<i64>,
    latest_dts: Option<i64>,
    unchecked: VecDeque<i64>,
    aligned: u64,
    misaligned: u64,
    start_offset_ms: Option<f64>,
}

#[derive(Default)]
pub struct Tracks {
    pub list: BTreeMap<u8, Track>,
    keys0: VecDeque<i64>,
    first0: Option<i64>,
    latest0: Option<i64>,
    /// The fixed keyframe interval the profile wants of every track, when it wants one.
    pub keyint_target_s: Option<f64>,
}

fn prefixed(track: u8, notes: Vec<Note>) -> Vec<Note> {
    notes
        .into_iter()
        .map(|mut n| {
            n.msg = format!("track {track}: {}", n.msg);
            n
        })
        .collect()
}

fn audio_prefixed(track: u8, notes: Vec<Note>) -> Vec<Note> {
    notes
        .into_iter()
        .map(|mut n| {
            n.msg = format!("track {track} audio: {}", n.msg);
            n
        })
        .collect()
}

impl Tracks {
    /// The decoder that measures track `id`'s sound: it gets the recording's header now, and
    /// the rest as it is written.
    pub fn attach_meter(&mut self, id: u8, meter: tokio::sync::mpsc::Sender<Vec<u8>>) {
        if let Some(a) = self.list.get_mut(&id).and_then(|t| t.audio.as_mut()) {
            if let Some(h) = a.header.take() {
                let _ = meter.try_send(h);
            }
            a.meter = Some(meter);
        }
    }

    /// Levels of track `id`'s sound, decoded by its meter; the notes they bring.
    pub fn set_levels(&mut self, id: u8, l: crate::analyze::Levels, now: Instant) -> Vec<Note> {
        match self.list.get_mut(&id).and_then(|t| t.audio.as_mut()) {
            Some(a) => audio_prefixed(id, a.analyzer.set_levels(l, now)),
            None => vec![],
        }
    }

    /// Track 0's pictures (the session's own): the times the other tracks are held against.
    pub fn on_track0(&mut self, m: &Media) {
        if let Media::Video { dts_us, key, .. } = m {
            self.first0.get_or_insert(*dts_us);
            self.latest0 = Some(self.latest0.map_or(*dts_us, |l| l.max(*dts_us)));
            if *key {
                self.keys0.push_back(*dts_us);
                if self.keys0.len() > 64 {
                    self.keys0.pop_front();
                }
            }
        }
    }

    /// One message of track `id` (not 0): its notes, and the name of a recording it started.
    pub fn feed(&mut self, id: u8, m: Media, now: Instant, t0: Instant, dir: &Path, segment: u32) -> (Vec<Note>, Option<String>) {
        let first0 = self.first0;
        let target = self.keyint_target_s;
        let track = self.list.entry(id).or_insert_with(|| Track {
            analyzer: {
                let mut a = Analyzer::new(t0);
                a.keyint_target_s = target;
                a
            },
            audio: None,
            rec: None,
            first_dts: None,
            latest_dts: None,
            unchecked: VecDeque::new(),
            aligned: 0,
            misaligned: 0,
            start_offset_ms: None,
        });
        if matches!(m, Media::AudioConfig { .. } | Media::Audio { .. }) {
            return track.audio_media(id, &m, now, t0, dir, segment);
        }
        let mut notes = prefixed(id, track.analyzer.on_media(&m, now));
        let mut started = None;
        match &m {
            Media::VideoConfig { codec, record } if track.rec.is_none() => {
                let (w, h) = track.analyzer.stats.video.sps.as_ref().map_or((0, 0), |s| (s.width, s.height));
                let name = format!("seg{segment:02}-track{id}.mkv");
                let video = VideoTrack { codec: *codec, record: record.to_vec(), width: w, height: h };
                match Rec::create(dir, name.clone(), Some(video), None) {
                    Ok((rec, _)) => {
                        track.rec = Some(rec);
                        started = Some(name);
                    }
                    Err(e) => {
                        notes.push(Note { level: Level::Error, kind: "recording", msg: format!("track {id}: couldn't create {name}: {e}") })
                    }
                }
            }
            Media::Video { pts_us, dts_us, key, data } => {
                if let Some(r) = &mut track.rec {
                    let bytes = r.mkv.video(*pts_us, *key, data);
                    r.write(&bytes);
                }
                if track.first_dts.is_none() {
                    track.first_dts = Some(*dts_us);
                }
                track.latest_dts = Some(track.latest_dts.map_or(*dts_us, |l| l.max(*dts_us)));
                if *key {
                    track.unchecked.push_back(*dts_us);
                    if track.unchecked.len() > 64 {
                        track.unchecked.pop_front();
                    }
                }
            }
            _ => {}
        }
        // one timestamp base: the track starts where track 0 does
        if track.start_offset_ms.is_none()
            && let (Some(f), Some(f0)) = (track.first_dts, first0)
        {
            let off = f - f0;
            track.start_offset_ms = Some(off as f64 / 1000.0);
            if off.abs() > BASE_TOLERANCE_US {
                notes.push(Note {
                    level: Level::Error,
                    kind: "track-base",
                    msg: format!(
                        "track {id} starts {:.0} ms from track 0: the tracks should share one timestamp base",
                        off as f64 / 1000.0
                    ),
                });
            }
        }
        (notes, started)
    }

    /// Each track's tick, and its keyframes held against track 0's once both have gone past them.
    /// `fps0` is track 0's frame rate (one frame of the slower track is the tolerance).
    pub fn tick(&mut self, now: Instant, fps0: f64) -> Vec<Note> {
        let mut out = vec![];
        for (id, t) in self.list.iter_mut() {
            if let Some(a) = &mut t.audio {
                out.extend(audio_prefixed(*id, a.analyzer.tick(now)));
            }
            if !t.analyzer.stats.has_video {
                continue;
            }
            out.extend(prefixed(*id, t.analyzer.tick(now)));
            let Some(latest0) = self.latest0 else { continue };
            let Some(latest) = t.latest_dts else { continue };
            let slowest = t.analyzer.stats.video.fps.min(fps0);
            let tolerance = if slowest > 1.0 { (1e6 / slowest) as i64 } else { 40_000 };
            while let Some(&k) = t.unchecked.front() {
                if k > latest0.min(latest) - SETTLE_US {
                    break;
                }
                t.unchecked.pop_front();
                let nearest = self.keys0.iter().map(|k0| (k0 - k).abs()).min();
                if nearest.is_some_and(|d| d <= tolerance) {
                    t.aligned += 1;
                } else {
                    t.misaligned += 1;
                    if t.misaligned <= SAY_MISALIGNED {
                        out.push(Note {
                            level: Level::Error,
                            kind: "track-keyframes",
                            msg: format!(
                                "track {id}'s keyframe at {:.2} s has no track-0 keyframe within one frame ({})",
                                k as f64 / 1e6,
                                nearest
                                    .map_or("track 0 has none near".to_string(), |d| format!("nearest {:.0} ms away", d as f64 / 1000.0))
                            ),
                        });
                    }
                }
            }
        }
        out
    }

    /// The sender came back: the tracks start again (their recordings were closed with the old
    /// connection).
    pub fn restarted(&mut self) {
        for t in self.list.values_mut() {
            t.analyzer.restarted();
            if let Some(a) = &mut t.audio {
                a.analyzer.restarted();
            }
            t.first_dts = None;
            t.latest_dts = None;
            t.unchecked.clear();
            t.start_offset_ms = None;
        }
        self.keys0.clear();
        self.first0 = None;
        self.latest0 = None;
    }

    /// The connection ended: each track's bitrates as they were while media flowed.
    pub fn connection_ended(&mut self) {
        for t in self.list.values_mut() {
            t.analyzer.connection_ended();
            if let Some(a) = &mut t.audio {
                a.analyzer.connection_ended();
            }
        }
    }

    /// Closes the tracks' recordings: (file, bytes) of each.
    pub fn finish(&mut self) -> Vec<(String, u64)> {
        let mut out = vec![];
        for t in self.list.values_mut() {
            out.extend(t.rec.take().map(Rec::close));
            if let Some(a) = &mut t.audio {
                // the decoder ends when its input does
                a.meter = None;
                a.header = None;
                out.extend(a.rec.take().map(Rec::close));
            }
        }
        out
    }

    /// The bytes written so far to each open recording.
    pub fn recording_bytes(&self) -> Vec<(String, u64)> {
        self.list
            .values()
            .flat_map(|t| t.rec.iter().chain(t.audio.as_ref().and_then(|a| a.rec.as_ref())))
            .map(|r| (r.name.clone(), r.len))
            .collect()
    }

    /// Each track's stats, for the profile's checks.
    pub fn stats(&self) -> Vec<(u8, &Stats)> {
        self.list.iter().map(|(id, t)| (*id, &t.analyzer.stats)).collect()
    }

    /// The tracks for the report; `s0` is track 0's stats.
    pub fn summary(&self, s0: &Stats) -> Value {
        if self.list.is_empty() {
            return Value::Null;
        }
        let tracks: Vec<Value> = self
            .list
            .iter()
            .map(|(id, t)| {
                let s = &t.analyzer.stats;
                let a = t.audio.as_ref().map(|a| &a.analyzer.stats).filter(|a| a.has_audio);
                json!({
                    "track": id,
                    "has_video": s.has_video,
                    "has_audio": a.is_some(),
                    "video": if s.has_video { json!(s.video) } else { Value::Null },
                    "audio": a.map(|a| json!(a.audio)),
                    "levels": a.map(|a| json!(a.levels)),
                    "audio_recording": t.audio.as_ref().and_then(|a| a.recording.clone()),
                    "keyframes_aligned": t.aligned,
                    "keyframes_misaligned": t.misaligned,
                    "start_offset_ms": t.start_offset_ms,
                    "recording": t.rec.as_ref().map(|r| r.name.clone()),
                })
            })
            .collect();
        let total: f64 = s0.video.kbps + self.list.values().map(|t| t.analyzer.stats.video.kbps).sum::<f64>();
        let peak: f64 = s0.video.peak_kbps + self.list.values().map(|t| t.analyzer.stats.video.peak_kbps).sum::<f64>();
        json!({ "count": self.list.len() + 1, "tracks": tracks, "total_video_kbps": total, "peak_video_kbps_sum": peak })
    }

    /// Problems for the final report.
    pub fn problems(&self) -> Vec<String> {
        let mut out = vec![];
        for (id, t) in &self.list {
            let v = &t.analyzer.stats.video;
            out.extend(crate::analyze::picture_timestamp_problems(v).into_iter().map(|p| format!("track {id}: {p}")));
            if let Some(a) = t.audio.as_ref().filter(|a| a.analyzer.stats.audio.pts_backwards > 0) {
                out.push(format!("track {id}: {} sound timestamps went backwards", a.analyzer.stats.audio.pts_backwards));
            }
            if t.misaligned > 0 {
                out.push(format!("track {id}: {} keyframes not aligned with track 0's", t.misaligned));
            }
            if t.start_offset_ms.is_some_and(|o| o.abs() > BASE_TOLERANCE_US as f64 / 1000.0) {
                out.push(format!("track {id} has its own timestamp base ({:.0} ms from track 0)", t.start_offset_ms.unwrap_or(0.0)));
            }
        }
        out
    }
}
