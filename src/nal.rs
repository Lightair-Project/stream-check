//! H.264 and HEVC bitstream reading: NAL units, parameter sets (profile, level, size, timing,
//! reordering) and the slice type of a picture. Written for measuring a stream, not decoding it.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VCodec {
    H264,
    Hevc,
    Av1,
    Vp9,
    Vp8,
}

impl VCodec {
    pub fn name(self) -> &'static str {
        match self {
            VCodec::H264 => "H.264",
            VCodec::Hevc => "HEVC",
            VCodec::Av1 => "AV1",
            VCodec::Vp9 => "VP9",
            VCodec::Vp8 => "VP8",
        }
    }

    /// A codec whose pictures are NAL units (H.264, HEVC).
    pub fn is_nal(self) -> bool {
        matches!(self, VCodec::H264 | VCodec::Hevc)
    }
}

/// Splits an Annex B byte stream (start codes) into NAL units, start codes removed.
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = vec![];
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = vec![];
    for (n, &s) in starts.iter().enumerate() {
        let mut e = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Splits length-prefixed NAL units (`size` bytes of length each).
pub fn split_lengths(data: &[u8], size: usize) -> Vec<&[u8]> {
    let mut out = vec![];
    let mut i = 0;
    while i + size <= data.len() {
        let n = data[i..i + size].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        i += size;
        if n == 0 || i + n > data.len() {
            break;
        }
        out.push(&data[i..i + n]);
        i += n;
    }
    out
}

/// NAL units as 4-byte length-prefixed bytes.
pub fn to_lengths(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![];
    for n in nals {
        out.extend((n.len() as u32).to_be_bytes());
        out.extend(*n);
    }
    out
}

pub fn nal_type(codec: VCodec, nal: &[u8]) -> u8 {
    match codec {
        VCodec::H264 => nal[0] & 0x1f,
        VCodec::Hevc => (nal[0] >> 1) & 0x3f,
        _ => 0,
    }
}

pub fn is_parameter_set(codec: VCodec, t: u8) -> bool {
    match codec {
        VCodec::H264 => matches!(t, 7 | 8),
        VCodec::Hevc => matches!(t, 32..=34),
        _ => false,
    }
}

/// A slice NAL (picture data) of this codec.
pub fn is_slice(codec: VCodec, t: u8) -> bool {
    match codec {
        VCodec::H264 => matches!(t, 1 | 5),
        VCodec::Hevc => t < 32 && (t <= 9 || (16..=21).contains(&t)),
        _ => false,
    }
}

pub fn is_keyframe_nal(codec: VCodec, t: u8) -> bool {
    match codec {
        VCodec::H264 => t == 5,
        VCodec::Hevc => (16..=21).contains(&t),
        _ => false,
    }
}

/// Removes emulation prevention bytes (00 00 03).
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

pub struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0 }
    }
    pub fn u(&mut self, n: usize) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self.d.get(self.pos / 8)?;
            v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u64;
            self.pos += 1;
        }
        Some(v)
    }
    pub fn flag(&mut self) -> Option<bool> {
        Some(self.u(1)? == 1)
    }
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.d.len() * 8).then_some(())
    }
    pub fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        Some((1u64 << zeros) - 1 + self.u(zeros)?)
    }
    pub fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// What a sequence parameter set says about a stream.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Sps {
    pub profile: String,
    pub profile_idc: u8,
    /// Level as shown to people: "4.1".
    pub level: String,
    pub level_idc: u8,
    pub width: u32,
    pub height: u32,
    pub chroma_format: u8,
    pub bit_depth: u8,
    /// Frame rate the stream declares (VUI timing), if it does.
    pub declared_fps: Option<f64>,
    /// Pictures that may precede one in output order (0: no B-frames or reordering).
    pub max_reorder: Option<u32>,
    pub interlaced: bool,
    /// The decoder's codec string for WebCodecs ("avc1.640028", "hvc1.1.6.L120.90").
    pub codec_string: String,
    #[serde(skip)]
    pub general: [u8; 12],
}

fn h264_profile_name(p: u8, constraint: u8) -> String {
    match p {
        66 if constraint & 0x40 != 0 => "Constrained Baseline".into(),
        66 => "Baseline".into(),
        77 => "Main".into(),
        88 => "Extended".into(),
        100 => "High".into(),
        110 => "High 10".into(),
        122 => "High 4:2:2".into(),
        244 => "High 4:4:4".into(),
        n => format!("profile {n}"),
    }
}

fn hrd(b: &mut Bits) -> Option<()> {
    let cpb = b.ue()? + 1;
    b.skip(8)?;
    for _ in 0..cpb {
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
    }
    b.skip(20)
}

