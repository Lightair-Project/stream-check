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

/// Measures sound as it comes: EBU R128 loudness, peaks per channel, and silence. It takes
/// interleaved samples and gives [`Levels`] every 300 ms of sound.
pub struct Meter {
    meter: Option<EbuR128>,
    channels: usize,
    /// Samples (all channels) in 100 ms.
    chunk: usize,
    pending: Vec<f32>,
    quiet_chunks: u32,
    peak: [f64; 2],
    since_emit: u32,
}

impl Meter {
    pub fn new(channels: u32, rate: u32) -> Meter {
        let channels = channels.max(1);
        Meter {
            meter: EbuR128::new(channels, rate.max(8000), Mode::M | Mode::S | Mode::I).ok(),
            channels: channels as usize,
            chunk: (rate.max(8000) / 10) as usize * channels as usize,
            pending: vec![],
            quiet_chunks: 0,
            peak: [0.0; 2],
            since_emit: 0,
        }
    }

    /// Interleaved samples in; the levels of each 300 ms finished out.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Levels> {
        let mut out = vec![];
        self.pending.extend_from_slice(samples);
        let mut at = 0;
        while self.pending.len() - at >= self.chunk {
            let chunk: Vec<f32> = self.pending[at..at + self.chunk].to_vec();
            at += self.chunk;
            if let Some(l) = self.chunk_done(&chunk) {
                out.push(l);
            }
        }
        self.pending.drain(..at);
        out
    }

    fn chunk_done(&mut self, samples: &[f32]) -> Option<Levels> {
        let mut sumsq = 0f64;
        for (i, s) in samples.iter().enumerate() {
            // the first two channels are left and right; a mono sound counts as both
            let ch = if self.channels == 1 { 0 } else { i % self.channels };
            if ch < 2 {
                self.peak[ch] = self.peak[ch].max(s.abs() as f64);
            }
            sumsq += (*s as f64) * (*s as f64);
        }
        if self.channels == 1 {
            self.peak[1] = self.peak[0];
        }
        let rms = db((sumsq / samples.len().max(1) as f64).sqrt());
        self.quiet_chunks = if rms < -60.0 { self.quiet_chunks + 1 } else { 0 };
        if let Some(m) = &mut self.meter {
            let _ = m.add_frames_f32(samples);
        }
        self.since_emit += 1;
        if self.since_emit < 3 {
            return None;
        }
        self.since_emit = 0;
        let m = self.meter.as_ref();
        let l = Levels {
            available: true,
            momentary_lufs: m.and_then(|m| m.loudness_momentary().ok()).and_then(finite),
            short_term_lufs: m.and_then(|m| m.loudness_shortterm().ok()).and_then(finite),
            integrated_lufs: m.and_then(|m| m.loudness_global().ok()).and_then(finite),
            peak_l_db: Some(db(self.peak[0])),
            peak_r_db: Some(db(self.peak[1])),
            rms_db: Some(rms),
            silent: self.quiet_chunks >= 20,
            ..Default::default()
        };
        self.peak = [0.0; 2];
        Some(l)
    }
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
        let mut meter = Meter::new(2, 48000);
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
            for l in meter.push(&samples) {
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

/// What ffprobe makes of a recording: the proof that it plays. `Ok(None)`: nothing to check (no
/// file, or an empty one); `Err`: ffprobe couldn't read it, with ffprobe's own first error line.
pub fn probe(path: &Path) -> Result<Option<Value>, String> {
    if std::fs::metadata(path).map_or(true, |m| m.len() == 0) {
        return Ok(None);
    }
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe didn't start: {e}"))?;
    let first_error = || String::from_utf8_lossy(&out.stderr).lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string);
    if !out.status.success() {
        return Err(first_error().unwrap_or_else(|| format!("ffprobe failed ({})", out.status)));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe's answer couldn't be read: {e}"))?;
    let streams: Vec<Value> = v["streams"]
        .as_array()
        .into_iter()
        .flatten()
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
        return Err(first_error().unwrap_or_else(|| "ffprobe found no picture or sound in it".into()));
    }
    Ok(Some(json!({
        "format": v["format"]["format_name"],
        "duration_s": v["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok()),
        "streams": streams,
    })))
}
