//! AV1, VP9 and VP8 as encoders write them: what the lab reads from the sequence header or key
//! frame header (size, bit depth, level, codec string) and how it counts key frames. ffprobe is
//! the other opinion each reading is held against.

use std::path::Path;
use std::time::{Duration, Instant};

use stream_check::analyze::Analyzer;
use stream_check::nal::VCodec;
use stream_check::rtmp::FlvDemux;
use stream_check::vp9;

use crate::support::{Tag, ffmpeg, ffprobe, flv_tags, have_ffmpeg};

/// Every tag of an FLV through the RTMP demuxer and the analyzer, each at its own moment.
fn analyse(tags: &[Tag]) -> Analyzer {
    let t0 = Instant::now();
    let mut a = Analyzer::new(t0);
    let mut d = FlvDemux::new();
    let mut last = t0;
    for t in tags {
        let (mut out, mut warn) = (vec![], vec![]);
        match t.kind {
            9 => d.video(t.ts, &t.body, &mut out, &mut warn),
            8 => d.audio(t.ts, &t.body, &mut out, &mut warn),
            _ => continue,
        }
        assert!(warn.is_empty(), "{warn:?}");
        last = t0 + Duration::from_millis(t.ts as u64);
        for m in out {
            a.on_media(&m, last);
        }
    }
    a.tick(last);
    a
}

fn key_frames(path: &Path) -> u64 {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v", "-show_entries", "frame=key_frame", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.trim() == "1").count() as u64
}

fn av1_flv(dir: &Path) -> Option<std::path::PathBuf> {
    let f = dir.join("av1.flv");
    let made = ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-t",
        "2",
        "-c:v",
        "libsvtav1",
        "-g",
        "30",
        "-pix_fmt",
        "yuv420p10le",
        "-svtav1-params",
        "lp=1",
        "-f",
        "flv",
        f.to_str().unwrap(),
    ]);
    made.then_some(f)
}

#[test]
fn av1_over_enhanced_flv_gives_its_size_depth_level_and_codec_string() {
    let dir = tempfile::tempdir().unwrap();
    let Some(f) = av1_flv(dir.path()).filter(|_| have_ffmpeg()) else {
        eprintln!("no ffmpeg with SVT-AV1 writing AV1 into FLV: skipped");
        return;
    };
    let probe = &ffprobe(&f, &[])["streams"][0];
    let level = probe["level"].as_u64().expect("ffprobe's AV1 level");
    let a = analyse(&flv_tags(&f));
    let v = &a.stats.video;
    assert_eq!(v.codec, Some(VCodec::Av1));
    let sps = v.sps.as_ref().expect("the sequence header was read");
    assert_eq!((sps.width, sps.height, sps.bit_depth), (640, 360, 10), "{sps:?}");
    assert_eq!(sps.level_idc as u64, level, "{sps:?}");
    // WebCodecs: av01.<profile>.<level><tier>.<depth>
    assert_eq!(sps.codec_string, format!("av01.0.{level:02}M.10"));
}

#[test]
fn av1_key_frames_are_counted_from_the_bitstream_whatever_the_tags_say() {
    let dir = tempfile::tempdir().unwrap();
    let Some(f) = av1_flv(dir.path()).filter(|_| have_ffmpeg()) else {
        eprintln!("no ffmpeg with SVT-AV1 writing AV1 into FLV: skipped");
        return;
    };
    let expected = key_frames(&f);
    assert!(expected >= 2, "the fixture has key frames ({expected})");
    let mut tags = flv_tags(&f);
    // every coded frame marked as an inter frame by its tag
    for t in tags.iter_mut().filter(|t| t.kind == 9 && t.body[0] & 0x80 != 0) {
        t.body[0] = 0x80 | (2 << 4) | (t.body[0] & 0x0f);
    }
    let a = analyse(&tags);
    assert_eq!(a.stats.video.keyframes, expected);
}

