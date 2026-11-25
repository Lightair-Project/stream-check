//! Checking a file (a recording, a replay-buffer clip): MP4 and MOV (fragmented and hybrid too),
//! Matroska, FLV, MPEG-TS. ffmpeg is the demuxer: it copies the file's packets out as FLV, which
//! the lab reads with the same code as an RTMP stream, and they go through the same analysis as a
//! live stream, on the file's own clock (a file is read as fast as it can be, but measured as if
//! it played). ffprobe describes the file (its verdict, duration, chapters), every audio track is
//! measured and its loudness taken, and the session ends with a report like any other.

use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::analyze::{Analyzer, Level, Levels, Note};
use crate::helper::Meter;
use crate::hub::{Hub, Proto, Publisher, Session};
use crate::media::Media;
use crate::rtmp::FlvDemux;

/// Codecs whose packets the lab reads one by one (the rest are described by ffprobe only).
const PACKET_VIDEO: [&str; 4] = ["h264", "hevc", "av1", "vp9"];
const PACKET_AUDIO: [&str; 2] = ["aac", "opus"];

/// How many of an extra track's own events go into the session's log.
const TRACK_EVENTS: usize = 100;

/// What to check and how.
#[derive(Debug, Clone)]
pub struct Request {
    pub path: PathBuf,
    /// The platform profile whose rules the file is held to ("open" when not given).
    pub profile: String,
    /// The name the session goes by (the file's name when not given).
    pub key: Option<String>,
}

/// Starts checking a file: the session exists (and is listed) when this returns, and ends with
/// its report when the file has been read.
pub fn start(hub: &Arc<Hub>, req: Request) -> Result<Arc<Session>, String> {
    let path = req.path.canonicalize().map_err(|e| format!("{}: {e}", req.path.display()))?;
    if !path.is_file() {
        return Err(format!("{} isn't a file", path.display()));
    }
    if !crate::helper::ffmpeg_available() {
        return Err("checking a file needs ffmpeg and ffprobe on the PATH".into());
    }
    if crate::profiles::find(&req.profile).is_none() {
        return Err(format!("no profile '{}' (twitch, twitch-dual, youtube, kick, open)", req.profile));
    }
    let name = path.file_name().map_or("file".into(), |n| n.to_string_lossy().into_owned());
    let key = req.key.clone().unwrap_or(name);
    let remote = SocketAddr::from(([0, 0, 0, 0], 0));
    let publisher = hub.authorize_pull(Proto::File, &req.profile, &key, remote).map_err(|r| r.msg)?;
    let session = publisher.session.clone();
    session.set_virtual_clock(true);
    session.set_source(json!({ "kind": "file", "path": path }));
    let hub = hub.clone();
    tokio::spawn(async move {
        let s = publisher.session.clone();
        let reason = tokio::task::spawn_blocking(move || {
            let reason = check(&publisher, &path);
            (publisher, reason)
        })
        .await;
        match reason {
            Ok((publisher, reason)) => {
                hub.finalize(s, reason).await;
                drop(publisher);
            }
            Err(e) => hub.finalize(s, format!("the check stopped: {e}")).await,
        }
    });
    Ok(session)
}

