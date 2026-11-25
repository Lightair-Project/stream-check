//! RTP payloads into pictures, per codec: H.264 (RFC 6184), HEVC (RFC 7798), AV1 (the AV1 RTP
//! payload format), VP8 (RFC 7741) and VP9 (RFC 9628). Packets come in sequence order, one
//! picture's at a time; a picture's units come out as NAL units (H.264, HEVC) or as one frame
//! (AV1: a temporal unit of OBUs each with its size; VP9: a frame or a superframe; VP8: a frame).

use crate::av1;
use crate::nal::VCodec;
use crate::vp9;
use crate::whip::H264Depay;

/// HEVC RTP payloads into NAL units (single, aggregation packets, fragmentation units). Senders
/// that use DONL (sprop-max-don-diff above 0) aren't read.
#[derive(Default)]
pub struct HevcDepay {
    fu: Vec<u8>,
    in_fu: bool,
    pub nals: Vec<Vec<u8>>,
}

impl HevcDepay {
    pub fn push(&mut self, p: &[u8]) {
        if p.len() < 3 {
            return;
        }
        match (p[0] >> 1) & 0x3f {
            48 => {
                let mut i = 2;
                while i + 2 <= p.len() {
                    let n = u16::from_be_bytes([p[i], p[i + 1]]) as usize;
                    i += 2;
                    if n == 0 || i + n > p.len() {
                        break;
                    }
                    self.nals.push(p[i..i + n].to_vec());
                    i += n;
                }
            }
            49 => {
                let fu = p[2];
                if fu & 0x80 != 0 {
                    // the NAL header back: the FU's own, with the fragment's type
                    self.fu.clear();
                    self.fu.push((p[0] & 0x81) | ((fu & 0x3f) << 1));
                    self.fu.push(p[1]);
                    self.in_fu = true;
                }
                if self.in_fu {
                    self.fu.extend(&p[3..]);
                    if fu & 0x40 != 0 {
                        self.nals.push(std::mem::take(&mut self.fu));
                        self.in_fu = false;
                    }
                }
            }
            // PACI: not used by WebRTC senders
            50 => {}
            _ => self.nals.push(p.to_vec()),
        }
    }
}

/// AV1 RTP payloads into a temporal unit: OBU elements (fragments joined across packets) written
/// back with their size fields, temporal delimiters left out.
#[derive(Default)]
pub struct Av1Depay {
    /// An OBU element still being built (it went on into the next packet).
    pending: Option<Vec<u8>>,
    pub tu: Vec<u8>,
}

impl Av1Depay {
    fn element(&mut self, e: &[u8]) {
        let Some(&h) = e.first() else { return };
        let hlen = if h & 0x04 != 0 { 2 } else { 1 };
        if e.len() < hlen || (h >> 3) & 0x0f == av1::OBU_TEMPORAL_DELIMITER {
            return;
        }
        if h & 0x02 != 0 {
            if let Some(o) = av1::obu(e) {
                self.tu.extend(o.whole);
            }
        } else {
            av1::with_size(&e[..hlen], &e[hlen..], &mut self.tu);
        }
    }

    pub fn push(&mut self, p: &[u8]) {
        let Some(&agg) = p.first() else { return };
        let (z, y, w) = (agg & 0x80 != 0, agg & 0x40 != 0, ((agg >> 4) & 3) as usize);
        let mut i = 1;
        let mut k = 0;
        while i < p.len() {
            let last_by_count = w != 0 && k == w - 1;
            let len = if last_by_count {
                p.len() - i
            } else {
                let Some((n, used)) = av1::leb128(&p[i..]) else { return };
                i += used;
                n as usize
            };
            let Some(e) = p.get(i..i + len) else { return };
            i += len;
            let is_last = last_by_count || i >= p.len();
            let mut whole = if k == 0 && z {
                // the rest of the element the previous packet began (none if that packet was lost)
                match self.pending.take() {
                    Some(mut started) => {
                        started.extend(e);
                        started
                    }
                    None => {
                        k += 1;
                        continue;
                    }
                }
            } else {
                e.to_vec()
            };
            if is_last && y {
                self.pending = Some(std::mem::take(&mut whole));
            } else {
                self.element(&whole);
            }
            k += 1;
            if last_by_count {
                break;
            }
        }
    }
}

/// VP8 RTP payloads into a frame: the payload descriptor skipped.
#[derive(Default)]
pub struct Vp8Depay {
    pub frame: Vec<u8>,
}

