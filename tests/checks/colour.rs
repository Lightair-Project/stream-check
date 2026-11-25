//! Colour tags: what an SPS or sequence header says about range and colours, the warnings for
//! tags that are missing or contradict each other, and the platform rules on range, HDR and the
//! codec profile.

use std::time::{Duration, Instant};

use stream_check::analyze::{Analyzer, Level, Stats};
use stream_check::levels::colour_problems;
use stream_check::nal::{Colour, Sps, VCodec};
use stream_check::profiles::{check, find};
use stream_check::rtmp::FlvDemux;

use crate::support::{ffmpeg, ffprobe, flv_tags, have_ffmpeg};

/// A 1080p 8-bit 4:2:0 picture with VUI and these tags (range, primaries, transfer, matrix).
fn tagged(full: bool, p: u8, t: u8, m: u8) -> Sps {
    Sps {
        width: 1920,
        height: 1080,
        bit_depth: 8,
        bit_depth_chroma: 8,
        chroma_format: 1,
        vui: true,
        colour: Colour { full_range: Some(full), primaries: Some(p), transfer: Some(t), matrix: Some(m), ..Default::default() },
        ..Default::default()
    }
}

#[test]
fn a_well_tagged_hd_picture_has_no_colour_warnings() {
    assert_eq!(colour_problems(&tagged(false, 1, 1, 1)), Vec::<String>::new());
}

#[test]
fn a_picture_without_vui_or_colour_description_is_said_to_be_untagged() {
    let no_vui = Sps { vui: false, ..tagged(false, 1, 1, 1) };
    assert_eq!(colour_problems(&no_vui).len(), 1);
    let mut no_description = tagged(false, 1, 1, 1);
    no_description.colour = Colour { full_range: Some(false), ..Default::default() };
    let w = colour_problems(&no_description);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("colour description"), "{w:?}");
}

#[test]
fn hd_tagged_bt601_is_a_warning() {
    let w = colour_problems(&tagged(false, 6, 6, 6));
    assert!(w.iter().any(|w| w.contains("BT.601")), "{w:?}");
    // the same tags on an SD picture are what SD is
    let sd = Sps { width: 720, height: 480, ..tagged(false, 6, 6, 6) };
    assert!(colour_problems(&sd).is_empty(), "{:?}", colour_problems(&sd));
}

#[test]
fn primaries_and_matrix_of_different_families_contradict_each_other() {
    let w = colour_problems(&tagged(false, 1, 1, 9));
    assert!(w.iter().any(|w| w.contains("contradict")), "{w:?}");
}

#[test]
fn an_hdr_transfer_needs_bt2020_primaries_and_ten_bits() {
    let w = colour_problems(&tagged(false, 1, 16, 1));
    assert_eq!(w.len(), 2, "PQ with BT.709 primaries at 8 bits: {w:?}");
    let ten = Sps { bit_depth: 10, bit_depth_chroma: 10, ..tagged(false, 9, 16, 9) };
    assert!(colour_problems(&ten).is_empty(), "{:?}", colour_problems(&ten));
}

#[test]
fn luma_and_chroma_depths_are_compared_only_when_the_chroma_depth_is_known() {
    let unknown = Sps { bit_depth_chroma: 0, ..tagged(false, 1, 1, 1) };
    assert!(colour_problems(&unknown).is_empty(), "{:?}", colour_problems(&unknown));
    let differs = Sps { bit_depth_chroma: 10, ..tagged(false, 1, 1, 1) };
    assert_eq!(colour_problems(&differs).len(), 1);
}

fn stats(codec: VCodec, sps: Sps) -> Stats {
    let mut s = Stats { has_video: true, has_audio: true, ..Default::default() };
    s.video.codec = Some(codec);
    s.video.sps = Some(sps);
    s
}

fn rules(profile: &str, s: &Stats) -> Vec<(&'static str, Level)> {
    check(find(profile).unwrap(), s, 10.0).into_iter().map(|v| (v.rule, v.level)).collect()
}

