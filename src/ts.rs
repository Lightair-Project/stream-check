//! MPEG transport stream reading: PAT, PMT, PES with PTS/DTS, H.264 and HEVC pictures (Annex B)
//! and AAC (ADTS), into [`Media`].

use std::collections::HashMap;
use std::sync::Arc;

use crate::media::{ACodec, AnnexB, Media, make_asc};
use crate::nal::{self, VCodec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    H264,
    Hevc,
    Aac,
    Other(u8),
}

#[derive(Default)]
struct Pes {
    data: Vec<u8>,
    started: bool,
}

pub struct TsDemux {
    pending: Vec<u8>,
    pmt_pid: Option<u16>,
    streams: HashMap<u16, Kind>,
    pes: HashMap<u16, Pes>,
    video: HashMap<u16, AnnexB>,
    aac_sent: HashMap<u16, (u32, u8)>,
    pub warnings: Vec<String>,
    /// Transport stream packets that did not start with the sync byte.
    pub sync_losses: u64,
    /// Continuity counter jumps (lost packets), per all PIDs.
    pub discontinuities: u64,
    cc: HashMap<u16, u8>,
}

impl Default for TsDemux {
    fn default() -> Self {
        Self::new()
    }
}

impl TsDemux {
    pub fn new() -> Self {
        TsDemux {
            pending: vec![],
            pmt_pid: None,
            streams: HashMap::new(),
            pes: HashMap::new(),
            video: HashMap::new(),
            aac_sent: HashMap::new(),
            warnings: vec![],
            sync_losses: 0,
            discontinuities: 0,
            cc: HashMap::new(),
        }
    }

    pub fn push(&mut self, data: &[u8], out: &mut Vec<Media>) {
        self.pending.extend_from_slice(data);
        let mut i = 0;
        while self.pending.len() - i >= 188 {
            if self.pending[i] != 0x47 {
                self.sync_losses += 1;
                i += 1;
                continue;
            }
            let pkt: [u8; 188] = self.pending[i..i + 188].try_into().unwrap();
            self.packet(&pkt, out);
            i += 188;
        }
        self.pending.drain(..i);
    }

    /// Hands over the last pictures still being collected.
    pub fn finish(&mut self, out: &mut Vec<Media>) {
        let pids: Vec<u16> = self.pes.keys().copied().collect();
        for pid in pids {
            self.flush(pid, out);
        }
    }

    fn packet(&mut self, p: &[u8; 188], out: &mut Vec<Media>) {
        let pid = ((p[1] as u16 & 0x1f) << 8) | p[2] as u16;
        let pusi = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0f;
        if pid != 0x1fff {
            if let Some(prev) = self.cc.insert(pid, cc) {
                if afc & 1 != 0 && cc != (prev + 1) & 0x0f && cc != prev {
                    self.discontinuities += 1;
                }
            }
        }
        let mut at = 4;
        if afc & 2 != 0 {
            at += 1 + p[4] as usize;
        }
        if afc & 1 == 0 || at >= 188 {
            return;
        }
        let payload = &p[at..];
        if pid == 0 {
            self.pat(payload, pusi);
        } else if Some(pid) == self.pmt_pid {
            self.pmt(payload, pusi);
        } else if self.streams.contains_key(&pid) {
            if pusi {
                self.flush(pid, out);
                let e = self.pes.entry(pid).or_default();
                e.data.clear();
                e.started = true;
            }
            let e = self.pes.entry(pid).or_default();
            if e.started {
                e.data.extend_from_slice(payload);
            }
        }
    }

    fn pat(&mut self, payload: &[u8], pusi: bool) {
        let s = if pusi { 1 + payload[0] as usize } else { 0 };
        let Some(sec) = payload.get(s..) else { return };
        if sec.len() < 12 || sec[0] != 0 {
            return;
        }
        let len = ((sec[1] as usize & 0x0f) << 8) | sec[2] as usize;
        let end = (3 + len).saturating_sub(4).min(sec.len());
        let mut i = 8;
        while i + 4 <= end {
            let program = u16::from_be_bytes([sec[i], sec[i + 1]]);
            let pid = ((sec[i + 2] as u16 & 0x1f) << 8) | sec[i + 3] as u16;
            if program != 0 {
                self.pmt_pid = Some(pid);
                break;
            }
            i += 4;
        }
    }

