//! An uncompressed picture (an NDI source's, a camera's): whether it is black, whether it stopped
//! moving, and the verdict on a session's picture as a whole.

use stream_check::picture::{Layout, Measure, PictureCheck, measure};

const W: usize = 64;
const H: usize = 36;

/// A BGRA frame filled by `f(x, y)` with a grey level.
fn bgra(f: impl Fn(usize, usize) -> u8) -> Vec<u8> {
    let mut d = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let v = f(x, y);
            d[(y * W + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    d
}

fn m(frame: &[u8]) -> Measure {
    measure(frame, W, H, W * 4, Layout::Bgra)
}

/// A frame that differs from every other `n`.
fn moving(n: usize) -> Measure {
    m(&bgra(|x, y| ((x + y + n) % 64 * 3 + 40) as u8))
}

fn kinds(notes: &[stream_check::analyze::Note]) -> Vec<&'static str> {
    notes.iter().map(|n| n.kind).collect()
}

#[test]
fn a_black_picture_is_said_and_so_is_its_return() {
    let mut c = PictureCheck::default();
    let black = m(&bgra(|_, _| 0));
    assert_eq!(kinds(&c.frame(0.0, &moving(0))), Vec::<&str>::new());
    assert_eq!(kinds(&c.frame(1.0, &black)), vec!["picture-black"]);
    assert!(c.stats.black_now);
    assert_eq!(kinds(&c.frame(2.0, &black)), Vec::<&str>::new(), "said once");
    assert!(kinds(&c.frame(3.0, &moving(1))).contains(&"picture-back"));
    assert!(!c.stats.black_now);
    assert!((c.stats.black_s - 2.0).abs() < 1e-9, "{}", c.stats.black_s);
}

#[test]
fn a_dark_picture_with_detail_is_not_black() {
    // mean brightness about 12, but half the picture is grey
    let dark = m(&bgra(|x, _| if x % 2 == 0 { 0 } else { 24 }));
    assert!(dark.luma < 16.0, "{}", dark.luma);
    let mut c = PictureCheck::default();
    c.frame(0.0, &dark);
    assert!(!c.stats.black_now);
}

#[test]
fn limited_range_black_from_a_yuv_source_is_black() {
    // UYVY: Y 16, U and V 128 is black in limited range
    let frame: Vec<u8> = std::iter::repeat_n([128u8, 16, 128, 16], W * H / 2).flatten().collect();
    let mut c = PictureCheck::default();
    let notes = c.frame(0.0, &measure(&frame, W, H, W * 2, Layout::Uyvy));
    assert_eq!(kinds(&notes), vec!["picture-black"]);
}

#[test]
fn a_picture_unchanged_for_two_seconds_is_frozen_until_it_moves() {
    let mut c = PictureCheck::default();
    let still = moving(7);
    let mut notes = vec![];
    // 30 fps: the same picture for 3 s
    for i in 0..=90 {
        notes.extend(c.frame(i as f64 / 30.0, &still));
    }
    assert_eq!(kinds(&notes), vec!["picture-frozen"], "said once");
    assert_eq!(c.stats.frozen_events, 1);
    assert!(c.stats.longest_frozen_s >= 2.9, "{}", c.stats.longest_frozen_s);
    assert!(kinds(&c.frame(3.1, &moving(8))).contains(&"picture-moving"));
    assert!(!c.stats.frozen_now);
}

#[test]
fn a_picture_that_keeps_changing_is_never_frozen() {
    let mut c = PictureCheck::default();
    for i in 0..150 {
        assert!(!kinds(&c.frame(i as f64 / 30.0, &moving(i))).contains(&"picture-frozen"));
    }
    assert_eq!(c.stats.frozen_events, 0);
    assert!(!c.never_changed());
}

#[test]
fn black_the_whole_time_is_a_problem_but_a_still_picture_is_not() {
    let mut black = PictureCheck::default();
    let b = m(&bgra(|_, _| 0));
    for i in 0..10 {
        black.frame(i as f64, &b);
    }
    assert_eq!(black.problems().len(), 1, "{:?}", black.problems());

    let mut still = PictureCheck::default();
    for i in 0..10 {
        still.frame(i as f64, &moving(3));
    }
    assert!(still.never_changed());
    assert!(still.problems().is_empty(), "{:?}", still.problems());
}

#[test]
fn no_picture_at_all_is_a_problem() {
    assert_eq!(PictureCheck::default().problems().len(), 1);
}
