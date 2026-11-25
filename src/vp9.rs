//! VP9 and VP8 bitstream reading: the frame header of a key frame (profile, size, bit depth,
//! colour), superframes, and the VP codec configuration record (vpcC). Written for measuring a
//! stream, not decoding it.

use crate::nal::{Bits, Sps};

/// What a VP9 or VP8 key frame's header says.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FrameInfo {
    pub key: bool,
    pub profile: u8,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    /// vpcC's chromaSubsampling: 0/1 4:2:0, 2 4:2:2, 3 4:4:4.
    pub chroma: u8,
    pub full_range: bool,
    /// VP9's color_space (0 unknown, 1 BT.601, 2 BT.709, ..., 7 RGB).
    pub color_space: u8,
}

/// The frames of a VP9 chunk: one, or those listed by a superframe index at its end.
pub fn vp9_frames(d: &[u8]) -> Vec<&[u8]> {
    let Some(&last) = d.last() else { return vec![] };
    if last & 0xe0 == 0xc0 {
        let frames = (last & 7) as usize + 1;
        let size_bytes = ((last >> 3) & 3) as usize + 1;
        let index = 2 + size_bytes * frames;
        if d.len() >= index && d[d.len() - index] == last {
            let mut out = vec![];
            let mut at = 0;
            let mut i = d.len() - index + 1;
            for _ in 0..frames {
                let n = d[i..i + size_bytes].iter().rev().fold(0usize, |a, &b| (a << 8) | b as usize);
                i += size_bytes;
                if at + n > d.len() - index {
                    break;
                }
                out.push(&d[at..at + n]);
                at += n;
            }
            return out;
        }
    }
    vec![d]
}

/// A superframe of several frames (with its index), or the one frame as it is.
pub fn vp9_superframe(frames: &[Vec<u8>]) -> Vec<u8> {
    if frames.len() == 1 {
        return frames[0].clone();
    }
    let largest = frames.iter().map(|f| f.len()).max().unwrap_or(0);
    let size_bytes = match largest {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xff_ffff => 3,
        _ => 4,
    };
    let marker = 0xc0 | (((size_bytes - 1) as u8) << 3) | ((frames.len().min(8) - 1) as u8);
    let mut out: Vec<u8> = frames.iter().take(8).flatten().copied().collect();
    out.push(marker);
    for f in frames.iter().take(8) {
        out.extend(&(f.len() as u32).to_le_bytes()[..size_bytes]);
    }
    out.push(marker);
    out
}

/// Reads one VP9 frame's uncompressed header far enough to know whether it is a key frame and,
/// if so, its size and colour.
pub fn vp9_frame(d: &[u8]) -> Option<FrameInfo> {
    let mut b = Bits::new(d);
    if b.u(2)? != 2 {
        return None;
    }
    let low = b.u(1)? as u8;
    let high = b.u(1)? as u8;
    let profile = (high << 1) | low;
    if profile == 3 {
        b.skip(1)?;
    }
    let mut f = FrameInfo { profile, ..Default::default() };
    if b.flag()? {
        // show_existing_frame: a frame already decoded, shown again
        return Some(f);
    }
    f.key = b.u(1)? == 0;
    b.skip(2)?; // show_frame, error_resilient_mode
    if !f.key {
        return Some(f);
    }
    if b.u(24)? != 0x49_83_42 {
        return None;
    }
    f.bit_depth = if profile >= 2 { if b.flag()? { 12 } else { 10 } } else { 8 };
    f.color_space = b.u(3)? as u8;
    let (mut ssx, mut ssy) = (true, true);
    if f.color_space != 7 {
        f.full_range = b.flag()?;
        if profile == 1 || profile == 3 {
            ssx = b.flag()?;
            ssy = b.flag()?;
            b.skip(1)?;
        }
    } else {
        f.full_range = true;
        (ssx, ssy) = (false, false);
        if profile == 1 || profile == 3 {
            b.skip(1)?;
        }
    }
    f.chroma = match (ssx, ssy) {
        (true, true) => 1,
        (true, false) => 2,
        _ => 3,
    };
    f.width = b.u(16)? as u32 + 1;
    f.height = b.u(16)? as u32 + 1;
    Some(f)
}

/// A VP9 chunk's key frame header if it has one, else its first frame's.
pub fn vp9_chunk(d: &[u8]) -> Option<FrameInfo> {
    let frames: Vec<FrameInfo> = vp9_frames(d).into_iter().filter_map(vp9_frame).collect();
    frames.iter().find(|f| f.key).or(frames.first()).copied()
}

