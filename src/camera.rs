//! Reading a camera on this Mac (`camera://Lightair Virtual Camera`), a virtual camera above
//! all: ffmpeg captures it (AVFoundation) and describes every frame (its time, size, pixel
//! format, mean colours and a checksum), which the lab measures like a stream's pictures: size,
//! frame rate, timing, and whether the picture is the expected one (not black, moving). It also
//! makes the thumbnails. The session ends when the camera stops giving frames, or at `end`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};

use crate::analyze::Level;
use crate::hub::{Hub, Proto, Session};
use crate::media::Media;
use crate::picture::{Measure, PictureCheck, from_yuv};

/// No frame for this long ends the session.
const SILENT_END: Duration = Duration::from_secs(10);

/// The cameras ffmpeg sees (screens left out: the lab doesn't record screens).
pub fn list() -> Result<Vec<String>, String> {
    if !cfg!(target_os = "macos") {
        return Err("cameras are read on macOS only".into());
    }
    if !crate::helper::ffmpeg_available() {
        return Err("listing cameras needs ffmpeg on the PATH".into());
    }
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg didn't start: {e}"))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let mut video = false;
    let mut names = vec![];
    for line in text.lines() {
        if line.contains("AVFoundation video devices") {
            video = true;
        } else if line.contains("AVFoundation audio devices") {
            video = false;
        } else if video {
            // "[AVFoundation indev @ 0x…] [1] Lightair Virtual Camera"
            if let Some(name) = line.rsplit_once("] [").and_then(|(_, r)| r.split_once("] ")).map(|(_, n)| n.trim()) {
                if !name.starts_with("Capture screen") {
                    names.push(name.to_string());
                }
            }
        }
    }
    Ok(names)
}

/// "%20" and friends in a URL's name.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One frame line of ffmpeg's showinfo.
#[derive(Debug, Default)]
struct Info {
    /// The first showinfo (the camera's own frame); the other is the measured small copy.
    original: bool,
    pts_time: f64,
    fmt: String,
    size: (u32, u32),
    mean: [f64; 3],
    stdev: f64,
    checksum: u64,
}