#[test]
fn a_full_range_picture_is_a_warning_where_the_platform_expects_limited_range() {
    let s = stats(VCodec::H264, Sps { profile_idc: 100, ..tagged(true, 1, 1, 1) });
    assert!(rules("twitch", &s).contains(&("video-range", Level::Warn)));
    assert!(!rules("open", &s).iter().any(|r| r.0 == "video-range"));
}

#[test]
fn hdr_is_a_warning_on_platforms_that_show_sdr_only() {
    let pq = Sps { profile_idc: 2, bit_depth: 10, bit_depth_chroma: 10, ..tagged(false, 9, 16, 9) };
    let s = stats(VCodec::Hevc, pq);
    assert!(rules("kick", &s).contains(&("video-hdr", Level::Warn)), "{:?}", rules("kick", &s));
    assert!(!rules("youtube", &s).iter().any(|r| r.0 == "video-hdr"));
}

#[test]
fn a_codec_profile_the_platform_does_not_take_is_an_error() {
    // HEVC Main 10: YouTube takes it, Twitch-like takes only Main
    let main10 = stats(VCodec::Hevc, Sps { profile: "Main 10".into(), profile_idc: 2, ..tagged(false, 1, 1, 1) });
    assert!(!rules("youtube", &main10).iter().any(|r| r.0 == "video-profile"));
    // H.264 High 10 on Twitch-like
    let high10 = stats(VCodec::H264, Sps { profile: "High 10".into(), profile_idc: 110, ..tagged(false, 1, 1, 1) });
    assert!(rules("twitch", &high10).contains(&("video-profile", Level::Error)), "{:?}", rules("twitch", &high10));
    assert!(!rules("open", &high10).iter().any(|r| r.0 == "video-profile"));
}

/// The first video config of an FLV, as the analyzer reads it.
fn first_sps(path: &std::path::Path) -> Sps {
    let t0 = Instant::now();
    let mut a = Analyzer::new(t0);
    let mut d = FlvDemux::new();
    for t in flv_tags(path).iter().filter(|t| t.kind == 9) {
        let (mut out, mut warn) = (vec![], vec![]);
        d.video(t.ts, &t.body, &mut out, &mut warn);
        for m in out {
            a.on_media(&m, t0 + Duration::from_millis(t.ts as u64));
        }
        if let Some(s) = &a.stats.video.sps {
            return s.clone();
        }
    }
    panic!("no picture size read from {}", path.display());
}

#[test]
fn x264s_vui_colour_tags_are_read() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tagged.flv");
    let tags = "colorprim=bt709:transfer=bt709:colormatrix=bt709:range=pc";
    let args = ["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30", "-t", "0.2", "-c:v", "libx264", "-pix_fmt", "yuvj420p"];
    assert!(ffmpeg(&[&args[..], &["-x264-params", tags, "-f", "flv", f.to_str().unwrap()]].concat()));
    let probe = &ffprobe(&f, &[])["streams"][0];
    assert_eq!((probe["color_range"].as_str(), probe["color_primaries"].as_str()), (Some("pc"), Some("bt709")), "the fixture's tags");
    let c = first_sps(&f).colour;
    assert_eq!((c.full_range, c.primaries, c.transfer, c.matrix), (Some(true), Some(1), Some(1), Some(1)), "{c:?}");
    assert_eq!(c.primaries_name.as_deref(), Some("BT.709"));
}

#[test]
fn an_av1_sequence_header_tagged_full_range_is_flagged_on_youtube() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("full.flv");
    let made = ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-t",
        "0.5",
        "-c:v",
        "libsvtav1",
        "-pix_fmt",
        "yuv420p",
        "-color_range",
        "pc",
        "-svtav1-params",
        "lp=1",
        "-f",
        "flv",
        f.to_str().unwrap(),
    ]);
    if !made {
        eprintln!("no ffmpeg with SVT-AV1 writing AV1 into FLV: skipped");
        return;
    }
    let sps = first_sps(&f);
    assert_eq!(sps.colour.full_range, Some(true), "{sps:?}");
    let s = stats(VCodec::Av1, sps);
    assert!(rules("youtube", &s).contains(&("video-range", Level::Warn)), "{:?}", rules("youtube", &s));
}
