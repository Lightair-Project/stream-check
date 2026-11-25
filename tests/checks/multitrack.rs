//! Twitch Dual Format over Enhanced RTMP multitrack: a landscape and a vertical picture in one
//! stream, as ffmpeg 8 writes it (track 0 as classic AVC tags, track 1 as Multitrack/OneTrack),
//! with or without the broadcast performance metrics a sender puts before every keyframe. The
//! tags go through the RTMP demuxer into a session on the stream's own clock, which ends with
//! its report.

use std::time::Duration;

use serde_json::Value;
use stream_check::hub::Proto;
use stream_check::rtmp::FlvDemux;

use crate::support::{Tag, bpm_nals, ffmpeg, flv_tags, have_ffmpeg, hub};

/// Six seconds of a 1280x720 and a 1080x1920 picture with AAC sound, keyframes every 2 s on
/// both, in one FLV. `None` when this ffmpeg can't write two pictures into FLV (before 8).
fn dual_format(dir: &std::path::Path, tracks: &[&str]) -> Option<Vec<Tag>> {
    if !have_ffmpeg() {
        return None;
    }
    let f = dir.join("dual.flv");
    let mut args = vec!["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"];
    args.extend(["-f", "lavfi", "-i", "testsrc2=size=1080x1920:rate=30"]);
    args.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "6"]);
    for t in tracks {
        args.extend(["-map", t]);
    }
    args.extend(["-c:v", "libx264", "-preset", "ultrafast", "-b:v", "1000k", "-maxrate", "1000k", "-bufsize", "1000k"]);
    args.extend(["-g", "60", "-keyint_min", "60", "-sc_threshold", "0", "-bf", "0", "-pix_fmt", "yuv420p"]);
    args.extend(["-c:a", "aac", "-ar", "48000", "-f", "flv", f.to_str().unwrap()]);
    ffmpeg(&args).then(|| flv_tags(&f))
}

/// Which video track a tag carries, and where its NAL units start if it is a keyframe's.
fn keyframe_nals_at(t: &Tag) -> Option<(u8, usize)> {
    let b = &t.body;
    if t.kind != 9 || b.len() < 8 {
        return None;
    }
    if b[0] & 0x80 == 0 {
        // classic AVC: key frame, NAL units, after the composition time
        return (b[0] == 0x17 && b[1] == 1).then_some((0, 5));
    }
    let (frame_type, packet_type) = ((b[0] >> 4) & 7, b[0] & 0x0f);
    if frame_type != 1 || packet_type != 6 {
        return None;
    }
    // Multitrack: OneTrack layout, its packet type, FourCC, track id, (composition time)
    let (layout, inner) = (b[1] >> 4, b[1] & 0x0f);
    assert_eq!(layout, 0, "ffmpeg writes the OneTrack layout");
    match inner {
        1 => Some((b[6], 10)),
        3 => Some((b[6], 7)),
        _ => None,
    }
}

/// The tags with BPM SEI put in front of every keyframe of the tracks named.
fn with_bpm(mut tags: Vec<Tag>, on: &[u8]) -> Vec<Tag> {
    let mut found = 0;
    for t in &mut tags {
        if let Some((_, at)) = keyframe_nals_at(t).filter(|(track, _)| on.contains(track)) {
            let since = if t.ts == 0 { 0 } else { 60 };
            t.body.splice(at..at, bpm_nals(since));
            found += 1;
        }
    }
    assert!(found >= on.len() * 3, "keyframes of every track named ({found})");
    tags
}

/// The tags fed into a session of `profile` on the stream's own clock (a tick every second of
/// it), then the session ended; its report.
async fn report(profile: &str, tags: &[Tag]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path());
    let p = hub.authorize(Proto::Rtmp, profile, "live_123456789_dualtest", "127.0.0.1:50000".parse().unwrap()).expect("authorized");
    let s = p.session.clone();
    s.set_virtual_clock(true);
    let t0 = s.t0;
    let at = |ms: u32| t0 + Duration::from_millis(ms as u64);
    let mut d = FlvDemux::new();
    let mut next = 1000;
    let mut last = 0;
    for t in tags {
        while t.ts >= next {
            s.tick_at(at(next));
            next += 1000;
        }
        let (mut out, mut warn) = (vec![], vec![]);
        match t.kind {
            9 => d.video(t.ts, &t.body, &mut out, &mut warn),
            8 => d.audio(t.ts, &t.body, &mut out, &mut warn),
            _ => {}
        }
        assert!(warn.is_empty(), "{warn:?}");
        for m in out {
            p.media_at(m, at(t.ts));
        }
        last = t.ts;
    }
    s.tick_at(at(last));
    assert!(last >= 5000, "the rules across tracks need 5 s of stream");
    hub.finalize(s.clone(), "the test ended".into()).await;
    drop(p);
    s.inner.lock().report.clone().expect("a report")
}