fn parse_info(line: &str) -> Option<Info> {
    let at = line.find("[Parsed_showinfo_")?;
    if !line.contains(" n:") || !line.contains("pts_time:") {
        return None;
    }
    let original = line[at..].starts_with("[Parsed_showinfo_0 ");
    let field = |k: &str| -> Option<&str> {
        let i = line.find(&format!(" {k}:"))? + k.len() + 2;
        let rest = line[i..].trim_start();
        Some(rest.split_whitespace().next().unwrap_or(""))
    };
    let list = |k: &str| -> Vec<f64> {
        line.find(&format!(" {k}:["))
            .and_then(|i| line[i..].split_once('[').and_then(|(_, r)| r.split_once(']')))
            .map_or(vec![], |(inner, _)| inner.split_whitespace().filter_map(|x| x.parse().ok()).collect())
    };
    let size = field("s").and_then(|s| s.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((0, 0));
    let mean = list("mean");
    Some(Info {
        original,
        pts_time: field("pts_time")?.parse().ok()?,
        fmt: field("fmt").unwrap_or("").to_string(),
        size,
        mean: [mean.first().copied().unwrap_or(0.0), mean.get(1).copied().unwrap_or(128.0), mean.get(2).copied().unwrap_or(128.0)],
        stdev: list("stdev").first().copied().unwrap_or(0.0),
        checksum: field("checksum").and_then(|c| u64::from_str_radix(c, 16).ok()).unwrap_or(0),
    })
}

/// The modes a camera offers, from ffmpeg's answer to one it hasn't: "Supported modes:" then
/// lines like "[avfoundation @ 0x…]   1920x1080@[60.000000 60.000000]fps": (size, lowest and
/// highest frame rate).
pub fn modes(text: &str) -> Vec<((u32, u32), f64, f64)> {
    let mut out = vec![];
    for line in text.lines() {
        let Some((before, after)) = line.split_once("@[") else { continue };
        let Some(size) = before.split_whitespace().last() else { continue };
        let Some((w, h)) = size.split_once('x') else { continue };
        let Some((rates, _)) = after.split_once(']') else { continue };
        let r: Vec<f64> = rates.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if let (Ok(w), Ok(h), Some(&lo), Some(&hi)) = (w.parse(), h.parse(), r.first(), r.last()) {
            out.push(((w, h), lo.min(hi), hi.max(lo)));
        }
    }
    out
}

/// The offered mode to retry with, and the frame rate to ask for: the size asked for at the
/// rate nearest the one asked for (within the mode's range when it can be), else the nearest
/// size; without a size, the largest mode that does the rate, else the one nearest it.
pub fn pick_mode(offered: &[((u32, u32), f64, f64)], size: Option<(u32, u32)>, fps: f64) -> Option<((u32, u32), f64)> {
    let off = |m: &((u32, u32), f64, f64)| {
        if fps < m.1 {
            m.1 - fps
        } else if fps > m.2 {
            fps - m.2
        } else {
            0.0
        }
    };
    let area = |s: (u32, u32)| s.0 as i64 * s.1 as i64;
    let pick = match size {
        Some(want) if offered.iter().any(|m| m.0 == want) => {
            offered.iter().filter(|m| m.0 == want).min_by(|a, b| off(a).total_cmp(&off(b)))
        }
        Some(want) => offered.iter().min_by(|a, b| {
            let (da, db) = ((area(a.0) - area(want)).abs(), (area(b.0) - area(want)).abs());
            da.cmp(&db).then(off(a).total_cmp(&off(b)))
        }),
        None => offered.iter().min_by(|a, b| off(a).total_cmp(&off(b)).then(area(b.0).cmp(&area(a.0)))),
    }?;
    Some((pick.0, fps.clamp(pick.1, pick.2)))
}

/// Whether ffmpeg's failure reads as macOS refusing this process the camera: AVFoundation then
/// fails every device with "Input/output error" before any mode is looked at (a mode it hasn't
/// lists the modes instead).
pub fn camera_denied(said: &[String]) -> bool {
    let text = said.join("\n").to_ascii_lowercase();
    if !modes(&text).is_empty() || text.contains("not supported") {
        return false;
    }
    text.contains("not authorized")
        || text.contains("not permitted")
        || text.contains("failed to create av capture input device")
        || text.contains("input/output error")
}

/// What to say when the camera didn't start, from what ffmpeg said.
pub fn start_failure(said: &[String], tried_modes: bool) -> String {
    let why = said
        .iter()
        .rev()
        .find(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("not supported") || l.contains("cannot") || l.contains("failed") || l.contains("no frame")
        })
        .cloned()
        .unwrap_or_else(|| said.last().cloned().unwrap_or_default());
    if !tried_modes && camera_denied(said) {
        return format!(
            "this app isn't allowed to use cameras: System Settings › Privacy & Security › Camera (ffmpeg said: {})",
            why.trim()
        );
    }
    format!("the camera didn't start: {}", why.trim())
}