pub fn parse_h264_sps(nal: &[u8]) -> Option<Sps> {
    let d = unescape(nal);
    if d.len() < 4 {
        return None;
    }
    let (profile_idc, constraint, level_idc) = (d[1], d[2], d[3]);
    let mut b = Bits::new(&d[4..]);
    b.ue()?;
    let mut chroma = 1;
    let mut depth = 8;
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = b.ue()? as u8;
        if chroma == 3 {
            b.skip(1)?;
        }
        depth = 8 + b.ue()? as u8;
        b.ue()?;
        b.skip(1)?;
        if b.flag()? {
            for i in 0..(if chroma != 3 { 8 } else { 12 }) {
                if b.flag()? {
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..(if i < 6 { 16 } else { 64 }) {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    b.ue()?;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.skip(1)?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?;
    b.skip(1)?;
    let w_mbs = b.ue()? + 1;
    let h_map = b.ue()? + 1;
    let frame_mbs_only = b.flag()?;
    if !frame_mbs_only {
        b.skip(1)?;
    }
    b.skip(1)?;
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if b.flag()? {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let (sub_w, sub_h) = match chroma {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_unit_y = sub_h * if frame_mbs_only { 1 } else { 2 };
    let width = (w_mbs * 16 - (cl + cr) * sub_w) as u32;
    let height = (h_map * 16 * if frame_mbs_only { 1 } else { 2 } - (ct + cb) * crop_unit_y) as u32;

    let (mut declared_fps, mut max_reorder) = (None, None);
    if b.flag().unwrap_or(false) {
        let _ = (|| -> Option<()> {
            if b.flag()? && b.u(8)? == 255 {
                b.skip(32)?;
            }
            if b.flag()? {
                b.skip(1)?;
            }
            if b.flag()? {
                b.skip(4)?;
                if b.flag()? {
                    b.skip(24)?;
                }
            }
            if b.flag()? {
                b.ue()?;
                b.ue()?;
            }
            if b.flag()? {
                let units = b.u(32)?;
                let scale = b.u(32)?;
                if units > 0 {
                    declared_fps = Some(scale as f64 / (2 * units) as f64);
                }
                b.skip(1)?;
            }
            let nal_hrd = b.flag()?;
            if nal_hrd {
                hrd(&mut b)?;
            }
            let vcl_hrd = b.flag()?;
            if vcl_hrd {
                hrd(&mut b)?;
            }
            if nal_hrd || vcl_hrd {
                b.skip(1)?;
            }
            b.skip(1)?;
            if b.flag()? {
                b.skip(1)?;
                b.ue()?;
                b.ue()?;
                b.ue()?;
                b.ue()?;
                max_reorder = Some(b.ue()? as u32);
            }
            Some(())
        })();
    }
    Some(Sps {
        profile: h264_profile_name(profile_idc, constraint),
        profile_idc,
        level: if level_idc == 11 && constraint & 0x10 != 0 { "1b".into() } else { format!("{}.{}", level_idc / 10, level_idc % 10) },
        level_idc,
        width,
        height,
        chroma_format: chroma,
        bit_depth: depth,
        declared_fps,
        max_reorder,
        interlaced: !frame_mbs_only,
        codec_string: format!("avc1.{profile_idc:02x}{constraint:02x}{level_idc:02x}"),
        general: [0; 12],
    })
}

pub fn parse_hevc_sps(nal: &[u8]) -> Option<Sps> {
    let d = unescape(nal);
    if d.len() < 16 {
        return None;
    }
    let mut b = Bits::new(&d[2..]);
    b.skip(4)?;
    let max_sub = b.u(3)? as usize;
    b.skip(1)?;
    let profile_space = b.u(2)? as u8;
    let tier = b.u(1)? as u8;
    let profile_idc = b.u(5)? as u8;
    let compat = b.u(32)? as u32;
    let flags = b.u(48)?; // progressive, interlaced, non-packed, frame-only + 44 bits
    let level_idc = b.u(8)? as u8;
    let (mut sub_profile, mut sub_level) = ([false; 8], [false; 8]);
    for i in 0..max_sub {
        sub_profile[i] = b.flag()?;
        sub_level[i] = b.flag()?;
    }
    if max_sub > 0 {
        b.skip(2 * (8 - max_sub))?;
    }
    for i in 0..max_sub {
        if sub_profile[i] {
            b.skip(88)?;
        }
        if sub_level[i] {
            b.skip(8)?;
        }
    }
    b.ue()?;
    let chroma = b.ue()? as u8;
    if chroma == 3 {
        b.skip(1)?;
    }
    let (w, h) = (b.ue()?, b.ue()?);
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if b.flag()? {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let (sw, sh) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let depth = 8 + b.ue()? as u8;
    b.ue()?;
    b.ue()?;
    let present = b.flag()?;
    let mut reorder = None;
    let from = if present { 0 } else { max_sub };
    for i in from..=max_sub {
        b.ue()?;
        let r = b.ue()?;
        b.ue()?;
        if i == max_sub {
            reorder = Some(r as u32);
        }
    }
    let profile_name = match profile_idc {
        1 => "Main",
        2 => "Main 10",
        3 => "Main Still Picture",
        4 => "Range Extensions",
        n => return_profile(n),
    };
    // the "general" bytes of the codec string: compatibility flags, reversed bit order
    let compat_rev = compat.reverse_bits();
    let mut general = [0u8; 12];
    general[..4].copy_from_slice(&compat.to_be_bytes());
    general[4..10].copy_from_slice(&flags.to_be_bytes()[2..8]);
    let mut constraint = flags.to_be_bytes()[2..8].to_vec();
    while constraint.last() == Some(&0) {
        constraint.pop();
    }
    let space = ["", "A", "B", "C"][profile_space as usize];
    let mut codec_string = format!("hvc1.{space}{profile_idc}.{compat_rev:X}.{}{level_idc}", if tier == 1 { "H" } else { "L" });
    for c in constraint {
        codec_string.push_str(&format!(".{c:X}"));
    }
    Some(Sps {
        profile: profile_name.into(),
        profile_idc,
        level: format!("{}.{}", level_idc / 30, (level_idc % 30) / 3),
        level_idc,
        width: (w - (cl + cr) * sw) as u32,
        height: (h - (ct + cb) * sh) as u32,
        chroma_format: chroma,
        bit_depth: depth,
        declared_fps: None,
        max_reorder: reorder,
        interlaced: flags >> 46 & 1 == 1 && flags >> 47 & 1 == 0,
        codec_string,
        general,
    })
}

fn return_profile(n: u8) -> &'static str {
    match n {
        9 => "Screen Content",
        _ => "other",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PicType {
    I,
    P,
    B,
}

/// What the HEVC parameter sets say about slice headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct HevcPps {
    pub dependent_slices: bool,
    pub output_flag_present: bool,
    pub extra_bits: u8,
}

pub fn parse_hevc_pps(nal: &[u8]) -> Option<(u64, HevcPps)> {
    let d = unescape(nal);
    let mut b = Bits::new(d.get(2..)?);
    let id = b.ue()?;
    b.ue()?;
    let dependent_slices = b.flag()?;
    let output_flag_present = b.flag()?;
    let extra_bits = b.u(3)? as u8;
    Some((id, HevcPps { dependent_slices, output_flag_present, extra_bits }))
}

/// The type of the picture a slice NAL starts (first slice of a picture only).
pub fn slice_type(codec: VCodec, nal: &[u8], pps: &[(u64, HevcPps)]) -> Option<PicType> {
    let d = unescape(&nal[..nal.len().min(64)]);
    match codec {
        VCodec::H264 => {
            let mut b = Bits::new(d.get(1..)?);
            b.ue()?;
            Some(match b.ue()? % 5 {
                0 => PicType::P,
                1 => PicType::B,
                _ => PicType::I,
            })
        }
        VCodec::Hevc => {
            let t = nal_type(codec, nal);
            let mut b = Bits::new(d.get(2..)?);
            if !b.flag()? {
                return None;
            }
            if (16..=23).contains(&t) {
                b.skip(1)?;
            }
            let id = b.ue()?;
            let p = pps.iter().find(|(i, _)| *i == id).map(|(_, p)| *p).unwrap_or_default();
            b.skip(p.extra_bits as usize)?;
            Some(match b.ue()? {
                0 => PicType::B,
                1 => PicType::P,
                _ => PicType::I,
            })
        }
        _ => None,
    }
}

/// AVCDecoderConfigurationRecord from parameter sets (4-byte NAL lengths).
pub fn build_avcc(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Option<Vec<u8>> {
    let s = sps.first()?;
    if s.len() < 4 {
        return None;
    }
    let mut out = vec![1, s[1], s[2], s[3], 0xff, 0xe0 | sps.len() as u8];
    for s in sps {
        out.extend((s.len() as u16).to_be_bytes());
        out.extend(s);
    }
    out.push(pps.len() as u8);
    for p in pps {
        out.extend((p.len() as u16).to_be_bytes());
        out.extend(p);
    }
    Some(out)
}

/// HEVCDecoderConfigurationRecord from parameter sets (4-byte NAL lengths).
pub fn build_hvcc(vps: &[Vec<u8>], sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Option<Vec<u8>> {
    let info = parse_hevc_sps(sps.first()?)?;
    let mut out = vec![1];
    // profile space/tier/idc, compatibility flags, constraint flags, level
    let d = unescape(&sps[0]);
    out.push(d[3]);
    out.extend(&info.general[..4]);
    out.extend(&info.general[4..10]);
    out.push(info.level_idc);
    out.extend([0xf0, 0, 0xfc, 0xfc | info.chroma_format]);
    out.push(0xf8 | (info.bit_depth - 8));
    out.push(0xf8 | (info.bit_depth - 8));
    out.extend([0, 0, 0x0f]);
    let arrays: Vec<(u8, &[Vec<u8>])> = [(32, vps), (33, sps), (34, pps)].into_iter().filter(|(_, v)| !v.is_empty()).collect();
    out.push(arrays.len() as u8);
    for (t, set) in arrays {
        out.push(0x80 | t);
        out.extend((set.len() as u16).to_be_bytes());
        for n in set {
            out.extend((n.len() as u16).to_be_bytes());
            out.extend(n);
        }
    }
    Some(out)
}

/// Parameter sets out of an avcC or hvcC record: (vps, sps, pps).
pub type ParameterSets = (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>);

pub fn read_avcc(r: &[u8]) -> Option<ParameterSets> {
    if r.len() < 7 {
        return None;
    }
    let mut i = 5;
    let mut sps = vec![];
    for _ in 0..(r[i] & 0x1f) {
        let n = u16::from_be_bytes([*r.get(i + 1)?, *r.get(i + 2)?]) as usize;
        sps.push(r.get(i + 3..i + 3 + n)?.to_vec());
        i += 2 + n;
    }
    i += 1;
    let mut pps = vec![];
    for _ in 0..*r.get(i)? {
        let n = u16::from_be_bytes([*r.get(i + 1)?, *r.get(i + 2)?]) as usize;
        pps.push(r.get(i + 3..i + 3 + n)?.to_vec());
        i += 2 + n;
    }
    Some((vec![], sps, pps))
}

pub fn read_hvcc(r: &[u8]) -> Option<ParameterSets> {
    if r.len() < 23 {
        return None;
    }
    let mut i = 22;
    let (mut vps, mut sps, mut pps) = (vec![], vec![], vec![]);
    for _ in 0..r[i] {
        let t = r.get(i + 1)? & 0x3f;
        let count = u16::from_be_bytes([*r.get(i + 2)?, *r.get(i + 3)?]) as usize;
        i += 3;
        for _ in 0..count {
            let n = u16::from_be_bytes([*r.get(i + 1)?, *r.get(i + 2)?]) as usize;
            let nal = r.get(i + 3..i + 3 + n)?.to_vec();
            match t {
                32 => vps.push(nal),
                33 => sps.push(nal),
                34 => pps.push(nal),
                _ => {}
            }
            i += 2 + n;
        }
    }
    Some((vps, sps, pps))
}

pub fn parse_sps(codec: VCodec, nal: &[u8]) -> Option<Sps> {
    match codec {
        VCodec::H264 => parse_h264_sps(nal),
        VCodec::Hevc => parse_hevc_sps(nal),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1280x720 High@3.1 from x264 (with VUI timing 30 fps)
    const H264_SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0,
        0xf1, 0x83, 0x19, 0x60,
    ];

    #[test]
    fn an_h264_sps_gives_profile_level_and_size() {
        let s = parse_h264_sps(H264_SPS).unwrap();
        assert_eq!((s.profile.as_str(), s.level.as_str()), ("High", "3.1"));
        assert_eq!((s.width, s.height), (1280, 720));
        assert_eq!(s.codec_string, "avc1.64001f");
        assert!(s.declared_fps.is_some());
    }

    #[test]
    fn annexb_and_length_prefixed_units_split_the_same() {
        let a = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        let nals = split_annexb(&a);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
        let l = to_lengths(&nals);
        assert_eq!(split_lengths(&l, 4), nals);
    }

    #[test]
    fn slice_types_are_read() {
        // first_mb 0 (1), slice_type 7 = I (0001000 -> ue 7)
        assert_eq!(slice_type(VCodec::H264, &[0x65, 0b1000_1000], &[]), Some(PicType::I));
        // first_mb 0 (1), slice_type 5 = P (00110 -> 5)
        assert_eq!(slice_type(VCodec::H264, &[0x41, 0b1001_1000], &[]), Some(PicType::P));
        // slice_type 6 = B (00111)
        assert_eq!(slice_type(VCodec::H264, &[0x01, 0b1001_1100], &[]), Some(PicType::B));
    }

    #[test]
    fn avcc_roundtrips() {
        let sps = vec![H264_SPS.to_vec()];
        let pps = vec![vec![0x68, 0xeb, 0xe3, 0xcb]];
        let rec = build_avcc(&sps, &pps).unwrap();
        let (_, s, p) = read_avcc(&rec).unwrap();
        assert_eq!((s, p), (sps, pps));
    }
}
