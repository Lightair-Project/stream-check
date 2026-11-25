//! Fixtures shared by the tests: ffmpeg-made media, FLV tags, SEI messages as a sender writes
//! them, and a lab whose sessions are fed on their own clock.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use stream_check::hub::{Config, Hub};

pub fn have_ffmpeg() -> bool {
    stream_check::helper::ffmpeg_available()
}

/// Runs ffmpeg quietly with these arguments; whether it succeeded.
pub fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// ffprobe's JSON for a file (streams, and whatever `extra` asks for).
pub fn ffprobe(path: &Path, extra: &[&str]) -> serde_json::Value {
    let out =
        Command::new("ffprobe").args(["-v", "error", "-show_streams", "-of", "json"]).args(extra).arg(path).output().expect("ffprobe runs");
    serde_json::from_slice(&out.stdout).expect("ffprobe's JSON")
}

/// One FLV tag: 8 audio, 9 video, 18 script data.
pub struct Tag {
    pub kind: u8,
    pub ts: u32,
    pub body: Vec<u8>,
}

/// The tags of an FLV file.
pub fn flv_tags(path: &Path) -> Vec<Tag> {
    let d = std::fs::read(path).expect("the FLV file");
    assert_eq!(&d[..3], b"FLV");
    let mut i = u32::from_be_bytes([d[5], d[6], d[7], d[8]]) as usize + 4;
    let mut out = vec![];
    while i + 11 <= d.len() {
        let size = u32::from_be_bytes([0, d[i + 1], d[i + 2], d[i + 3]]) as usize;
        let ts = u32::from_be_bytes([d[i + 7], d[i + 4], d[i + 5], d[i + 6]]);
        let Some(body) = d.get(i + 11..i + 11 + size) else { break };
        out.push(Tag { kind: d[i] & 0x1f, ts, body: body.to_vec() });
        i += 11 + size + 4;
    }
    out
}

/// Emulation prevention: a 3 after two zeros wherever the next byte could start a start code.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// An H.264 SEI NAL unit holding one message of this payload type.
pub fn h264_sei(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut rbsp = vec![kind];
    let mut n = payload.len();
    while n >= 255 {
        rbsp.push(255);
        n -= 255;
    }
    rbsp.push(n as u8);
    rbsp.extend(payload);
    rbsp.push(0x80);
    let mut nal = vec![0x06];
    nal.extend(escape(&rbsp));
    nal
}

/// The time every BPM message here carries: one RFC 3339 time of event 4 (packet interleave).
fn bpm_time(out: &mut Vec<u8>, time: &str) {
    out.extend([0x00, 1, 4]);
    out.extend(time.as_bytes());
    out.push(0);
}

/// A BPM timestamp message (user data unregistered payload).
pub fn bpm_ts(time: &str) -> Vec<u8> {
    let mut p = stream_check::sei::UUID_BPM_TS.to_vec();
    bpm_time(&mut p, time);
    p
}

/// A BPM session metrics message: rendered, lagged, dropped, output.
pub fn bpm_sm(time: &str, counts: [u32; 4]) -> Vec<u8> {
    let mut p = stream_check::sei::UUID_BPM_SM.to_vec();
    bpm_time(&mut p, time);
    p.push(3);
    for (tag, v) in counts.iter().enumerate() {
        p.push(tag as u8 + 1);
        p.extend(v.to_be_bytes());
    }
    p
}

/// A BPM encoded rendition metrics message: input, skipped, output.
pub fn bpm_erm(time: &str, counts: [u32; 3]) -> Vec<u8> {
    let mut p = stream_check::sei::UUID_BPM_ERM.to_vec();
    bpm_time(&mut p, time);
    p.push(2);
    for (tag, v) in counts.iter().enumerate() {
        p.push(tag as u8 + 1);
        p.extend(v.to_be_bytes());
    }
    p
}

/// The three BPM SEI NAL units a sender puts in front of a keyframe, 4-byte length-prefixed.
pub fn bpm_nals(frames_since_key: u32) -> Vec<u8> {
    let time = "2026-11-26T12:00:00.000Z";
    let mut out = vec![];
    for payload in
        [bpm_ts(time), bpm_sm(time, [frames_since_key, 0, 0, frames_since_key]), bpm_erm(time, [frames_since_key, 0, frames_since_key])]
    {
        let nal = h264_sei(5, &payload);
        out.extend((nal.len() as u32).to_be_bytes());
        out.extend(nal);
    }
    out
}

/// A lab without listeners, for sessions fed by the test itself.
pub fn hub(dir: &Path) -> Arc<Hub> {
    let mut cfg = Config::for_test(dir.to_path_buf());
    // no audio helper: nothing decodes on the wall clock behind the test's back
    cfg.ffmpeg = false;
    Hub::new(cfg)
}

/// Waits (up to `secs`) for something the lab does on its own tasks.
pub async fn until(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
