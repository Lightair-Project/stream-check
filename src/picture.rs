//! What an uncompressed picture (an NDI source's, a camera's) looks like: its colours and
//! brightness, and whether it's black or not moving, frame by frame. Also thumbnails of such
//! pictures, made by ffmpeg from raw frames.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use serde::Serialize;

use crate::analyze::{Level, Note};

/// One frame's measurements.
#[derive(Debug, Clone, Copy, Default)]
pub struct Measure {
    /// Mean brightness, 0 (black) to 255 (white), full range.
    pub luma: f64,
    /// How much the brightness varies over the picture (0: one flat colour).
    pub luma_stdev: f64,
    /// Mean red, green and blue, 0 to 255.
    pub rgb: [f64; 3],
    /// A fingerprint of the picture: the same for identical frames.
    pub sig: u64,
}

/// Brightness below this, with little variation, is a black picture.
const BLACK_LUMA: f64 = 16.0;
const BLACK_STDEV: f64 = 10.0;
/// An identical picture for this long is a frozen one.
const FROZEN_S: f64 = 2.0;

#[derive(Debug, Clone, Default, Serialize)]
pub struct PictureStats {
    pub frames: u64,
    pub mean_luma: f64,
    pub luma_stdev: f64,
    pub mean_rgb: [f64; 3],
    /// "#rrggbb" of the mean colour, for people.
    pub mean_colour: String,
    pub black_now: bool,
    pub black_frames: u64,
    pub black_s: f64,
    pub frozen_now: bool,
    pub frozen_events: u32,
    pub longest_frozen_s: f64,
    /// Frames that differed from the one before.
    pub changed_frames: u64,
}

#[derive(Default)]
pub struct PictureCheck {
    pub stats: PictureStats,
    last_sig: Option<u64>,
    same_since: Option<f64>,
    black_since: Option<f64>,
    last_t: Option<f64>,
}

impl PictureCheck {
    /// One frame at second `t` of the session.
    pub fn frame(&mut self, t: f64, m: &Measure) -> Vec<Note> {
        let mut out = vec![];
        let s = &mut self.stats;
        s.frames += 1;
        s.mean_luma = m.luma;
        s.luma_stdev = m.luma_stdev;
        s.mean_rgb = m.rgb.map(|c| (c * 10.0).round() / 10.0);
        let c = m.rgb.map(|c| c.round().clamp(0.0, 255.0) as u8);
        s.mean_colour = format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
        let dt = self.last_t.map_or(0.0, |l| (t - l).max(0.0));
        self.last_t = Some(t);
        let black = m.luma < BLACK_LUMA && m.luma_stdev < BLACK_STDEV;
        if black {
            s.black_frames += 1;
            s.black_s += dt;
            if !s.black_now {
                s.black_now = true;
                self.black_since = Some(t);
                out.push(Note { level: Level::Warn, kind: "picture-black", msg: "the picture is black".into() });
            }
        } else if s.black_now {
            s.black_now = false;
            let since = self.black_since.unwrap_or(t);
            out.push(Note {
                level: Level::Info,
                kind: "picture-back",
                msg: format!("the picture came back after {:.1} s of black", t - since),
            });
        }
        if self.last_sig == Some(m.sig) {
            let since = *self.same_since.get_or_insert(t - dt);
            let held = t - since;
            if held >= FROZEN_S && !s.frozen_now {
                s.frozen_now = true;
                s.frozen_events += 1;
                out.push(Note {
                    level: Level::Warn,
                    kind: "picture-frozen",
                    msg: format!("the picture hasn't changed for {FROZEN_S:.0} s"),
                });
            }
            if s.frozen_now {
                s.longest_frozen_s = s.longest_frozen_s.max(held);
            }
        } else {
            if self.last_sig.is_some() {
                s.changed_frames += 1;
            }
            if s.frozen_now {
                s.frozen_now = false;
                let since = self.same_since.unwrap_or(t);
                out.push(Note {
                    level: Level::Info,
                    kind: "picture-moving",
                    msg: format!("the picture moves again after {:.1} s", t - since),
                });
            }
            self.same_since = None;
        }
        self.last_sig = Some(m.sig);
        out
    }

    /// What's wrong with the picture as a whole, for the report's verdict.
    pub fn problems(&self) -> Vec<String> {
        let s = &self.stats;
        if s.frames == 0 {
            return vec!["no picture arrived".into()];
        }
        if s.black_frames == s.frames {
            return vec!["the picture was black the whole time".into()];
        }
        vec![]
    }

    /// Whether every frame was the same picture (said, but not a problem: a still scene is one).
    pub fn never_changed(&self) -> bool {
        self.stats.frames > 1 && self.stats.changed_frames == 0
    }
}