fn spawn(name: &str, fps: Option<f64>, size: Option<(u32, u32)>, thumbs: &std::path::Path) -> std::io::Result<Child> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-loglevel", "info", "-f", "avfoundation"]);
    if let Some(f) = fps {
        cmd.args(["-framerate", &format!("{f}")]);
    }
    if let Some((w, h)) = size {
        cmd.args(["-video_size", &format!("{w}x{h}")]);
    }
    cmd.args(["-i", &format!("{name}:none")])
        .args([
            "-filter_complex",
            &format!(
                "[0:v]showinfo,split[a][b];[a]scale=160:-2,format=yuv420p,showinfo[m];[b]fps=1/{},scale=320:-2[t]",
                crate::helper::THUMB_EVERY_S
            ),
            "-map",
            "[m]",
            "-f",
            "null",
            "-",
            "-map",
            "[t]",
            "-q:v",
            "6",
            "-f",
            "image2",
            "-start_number",
            "0",
        ])
        .arg(thumbs.join("s01_%05d.jpg"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// What ffmpeg said before the first frame, and the first frame line.
struct Started {
    child: Child,
    lines: tokio::io::Lines<BufReader<ChildStderr>>,
    said: Vec<String>,
    first: Info,
}

async fn try_start(name: &str, fps: Option<f64>, size: Option<(u32, u32)>, thumbs: &std::path::Path) -> Result<Started, Vec<String>> {
    let mut child = spawn(name, fps, size, thumbs).map_err(|e| vec![format!("ffmpeg didn't start: {e}")])?;
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut said = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Err(_) => {
                said.push("no frame from the camera in 8 s".into());
                return Err(said);
            }
            Ok(Ok(Some(l))) => {
                if let Some(info) = parse_info(&l) {
                    if info.original {
                        return Ok(Started { child, lines, said, first: info });
                    }
                    continue;
                }
                if said.len() < 200 {
                    said.push(l);
                }
            }
            Ok(_) => return Err(said),
        }
    }
}

