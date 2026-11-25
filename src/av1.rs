//! AV1 bitstream reading: OBUs, the sequence header (profile, level, tier, size, bit depth,
//! colour, frame rate) and whether a temporal unit starts with a key frame. The codec
//! configuration record (av1C) is read and made here too. Written for measuring a stream, not
//! decoding it.

use crate::nal::{Bits, Sps};

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_FRAME: u8 = 6;
pub const OBU_PADDING: u8 = 15;

/// Reads a LEB128 number: (value, bytes used).
pub fn leb128(d: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *d.get(i)?;
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// A LEB128 number as bytes.
pub fn write_leb128(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// One OBU: its type, the header bytes (one or two), its payload, and all its bytes.
#[derive(Debug, Clone, Copy)]
pub struct Obu<'a> {
    pub kind: u8,
    pub header: &'a [u8],
    pub payload: &'a [u8],
    pub whole: &'a [u8],
}

/// Reads one OBU at the start of `d`. An OBU without a size field runs to the end of `d`.
pub fn obu(d: &[u8]) -> Option<Obu<'_>> {
    let h = *d.first()?;
    let kind = (h >> 3) & 0x0f;
    let ext = h & 0x04 != 0;
    let has_size = h & 0x02 != 0;
    let hlen = if ext { 2 } else { 1 };
    if d.len() < hlen {
        return None;
    }
    let (size, used) = if has_size { leb128(&d[hlen..])? } else { ((d.len() - hlen) as u64, 0) };
    let start = hlen + used;
    let end = start.checked_add(size as usize)?;
    if end > d.len() {
        return None;
    }
    Some(Obu { kind, header: &d[..hlen], payload: &d[start..end], whole: &d[..end] })
}

/// The OBUs of a temporal unit in the low-overhead format (every OBU with its size).
pub fn obus(mut d: &[u8]) -> Vec<Obu<'_>> {
    let mut out = vec![];
    while !d.is_empty() {
        let Some(o) = obu(d) else { break };
        let n = o.whole.len();
        out.push(o);
        if n == 0 {
            break;
        }
        d = &d[n..];
    }
    out
}

/// An OBU written with its size field (for one that came without).
pub fn with_size(header: &[u8], payload: &[u8], out: &mut Vec<u8>) {
    let Some(&h) = header.first() else { return };
    out.push(h | 0x02);
    out.extend(&header[1..]);
    write_leb128(out, payload.len() as u64);
    out.extend(payload);
}

/// A temporal unit as the lab passes it on: every OBU with its size, temporal delimiters and
/// padding left out (as Matroska wants it).
pub fn normalise(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len());
    for o in obus(d) {
        if o.kind == OBU_TEMPORAL_DELIMITER || o.kind == OBU_PADDING {
            continue;
        }
        out.extend(o.whole);
    }
    out
}

/// What a sequence header says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SeqHeader {
    pub profile: u8,
    pub level_idx: u8,
    pub tier: u8,
    pub reduced_still_picture_header: bool,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub mono: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub color_primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
    pub fps: Option<f64>,
    pub initial_presentation_delay: Option<u8>,
}

fn uvlc(b: &mut Bits) -> Option<u64> {
    let mut zeros = 0;
    while !b.flag()? {
        zeros += 1;
        if zeros >= 32 {
            return None;
        }
    }
    Some(b.u(zeros)? + (1u64 << zeros) - 1)
}