fn rules(r: &Value) -> Vec<String> {
    r["violations"].as_array().unwrap().iter().map(|v| v["rule"].as_str().unwrap().to_string()).collect()
}

fn track1(r: &Value) -> &Value {
    let t = &r["tracks"]["tracks"][0];
    assert_eq!(t["track"], 1, "{}", r["tracks"]);
    t
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dual_format_stream_with_bpm_before_every_keyframe_passes_twitch_dual() {
    let dir = tempfile::tempdir().unwrap();
    let Some(tags) = dual_format(dir.path(), &["0:v", "1:v", "2:a"]) else {
        eprintln!("no ffmpeg (8 or later) writing two pictures into FLV: skipped");
        return;
    };
    let r = report("twitch-dual", &with_bpm(tags, &[0, 1])).await;
    assert_eq!(r["ok"], true, "{}", r["problems"]);
    assert!(rules(&r).is_empty(), "{:?}", r["violations"]);
    // both pictures were read
    let v0 = &r["stats"]["video"];
    assert_eq!((v0["sps"]["width"].as_u64(), v0["sps"]["height"].as_u64()), (Some(1280), Some(720)));
    let v1 = &track1(&r)["video"];
    assert_eq!((v1["sps"]["width"].as_u64(), v1["sps"]["height"].as_u64()), (Some(1080), Some(1920)));
    assert_eq!(track1(&r)["keyframes_misaligned"], 0);
    // BPM in front of every keyframe, on each track
    for v in [v0, v1] {
        assert_eq!(v["bpm"]["every_keyframe"], true, "{}", v["bpm"]);
        assert!(v["bpm"]["keyframes_checked"].as_u64() >= Some(3));
    }
    // the vertical track is its own recording, and it plays
    let seg = r["segments"].as_array().unwrap().iter().find(|s| s["file"] == "seg01-track1.mkv").expect("the track's recording");
    let streams = seg["probe"]["streams"].as_array().expect("ffprobe read it");
    assert!(streams.iter().any(|s| s["codec"] == "h264" && s["width"] == 1080 && s["height"] == 1920), "{}", seg["probe"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_track_without_bpm_fails_twitch_dual_by_its_own_rule() {
    let dir = tempfile::tempdir().unwrap();
    let Some(tags) = dual_format(dir.path(), &["0:v", "1:v", "2:a"]) else {
        eprintln!("no ffmpeg (8 or later) writing two pictures into FLV: skipped");
        return;
    };
    let r = report("twitch-dual", &with_bpm(tags, &[0])).await;
    assert_eq!(r["ok"], false);
    let problems: Vec<&str> = r["problems"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(problems.iter().any(|p| p.contains("track-bpm-missing")), "{problems:?}");
    assert!(!rules(&r).contains(&"bpm-missing".to_string()), "track 0 had its BPM: {:?}", rules(&r));
    assert_eq!(track1(&r)["video"]["bpm"]["present"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn twitch_takes_a_stream_without_bpm_and_twitch_dual_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let Some(tags) = dual_format(dir.path(), &["0:v", "2:a"]) else {
        eprintln!("no ffmpeg: skipped");
        return;
    };
    let twitch = report("twitch", &tags).await;
    assert!(!rules(&twitch).iter().any(|r| r.contains("bpm")), "{:?}", rules(&twitch));
    assert_eq!(twitch["ok"], true, "{}", twitch["problems"]);
    let dual = report("twitch-dual", &tags).await;
    assert!(rules(&dual).contains(&"bpm-missing".to_string()), "{:?}", rules(&dual));
}