    fn pmt(&mut self, payload: &[u8], pusi: bool) {
        let s = if pusi { 1 + payload[0] as usize } else { 0 };
        let Some(sec) = payload.get(s..) else { return };
        if sec.len() < 16 || sec[0] != 2 {
            return;
        }
        let len = ((sec[1] as usize & 0x0f) << 8) | sec[2] as usize;
        let end = (3 + len).saturating_sub(4).min(sec.len());
        let info = ((sec[10] as usize & 0x0f) << 8) | sec[11] as usize;
        let mut i = 12 + info;
        while i + 5 <= end {
            let t = sec[i];
            let pid = ((sec[i + 1] as u16 & 0x1f) << 8) | sec[i + 2] as u16;
            let es_info = ((sec[i + 3] as usize & 0x0f) << 8) | sec[i + 4] as usize;
            let kind = match t {
                0x1b => Kind::H264,
                0x24 => Kind::Hevc,
                0x0f => Kind::Aac,
                other => Kind::Other(other),
            };
            if let Kind::Other(t) = kind {
                if !self.streams.contains_key(&pid) {
                    self.warnings.push(format!("a stream of type 0x{t:02x} (PID {pid}) isn't supported by Stream Check"));
                }
            }
            if self.streams.insert(pid, kind).is_none() {
                match kind {
                    Kind::H264 => {
                        self.video.insert(pid, AnnexB::new(VCodec::H264));
                    }
                    Kind::Hevc => {
                        self.video.insert(pid, AnnexB::new(VCodec::Hevc));
                    }
                    _ => {}
                }
            }
            i += 5 + es_info;
        }
    }