/// Reads a sequence header OBU's payload.
pub fn parse_seq_header(p: &[u8]) -> Option<SeqHeader> {
    let mut b = Bits::new(p);
    let mut s = SeqHeader { profile: b.u(3)? as u8, ..Default::default() };
    b.skip(1)?; // still_picture
    s.reduced_still_picture_header = b.flag()?;
    if s.reduced_still_picture_header {
        s.level_idx = b.u(5)? as u8;
    } else {
        let timing = b.flag()?;
        let mut decoder_model = false;
        let mut delay_len = 0;
        if timing {
            let units = b.u(32)?;
            let scale = b.u(32)?;
            let equal = b.flag()?;
            let mut ticks = 1;
            if equal {
                ticks = uvlc(&mut b)? + 1;
            }
            if equal && units > 0 {
                s.fps = Some(scale as f64 / (units * ticks) as f64);
            }
            decoder_model = b.flag()?;
            if decoder_model {
                delay_len = b.u(5)? as usize + 1;
                b.skip(32 + 5 + 5)?;
            }
        }
        let display_delay = b.flag()?;
        let points = b.u(5)? + 1;
        for i in 0..points {
            b.skip(12)?; // operating_point_idc
            let level = b.u(5)? as u8;
            let tier = if level > 7 { b.u(1)? as u8 } else { 0 };
            if i == 0 {
                s.level_idx = level;
                s.tier = tier;
            }
            if decoder_model && b.flag()? {
                b.skip(delay_len * 2 + 1)?;
            }
            if display_delay && b.flag()? {
                let d = b.u(4)? as u8;
                if i == 0 {
                    s.initial_presentation_delay = Some(d);
                }
            }
        }
    }
    let wbits = b.u(4)? as usize + 1;
    let hbits = b.u(4)? as usize + 1;
    s.width = b.u(wbits)? as u32 + 1;
    s.height = b.u(hbits)? as u32 + 1;
    if !s.reduced_still_picture_header && b.flag()? {
        b.skip(4 + 3)?; // frame id lengths
    }
    b.skip(3)?; // 128x128 superblocks, filter intra, intra edge filter
    if !s.reduced_still_picture_header {
        b.skip(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = b.flag()?;
        if order_hint {
            b.skip(2)?; // jnt comp, ref frame mvs
        }
        let force_screen_content = if b.flag()? { 2 } else { b.u(1)? };
        if force_screen_content > 0 && !b.flag()? {
            b.skip(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.skip(3)?;
        }
    }
    b.skip(3)?; // superres, cdef, restoration
    // colour_config
    let high = b.flag()?;
    s.bit_depth = if s.profile == 2 && high {
        if b.flag()? { 12 } else { 10 }
    } else if high {
        10
    } else {
        8
    };
    s.mono = if s.profile == 1 { false } else { b.flag()? };
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if b.flag()? {
        cp = b.u(8)? as u8;
        tc = b.u(8)? as u8;
        mc = b.u(8)? as u8;
    }
    s.color_primaries = cp;
    s.transfer = tc;
    s.matrix = mc;
    if s.mono {
        s.full_range = b.flag()?;
        s.subsampling_x = true;
        s.subsampling_y = true;
    } else if cp == 1 && tc == 13 && mc == 0 {
        s.full_range = true;
    } else {
        s.full_range = b.flag()?;
        match s.profile {
            0 => (s.subsampling_x, s.subsampling_y) = (true, true),
            1 => {}
            _ if s.bit_depth == 12 => {
                s.subsampling_x = b.flag()?;
                s.subsampling_y = s.subsampling_x && b.flag()?;
            }
            _ => s.subsampling_x = true,
        }
        if s.subsampling_x && s.subsampling_y {
            s.chroma_sample_position = b.u(2)? as u8;
        }
    }
    Some(s)
}

/// The first sequence header among a temporal unit's OBUs (or a config record's).
pub fn find_seq_header(d: &[u8]) -> Option<(SeqHeader, Vec<u8>)> {
    obus(d).into_iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).and_then(|o| Some((parse_seq_header(o.payload)?, o.whole.to_vec())))
}

/// Whether a temporal unit holds a shown key frame. `reduced` is the sequence header's
/// reduced_still_picture_header (every frame is then a key frame).
pub fn is_key(d: &[u8], reduced: bool) -> bool {
    for o in obus(d) {
        if o.kind != OBU_FRAME && o.kind != OBU_FRAME_HEADER {
            continue;
        }
        if reduced {
            return true;
        }
        let mut b = Bits::new(o.payload);
        // show_existing_frame, frame_type (0: KEY_FRAME), show_frame
        let (Some(existing), Some(ty), Some(shown)) = (b.flag(), b.u(2), b.flag()) else { return false };
        return !existing && ty == 0 && shown;
    }
    false
}

/// The av1C record's header (4 bytes) for a sequence header.
fn record_header(s: &SeqHeader) -> [u8; 4] {
    let high = s.bit_depth > 8;
    let twelve = s.bit_depth == 12;
    [
        0x81,
        (s.profile << 5) | (s.level_idx & 0x1f),
        (s.tier << 7)
            | ((high as u8) << 6)
            | ((twelve as u8) << 5)
            | ((s.mono as u8) << 4)
            | ((s.subsampling_x as u8) << 3)
            | ((s.subsampling_y as u8) << 2)
            | (s.chroma_sample_position & 3),
        s.initial_presentation_delay.map_or(0, |d| 0x10 | (d & 0x0f)),
    ]
}

/// An AV1CodecConfigurationRecord holding this sequence header OBU.
pub fn build_av1c(s: &SeqHeader, seq_obu: &[u8]) -> Vec<u8> {
    let mut out = record_header(s).to_vec();
    out.extend(seq_obu);
    out
}

/// The sequence header in an av1C record's configOBUs, if it has one.
pub fn read_av1c(r: &[u8]) -> Option<(SeqHeader, Vec<u8>)> {
    if r.len() < 4 || r[0] & 0x80 == 0 {
        return None;
    }
    find_seq_header(&r[4..])
}

/// The level as people say it ("4.0"); index 31 is "no level".
pub fn level_name(idx: u8) -> String {
    if idx == 31 { "max".into() } else { format!("{}.{}", 2 + (idx >> 2), idx & 3) }
}

/// What the stats show for a sequence header, in the shape the H.264 and HEVC path fills.
pub fn info(s: &SeqHeader) -> Sps {
    let profile = match s.profile {
        0 => "Main".to_string(),
        1 => "High".to_string(),
        2 => "Professional".to_string(),
        n => format!("profile {n}"),
    };
    Sps {
        profile,
        profile_idc: s.profile,
        level: format!("{}{}", level_name(s.level_idx), if s.tier == 1 { " High tier" } else { "" }),
        level_idc: s.level_idx,
        width: s.width,
        height: s.height,
        chroma_format: if s.mono {
            0
        } else {
            match (s.subsampling_x, s.subsampling_y) {
                (true, true) => 1,
                (true, false) => 2,
                _ => 3,
            }
        },
        bit_depth: s.bit_depth,
        declared_fps: s.fps,
        // a temporal unit is decoded and shown in the same order
        max_reorder: Some(0),
        interlaced: false,
        codec_string: codec_string(s),
        ..Default::default()
    }
}

/// The WebCodecs codec string: "av01.0.08M.08".
pub fn codec_string(s: &SeqHeader) -> String {
    format!("av01.{}.{:02}{}.{:02}", s.profile, s.level_idx, if s.tier == 1 { 'H' } else { 'M' }, s.bit_depth)
}