/// Waits until a session has ended (its report is written).
pub async fn ended(s: &Arc<Session>) {
    loop {
        if s.inner.lock().report.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Reads the whole file into the session; says why the session ends.
fn check(p: &Publisher, path: &Path) -> String {
    let s = &p.session;
    let t0 = s.t0;
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    let container = container_layout(path);
    if let Some(c) = &container {
        for p in c["problems"].as_array().into_iter().flatten() {
            s.add_problem(p.as_str().unwrap_or_default());
        }
    }
    s.update_source(|v| {
        v["bytes"] = json!(size);
        v["container"] = container.clone().unwrap_or(Value::Null);
    });
    let (probe, probe_errors) = match probe_full(path) {
        Ok(v) => v,
        Err(e) => {
            s.add_problem(format!("ffprobe could not read the file: {e}"));
            s.update_source(|v| v["ffprobe_error"] = json!(e));
            return "the file can't be read".into();
        }
    };
    let streams: Vec<Value> = probe["streams"].as_array().cloned().unwrap_or_default();
    let duration = probe["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok());
    let format_start = probe["format"]["start_time"].as_str().and_then(|d| d.parse::<f64>().ok()).unwrap_or(0.0);
    let of = |kind: &str| -> Vec<Value> { streams.iter().filter(|x| x["codec_type"] == kind).cloned().collect() };
    let (videos, audios) = (of("video"), of("audio"));
    if videos.is_empty() && audios.is_empty() {
        s.add_problem("the file has neither picture nor sound");
    }
    if !probe_errors.is_empty() {
        s.event_at(t0, Level::Warn, "ffprobe", format!("ffprobe said: {}", probe_errors[0]));
    }
    s.update_source(|v| {
        v["bytes"] = json!(size);
        v["format"] = probe["format"]["format_name"].clone();
        v["format_long"] = probe["format"]["format_long_name"].clone();
        v["duration_s"] = json!(duration);
        v["start_s"] = json!(format_start);
        v["tags"] = probe["format"]["tags"].clone();
        v["chapters"] = json!(chapters(&probe));
        v["streams"] = json!(streams.iter().map(describe_stream).collect::<Vec<_>>());
        v["ffprobe_errors"] = json!(probe_errors);
    });
    if let Some(d) = duration {
        s.set_media_duration(d);
    }
    let chapters_n = probe["chapters"].as_array().map_or(0, Vec::len);
    s.event_at(
        t0,
        Level::Info,
        "file",
        format!(
            "{}, {:.1} s, {} picture and {} sound track(s){}",
            probe["format"]["format_long_name"].as_str().unwrap_or("?"),
            duration.unwrap_or(0.0),
            videos.len(),
            audios.len(),
            if chapters_n > 0 { format!(", {chapters_n} chapter(s)") } else { String::new() }
        ),
    );

    let start_of = |x: &Value| x["start_time"].as_str().and_then(|d| d.parse::<f64>().ok()).unwrap_or(format_start);
    let index = |x: &Value| x["index"].as_u64().unwrap_or(0) as usize;
    let codec = |x: &Value| x["codec_name"].as_str().unwrap_or("").to_string();
    let main_v = videos.first().filter(|v| PACKET_VIDEO.contains(&codec(v).as_str()));
    let main_a = audios.first().filter(|a| PACKET_AUDIO.contains(&codec(a).as_str()));
    for x in videos.iter().take(1).chain(audios.iter().take(1)) {
        let c = codec(x);
        if !PACKET_VIDEO.contains(&c.as_str()) && !PACKET_AUDIO.contains(&c.as_str()) {
            s.event_at(
                t0,
                Level::Warn,
                "file",
                format!("{c} isn't read packet by packet (H.264, HEVC, AV1, VP9, AAC and Opus are): ffprobe's description only"),
            );
        }
    }

    let thumbs_dir = s.dir.join("thumbs");
    let (levels0, extra_audio, extra_video) = std::thread::scope(|scope| {
        // thumbnails, from keyframes only (fast)
        if let Some(v) = videos.first() {
            let i = index(v);
            scope.spawn(move || thumbnails(path, i, &thumbs_dir));
        }
        // each audio track beyond the first: its packets and its loudness, on its own
        let extra_audio: Vec<_> = audios
            .iter()
            .skip(1)
            .map(|a| {
                let (i, c, start) = (index(a), codec(a), start_of(a) - format_start);
                scope.spawn(move || {
                    let levels = loudness(path, i, start);
                    let packets = PACKET_AUDIO.contains(&c.as_str()).then_some(i);
                    track_pass(path, packets, levels, t0)
                })
            })
            .collect();
        let extra_video: Vec<_> = videos
            .iter()
            .skip(1)
            .map(|v| {
                let (i, c) = (index(v), codec(v));
                scope.spawn(move || track_pass(path, PACKET_VIDEO.contains(&c.as_str()).then_some(i), vec![], t0))
            })
            .collect();
        let levels0 = audios.first().map_or(vec![], |a| loudness(path, index(a), start_of(a) - format_start));
        let extra_audio: Vec<_> = extra_audio.into_iter().map(|h| h.join().unwrap_or_default()).collect();
        let extra_video: Vec<_> = extra_video.into_iter().map(|h| h.join().unwrap_or_default()).collect();
        (levels0, extra_audio, extra_video)
    });

    // the main pass: the first picture and the first sound, into the session
    let maps: Vec<usize> = main_v.iter().chain(main_a.iter()).map(|x| index(x)).collect();
    let mut feed = SessionFeed { p, levels: levels0.into_iter().peekable(), killed: false };
    let summary = if maps.is_empty() {
        // nothing to read packet by packet: the sound's loudness still counts
        let mut at = t0;
        for (t, l) in feed.levels.by_ref() {
            at = t0 + Duration::from_secs_f64(t.max(0.0));
            s.set_levels_at(l, at);
        }
        PassSummary { last: Some(at), ..Default::default() }
    } else {
        flv_pass(path, &maps, t0, &mut feed)
    };
    if let Some(e) = &summary.error {
        s.add_problem(format!("ffmpeg couldn't read the file to its end: {e}"));
    }
    for w in &summary.warnings {
        s.event_at(summary.last.unwrap_or(t0), Level::Warn, "ffmpeg", format!("ffmpeg said while reading: {w}"));
    }
    // averages over the whole file, rather than the last two seconds
    let last = summary.last.unwrap_or(t0);
    let span = duration.unwrap_or_else(|| last.duration_since(t0).as_secs_f64()).max(0.001);
    s.with_stats(|st| {
        st.video.kbps = st.video.bytes as f64 * 8.0 / span / 1000.0;
        st.audio.kbps = st.audio.bytes as f64 * 8.0 / span / 1000.0;
        st.total_kbps = st.video.kbps + st.audio.kbps;
        st.stalls.stalled_now = false;
    });

    // the tracks beyond the first, and their events
    let mut audio_tracks = vec![];
    if let Some(a) = audios.first() {
        let st = s.inner.lock().analyzer.stats.clone();
        audio_tracks.push(track_json(a, json!(st.audio), json!(st.levels), json!(st.av)));
    }
    for (n, (a, r)) in audios.iter().skip(1).zip(extra_audio).enumerate() {
        for (t, note) in r.notes.iter().take(TRACK_EVENTS) {
            s.event_at(t0 + Duration::from_secs_f64(*t), note.level, note.kind, format!("audio track {}: {}", n + 1, note.msg));
        }
        let mut audio = json!(r.stats.audio);
        if r.stats.audio.frames > 0 {
            audio["kbps"] = json!(r.stats.audio.bytes as f64 * 8.0 / span / 1000.0);
        }
        audio_tracks.push(track_json(a, audio, json!(r.stats.levels), Value::Null));
    }
    let mut video_tracks = vec![];
    if let Some(v) = videos.first() {
        let st = s.inner.lock().analyzer.stats.video.clone();
        video_tracks.push(track_json(v, json!(st), Value::Null, Value::Null));
    }
    for (n, (v, r)) in videos.iter().skip(1).zip(extra_video).enumerate() {
        for (t, note) in r.notes.iter().take(TRACK_EVENTS) {
            s.event_at(t0 + Duration::from_secs_f64(*t), note.level, note.kind, format!("video track {}: {}", n + 1, note.msg));
        }
        let mut video = json!(r.stats.video);
        video["kbps"] = json!(r.stats.video.bytes as f64 * 8.0 / span / 1000.0);
        video_tracks.push(track_json(v, video, Value::Null, Value::Null));
    }
    // where picture and sound start, and how long each lasts
    let v_start = videos.first().map(start_of);
    let a_start = audios.first().map(start_of);
    let stream_dur = |x: &Value| stream_duration(x);
    let (v_dur, a_dur) = (videos.first().and_then(stream_dur), audios.first().and_then(stream_dur));
    if let (Some(v), Some(a)) = (v_dur, a_dur) {
        if (v - a).abs() > 1.0 {
            s.event_at(last, Level::Warn, "file", format!("the picture lasts {v:.2} s and the sound {a:.2} s"));
        }
    }
    let av_start = match (v_start, a_start) {
        (Some(v), Some(a)) => Some((a - v) * 1000.0),
        _ => None,
    };
    if let Some(o) = av_start.filter(|o| o.abs() > 100.0) {
        let when = if o > 0.0 { format!("{o:.0} ms after") } else { format!("{:.0} ms before", -o) };
        s.event_at(t0, Level::Warn, "file", format!("the sound starts {when} the picture"));
    }
    s.update_source(|v| {
        v["video_tracks"] = json!(video_tracks);
        v["audio_tracks"] = json!(audio_tracks);
        v["av_start_offset_ms"] = json!(av_start);
        v["video_duration_s"] = json!(v_dur);
        v["audio_duration_s"] = json!(a_dur);
        v["average_kbps"] = json!(size as f64 * 8.0 / span / 1000.0);
        v["read_packet_by_packet"] = json!(!maps.is_empty());
    });
    if feed.killed { format!("{} before the end of the file", p.session.stop_reason()) } else { "the file was read to its end".into() }
}

/// One track's description in the report: what ffprobe says, then what the lab measured.
fn track_json(x: &Value, measured: Value, levels: Value, av: Value) -> Value {
    let mut v = describe_stream(x);
    v["measured"] = measured;
    if !levels.is_null() {
        v["levels"] = levels;
    }
    if !av.is_null() {
        v["av"] = av;
    }
    v
}

fn describe_stream(x: &Value) -> Value {
    let num = |k: &str| x[k].as_str().and_then(|d| d.parse::<f64>().ok());
    json!({
        "index": x["index"],
        "type": x["codec_type"],
        "codec": x["codec_name"],
        "profile": x["profile"],
        "width": x["width"],
        "height": x["height"],
        "pix_fmt": x["pix_fmt"],
        "frame_rate": x["avg_frame_rate"],
        "sample_rate": x["sample_rate"],
        "channels": x["channels"],
        "channel_layout": x["channel_layout"],
        "start_s": num("start_time"),
        "duration_s": stream_duration(x),
        "bit_rate": x["bit_rate"],
        "frames": x["nb_frames"],
        "title": x["tags"]["title"],
        "language": x["tags"]["language"],
        "handler": x["tags"]["handler_name"],
        "default": x["disposition"]["default"],
    })
}

/// A stream's duration: ffprobe's, or Matroska's DURATION tag ("00:00:08.000000000").
fn stream_duration(x: &Value) -> Option<f64> {
    if let Some(d) = x["duration"].as_str().and_then(|d| d.parse::<f64>().ok()) {
        return Some(d);
    }
    let tag = x["tags"]["DURATION"].as_str().or(x["tags"]["duration"].as_str())?;
    let mut parts = tag.split(':').rev();
    let secs: f64 = parts.next()?.parse().ok()?;
    let mins: f64 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0.0);
    let hours: f64 = parts.next().and_then(|h| h.parse().ok()).unwrap_or(0.0);
    Some(hours * 3600.0 + mins * 60.0 + secs)
}

fn chapters(probe: &Value) -> Vec<Value> {
    probe["chapters"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            json!({
                "start_s": c["start_time"].as_str().and_then(|d| d.parse::<f64>().ok()),
                "end_s": c["end_time"].as_str().and_then(|d| d.parse::<f64>().ok()),
                "title": c["tags"]["title"],
            })
        })
        .collect()
}

/// ffprobe's whole description of a file (format, streams, chapters), and what it complained of.
pub fn probe_full(path: &Path) -> Result<(Value, Vec<String>), String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_format", "-show_streams", "-show_chapters", "-of", "json"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe didn't start: {e}"))?;
    let errors: Vec<String> =
        String::from_utf8_lossy(&out.stderr).lines().filter(|l| !l.trim().is_empty()).take(20).map(str::to_string).collect();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    if !out.status.success() || v["format"].is_null() {
        return Err(errors.first().cloned().unwrap_or_else(|| "it isn't a media file ffprobe knows".into()));
    }
    Ok((v, errors))
}

