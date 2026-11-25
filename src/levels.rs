//! What a stream's SPS promises against what it carries: the level's limits on picture size,
//! sample rate, bitrate and reference frames (H.264 Table A-1, HEVC Table A.8 and A.9), and
//! colour tags that are missing or unusual for the picture.

use crate::nal::{Sps, VCodec};

/// One level's limits, in the units each codec's table uses.
struct Limits {
    /// H.264: macroblocks per frame (MaxFS); HEVC: luma samples per picture (MaxLumaPs).
    max_frame: u64,
    /// H.264: macroblocks per second (MaxMBPS); HEVC: luma samples per second (MaxLumaSr).
    max_rate: u64,
    /// H.264: macroblocks in the decoded picture buffer (MaxDpbMbs); HEVC: unused.
    max_dpb: u64,
    /// Video bitrate in kbit/s for the baseline factor (H.264 VCL, HEVC Main tier VCL).
    max_kbps: u64,
}

fn h264_limits(level_idc: u8, constraint_set3: bool) -> Option<Limits> {
    let l = |max_rate, max_frame, max_dpb, max_kbps| Some(Limits { max_frame, max_rate, max_dpb, max_kbps });
    match level_idc {
        9 => l(1485, 99, 396, 128),
        10 => l(1485, 99, 396, 64),
        11 if constraint_set3 => l(1485, 99, 396, 128),
        11 => l(3000, 396, 900, 192),
        12 => l(6000, 396, 2376, 384),
        13 => l(11880, 396, 2376, 768),
        20 => l(11880, 396, 2376, 2000),
        21 => l(19800, 792, 4752, 4000),
        22 => l(20250, 1620, 8100, 4000),
        30 => l(40500, 1620, 8100, 10000),
        31 => l(108000, 3600, 18000, 14000),
        32 => l(216000, 5120, 20480, 20000),
        40 => l(245760, 8192, 32768, 20000),
        41 => l(245760, 8192, 32768, 50000),
        42 => l(522240, 8704, 34816, 50000),
        50 => l(589824, 22080, 110400, 135000),
        51 => l(983040, 36864, 184320, 240000),
        52 => l(2073600, 36864, 184320, 240000),
        60 => l(4177920, 139264, 696320, 240000),
        61 => l(8355840, 139264, 696320, 480000),
        62 => l(16711680, 139264, 696320, 800000),
        _ => None,
    }
}

fn hevc_limits(level_idc: u8, high_tier: bool) -> Option<Limits> {
    let l = |max_frame, max_rate, main, high: u64| {
        Some(Limits { max_frame, max_rate, max_dpb: 0, max_kbps: if high_tier && high > 0 { high } else { main } })
    };
    match level_idc {
        30 => l(36864, 552960, 128, 0),
        60 => l(122880, 3686400, 1500, 0),
        63 => l(245760, 7372800, 3000, 0),
        90 => l(552960, 16588800, 6000, 0),
        93 => l(983040, 33177600, 10000, 0),
        120 => l(2228224, 66846720, 12000, 30000),
        123 => l(2228224, 133693440, 20000, 50000),
        150 => l(8912896, 267386880, 25000, 100000),
        153 => l(8912896, 534773760, 40000, 160000),
        156 => l(8912896, 1069547520, 60000, 240000),
        180 => l(35651584, 1069547520, 60000, 240000),
        183 => l(35651584, 2139095040, 120000, 480000),
        186 => l(35651584, 4278190080, 240000, 800000),
        _ => None,
    }
}

/// H.264's factor from VCL to NAL bitrate (cpbBrNalFactor / 1000) by profile.
fn h264_nal_factor(profile_idc: u8) -> f64 {
    match profile_idc {
        100 => 1.5,
        110 => 3.6,
        122 | 244 => 4.8,
        _ => 1.2,
    }
}