    fn flush(&mut self, pid: u16, out: &mut Vec<Media>) {
        let Some(e) = self.pes.get_mut(&pid) else { return };
        let data = std::mem::take(&mut e.data);
        e.started = false;
        if data.len() < 9 || data[..3] != [0, 0, 1] {
            return;
        }
        let flags = data[7] >> 6;
        let hdr = 9 + data[8] as usize;
        let ts = |b: &[u8]| -> i64 {
            (((b[0] as i64 >> 1) & 7) << 30)
                | ((b[1] as i64) << 22)
                | (((b[2] as i64) >> 1) << 15)
                | ((b[3] as i64) << 7)
                | ((b[4] as i64) >> 1)
        };
        if data.len() < hdr {
            return;
        }
        let (mut pts, mut dts) = (None, None);
        if flags & 2 != 0 && data.len() >= 14 {
            pts = Some(ts(&data[9..14]));
            dts = pts;
        }
        if flags == 3 && data.len() >= 19 {
            dts = Some(ts(&data[14..19]));
        }
        let Some(pts) = pts else { return };
        let dts = dts.unwrap_or(pts);
        let us = |t: i64| t * 1000 / 90;
        let es = &data[hdr..];
        match self.streams.get(&pid).copied() {
            Some(Kind::H264) | Some(Kind::Hevc) => {
                let Some(a) = self.video.get_mut(&pid) else { return };
                let codec = a.codec;
                let nals = nal::split_annexb(es);
                // one PES is one access unit, unless delimiters say otherwise
                let mut unit: Vec<&[u8]> = vec![];
                let mut units: Vec<Vec<&[u8]>> = vec![];
                for n in nals {
                    let t = nal::nal_type(codec, n);
                    let aud = matches!((codec, t), (VCodec::H264, 9) | (VCodec::Hevc, 35));
                    if aud && !unit.is_empty() {
                        units.push(std::mem::take(&mut unit));
                    }
                    unit.push(n);
                }
                if !unit.is_empty() {
                    units.push(unit);
                }
                for u in units {
                    a.picture(&u, us(pts), us(dts), out);
                }
            }
            Some(Kind::Aac) => {
                let mut i = 0;
                let mut n = 0i64;
                while i + 7 <= es.len() && es[i] == 0xff && es[i + 1] & 0xf0 == 0xf0 {
                    let protection_absent = es[i + 1] & 1 == 1;
                    let object = ((es[i + 2] >> 6) & 3) + 1;
                    let idx = (es[i + 2] >> 2) & 0x0f;
                    let ch = ((es[i + 2] & 1) << 2) | (es[i + 3] >> 6);
                    let len = (((es[i + 3] & 3) as usize) << 11) | ((es[i + 4] as usize) << 3) | ((es[i + 5] >> 5) as usize);
                    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
                    let rate = RATES.get(idx as usize).copied().unwrap_or(48000);
                    let head = if protection_absent { 7 } else { 9 };
                    if len < head || i + len > es.len() {
                        break;
                    }
                    if self.aac_sent.get(&pid) != Some(&(rate, ch)) {
                        self.aac_sent.insert(pid, (rate, ch));
                        out.push(Media::AudioConfig {
                            codec: ACodec::Aac,
                            config: Arc::new(make_asc(object, rate, ch)),
                            sample_rate: rate,
                            channels: ch,
                        });
                    }
                    let pts_us = us(pts) + n * 1_024_000_000 / rate as i64;
                    out.push(Media::Audio { pts_us, data: Arc::new(es[i + head..i + len].to_vec()) });
                    n += 1;
                    i += len;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x47, (pusi as u8) << 6 | (pid >> 8) as u8, pid as u8, 0x10 | cc];
        let room = 184;
        if payload.len() < room {
            // stuff with an adaptation field
            let stuff = room - payload.len();
            p[3] |= 0x20;
            p.push((stuff - 1) as u8);
            if stuff > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xff, stuff - 2));
            }
        }
        p.extend(payload);
        p
    }

    fn pts(v: i64, marker: u8) -> [u8; 5] {
        [
            marker << 4 | (((v >> 30) & 7) as u8) << 1 | 1,
            (v >> 22) as u8,
            (((v >> 15) & 0x7f) as u8) << 1 | 1,
            (v >> 7) as u8,
            ((v & 0x7f) as u8) << 1 | 1,
        ]
    }

    #[test]
    fn a_video_and_an_audio_stream_are_demuxed() {
        let mut s = vec![];
        // PAT: program 1 -> PMT 0x100
        s.extend(packet(0, true, 0, &[0, 0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0x00, 0, 0, 0, 0]));
        // PMT: H.264 on 0x101, AAC on 0x102
        s.extend(packet(
            0x100,
            true,
            0,
            &[0, 2, 0xb0, 23, 0, 1, 0xc1, 0, 0, 0xe1, 1, 0xf0, 0, 0x1b, 0xe1, 1, 0xf0, 0, 0x0f, 0xe1, 2, 0xf0, 0, 0, 0, 0, 0],
        ));
        // a picture: SPS PPS IDR with an AUD
        let mut es = vec![0, 0, 0, 1, 9, 0xf0];
        es.extend([0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0xac, 0xd9, 0x40, 0x50, 5, 0xbb, 1, 0x10]);
        es.extend([0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb]);
        es.extend([0, 0, 0, 1, 0x65, 0x88, 0x84]);
        let mut pes = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0xc0, 10];
        pes.extend(pts(90_000, 3));
        pes.extend(pts(90_000 - 3000, 1));
        pes.extend(&es);
        for (i, c) in pes.chunks(184).enumerate() {
            s.extend(packet(0x101, i == 0, i as u8, c));
        }
        // AAC: one ADTS frame, 48 kHz stereo, 5 bytes of payload
        let mut aes = vec![0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc];
        aes.extend([1, 2, 3, 4, 5]);
        let len = aes.len();
        aes[3] = (aes[3] & !3) | ((len >> 11) as u8 & 3);
        aes[4] = (len >> 3) as u8;
        aes[5] = ((len & 7) << 5) as u8 | 0x1f;
        let mut apes = vec![0, 0, 1, 0xc0, 0, 0, 0x80, 0x80, 5];
        apes.extend(pts(90_000, 2));
        apes.extend(&aes);
        s.extend(packet(0x102, true, 0, &apes));
        let mut d = TsDemux::new();
        let mut out = vec![];
        d.push(&s, &mut out);
        d.finish(&mut out);
        assert!(out.iter().any(|m| matches!(m, Media::VideoConfig { codec: VCodec::H264, .. })));
        let video: Vec<_> = out
            .iter()
            .filter_map(|m| if let Media::Video { pts_us, dts_us, key, .. } = m { Some((*pts_us, *dts_us, *key)) } else { None })
            .collect();
        assert_eq!(video, vec![(1_000_000, 966_666, true)]);
        assert!(out.iter().any(|m| matches!(m, Media::AudioConfig { sample_rate: 48000, channels: 2, .. })));
        assert!(out.iter().any(|m| matches!(m, Media::Audio { pts_us: 1_000_000, data } if data.len() == 5)));
        assert_eq!(d.sync_losses, 0);
    }
}
