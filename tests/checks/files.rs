//! check-file: a recording read through the live analysis on its own clock. The MP4 box layout
//! (progressive, fragmented, cut off), every sound track measured on its own, chapters, and a
//! platform's rules held against the file. Fixtures are made by ffmpeg; skipped without it.

use std::path::{Path, PathBuf};

use serde_json::Value;
use stream_check::file_check::{Request, ended, start};

use crate::support::{ffmpeg, have_ffmpeg, hub};

const PICTURE: [&str; 4] = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30"];
const TONE: [&str; 4] = ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"];
const H264_AAC: [&str; 12] = ["-c:v", "libx264", "-preset", "ultrafast", "-g", "30", "-bf", "0", "-c:a", "aac", "-ar", "48000"];

fn make(dir: &Path, name: &str, args: &[&str]) -> PathBuf {
    let f = dir.join(name);
    let mut all: Vec<&str> = args.to_vec();
    all.push(f.to_str().unwrap());
    assert!(ffmpeg(&all), "ffmpeg made {name}");
    f
}

async fn check(path: &Path, profile: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path());
    let s = start(&hub, Request { path: path.to_path_buf(), profile: profile.into(), key: None }).expect("the check starts");
    tokio::time::timeout(std::time::Duration::from_secs(60), ended(&s)).await.expect("the check ends");
    s.inner.lock().report.clone().unwrap()
}

fn problems(r: &Value) -> Vec<String> {
    r["problems"].as_array().unwrap().iter().filter_map(|p| p.as_str().map(str::to_string)).collect()
}

/// Two seconds of picture and sound as an MP4, with these `-movflags` (none: the index last).
fn mp4(dir: &Path, name: &str, movflags: Option<&str>) -> PathBuf {
    let mut args = PICTURE.to_vec();
    args.extend(TONE);
    args.extend(["-t", "2"]);
    args.extend(H264_AAC);
    if let Some(m) = movflags {
        args.extend(["-movflags", m]);
    }
    args.extend(["-f", "mp4"]);
    make(dir, name, &args)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_mp4_with_its_index_first_is_progressive_and_passes() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = mp4(dir.path(), "progressive.mp4", Some("+faststart"));
    let r = check(&f, "open").await;
    assert_eq!(r["ok"], true, "{:?}", problems(&r));
    let c = &r["source"]["container"];
    assert_eq!(c["layout"], "progressive", "{c}");
    assert_eq!(c["moov_before_mdat"], true, "{c}");
    let v = &r["stats"]["video"];
    assert_eq!((v["codec"].as_str(), v["sps"]["width"].as_u64()), (Some("h264"), Some(320)));
    assert!((v["keyint_avg_s"].as_f64().unwrap() - 1.0).abs() < 0.05, "{}", v["keyint_avg_s"]);
    assert_eq!(r["source"]["audio_tracks"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fragmented_mp4_is_read_as_fragmented() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = mp4(dir.path(), "fragmented.mp4", Some("frag_keyframe+empty_moov+default_base_moof"));
    let r = check(&f, "open").await;
    assert_eq!(r["ok"], true, "{:?}", problems(&r));
    let c = &r["source"]["container"];
    assert_eq!(c["layout"], "fragmented", "{c}");
    assert!(c["fragments"].as_u64() >= Some(2), "{c}");
    assert_eq!(r["stats"]["video"]["keyframes"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mp4_cut_off_before_its_index_is_an_unfinished_recording() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // the index (moov) at the end, as a recorder writes it when it stops; then cut in the middle
    let whole = mp4(dir.path(), "whole.mp4", None);
    let bytes = std::fs::read(&whole).unwrap();
    let cut = dir.path().join("cut.mp4");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    let r = check(&cut, "open").await;
    assert_eq!(r["ok"], false);
    let c = &r["source"]["container"];
    assert_eq!(c["layout"], "unfinished", "{c}");
    let p = problems(&r);
    assert!(p.iter().any(|p| p.contains("cut off")), "{p:?}");
    assert!(p.iter().any(|p| p.contains("no moov")), "{p:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_sound_track_of_a_file_is_measured_on_its_own() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // a loud tone and the same tone 20 dB quieter, each its own track
    let mut args = PICTURE.to_vec();
    args.extend(TONE);
    args.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]);
    args.extend(["-t", "3", "-map", "0:v", "-map", "1:a", "-map", "2:a", "-filter:a:1", "volume=-20dB"]);
    args.extend(H264_AAC);
    args.extend(["-f", "mp4"]);
    let f = make(dir.path(), "two-tracks.mp4", &args);
    let r = check(&f, "open").await;
    let tracks = r["source"]["audio_tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2, "{}", r["source"]["audio_tracks"]);
    let lufs: Vec<f64> = tracks.iter().map(|t| t["levels"]["integrated_lufs"].as_f64().expect("loudness of each track")).collect();
    assert!((lufs[0] - lufs[1] - 20.0).abs() < 1.5, "{lufs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_files_chapters_are_listed() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let meta = dir.path().join("chapters.txt");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=Intro\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Main\n",
    )
    .unwrap();
    let mut args = PICTURE.to_vec();
    args.extend(TONE);
    args.extend(["-f", "ffmetadata", "-i", meta.to_str().unwrap(), "-map", "0:v", "-map", "1:a", "-map_chapters", "2", "-t", "2"]);
    args.extend(H264_AAC);
    args.extend(["-f", "mp4"]);
    let f = make(dir.path(), "chapters.mp4", &args);
    let r = check(&f, "open").await;
    let titles: Vec<&str> = r["source"]["chapters"].as_array().unwrap().iter().filter_map(|c| c["title"].as_str()).collect();
    assert_eq!(titles, vec!["Intro", "Main"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_is_held_to_its_profiles_rules_on_its_own_clock() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // ten seconds with a keyframe every 5 s, read in far less than ten seconds
    let mut args = PICTURE.to_vec();
    args.extend(TONE);
    args.extend(["-t", "10", "-c:v", "libx264", "-preset", "ultrafast", "-g", "150", "-keyint_min", "150", "-sc_threshold", "0"]);
    args.extend(["-bf", "0", "-c:a", "aac", "-ar", "48000", "-f", "mp4"]);
    let f = make(dir.path(), "slow-keyframes.mp4", &args);
    let r = check(&f, "twitch").await;
    assert!(problems(&r).iter().any(|p| p.contains("keyframe-interval")), "{:?}", problems(&r));
    assert!((r["duration_s"].as_f64().unwrap() - 10.0).abs() < 0.2, "the file's own duration: {}", r["duration_s"]);
}
