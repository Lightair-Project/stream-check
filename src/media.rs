//! What every ingest hands to the lab: the same few events whatever the protocol was.

use std::sync::Arc;

use serde::Serialize;

use crate::nal::{self, HevcPps, Sps, VCodec};
use crate::{av1, vp9};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ACodec {
    Aac,
    Opus,
}

impl ACodec {
    pub fn name(self) -> &'static str {
        match self {
            ACodec::Aac => "AAC",
            ACodec::Opus => "Opus",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Media {
    /// The codec record, before the first picture and again when it changes: avcC or hvcC (the
    /// parameter sets), av1C with the sequence header, or for VP9 and VP8 a vpcC record followed
    /// by the picture's width and height (see [`vp9::build_record`]).
    VideoConfig { codec: VCodec, record: Arc<Vec<u8>> },
    /// AudioSpecificConfig for AAC; empty for Opus.
    AudioConfig { codec: ACodec, config: Arc<Vec<u8>>, sample_rate: u32, channels: u8 },
    /// One picture: 4-byte length-prefixed NAL units (H.264, HEVC), a temporal unit of OBUs each
    /// with its size and no temporal delimiter (AV1), or a frame or superframe (VP9, VP8).
    /// Times in microseconds.
    Video { pts_us: i64, dts_us: i64, key: bool, data: Arc<Vec<u8>> },
    /// One AAC access unit (raw, no ADTS) or one Opus packet.
    Audio { pts_us: i64, data: Arc<Vec<u8>> },
    /// The sender's own description of the stream (RTMP onMetaData).
    Metadata(serde_json::Value),
    /// A track other than the first of an Enhanced RTMP multitrack stream (Twitch Dual Format's
    /// vertical picture, say): its id and what it carried. Track 0 comes as plain media.
    Track(u8, Box<Media>),
}

/// Turns Annex B pictures (MPEG-TS, RTP) into [`Media`]: parameter sets are collected into a
/// config record, and a picture goes out length-prefixed without them.
pub struct AnnexB {
    pub codec: VCodec,
    vps: Vec<Vec<u8>>,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    sent: Option<nal::ParameterSets>,
}

impl AnnexB {
    pub fn new(codec: VCodec) -> Self {
        AnnexB { codec, vps: vec![], sps: vec![], pps: vec![], sent: None }
    }

    /// One access unit as NAL units (start codes removed).
    pub fn picture(&mut self, nals: &[&[u8]], pts_us: i64, dts_us: i64, out: &mut Vec<Media>) {
        let mut rest: Vec<&[u8]> = vec![];
        let mut key = false;
        let mut changed = false;
        for n in nals {
            if n.is_empty() {
                continue;
            }
            let t = nal::nal_type(self.codec, n);
            let set = match (self.codec, t) {
                (VCodec::Hevc, 32) => Some(&mut self.vps),
                (VCodec::H264, 7) | (VCodec::Hevc, 33) => Some(&mut self.sps),
                (VCodec::H264, 8) | (VCodec::Hevc, 34) => Some(&mut self.pps),
                _ => None,
            };
            if let Some(set) = set {
                if !set.iter().any(|s| s == n) {
                    // a changed set replaces the old one of its kind
                    if set.len() >= 4 {
                        set.clear();
                    }
                    set.push(n.to_vec());
                    changed = true;
                }
                continue;
            }
            // access unit delimiter, SEI: not picture data, kept out of the length-prefixed unit
            if matches!((self.codec, t), (VCodec::H264, 9) | (VCodec::Hevc, 35)) {
                continue;
            }
            key |= nal::is_keyframe_nal(self.codec, t);
            rest.push(n);
        }
        if changed || self.sent.is_none() {
            let record = match self.codec {
                VCodec::H264 => nal::build_avcc(&self.sps, &self.pps),
                VCodec::Hevc => nal::build_hvcc(&self.vps, &self.sps, &self.pps),
                // not NAL codecs: no parameter sets to collect
                VCodec::Av1 | VCodec::Vp9 | VCodec::Vp8 => None,
            };
            let now = (self.vps.clone(), self.sps.clone(), self.pps.clone());
            if let Some(record) = record {
                if self.sent.as_ref() != Some(&now) {
                    self.sent = Some(now);
                    out.push(Media::VideoConfig { codec: self.codec, record: Arc::new(record) });
                }
            }
        }
        if self.sent.is_some() && rest.iter().any(|n| nal::is_slice(self.codec, nal::nal_type(self.codec, n))) {
            out.push(Media::Video { pts_us, dts_us, key, data: Arc::new(nal::to_lengths(&rest)) });
        }
    }
}

/// What the stats show about a video codec record: profile, level, size, bit depth and the
/// WebCodecs codec string.
pub fn video_info(codec: VCodec, record: &[u8]) -> Option<Sps> {
    match codec {
        VCodec::H264 => nal::read_avcc(record).and_then(|(_, sps, _)| nal::parse_sps(codec, sps.first()?)),
        VCodec::Hevc => nal::read_hvcc(record).and_then(|(_, sps, _)| nal::parse_sps(codec, sps.first()?)),
        VCodec::Av1 => av1::read_av1c(record).map(|(s, _)| av1::info(&s)),
        VCodec::Vp9 => vp9::info(false, record),
        VCodec::Vp8 => vp9::info(true, record),
    }
}

/// Turns AV1, VP9 and VP8 frames into [`Media`]. Key frames are read from the bitstream. The
/// codec record is passed on once the bitstream has said enough (AV1: a sequence header, from
/// the sender's av1C or in band; VP9 and VP8: a key frame, for the picture's size), and again
/// when it changes. Frames before the first record are dropped, as a decoder would.
pub struct Frames {
    pub codec: VCodec,
    /// The sender's vpcC values (VP9, VP8).
    vpcc: Option<[u8; 8]>,
    /// AV1: the sequence header OBU's payload last passed on, and whether every frame is a key frame.
    seq: Option<Vec<u8>>,
    reduced: bool,
    /// VP9, VP8: the size last passed on.
    size: Option<(u32, u32)>,
}

impl Frames {
    pub fn new(codec: VCodec) -> Self {
        Frames { codec, vpcc: None, seq: None, reduced: false, size: None }
    }

    /// Whether a record has been passed on (frames are taken).
    pub fn ready(&self) -> bool {
        self.seq.is_some() || self.size.is_some()
    }

    /// The sender's own record (av1C or vpcC).
    pub fn config(&mut self, record: &[u8], out: &mut Vec<Media>) {
        match self.codec {
            VCodec::Av1 => {
                if let Some((s, _)) = av1::read_av1c(record) {
                    let payload =
                        av1::obus(&record[4..]).into_iter().find(|o| o.kind == av1::OBU_SEQUENCE_HEADER).map(|o| o.payload.to_vec());
                    if payload.is_some() && payload != self.seq {
                        self.seq = payload;
                        self.reduced = s.reduced_still_picture_header;
                        out.push(Media::VideoConfig { codec: self.codec, record: Arc::new(record.to_vec()) });
                    }
                }
            }
            _ => self.vpcc = vp9::read_vpcc(record),
        }
    }

    /// One frame (AV1: a temporal unit). `key_hint` is what the container said, used only when
    /// the bitstream can't be read.
    pub fn frame(&mut self, pts_us: i64, dts_us: i64, key_hint: bool, data: &[u8], out: &mut Vec<Media>) {
        let (key, data) = match self.codec {
            VCodec::Av1 => {
                let data = av1::normalise(data);
                if let Some(o) = av1::obus(&data).into_iter().find(|o| o.kind == av1::OBU_SEQUENCE_HEADER) {
                    if self.seq.as_deref() != Some(o.payload) {
                        if let Some(s) = av1::parse_seq_header(o.payload) {
                            self.seq = Some(o.payload.to_vec());
                            self.reduced = s.reduced_still_picture_header;
                            let record = av1::build_av1c(&s, o.whole);
                            out.push(Media::VideoConfig { codec: self.codec, record: Arc::new(record) });
                        }
                    }
                }
                (av1::is_key(&data, self.reduced), data)
            }
            VCodec::Vp9 | VCodec::Vp8 => {
                let f = if self.codec == VCodec::Vp9 { vp9::vp9_chunk(data) } else { vp9::vp8_frame(data) };
                let key = f.map_or(key_hint, |f| f.key);
                if let Some(f) = f.filter(|f| f.key && f.width > 0) {
                    if self.size != Some((f.width, f.height)) {
                        self.size = Some((f.width, f.height));
                        let vpcc = vp9::build_vpcc(self.vpcc, &f, vp9::vp9_level_for(f.width, f.height));
                        let record = vp9::build_record(&vpcc, f.width, f.height);
                        out.push(Media::VideoConfig { codec: self.codec, record: Arc::new(record) });
                    }
                }
                (key, data.to_vec())
            }
            VCodec::H264 | VCodec::Hevc => (key_hint, data.to_vec()),
        };
        if self.ready() && !data.is_empty() {
            out.push(Media::Video { pts_us, dts_us, key, data: Arc::new(data) });
        }
    }
}

/// The audio config of AAC-LC and friends: (sample rate, channels) out of an
/// AudioSpecificConfig.
pub fn read_asc(asc: &[u8]) -> Option<(u32, u8, u8)> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let mut b = nal::Bits::new(asc);
    let mut object = b.u(5)? as u8;
    if object == 31 {
        object = 32 + b.u(6)? as u8;
    }
    let idx = b.u(4)? as usize;
    let rate = if idx == 15 { b.u(24)? as u32 } else { *RATES.get(idx)? };
    let channels = b.u(4)? as u8;
    Some((rate, channels, object))
}

/// An AudioSpecificConfig (AAC-LC) for a sample rate and channel count.
pub fn make_asc(object: u8, rate: u32, channels: u8) -> Vec<u8> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let idx = RATES.iter().position(|&r| r == rate).unwrap_or(3) as u16;
    let v: u16 = ((object as u16) << 11) | (idx << 7) | ((channels as u16) << 3);
    v.to_be_bytes().to_vec()
}

