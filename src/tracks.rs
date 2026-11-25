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
use crate::mkv::{self, Mkv, Offsets, VideoTrack};

/// A track's start more than this from track 0's is a second timestamp base.
const BASE_TOLERANCE_US: i64 = 1_000_000;
/// Keyframes are compared once both tracks have media this far past them.
const SETTLE_US: i64 = 500_000;
/// How many misaligned keyframes are said one by one per track (the rest are counted).
const SAY_MISALIGNED: u64 = 5;

/// A track's recording: one Matroska file of its picture, playable on its own.
struct Rec {
    name: String,
    file: File,
    len: u64,
    offsets: Offsets,
    mkv: Mkv,
}

pub struct Track {
    pub analyzer: Analyzer,
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

impl Tracks {
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
        let track = self.list.entry(id).or_insert_with(|| Track {
            analyzer: Analyzer::new(t0),
            rec: None,
            first_dts: None,
            latest_dts: None,
            unchecked: VecDeque::new(),
            aligned: 0,
            misaligned: 0,
            start_offset_ms: None,
        });
        let mut notes = prefixed(id, track.analyzer.on_media(&m, now));
        let mut started = None;
        match &m {
            Media::VideoConfig { codec, record } if track.rec.is_none() => {
                let (w, h) = track.analyzer.stats.video.sps.as_ref().map_or((0, 0), |s| (s.width, s.height));
                let name = format!("seg{segment:02}-track{id}.mkv");
                match File::create(dir.join(&name)) {
                    Ok(mut file) => {
                        let (m, header) = Mkv::new(Some(VideoTrack { codec: *codec, record: record.to_vec(), width: w, height: h }), None);
                        let len = if file.write_all(&header).is_ok() { header.len() as u64 } else { 0 };
                        track.rec = Some(Rec { name: name.clone(), file, len, offsets: m.offsets, mkv: m });
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
                    if !bytes.is_empty() && r.file.write_all(&bytes).is_ok() {
                        r.len += bytes.len() as u64;
                    }
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
        }
    }

    /// Closes the tracks' recordings: (file, bytes) of each.
    pub fn finish(&mut self) -> Vec<(String, u64)> {
        let mut out = vec![];
        for t in self.list.values_mut() {
            if let Some(mut r) = t.rec.take() {
                let tail = r.mkv.flush();
                if !tail.is_empty() && r.file.write_all(&tail).is_ok() {
                    r.len += tail.len() as u64;
                }
                let _ = mkv::patch(&mut r.file, r.offsets, r.len, r.mkv.duration_ms());
                let _ = r.file.flush();
                out.push((r.name, r.len));
            }
        }
        out
    }

    /// The bytes written so far to each open recording.
    pub fn recording_bytes(&self) -> Vec<(String, u64)> {
        self.list.values().filter_map(|t| t.rec.as_ref().map(|r| (r.name.clone(), r.len))).collect()
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
                json!({
                    "track": id,
                    "has_video": s.has_video,
                    "has_audio": s.has_audio,
                    "video": if s.has_video { json!(s.video) } else { Value::Null },
                    "audio": if s.has_audio { json!(s.audio) } else { Value::Null },
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
            if v.dts_backwards > 0 {
                out.push(format!("track {id}: {} picture timestamps went backwards", v.dts_backwards));
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