/// The top-level boxes of an MP4 or MOV file: whether it's fragmented, whether its index (moov)
/// comes first, and whether it was finished (a recording cut off has no moov, or ends inside a box).
fn container_layout(path: &Path) -> Option<Value> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut head = [0u8; 8];
    f.read_exact(&mut head).ok()?;
    if &head[4..8] != b"ftyp" && &head[4..8] != b"moov" && &head[4..8] != b"wide" && &head[4..8] != b"free" {
        return None;
    }
    use std::io::{Seek, SeekFrom};
    let mut at = 0u64;
    let mut boxes: Vec<(String, u32)> = vec![];
    let mut problems = vec![];
    let mut brand = None;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at)).ok()?;
        let mut h = [0u8; 16];
        let n = f.read(&mut h).ok()?;
        if n < 8 {
            break;
        }
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let kind: String = h[4..8].iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect();
        if size == 1 && n >= 16 {
            size = u64::from_be_bytes(h[8..16].try_into().ok()?);
        } else if size == 0 {
            size = len - at;
        }
        if kind == "ftyp" && n >= 12 {
            brand = Some(String::from_utf8_lossy(&h[8..12]).trim().to_string());
        }
        if size < 8 {
            problems.push(format!("a broken box '{kind}' at byte {at}"));
            break;
        }
        match boxes.last_mut() {
            Some((k, c)) if *k == kind => *c += 1,
            _ => boxes.push((kind.clone(), 1)),
        }
        if at + size > len {
            problems.push(format!("the file ends inside its '{kind}' box ({} of {size} bytes): it was cut off", len - at));
            break;
        }
        at += size;
    }
    let has = |k: &str| boxes.iter().any(|(b, _)| b == k);
    let count = |k: &str| boxes.iter().filter(|(b, _)| b == k).map(|(_, c)| *c).sum::<u32>();
    let fragmented = has("moof");
    if !has("moov") {
        problems.push("the file has no moov box (its index): the recording wasn't finished".into());
    }
    let moov_first = boxes.iter().position(|(b, _)| b == "moov") < boxes.iter().position(|(b, _)| b == "mdat");
    let layout = match (fragmented, has("moov")) {
        (true, true) if boxes.iter().position(|(b, _)| b == "moov") > boxes.iter().position(|(b, _)| b == "moof") => "hybrid",
        (true, _) => "fragmented",
        (false, true) => "progressive",
        (false, false) => "unfinished",
    };
    Some(json!({
        "brand": brand,
        "layout": layout,
        "fragmented": fragmented,
        "fragments": count("moof"),
        "moov_before_mdat": has("mdat") && moov_first,
        "boxes": boxes.iter().map(|(k, c)| if *c > 1 { format!("{k}×{c}") } else { k.clone() }).collect::<Vec<_>>().join(" "),
        "problems": problems,
    }))
}

