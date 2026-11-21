//! ffmpeg as the lab's decoder: the picture and sound of a connection go to it as Matroska, and
//! it gives back thumbnails (a JPEG every few seconds) and the sound as PCM, which is measured
//! here (EBU R128 loudness, peaks, silence). Without ffmpeg the lab still ingests, records and
//! measures timing; it just has no thumbnails or levels.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use ebur128::{EbuR128, Mode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::analyze::Levels;
use crate::hub::Session;

pub const THUMB_EVERY_S: f64 = 2.0;

pub fn ffmpeg_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()))
}

fn db(x: f64) -> f64 {
    if x <= 1e-6 { -120.0 } else { 20.0 * x.log10() }
}

fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

/// Starts the helper for one connection's segment; the sender takes the Matroska bytes.
pub fn spawn(session: Arc<Session>, seg: u32, dir: PathBuf) -> mpsc::Sender<Vec<u8>> {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(512);
    tokio::spawn(async move {
        let thumbs = dir.join("thumbs").join(format!("s{seg:02}_%05d.jpg"));
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-fflags",
            "+nobuffer",
            "-probesize",
            "65536",
            "-analyzeduration",
            "0",
            "-f",
            "matroska",
            "-i",
            "pipe:0",
        ])
        .args(["-map", "0:a:0?", "-ac", "2", "-ar", "48000", "-f", "f32le", "pipe:1"])
        .args([
            "-map",
            "0:v:0?",
            "-an",
            "-vf",
            &format!("fps=1/{THUMB_EVERY_S},scale=320:-2"),
            "-q:v",
            "6",
            "-f",
            "image2",
            "-start_number",
            "0",
        ])
        .arg(&thumbs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                session.event(crate::analyze::Level::Warn, "helper", format!("ffmpeg didn't start: {e}"));
                return;
            }
        };
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let writer = tokio::spawn(async move {
            let mut wrote = false;
            while let Some(b) = rx.recv().await {
                wrote = true;
                if stdin.write_all(&b).await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
            wrote
        });
        let errs = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s).await;
            s
        });
        let mut meter = EbuR128::new(2, 48000, Mode::M | Mode::S | Mode::I).ok();
        let mut quiet_chunks = 0u32;
        let mut peak = [0f64; 2];
        let mut since_emit = 0;
        let mut buf = vec![0u8; 4800 * 2 * 4];
        let mut fill = 0;
        loop {
            match stdout.read(&mut buf[fill..]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => fill += n,
            }
            if fill < buf.len() {
                continue;
            }
            fill = 0;
            let samples: Vec<f32> = buf.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            let mut sumsq = 0f64;
            for (i, s) in samples.iter().enumerate() {
                peak[i % 2] = peak[i % 2].max(s.abs() as f64);
                sumsq += (*s as f64) * (*s as f64);
            }
            let rms = db((sumsq / samples.len() as f64).sqrt());
            quiet_chunks = if rms < -60.0 { quiet_chunks + 1 } else { 0 };
            if let Some(m) = &mut meter {
                let _ = m.add_frames_f32(&samples);
            }
            since_emit += 1;
            if since_emit >= 3 {
                since_emit = 0;
                let l = Levels {
                    available: true,
                    momentary_lufs: meter.as_ref().and_then(|m| m.loudness_momentary().ok()).and_then(finite),
                    short_term_lufs: meter.as_ref().and_then(|m| m.loudness_shortterm().ok()).and_then(finite),
                    integrated_lufs: meter.as_ref().and_then(|m| m.loudness_global().ok()).and_then(finite),
                    peak_l_db: Some(db(peak[0])),
                    peak_r_db: Some(db(peak[1])),
                    rms_db: Some(rms),
                    silent: quiet_chunks >= 20,
                    ..Default::default()
                };
                peak = [0.0; 2];
                session.set_levels(l);
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        let wrote = writer.await.unwrap_or(false);
        let errors = errs.await.unwrap_or_default();
        // nothing was ever sent (a connection that gave no media): nothing to complain about
        let first = errors.lines().find(|l| wrote && !l.trim().is_empty());
        if let Some(first) = first {
            session.event(crate::analyze::Level::Info, "helper", format!("ffmpeg said: {first}"));
        }
    });
    tx
}

/// What ffprobe makes of a recording: the proof that it plays.
pub fn probe(path: &Path) -> Option<Value> {
    if std::fs::metadata(path).map_or(true, |m| m.len() == 0) {
        return None;
    }
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let streams: Vec<Value> = v["streams"]
        .as_array()?
        .iter()
        .map(|s| {
            json!({
                "type": s["codec_type"],
                "codec": s["codec_name"],
                "width": s["width"],
                "height": s["height"],
                "sample_rate": s["sample_rate"],
                "channels": s["channels"],
                "duration_s": s["duration"].as_str().and_then(|d| d.parse::<f64>().ok()),
            })
        })
        .collect();
    if streams.is_empty() {
        return None;
    }
    Some(json!({
        "format": v["format"]["format_name"],
        "duration_s": v["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok()),
        "streams": streams,
    }))
}