/// Reads a VP8 frame's header: key frame or not, and a key frame's size.
pub fn vp8_frame(d: &[u8]) -> Option<FrameInfo> {
    let tag = *d.first()?;
    let mut f = FrameInfo { profile: (tag >> 1) & 7, bit_depth: 8, chroma: 1, ..Default::default() };
    f.key = tag & 1 == 0;
    if !f.key {
        return Some(f);
    }
    if d.get(3..6)? != [0x9d, 0x01, 0x2a] {
        return None;
    }
    f.width = (u16::from_le_bytes([d[6], *d.get(7)?]) & 0x3fff) as u32;
    f.height = (u16::from_le_bytes([*d.get(8)?, *d.get(9)?]) & 0x3fff) as u32;
    Some(f)
}

/// What a vpcC record (with or without its version and flags) says: (profile, level, bit
/// depth, chroma subsampling, full range, primaries, transfer, matrix).
pub fn read_vpcc(r: &[u8]) -> Option<[u8; 8]> {
    // with the full box's version and flags first: version 1, flags 0
    let r = if r.len() >= 12 && r[0] == 1 && r[1..4] == [0, 0, 0] { &r[4..] } else { r };
    if r.len() < 6 {
        return None;
    }
    Some([r[0], r[1], r[2] >> 4, (r[2] >> 1) & 7, r[2] & 1, r[3], r[4], r[5]])
}

/// A vpcC record (version 1, with version and flags) for a stream: the sender's own values
/// where it gave them, the key frame's otherwise.
pub fn build_vpcc(sent: Option<[u8; 8]>, f: &FrameInfo, level: u8) -> Vec<u8> {
    let [profile, lvl, depth, chroma, range, cp, tc, mc] =
        sent.unwrap_or([f.profile, 0, f.bit_depth, f.chroma, f.full_range as u8, 2, 2, 2]);
    let lvl = if lvl == 0 { level } else { lvl };
    let depth = if depth == 0 { f.bit_depth.max(8) } else { depth };
    vec![1, 0, 0, 0, profile, lvl, (depth << 4) | ((chroma & 7) << 1) | (range & 1), cp, tc, mc, 0, 0]
}

/// The smallest VP9 level whose picture size takes this many pixels ("4.0" is 40).
pub fn vp9_level_for(width: u32, height: u32) -> u8 {
    let pixels = width as u64 * height as u64;
    const LEVELS: [(u64, u8); 9] = [
        (36_864, 10),
        (73_728, 11),
        (122_880, 20),
        (245_760, 21),
        (552_960, 30),
        (983_040, 31),
        (2_228_224, 41),
        (8_912_896, 51),
        (35_651_584, 61),
    ];
    LEVELS.iter().find(|(max, _)| pixels <= *max).map_or(62, |(_, l)| *l)
}

/// The codec record the lab passes on for VP9 and VP8: a vpcC record (12 bytes) and the
/// picture's width and height (two bytes each), since vpcC doesn't say the size.
pub fn build_record(vpcc: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = vpcc.to_vec();
    out.extend((width as u16).to_be_bytes());
    out.extend((height as u16).to_be_bytes());
    out
}

/// The vpcC part of a record made by [`build_record`].
pub fn record_vpcc(r: &[u8]) -> &[u8] {
    &r[..r.len().saturating_sub(4)]
}

/// What the stats show for a VP9 or VP8 record made by [`build_record`].
pub fn info(vp8: bool, record: &[u8]) -> Option<Sps> {
    if record.len() < 16 {
        return None;
    }
    let n = record.len();
    let width = u16::from_be_bytes([record[n - 4], record[n - 3]]) as u32;
    let height = u16::from_be_bytes([record[n - 2], record[n - 1]]) as u32;
    let [profile, level, depth, chroma, range, cp, tc, mc] = read_vpcc(record_vpcc(record))?;
    // 2 everywhere is "unspecified": the sender didn't describe its colour
    let described = (cp, tc, mc) != (2, 2, 2);
    let codec_string = if vp8 { "vp8".to_string() } else { format!("vp09.{profile:02}.{level:02}.{depth:02}") };
    Some(Sps {
        profile: if vp8 { format!("version {profile}") } else { format!("Profile {profile}") },
        profile_idc: profile,
        level: if vp8 || level == 0 { String::new() } else { format!("{}.{}", level / 10, level % 10) },
        level_idc: level,
        width,
        height,
        chroma_format: match chroma {
            0 | 1 => 1,
            2 => 2,
            _ => 3,
        },
        bit_depth: depth,
        bit_depth_chroma: depth,
        vui: described,
        colour: crate::av1::colour(
            range == 1,
            described.then_some((cp, tc, mc)),
            // 4:2:0 vertical (H.273 location 0) or co-located (location 2)
            match chroma {
                0 => Some(0),
                1 => Some(2),
                _ => None,
            },
        ),
        declared_fps: None,
        max_reorder: Some(0),
        interlaced: false,
        codec_string,
        ..Default::default()
    })
}