/// Where the stream goes past its level's limits, each with its kind ("size", "rate", "bitrate",
/// "buffer"). `fps` is the frame rate (measured, or declared when nothing is measured yet),
/// `peak_kbps` the highest video bitrate seen over 2 s.
pub fn level_problems(codec: VCodec, sps: &Sps, fps: f64, peak_kbps: f64) -> Vec<(&'static str, String)> {
    let mut out = vec![];
    let level = &sps.level;
    // a little slack: measured rates wobble
    let fps = fps / 1.03;
    match codec {
        VCodec::H264 => {
            let set3 = sps.codec_string.get(7..9).and_then(|c| u8::from_str_radix(c, 16).ok()).is_some_and(|c| c & 0x10 != 0);
            let Some(lim) = h264_limits(sps.level_idc, set3 && matches!(sps.profile_idc, 66 | 77 | 88)) else { return out };
            let (w, h) = (sps.width.div_ceil(16) as u64, sps.height.div_ceil(16) as u64);
            let fs = w * h;
            let side = ((lim.max_frame * 8) as f64).sqrt() as u64;
            if fs > lim.max_frame || w > side || h > side {
                out.push((
                    "size",
                    format!(
                        "{}x{} ({fs} macroblocks) is over level {level}'s picture size ({} macroblocks, at most {} on a side)",
                        sps.width, sps.height, lim.max_frame, side
                    ),
                ));
            } else if fps > 0.0 && fs as f64 * fps > lim.max_rate as f64 {
                out.push((
                    "rate",
                    format!(
                        "{}x{} at {:.0} fps is {:.0} macroblocks a second, over level {level}'s {}",
                        sps.width,
                        sps.height,
                        fps * 1.03,
                        fs as f64 * fps * 1.03,
                        lim.max_rate
                    ),
                ));
            }
            let max_kbps = lim.max_kbps as f64 * h264_nal_factor(sps.profile_idc);
            if peak_kbps > max_kbps * 1.1 {
                out.push((
                    "bitrate",
                    format!("video peaks at {peak_kbps:.0} kbps, over level {level}'s {max_kbps:.0} kbps for {}", sps.profile),
                ));
            }
            if let Some(refs) = sps.ref_frames.filter(|_| fs > 0) {
                let max = (lim.max_dpb / fs).min(16);
                if refs as u64 > max {
                    out.push(("buffer", format!("{refs} reference frames; level {level} holds {max} at {}x{}", sps.width, sps.height)));
                }
            }
        }
        VCodec::Hevc => {
            let Some(lim) = hevc_limits(sps.level_idc, sps.high_tier) else { return out };
            let ps = sps.width as u64 * sps.height as u64;
            let side = ((lim.max_frame * 8) as f64).sqrt() as u64;
            if ps > lim.max_frame || sps.width as u64 > side || sps.height as u64 > side {
                out.push((
                    "size",
                    format!(
                        "{}x{} is over level {level}'s picture size ({} luma samples, at most {side} on a side)",
                        sps.width, sps.height, lim.max_frame
                    ),
                ));
            } else if fps > 0.0 && ps as f64 * fps > lim.max_rate as f64 {
                out.push((
                    "rate",
                    format!(
                        "{}x{} at {:.0} fps is {:.0} luma samples a second, over level {level}'s {}",
                        sps.width,
                        sps.height,
                        fps * 1.03,
                        ps as f64 * fps * 1.03,
                        lim.max_rate
                    ),
                ));
            }
            // Main and Main 10's NAL factor is 1.1
            let max_kbps = lim.max_kbps as f64 * 1.1;
            if peak_kbps > max_kbps * 1.1 {
                out.push((
                    "bitrate",
                    format!(
                        "video peaks at {peak_kbps:.0} kbps, over level {level}'s {max_kbps:.0} kbps ({} tier)",
                        if sps.high_tier { "High" } else { "Main" }
                    ),
                ));
            }
            if let Some(dpb) = sps.ref_frames.filter(|_| ps > 0) {
                let max = if ps <= lim.max_frame / 4 {
                    16
                } else if ps <= lim.max_frame / 2 {
                    12
                } else if ps <= lim.max_frame * 3 / 4 {
                    8
                } else {
                    6
                };
                if dpb > max {
                    out.push(("buffer", format!("a picture buffer of {dpb}; level {level} holds {max} at {}x{}", sps.width, sps.height)));
                }
            }
        }
        _ => {}
    }
    out
}

/// Colour tags that are missing, contradict each other or are unusual for the picture.
pub fn colour_problems(sps: &Sps) -> Vec<String> {
    let mut out = vec![];
    let c = &sps.colour;
    if !sps.vui {
        out.push("no VUI in the SPS: range and colours aren't tagged (players assume limited range, and guess the colours)".into());
        return out;
    }
    if c.full_range.is_none() {
        out.push("no video signal type in the VUI: range and colours aren't tagged (players assume limited range)".into());
        return out;
    }
    let (Some(p), Some(t), Some(m)) = (c.primaries, c.transfer, c.matrix) else {
        out.push(
            "no colour description in the VUI (primaries, transfer, matrix): players guess, usually BT.709 for HD and BT.601 for SD".into(),
        );
        return out;
    };
    if p == 2 || t == 2 || m == 2 {
        out.push(format!("colour tags left unspecified (primaries {p}, transfer {t}, matrix {m}): players guess"));
    }
    let hd = sps.width >= 1280 || sps.height >= 720;
    let bt601 = |x: u8| x == 5 || x == 6;
    if hd && (bt601(m) || bt601(p)) {
        out.push(format!(
            "{}x{} tagged BT.601 (primaries {}, matrix {}); HD pictures are normally BT.709, and a mismatch shifts the colours",
            sps.width,
            sps.height,
            crate::nal::primaries_name(p),
            crate::nal::matrix_name(m)
        ));
    }
    // the family each code belongs to: 709, 601, 2020
    let family = |x: u8, is_matrix: bool| match x {
        1 => Some("BT.709"),
        5 | 6 => Some("BT.601"),
        9 => Some("BT.2020"),
        10 if is_matrix => Some("BT.2020"),
        _ => None,
    };
    if let (Some(fp), Some(fm)) = (family(p, false), family(m, true)) {
        if fp != fm {
            out.push(format!("{fp} primaries with a {fm} matrix: the tags contradict each other"));
        }
    }
    let hdr = t == 16 || t == 18;
    if hdr && p != 9 {
        out.push(format!(
            "an HDR transfer ({}) with {} primaries; HDR is BT.2020",
            crate::nal::transfer_name(t),
            crate::nal::primaries_name(p)
        ));
    }
    if hdr && sps.bit_depth < 10 {
        out.push(format!("an HDR transfer ({}) at {} bits; HDR needs 10", crate::nal::transfer_name(t), sps.bit_depth));
    }
    if m == 0 && sps.chroma_format != 3 {
        out.push("the RGB (identity) matrix with subsampled chroma".into());
    }
    if sps.bit_depth_chroma > 0 && sps.bit_depth != sps.bit_depth_chroma {
        out.push(format!("luma at {} bits and chroma at {}", sps.bit_depth, sps.bit_depth_chroma));
    }
    out
}

/// Whether the transfer is HDR (PQ or HLG).
pub fn is_hdr(sps: &Sps) -> bool {
    matches!(sps.colour.transfer, Some(16 | 18))
}
