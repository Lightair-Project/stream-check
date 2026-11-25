//! Level limits: a stream whose SPS names a level too low for what it carries (H.264 Table A-1,
//! HEVC Table A.8 and A.9). The numbers in these tests are the tables'.

use std::time::{Duration, Instant};

use stream_check::analyze::{Analyzer, Level};
use stream_check::levels::level_problems;
use stream_check::nal::{Sps, VCodec};
use stream_check::profiles::{self, find};
use stream_check::rtmp::FlvDemux;

use crate::support::{ffmpeg, flv_tags, have_ffmpeg};

fn h264(profile_idc: u8, level_idc: u8, width: u32, height: u32) -> Sps {
    Sps { profile: "test".into(), profile_idc, level: format!("{level_idc}"), level_idc, width, height, ..Default::default() }
}

fn hevc(level_idc: u8, high_tier: bool, width: u32, height: u32) -> Sps {
    Sps { profile: "Main".into(), profile_idc: 1, level: format!("{level_idc}"), level_idc, width, height, high_tier, ..Default::default() }
}

fn kinds(found: Vec<(&'static str, String)>) -> Vec<&'static str> {
    found.into_iter().map(|f| f.0).collect()
}

#[test]
fn h264_1080p_is_too_big_for_level_3() {
    // level 3.0 holds 1620 macroblocks a picture; 1080p is 8160
    assert_eq!(kinds(level_problems(VCodec::H264, &h264(100, 30, 1920, 1080), 30.0, 0.0)), vec!["size"]);
    assert!(level_problems(VCodec::H264, &h264(100, 40, 1920, 1080), 30.0, 0.0).is_empty());
}

#[test]
fn h264_720p60_is_too_fast_for_level_3_1_but_720p30_is_not() {
    // level 3.1: 108000 macroblocks a second; 720p is 3600 a picture
    assert_eq!(kinds(level_problems(VCodec::H264, &h264(100, 31, 1280, 720), 60.0, 0.0)), vec!["rate"]);
    assert!(level_problems(VCodec::H264, &h264(100, 31, 1280, 720), 30.0, 0.0).is_empty());
}

#[test]
fn a_measured_frame_rate_a_little_over_the_limit_is_not_flagged() {
    // 30 fps exactly fits level 3.1 at 720p; a measured 30.5 is wobble, not a breach
    assert!(level_problems(VCodec::H264, &h264(100, 31, 1280, 720), 30.5, 0.0).is_empty());
}

#[test]
fn h264_bitrate_limits_depend_on_the_profile() {
    // level 4.0's MaxBR is 20000: 24 Mbps of NAL stream for Main, 30 Mbps for High
    let main = h264(77, 40, 1920, 1080);
    let high = h264(100, 40, 1920, 1080);
    assert_eq!(kinds(level_problems(VCodec::H264, &main, 30.0, 28_000.0)), vec!["bitrate"]);
    assert!(level_problems(VCodec::H264, &high, 30.0, 28_000.0).is_empty());
    assert!(level_problems(VCodec::H264, &main, 30.0, 20_000.0).is_empty());
}

#[test]
fn h264_reference_frames_are_held_to_the_levels_picture_buffer() {
    // level 3.1 at 720p: 18000 macroblocks of buffer hold 5 pictures of 3600
    let mut sps = h264(100, 31, 1280, 720);
    sps.ref_frames = Some(6);
    assert_eq!(kinds(level_problems(VCodec::H264, &sps, 30.0, 0.0)), vec!["buffer"]);
    sps.ref_frames = Some(5);
    assert!(level_problems(VCodec::H264, &sps, 30.0, 0.0).is_empty());
}

#[test]
fn hevc_1080p60_needs_level_4_1() {
    // level 4: 66846720 luma samples a second; 1080p60 is 124416000; level 4.1 takes 133693440
    assert_eq!(kinds(level_problems(VCodec::Hevc, &hevc(120, false, 1920, 1080), 60.0, 0.0)), vec!["rate"]);
    assert!(level_problems(VCodec::Hevc, &hevc(123, false, 1920, 1080), 60.0, 0.0).is_empty());
}

#[test]
fn hevc_4k_is_too_big_for_level_4_1() {
    assert_eq!(kinds(level_problems(VCodec::Hevc, &hevc(123, false, 3840, 2160), 30.0, 0.0)), vec!["size"]);
}

#[test]
fn hevc_high_tier_takes_a_higher_bitrate_than_main_tier() {
    // level 4.1: 20 Mbps Main tier, 50 Mbps High tier
    assert_eq!(kinds(level_problems(VCodec::Hevc, &hevc(123, false, 1920, 1080), 30.0, 30_000.0)), vec!["bitrate"]);
    assert!(level_problems(VCodec::Hevc, &hevc(123, true, 1920, 1080), 30.0, 30_000.0).is_empty());
}

#[test]
fn an_x264_stream_forced_to_a_low_level_is_a_level_warning_on_every_platform() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("low.flv");
    // 720p60 marked level 3.0
    assert!(ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=60",
        "-t",
        "3",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-level",
        "3.0",
        "-g",
        "60",
        "-bf",
        "0",
        "-f",
        "flv",
        f.to_str().unwrap()
    ]));
    let t0 = Instant::now();
    let mut a = Analyzer::new(t0);
    let mut d = FlvDemux::new();
    let mut notes = vec![];
    let mut next = 1000;
    for t in flv_tags(&f).iter().filter(|t| t.kind == 9) {
        while t.ts >= next {
            notes.extend(a.tick(t0 + Duration::from_millis(next as u64)));
            next += 1000;
        }
        let (mut out, mut warn) = (vec![], vec![]);
        d.video(t.ts, &t.body, &mut out, &mut warn);
        for m in out {
            notes.extend(a.on_media(&m, t0 + Duration::from_millis(t.ts as u64)));
        }
    }
    assert_eq!(a.stats.video.sps.as_ref().unwrap().level_idc, 30, "the fixture's level");
    assert!(notes.iter().any(|n| n.kind == "video-level"), "{notes:?}");
    for p in ["open", "youtube", "twitch"] {
        let v = profiles::check(find(p).unwrap(), &a.stats, 3.0);
        assert!(v.iter().any(|v| v.rule == "video-level" && v.level == Level::Warn), "{p}: {v:?}");
    }
}