/// Opus packet facts from its TOC byte: frame duration in microseconds and channel layout.
pub fn opus_toc(packet: &[u8]) -> Option<(u32, u8)> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame_us = match config {
        0..=11 => [10_000, 20_000, 40_000, 60_000][(config & 3) as usize],
        12..=15 => [10_000, 20_000][(config & 1) as usize],
        _ => [2_500, 5_000, 10_000, 20_000][(config & 3) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*packet.get(1)? & 0x3f) as u32,
    };
    Some((frame_us * frames, if toc & 4 != 0 { 2 } else { 1 }))
}

pub type PpsList = Vec<(u64, HevcPps)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_roundtrips_and_opus_toc_reads() {
        assert_eq!(make_asc(2, 48000, 2), vec![0x11, 0x90]);
        assert_eq!(read_asc(&[0x11, 0x90]), Some((48000, 2, 2)));
        // 20 ms CELT fullband stereo, one frame
        assert_eq!(opus_toc(&[0xfc, 0xff]), Some((20_000, 2)));
    }

    #[test]
    fn annexb_pictures_become_config_then_length_prefixed_video() {
        let sps = [0x67u8, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10];
        let pps = [0x68u8, 0xeb, 0xe3, 0xcb];
        let idr = [0x65u8, 0x88, 0x84];
        let mut a = AnnexB::new(VCodec::H264);
        let mut out = vec![];
        a.picture(&[&sps, &pps, &idr], 0, 0, &mut out);
        assert!(matches!(out[0], Media::VideoConfig { .. }));
        let Media::Video { key, ref data, .. } = out[1] else { panic!() };
        assert!(key);
        assert_eq!(&data[..], [0, 0, 0, 3, 0x65, 0x88, 0x84]);
        // the same sets again: no new config
        let mut out = vec![];
        a.picture(&[&sps, &pps, &idr], 33_000, 33_000, &mut out);
        assert_eq!(out.len(), 1);
    }
}