/// Starts capturing a camera by name; the session starts with its first frame.
pub async fn start(
    hub: &Arc<Hub>,
    name: &str,
    query: &HashMap<String, String>,
    key: Option<String>,
    profile: &str,
) -> Result<Arc<Session>, String> {
    let names = list()?;
    if !names.iter().any(|n| n == name) {
        return Err(format!("no camera named '{name}'; there are: {}", names.join(", ")));
    }
    let mut fps: Option<f64> = query.get("fps").and_then(|f| f.parse().ok());
    let mut size: Option<(u32, u32)> =
        query.get("size").and_then(|s| s.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
    let name_key = key.unwrap_or_else(|| name.to_string());
    // the thumbnails go to the session's folder, which exists only once the session does: a
    // scratch folder until then
    let scratch = std::env::temp_dir().join(format!("stream-check-camera-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let mut tried_modes = false;
    let started = loop {
        match try_start(name, fps.or(Some(30.0)), size, &scratch).await {
            Ok(s) => break s,
            Err(said) => {
                let offered = modes(&said.join("\n"));
                if !tried_modes {
                    if let Some((pick, rate)) = pick_mode(&offered, size, fps.unwrap_or(30.0)) {
                        // the camera doesn't do that mode: take the nearest one it offers
                        tried_modes = true;
                        size = Some(pick);
                        fps = Some(rate);
                        continue;
                    }
                }
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(start_failure(&said, tried_modes));
            }
        }
    };
    let remote = SocketAddr::from(([127, 0, 0, 1], 0));
    let publisher = match hub.authorize_pull(Proto::Camera, profile, &name_key, remote) {
        Ok(p) => p,
        Err(r) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(r.msg);
        }
    };
    let session = publisher.session.clone();
    let declared = started.said.iter().find(|l| l.contains("Stream #0:0") && l.contains("Video:")).cloned();
    let declared_fps = declared.as_deref().and_then(|l| {
        let i = l.find(" fps")?;
        l[..i].rsplit([' ', ',']).next()?.parse::<f64>().ok()
    });
    session.set_source(json!({
        "kind": "camera",
        "name": name,
        "asked": { "fps": fps.or(Some(30.0)), "size": size.map(|(w, h)| format!("{w}x{h}")) },
        "ffmpeg_stream": declared.as_deref().map(|l| l.trim().to_string()),
        "declared_fps": declared_fps,
    }));
    tokio::spawn(run(publisher, started, scratch, declared_fps));
    Ok(session)
}

async fn run(p: crate::hub::Publisher, mut st: Started, scratch: std::path::PathBuf, declared_fps: Option<f64>) {
    let s = p.session.clone();
    let mut check = PictureCheck::default();
    let mut first_pts: Option<f64> = None;
    let mut base_us = 0i64;
    let mut size = (0u32, 0u32);
    let mut last_said = String::new();
    let mut pending = Some(st.first);
    let mut last_update = std::time::Instant::now();
    let thumbs_moved = std::sync::atomic::AtomicBool::new(false);
    let reason = loop {
        let info = match pending.take() {
            Some(i) => i,
            None => {
                let next = tokio::select! {
                    _ = p.killed() => break s.stop_reason(),
                    l = tokio::time::timeout(SILENT_END, st.lines.next_line()) => l,
                };
                match next {
                    Err(_) => break format!("no frame from the camera for {} s", SILENT_END.as_secs()),
                    Ok(Ok(Some(l))) => match parse_info(&l) {
                        Some(i) => i,
                        None => {
                            if !l.trim().is_empty() && !l.starts_with(' ') {
                                last_said = l;
                            }
                            continue;
                        }
                    },
                    Ok(_) => {
                        break format!("the camera stopped ({})", if last_said.is_empty() { "ffmpeg ended" } else { last_said.trim() });
                    }
                }
            }
        };
        let now = std::time::Instant::now();
        if info.original {
            if info.size != size {
                size = info.size;
                let fmt = info.fmt.clone();
                s.with_stats(|st| {
                    st.has_video = true;
                    st.video.sps = Some(crate::nal::Sps {
                        profile: format!("{fmt} (uncompressed)"),
                        width: size.0,
                        height: size.1,
                        declared_fps,
                        ..Default::default()
                    });
                });
                s.event(Level::Info, "video-config", format!("the camera gives {}x{} {fmt}", size.0, size.1));
            }
            let first = *first_pts.get_or_insert_with(|| {
                base_us = s.t0.elapsed().as_micros() as i64;
                info.pts_time
            });
            let pts = base_us + ((info.pts_time - first) * 1e6) as i64;
            p.media(Media::Video { pts_us: pts, dts_us: pts, key: false, data: Arc::new(vec![]) });
        } else {
            let (rgb, luma) = from_yuv(info.mean[0], info.mean[1], info.mean[2]);
            let m = Measure { luma, luma_stdev: info.stdev * 255.0 / 219.0, rgb, sig: info.checksum };
            let t = s.t0.elapsed().as_secs_f64();
            for n in check.frame(t, &m) {
                s.event(n.level, n.kind, n.msg);
            }
        }
        // the thumbnails were started before the session had a folder: move them over
        if !thumbs_moved.load(std::sync::atomic::Ordering::Relaxed) || last_update.elapsed() >= Duration::from_millis(500) {
            thumbs_moved.store(true, std::sync::atomic::Ordering::Relaxed);
            move_thumbs(&scratch, &s.dir.join("thumbs"));
        }
        if last_update.elapsed() >= Duration::from_millis(500) || now == last_update {
            last_update = now;
            let stats = check.stats.clone();
            s.update_source(|v| v["picture"] = json!(stats));
        }
    };
    let _ = st.child.kill().await;
    move_thumbs(&scratch, &s.dir.join("thumbs"));
    let _ = std::fs::remove_dir_all(&scratch);
    finish_picture(&s, &check);
    p.end(&reason);
}

/// The picture's final numbers and its problems into the report.
pub fn finish_picture(s: &Session, check: &PictureCheck) {
    let stats = check.stats.clone();
    let frozen = check.never_changed();
    s.update_source(|v| {
        v["picture"] = json!(stats);
        v["picture_never_changed"] = json!(frozen);
    });
    if frozen {
        s.event(Level::Warn, "picture-frozen", "every frame was the same picture");
    }
    for p in check.problems() {
        s.add_problem(p);
    }
}

fn move_thumbs(from: &std::path::Path, to: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(from) else { return };
    for e in rd.flatten() {
        let _ = std::fs::rename(e.path(), to.join(e.file_name()));
    }
}

/// The source description of a camera that couldn't be listed, for the API.
pub fn describe_error(e: &str) -> Value {
    json!({ "available": false, "error": e, "sources": [] })
}