impl Vp8Depay {
    pub fn push(&mut self, p: &[u8]) {
        let Some(&b0) = p.first() else { return };
        let mut i = 1;
        if b0 & 0x80 != 0 {
            let Some(&x) = p.get(1) else { return };
            i += 1;
            if x & 0x80 != 0 {
                i += if p.get(i).is_some_and(|b| b & 0x80 != 0) { 2 } else { 1 };
            }
            if x & 0x40 != 0 {
                i += 1;
            }
            if x & 0x30 != 0 {
                i += 1;
            }
        }
        // the start of partition 0 is the start of a frame
        if b0 & 0x10 != 0 && b0 & 0x07 == 0 {
            self.frame.clear();
        }
        if let Some(payload) = p.get(i..) {
            self.frame.extend(payload);
        }
    }
}

/// VP9 RTP payloads into a frame, or a superframe when a picture has several (spatial layers):
/// the payload descriptor (and a scalability structure) skipped.
#[derive(Default)]
pub struct Vp9Depay {
    cur: Vec<u8>,
    pub frames: Vec<Vec<u8>>,
}

impl Vp9Depay {
    fn payload(p: &[u8]) -> Option<usize> {
        let b0 = *p.first()?;
        let (pic_id, inter, layers, flexible, ss) = (b0 & 0x80 != 0, b0 & 0x40 != 0, b0 & 0x20 != 0, b0 & 0x10 != 0, b0 & 0x02 != 0);
        let mut i = 1;
        if pic_id {
            i += if *p.get(i)? & 0x80 != 0 { 2 } else { 1 };
        }
        if layers {
            i += if flexible { 1 } else { 2 };
        }
        if flexible && inter {
            // up to three reference differences, each saying whether another follows
            for _ in 0..3 {
                let b = *p.get(i)?;
                i += 1;
                if b & 1 == 0 {
                    break;
                }
            }
        }
        if ss {
            let b = *p.get(i)?;
            i += 1;
            let spatial = (b >> 5) as usize + 1;
            if b & 0x10 != 0 {
                i += 4 * spatial;
            }
            if b & 0x08 != 0 {
                let groups = *p.get(i)?;
                i += 1;
                for _ in 0..groups {
                    let g = *p.get(i)?;
                    i += 1 + ((g >> 2) & 3) as usize;
                }
            }
        }
        (i <= p.len()).then_some(i)
    }

    pub fn push(&mut self, p: &[u8]) {
        let Some(i) = Self::payload(p) else { return };
        let (begin, end) = (p[0] & 0x08 != 0, p[0] & 0x04 != 0);
        if begin {
            self.cur.clear();
        }
        self.cur.extend(&p[i..]);
        if end && !self.cur.is_empty() {
            self.frames.push(std::mem::take(&mut self.cur));
        }
    }
}

/// One video track's depayloader.
pub enum Depay {
    H264(H264Depay),
    Hevc(HevcDepay),
    Av1(Av1Depay),
    Vp8(Vp8Depay),
    Vp9(Vp9Depay),
}

impl Depay {
    pub fn new(codec: VCodec) -> Self {
        match codec {
            VCodec::H264 => Depay::H264(H264Depay::default()),
            VCodec::Hevc => Depay::Hevc(HevcDepay::default()),
            VCodec::Av1 => Depay::Av1(Av1Depay::default()),
            VCodec::Vp8 => Depay::Vp8(Vp8Depay::default()),
            VCodec::Vp9 => Depay::Vp9(Vp9Depay::default()),
        }
    }

    pub fn push(&mut self, p: &[u8]) {
        match self {
            Depay::H264(d) => d.push(p),
            Depay::Hevc(d) => d.push(p),
            Depay::Av1(d) => d.push(p),
            Depay::Vp8(d) => d.push(p),
            Depay::Vp9(d) => d.push(p),
        }
    }

    /// The picture's units so far: NAL units, or one frame.
    pub fn take(&mut self) -> Vec<Vec<u8>> {
        let one = |f: Vec<u8>| if f.is_empty() { vec![] } else { vec![f] };
        match self {
            Depay::H264(d) => std::mem::take(&mut d.nals),
            Depay::Hevc(d) => std::mem::take(&mut d.nals),
            Depay::Av1(d) => {
                d.pending = None;
                one(std::mem::take(&mut d.tu))
            }
            Depay::Vp8(d) => one(std::mem::take(&mut d.frame)),
            Depay::Vp9(d) => {
                if !d.cur.is_empty() {
                    let cur = std::mem::take(&mut d.cur);
                    d.frames.push(cur);
                }
                let frames = std::mem::take(&mut d.frames);
                if frames.is_empty() { vec![] } else { vec![vp9::vp9_superframe(&frames)] }
            }
        }
    }
}