/// Thumbnails every 2 s of the file, like a live session's, from keyframes only.
fn thumbnails(path: &Path, stream: usize, dir: &Path) {
    let _ = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-skip_frame", "nokey", "-i"])
        .arg(path)
        .args(["-map", &format!("0:{stream}"), "-an", "-vf", &format!("fps=1/{},scale=320:-2", crate::helper::THUMB_EVERY_S)])
        .args(["-q:v", "6", "-f", "image2", "-start_number", "0"])
        .arg(dir.join("s01_%05d.jpg"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A sound track decoded and measured: its levels every 300 ms, each at its second in the file.
fn loudness(path: &Path, stream: usize, start_s: f64) -> Vec<(f64, Levels)> {
    let Ok(mut child) = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", &format!("0:{stream}"), "-ac", "2", "-ar", "48000", "-f", "f32le", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return vec![];
    };
    let mut out = vec![];
    let mut meter = Meter::new(2, 48000);
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = vec![0u8; 48000 * 2 * 4 / 10];
    let mut fill = 0usize;
    let mut n = 0usize;
    loop {
        match stdout.read(&mut buf[fill..]) {
            Ok(0) | Err(_) => break,
            Ok(got) => fill += got,
        }
        // whole samples only; a partial one waits for the next read
        let whole = fill - fill % 4;
        let samples: Vec<f32> = buf[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        buf.copy_within(whole..fill, 0);
        fill -= whole;
        for l in meter.push(&samples) {
            n += 1;
            out.push((start_s + n as f64 * 0.3, l));
        }
    }
    let _ = child.wait();
    out
}

/// What a pass over packets reports back.
#[derive(Default)]
struct PassSummary {
    /// The moment of the last packet.
    last: Option<Instant>,
    /// ffmpeg failed: what it said first.
    error: Option<String>,
    /// What ffmpeg said while it still read the file to its end.
    warnings: Vec<String>,
}

/// Where a pass's media goes.
trait Feed {
    fn media(&mut self, m: Media, now: Instant);
    fn tick(&mut self, now: Instant);
    fn warn(&mut self, msg: String, now: Instant);
    fn stop(&mut self) -> bool {
        false
    }
}

/// Into the session (the first picture and sound), with the loudness applied at its moments.
struct SessionFeed<'a> {
    p: &'a Publisher,
    levels: std::iter::Peekable<std::vec::IntoIter<(f64, Levels)>>,
    killed: bool,
}

impl Feed for SessionFeed<'_> {
    fn media(&mut self, m: Media, now: Instant) {
        self.p.media_at(m, now);
    }
    fn tick(&mut self, now: Instant) {
        let s = &self.p.session;
        let t = now.saturating_duration_since(s.t0).as_secs_f64();
        while let Some((lt, _)) = self.levels.peek() {
            if *lt > t {
                break;
            }
            let (lt, l) = self.levels.next().unwrap();
            s.set_levels_at(l, s.t0 + Duration::from_secs_f64(lt.max(0.0)));
        }
        s.tick_at(now);
    }
    fn warn(&mut self, msg: String, now: Instant) {
        self.p.session.event_at(now, Level::Warn, "file", msg);
    }
    fn stop(&mut self) -> bool {
        self.killed = self.killed || self.p.is_killed();
        self.killed
    }
}

/// A track measured on its own (a second sound track, say), as the session measures the first.
#[derive(Default)]
struct TrackResult {
    stats: crate::analyze::Stats,
    notes: Vec<(f64, Note)>,
}

struct LocalFeed {
    t0: Instant,
    a: Analyzer,
    levels: std::iter::Peekable<std::vec::IntoIter<(f64, Levels)>>,
    notes: Vec<(f64, Note)>,
}

impl LocalFeed {
    fn keep(&mut self, notes: Vec<Note>, now: Instant) {
        let t = now.saturating_duration_since(self.t0).as_secs_f64();
        self.notes.extend(notes.into_iter().map(|n| (t, n)));
    }
}

impl Feed for LocalFeed {
    fn media(&mut self, m: Media, now: Instant) {
        let notes = self.a.on_media(&m, now);
        self.keep(notes, now);
    }
    fn tick(&mut self, now: Instant) {
        let t = now.saturating_duration_since(self.t0).as_secs_f64();
        while let Some((lt, _)) = self.levels.peek() {
            if *lt > t {
                break;
            }
            let (lt, l) = self.levels.next().unwrap();
            let at = self.t0 + Duration::from_secs_f64(lt.max(0.0));
            let notes = self.a.set_levels(l, at);
            self.keep(notes, at);
        }
        let notes = self.a.tick(now);
        self.keep(notes, now);
    }
    fn warn(&mut self, msg: String, now: Instant) {
        self.keep(vec![Note { level: Level::Warn, kind: "file", msg }], now);
    }
}

/// One extra track: its packets (when the lab reads its codec) and its loudness.
fn track_pass(path: &Path, packets: Option<usize>, levels: Vec<(f64, Levels)>, t0: Instant) -> TrackResult {
    let mut feed = LocalFeed { t0, a: Analyzer::new(t0), levels: levels.into_iter().peekable(), notes: vec![] };
    match packets {
        Some(i) => {
            flv_pass(path, &[i], t0, &mut feed);
        }
        None => {
            // loudness only
            let end = feed.levels.clone().last().map_or(0.0, |l| l.0);
            feed.tick(t0 + Duration::from_secs_f64(end + 1.0));
        }
    }
    TrackResult { stats: feed.a.stats, notes: feed.notes }
}

/// The streams `maps` of the file copied out by ffmpeg as FLV and read tag by tag, each at its
/// moment on the file's clock (`t0` is the file's start), with a tick every second of it.
fn flv_pass(path: &Path, maps: &[usize], t0: Instant, feed: &mut dyn Feed) -> PassSummary {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"]).arg(path);
    for m in maps {
        cmd.args(["-map", &format!("0:{m}")]);
    }
    cmd.args(["-c", "copy", "-f", "flv", "pipe:1"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return PassSummary { last: None, error: Some(format!("ffmpeg didn't start: {e}")), warnings: vec![] },
    };
    let mut stderr = child.stderr.take().unwrap();
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut r = std::io::BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
    let mut demux = FlvDemux::new();
    let mut last = t0;
    let mut next_tick = t0 + Duration::from_secs(1);
    let mut base: Option<u32> = None;
    let mut stopped = false;
    let mut n = 0u64;
    read_flv(&mut r, |kind, ts, body| {
        n += 1;
        if n % 256 == 0 && feed.stop() {
            stopped = true;
            return false;
        }
        let b = *base.get_or_insert(ts);
        let at = t0 + Duration::from_millis(ts.saturating_sub(b) as u64);
        let now = at.max(last);
        last = now;
        while next_tick <= now {
            feed.tick(next_tick);
            next_tick += Duration::from_secs(1);
        }
        let mut out = vec![];
        let mut warn = vec![];
        match kind {
            8 => demux.audio(ts, body, &mut out, &mut warn),
            9 => demux.video(ts, body, &mut out, &mut warn),
            _ => {}
        }
        for w in warn {
            feed.warn(w, now);
        }
        for m in out {
            feed.media(m, now);
        }
        true
    });
    if stopped {
        let _ = child.kill();
    }
    let ok = child.wait().is_ok_and(|s| s.success());
    feed.tick(last);
    let errors = errs.join().unwrap_or_default();
    let said: Vec<String> = errors.lines().filter(|l| !l.trim().is_empty()).take(20).map(str::to_string).collect();
    if ok || stopped {
        return PassSummary { last: Some(last), error: None, warnings: said };
    }
    PassSummary { last: Some(last), error: Some(said.first().cloned().unwrap_or_else(|| "ffmpeg failed".into())), warnings: vec![] }
}

/// The tags of an FLV byte stream: (tag type, timestamp in ms, body), until the end or until
/// `on` says stop.
fn read_flv(r: &mut impl Read, mut on: impl FnMut(u8, u32, &[u8]) -> bool) {
    let mut head = [0u8; 9];
    if r.read_exact(&mut head).is_err() || &head[..3] != b"FLV" {
        return;
    }
    let skip = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize;
    if skip > 9 {
        let mut rest = vec![0u8; skip - 9];
        if r.read_exact(&mut rest).is_err() {
            return;
        }
    }
    let mut body = vec![];
    loop {
        // the previous tag's size, then the tag's header
        let mut h = [0u8; 15];
        if r.read_exact(&mut h).is_err() {
            return;
        }
        let kind = h[4] & 0x1f;
        let size = u32::from_be_bytes([0, h[5], h[6], h[7]]) as usize;
        let ts = u32::from_be_bytes([h[11], h[8], h[9], h[10]]);
        body.resize(size, 0);
        if r.read_exact(&mut body).is_err() {
            return;
        }
        if !on(kind, ts, &body) {
            return;
        }
    }
}