/// Full-range brightness of an RGB colour (BT.709).
pub fn luma(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Colours of limited-range YUV (BT.709) means, as full-range RGB and brightness.
pub fn from_yuv(y: f64, u: f64, v: f64) -> ([f64; 3], f64) {
    let c = (y - 16.0) * 255.0 / 219.0;
    let (u, v) = (u - 128.0, v - 128.0);
    let rgb = [c + 1.793 * v, c - 0.213 * u - 0.533 * v, c + 2.112 * u].map(|x| x.clamp(0.0, 255.0));
    (rgb, c.clamp(0.0, 255.0))
}

/// Pixel layouts of raw frames the lab measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Bgra,
    Rgba,
    Uyvy,
}

/// Measures a raw frame from a grid of samples (fast, whatever the frame's size).
pub fn measure(data: &[u8], width: usize, height: usize, stride: usize, layout: Layout) -> Measure {
    const GX: usize = 96;
    const GY: usize = 54;
    let mut n = 0f64;
    let (mut sr, mut sg, mut sb, mut sl, mut sl2) = (0f64, 0f64, 0f64, 0f64, 0f64);
    let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
    if width == 0 || height == 0 {
        return Measure::default();
    }
    for gy in 0..GY {
        let y = (gy * height / GY).min(height - 1);
        for gx in 0..GX {
            let x = (gx * width / GX).min(width - 1);
            let (r, g, b) = match layout {
                Layout::Bgra | Layout::Rgba => {
                    let i = y * stride + x * 4;
                    let Some(p) = data.get(i..i + 3) else { continue };
                    let (r, b) = if layout == Layout::Bgra { (p[2], p[0]) } else { (p[0], p[2]) };
                    (r as f64, p[1] as f64, b as f64)
                }
                Layout::Uyvy => {
                    let i = y * stride + (x / 2) * 4;
                    let Some(p) = data.get(i..i + 4) else { continue };
                    let yy = if x % 2 == 0 { p[1] } else { p[3] };
                    let (rgb, _) = from_yuv(yy as f64, p[0] as f64, p[2] as f64);
                    (rgb[0], rgb[1], rgb[2])
                }
            };
            let l = luma(r, g, b);
            sr += r;
            sg += g;
            sb += b;
            sl += l;
            sl2 += l * l;
            n += 1.0;
            for v in [r as u8, g as u8, b as u8] {
                sig = (sig ^ v as u64).wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    if n == 0.0 {
        return Measure::default();
    }
    let mean = sl / n;
    Measure { luma: mean, luma_stdev: (sl2 / n - mean * mean).max(0.0).sqrt(), rgb: [sr / n, sg / n, sb / n], sig }
}

/// Thumbnails of raw frames: ffmpeg turns each frame given into the next `s01_NNNNN.jpg`, as
/// a live session's thumbnails are named (one every two seconds of the session).
pub struct RawThumbs {
    dir: PathBuf,
    child: Option<(Child, ChildStdin, (usize, usize, &'static str))>,
    pub written: u32,
}

impl RawThumbs {
    pub fn new(dir: &Path) -> RawThumbs {
        RawThumbs { dir: dir.to_path_buf(), child: None, written: 0 }
    }

    /// One frame, tightly packed (`pix_fmt` as ffmpeg names it: "bgra", "rgba", "uyvy422").
    pub fn frame(&mut self, data: &[u8], width: usize, height: usize, pix_fmt: &'static str) {
        if !crate::helper::ffmpeg_available() {
            return;
        }
        if self.child.as_ref().is_some_and(|c| c.2 != (width, height, pix_fmt)) {
            self.close();
        }
        if self.child.is_none() {
            let spawned = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "rawvideo", "-pix_fmt", pix_fmt])
                .args(["-s", &format!("{width}x{height}"), "-framerate", "1", "-i", "pipe:0"])
                .args(["-vf", "scale=320:-2", "-q:v", "6", "-f", "image2", "-start_number", &self.written.to_string()])
                .arg(self.dir.join("s01_%05d.jpg"))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = spawned else { return };
            let Some(stdin) = child.stdin.take() else { return };
            self.child = Some((child, stdin, (width, height, pix_fmt)));
        }
        if let Some((_, stdin, _)) = &mut self.child {
            if stdin.write_all(data).is_ok() {
                self.written += 1;
            } else {
                self.close();
            }
        }
    }

    pub fn close(&mut self) {
        if let Some((mut child, stdin, _)) = self.child.take() {
            drop(stdin);
            let _ = child.wait();
        }
    }
}

impl Drop for RawThumbs {
    fn drop(&mut self) {
        self.close();
    }
}