#[test]
fn vp9_over_enhanced_flv_gives_its_size_codec_string_and_key_frames_from_the_bitstream() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("vp9.flv");
    let made = ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-t",
        "3",
        "-c:v",
        "libvpx-vp9",
        "-deadline",
        "realtime",
        "-cpu-used",
        "8",
        "-g",
        "45",
        "-f",
        "flv",
        f.to_str().unwrap(),
    ]);
    if !made {
        eprintln!("no ffmpeg with libvpx writing VP9 into FLV: skipped");
        return;
    }
    let mut tags = flv_tags(&f);
    let coded = tags.iter().filter(|t| t.kind == 9 && matches!(t.body[0] & 0x0f, 1 | 3)).count();
    // every coded frame marked as a key frame by its tag
    for t in tags.iter_mut().filter(|t| t.kind == 9 && t.body[0] & 0x80 != 0) {
        t.body[0] = 0x80 | (1 << 4) | (t.body[0] & 0x0f);
    }
    let a = analyse(&tags);
    let v = &a.stats.video;
    assert_eq!(v.codec, Some(VCodec::Vp9));
    assert_eq!(v.frames, coded as u64);
    let expected = key_frames(&f);
    assert!(expected >= 2 && expected < coded as u64, "the fixture's key frames ({expected})");
    assert_eq!(v.keyframes, expected);
    let sps = v.sps.as_ref().unwrap();
    assert_eq!((sps.width, sps.height, sps.bit_depth), (640, 360, 8));
    // 640x360 takes VP9 level 2.1 (at most 245760 samples a picture)
    assert_eq!(sps.codec_string, "vp09.00.21.08");
}

#[test]
fn a_vp8_key_frame_gives_its_size_and_an_inter_frame_is_not_key() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("vp8.ivf");
    if !ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-t", "0.2", "-c:v", "libvpx", "-f", "ivf", f.to_str().unwrap()]) {
        eprintln!("no ffmpeg with libvpx: skipped");
        return;
    }
    // IVF: a 32-byte header, then each frame after a 12-byte header (its size first)
    let d = std::fs::read(&f).unwrap();
    let mut frames = vec![];
    let mut i = 32;
    while i + 12 <= d.len() {
        let n = u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]) as usize;
        frames.push(&d[i + 12..i + 12 + n]);
        i += 12 + n;
    }
    let first = vp9::vp8_frame(frames[0]).expect("a VP8 frame header");
    assert!(first.key);
    assert_eq!((first.width, first.height), (320, 240));
    assert!(!vp9::vp8_frame(frames[1]).unwrap().key);
}

#[test]
fn an_encoders_own_user_data_names_it() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("x264.flv");
    assert!(ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30",
        "-t",
        "0.5",
        "-c:v",
        "libx264",
        "-f",
        "flv",
        f.to_str().unwrap()
    ]));
    let a = analyse(&flv_tags(&f));
    let ud = &a.stats.video.bitstream.user_data;
    assert!(ud.iter().any(|u| u.text.as_deref().is_some_and(|t| t.starts_with("x264"))), "{ud:?}");
}

#[test]
fn a_vp9_key_frame_header_is_read_behind_a_hidden_frame_in_a_superframe() {
    // a hidden inter frame, then a shown key frame of 352x288 (profile 0)
    let inter = [0b1000_0100u8, 0, 0];
    let mut key = vec![0b1000_0010u8, 0x49, 0x83, 0x42];
    // colour space BT.709 (2), limited range, then width-1 and height-1 in 16 bits each
    let mut bits: u64 = 0;
    let mut n: usize = 0;
    for (v, w) in [(2u64, 3), (0, 1), (351, 16), (287, 16)] {
        bits = (bits << w) | v;
        n += w;
    }
    bits <<= 64 - n;
    key.extend(&bits.to_be_bytes()[..n.div_ceil(8)]);
    // the superframe index: a marker (two frames, one byte per size), the sizes, the marker
    let mut chunk = inter.to_vec();
    chunk.extend(&key);
    chunk.extend([0xc1, inter.len() as u8, key.len() as u8, 0xc1]);
    assert_eq!(vp9::vp9_frames(&chunk).len(), 2);
    let f = vp9::vp9_chunk(&chunk).expect("a frame header");
    assert!(f.key);
    assert_eq!((f.width, f.height, f.bit_depth, f.color_space), (352, 288, 8, 2));
}
