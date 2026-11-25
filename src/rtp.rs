//! RTP receive side for WHIP: a small jitter buffer that puts packets back in sequence order
//! (late and retransmitted packets are slotted in, not mistaken for new pictures), and the
//! assembly of pictures from packets in order (H.264, HEVC, AV1, VP9, VP8: see [`crate::depay`]).
//!
//! Retransmissions (NACK, with or without RTX) reach this module as packets with an *older*
//! sequence number and timestamp than the newest: the WebRTC stack unwraps RTX itself (the
//! original sequence number out of the first two bytes, the primary SSRC and payload type back),
//! so here they are simply late packets. Late packets that fill a gap in time are counted as
//! recovered; ones that arrive after the buffer gave up on them are counted as late and dropped.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::depay::Depay;
use crate::nal::VCodec;

/// What one track's RTP looked like, for the report.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RtpStats {
    pub received: u64,
    /// Packets that never arrived (the buffer gave up waiting for them).
    pub lost: u64,
    /// Packets that arrived out of order or as retransmissions, in time to be used.
    pub recovered: u64,
    /// Packets that arrived after the buffer had given up on them (dropped).
    pub late: u64,
    pub duplicates: u64,
    /// The farthest a packet was out of order, in packets.
    pub max_reorder: u32,
    pub pictures: u64,
    /// Pictures dropped because packets of them (or before them) were lost.
    pub pictures_dropped: u64,
}

/// A packet released in sequence order.
#[derive(Debug, Clone)]
pub struct Ordered {
    pub ts: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
    /// Packets were skipped (lost) just before this one.
    pub gap_before: bool,
}

struct Held {
    ts: u32,
    marker: bool,
    payload: Vec<u8>,
}

/// Puts packets in sequence order, waiting a little for missing ones.
pub struct Reorder {
    next: Option<u64>,
    highest: u64,
    buf: BTreeMap<u64, Held>,
    gap_since: Option<Instant>,
    max_wait: Duration,
    max_span: u64,
    gap_pending: bool,
    pub stats: RtpStats,
}

impl Reorder {
    pub fn new(max_wait: Duration, max_span: u64) -> Self {
        Reorder {
            next: None,
            highest: 0,
            buf: BTreeMap::new(),
            gap_since: None,
            max_wait,
            max_span,
            gap_pending: false,
            stats: RtpStats::default(),
        }
    }

    fn extend(&self, seq: u16) -> u64 {
        if self.next.is_none() && self.buf.is_empty() {
            return seq as u64 + 65_536;
        }
        let delta = seq.wrapping_sub(self.highest as u16) as i16 as i64;
        (self.highest as i64 + delta).max(0) as u64
    }

    pub fn push(&mut self, seq: u16, ts: u32, marker: bool, payload: &[u8], now: Instant) -> Vec<Ordered> {
        self.stats.received += 1;
        let ext = self.extend(seq);
        let next = *self.next.get_or_insert(ext);
        if ext < next {
            // after the buffer moved on: too late to use
            self.stats.late += 1;
            return vec![];
        }
        if self.buf.contains_key(&ext) {
            self.stats.duplicates += 1;
            return vec![];
        }
        if ext < self.highest {
            self.stats.recovered += 1;
            self.stats.max_reorder = self.stats.max_reorder.max((self.highest - ext) as u32);
        }
        self.highest = self.highest.max(ext);
        self.buf.insert(ext, Held { ts, marker, payload: payload.to_vec() });
        self.drain(now)
    }

    /// Gives up on packets that have been missing too long.
    pub fn tick(&mut self, now: Instant) -> Vec<Ordered> {
        self.drain(now)
    }

    fn drain(&mut self, now: Instant) -> Vec<Ordered> {
        let mut out = vec![];
        while let Some(next) = self.next {
            if let Some(h) = self.buf.remove(&next) {
                out.push(Ordered { ts: h.ts, marker: h.marker, payload: h.payload, gap_before: std::mem::take(&mut self.gap_pending) });
                self.next = Some(next + 1);
                self.gap_since = None;
                continue;
            }
            let Some(&first) = self.buf.keys().next() else { break };
            let since = *self.gap_since.get_or_insert(now);
            if self.highest - next > self.max_span || now.duration_since(since) >= self.max_wait {
                self.stats.lost += first - next;
                self.gap_pending = true;
                self.next = Some(first);
                self.gap_since = None;
                continue;
            }
            break;
        }
        out
    }

    /// Everything still held, in order (the stream ended).
    pub fn flush(&mut self) -> Vec<Ordered> {
        let mut out = vec![];
        let held = std::mem::take(&mut self.buf);
        let mut prev = self.next;
        for (seq, h) in held {
            if let Some(p) = prev {
                if seq > p {
                    self.stats.lost += seq - p;
                    self.gap_pending = true;
                }
            }
            out.push(Ordered { ts: h.ts, marker: h.marker, payload: h.payload, gap_before: std::mem::take(&mut self.gap_pending) });
            prev = Some(seq + 1);
        }
        self.next = prev;
        out
    }
}

/// A finished picture: its RTP timestamp and NAL units (no start codes), or for AV1, VP9 and
/// VP8 one unit: the whole frame.
#[derive(Debug, Clone)]
pub struct Picture {
    pub ts: u32,
    pub nals: Vec<Vec<u8>>,
}

fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < (1 << 31)
}

struct Open {
    ts: u32,
    depay: Depay,
    corrupt: bool,
}

/// Builds pictures from packets in sequence order. A picture ends on its marker bit or
/// when a newer timestamp starts; a packet with an older timestamp than the picture being built
/// is dropped, never taken for a new picture.
pub struct Pictures {
    codec: VCodec,
    cur: Option<Open>,
    last_ts: Option<u32>,
    pub pictures: u64,
    pub dropped: u64,
    pub late: u64,
}

impl Default for Pictures {
    fn default() -> Self {
        Pictures::new(VCodec::H264)
    }
}

impl Pictures {
    pub fn new(codec: VCodec) -> Self {
        Pictures { codec, cur: None, last_ts: None, pictures: 0, dropped: 0, late: 0 }
    }

    pub fn packet(&mut self, p: Ordered, out: &mut Vec<Picture>) {
        if let Some(cur) = &mut self.cur {
            if p.ts == cur.ts {
                if p.gap_before {
                    cur.corrupt = true;
                }
            } else if newer(p.ts, cur.ts) {
                self.finish(out);
            } else {
                // older than the picture being built: it belongs to one already given up on
                self.late += 1;
                return;
            }
        } else if self.last_ts.is_some_and(|l| p.ts == l || !newer(p.ts, l)) {
            // more of a picture already finished, or an older one
            self.late += 1;
            return;
        }
        let codec = self.codec;
        let cur = self.cur.get_or_insert_with(|| Open { ts: p.ts, depay: Depay::new(codec), corrupt: p.gap_before });
        cur.depay.push(&p.payload);
        if p.marker {
            self.finish(out);
        }
    }

    fn finish(&mut self, out: &mut Vec<Picture>) {
        let Some(mut cur) = self.cur.take() else { return };
        self.last_ts = Some(cur.ts);
        let nals = cur.depay.take();
        if nals.is_empty() {
            return;
        }
        if cur.corrupt {
            self.dropped += 1;
            return;
        }
        self.pictures += 1;
        out.push(Picture { ts: cur.ts, nals });
    }

    /// The stream ended: whatever is being built is a picture too.
    pub fn flush(&mut self, out: &mut Vec<Picture>) {
        self.finish(out);
    }
}

/// Reorder and picture building together, for one video track.
pub struct VideoAssembler {
    pub reorder: Reorder,
    pictures: Pictures,
}

impl VideoAssembler {
    /// For H.264.
    pub fn new() -> Self {
        Self::with_codec(VCodec::H264)
    }

    pub fn with_codec(codec: VCodec) -> Self {
        VideoAssembler { reorder: Reorder::new(Duration::from_millis(120), 120), pictures: Pictures::new(codec) }
    }

    pub fn push(&mut self, seq: u16, ts: u32, marker: bool, payload: &[u8], now: Instant) -> Vec<Picture> {
        let ordered = self.reorder.push(seq, ts, marker, payload, now);
        self.build(ordered)
    }

    pub fn tick(&mut self, now: Instant) -> Vec<Picture> {
        let ordered = self.reorder.tick(now);
        self.build(ordered)
    }

    pub fn finish(&mut self) -> Vec<Picture> {
        let ordered = self.reorder.flush();
        let mut out = vec![];
        for p in ordered {
            self.pictures.packet(p, &mut out);
        }
        self.pictures.flush(&mut out);
        out
    }

    fn build(&mut self, ordered: Vec<Ordered>) -> Vec<Picture> {
        let mut out = vec![];
        for p in ordered {
            self.pictures.packet(p, &mut out);
        }
        out
    }

    pub fn stats(&self) -> RtpStats {
        let mut s = self.reorder.stats.clone();
        s.pictures = self.pictures.pictures;
        s.pictures_dropped = self.pictures.dropped;
        // packets the picture builder refused as older than what it was building
        s.late += self.pictures.late;
        s
    }
}

impl Default for VideoAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Opus packets in order: late ones are dropped, so audio timestamps never go back.
pub struct AudioOrder {
    pub reorder: Reorder,
    last_ts: Option<u32>,
    late: u64,
}

impl AudioOrder {
    pub fn new() -> Self {
        AudioOrder { reorder: Reorder::new(Duration::from_millis(80), 50), last_ts: None, late: 0 }
    }

    fn keep(&mut self, ordered: Vec<Ordered>) -> Vec<Ordered> {
        let mut out = vec![];
        for p in ordered {
            if self.last_ts.is_some_and(|l| !newer(p.ts, l)) {
                self.late += 1;
                continue;
            }
            self.last_ts = Some(p.ts);
            out.push(p);
        }
        out
    }

    pub fn push(&mut self, seq: u16, ts: u32, payload: &[u8], now: Instant) -> Vec<Ordered> {
        let o = self.reorder.push(seq, ts, false, payload, now);
        self.keep(o)
    }

    pub fn tick(&mut self, now: Instant) -> Vec<Ordered> {
        let o = self.reorder.tick(now);
        self.keep(o)
    }

    pub fn stats(&self) -> RtpStats {
        let mut s = self.reorder.stats.clone();
        s.late += self.late;
        s
    }
}

impl Default for AudioOrder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIC_PACKETS: u16 = 3;

    /// Picture `n` as packets: (seq, ts, marker, payload): three single-NAL packets.
    fn picture(n: u32, first_seq: u16) -> Vec<(u16, u32, bool, Vec<u8>)> {
        (0..PIC_PACKETS)
            .map(|i| (first_seq.wrapping_add(i), 90_000 + n * 3000, i == PIC_PACKETS - 1, vec![0x41, n as u8, i as u8]))
            .collect()
    }

    fn stream(pictures: u32, first_seq: u16) -> Vec<(u16, u32, bool, Vec<u8>)> {
        (0..pictures).flat_map(|n| picture(n, first_seq.wrapping_add((n * PIC_PACKETS as u32) as u16))).collect()
    }

    fn run(packets: &[(u16, u32, bool, Vec<u8>)], order: &[usize], gaps_ms: &[(usize, u64)]) -> (Vec<Picture>, RtpStats) {
        let mut a = VideoAssembler::new();
        let t0 = Instant::now();
        let mut out = vec![];
        let mut at = 0u64;
        for (k, &i) in order.iter().enumerate() {
            if let Some((_, ms)) = gaps_ms.iter().find(|(after, _)| *after == k) {
                at += ms;
                out.extend(a.tick(t0 + Duration::from_millis(at)));
            }
            let (seq, ts, m, p) = &packets[i];
            out.extend(a.push(*seq, *ts, *m, p, t0 + Duration::from_millis(at)));
            at += 1;
        }
        out.extend(a.finish());
        let stats = a.stats();
        (out, stats)
    }

    fn assert_monotonic(pics: &[Picture]) {
        for w in pics.windows(2) {
            assert!(newer(w[1].ts, w[0].ts), "timestamps went back: {} then {}", w[0].ts, w[1].ts);
        }
    }

    #[test]
    fn packets_in_order_make_every_picture_once() {
        let s = stream(10, 100);
        let order: Vec<usize> = (0..s.len()).collect();
        let (pics, stats) = run(&s, &order, &[]);
        assert_eq!(pics.len(), 10);
        assert_monotonic(&pics);
        assert_eq!((stats.lost, stats.late, stats.recovered, stats.pictures_dropped), (0, 0, 0, 0));
        assert_eq!(pics[3].nals.len(), 3);
    }

    #[test]
    fn reordering_across_a_picture_boundary_is_put_right() {
        let s = stream(10, 100);
        // the last packet of picture 3 arrives after the first of picture 4
        let mut order: Vec<usize> = (0..s.len()).collect();
        order.swap(11, 12);
        let (pics, stats) = run(&s, &order, &[]);
        assert_eq!(pics.len(), 10);
        assert_monotonic(&pics);
        assert_eq!((stats.recovered, stats.late, stats.pictures_dropped), (1, 0, 0));
    }

    #[test]
    fn a_retransmitted_packet_in_time_completes_its_picture() {
        let s = stream(10, 100);
        // packet 10 (picture 3, second packet) is lost, and comes back four packets later
        let mut order: Vec<usize> = (0..s.len()).filter(|&i| i != 10).collect();
        order.insert(14, 10);
        let (pics, stats) = run(&s, &order, &[]);
        assert_eq!(pics.len(), 10, "no picture lost");
        assert_monotonic(&pics);
        assert_eq!((stats.lost, stats.recovered, stats.late, stats.pictures_dropped), (0, 1, 0, 0));
        assert_eq!(pics[3].nals.len(), 3, "the repaired picture is whole");
    }

    #[test]
    fn a_retransmission_after_the_buffer_gave_up_is_late_not_a_new_picture() {
        let s = stream(10, 100);
        // packet 10 never arrives in time: the buffer waits 120 ms and moves on; then it shows up
        let mut order: Vec<usize> = (0..s.len()).filter(|&i| i != 10).collect();
        order.push(10);
        let (pics, stats) = run(&s, &order, &[(29, 300)]);
        assert_monotonic(&pics);
        assert_eq!(pics.len(), 9, "the broken picture is dropped, the rest are whole: {}", pics.len());
        assert_eq!((stats.lost, stats.late, stats.pictures_dropped), (1, 1, 1));
        assert!(pics.iter().all(|p| p.nals.len() == 3));
    }

    #[test]
    fn sequence_numbers_wrapping_around_are_followed() {
        let s = stream(10, 65_525);
        let order: Vec<usize> = (0..s.len()).collect();
        let (pics, stats) = run(&s, &order, &[]);
        assert_eq!(pics.len(), 10);
        assert_monotonic(&pics);
        assert_eq!(stats.lost, 0);
    }

    #[test]
    fn duplicates_are_counted_and_ignored() {
        let s = stream(4, 100);
        let mut order: Vec<usize> = (0..s.len()).collect();
        order.insert(5, 4);
        let (pics, stats) = run(&s, &order, &[]);
        assert_eq!(pics.len(), 4);
        assert_eq!(stats.late + stats.duplicates, 1);
    }

    #[test]
    fn late_audio_never_takes_timestamps_back() {
        let mut a = AudioOrder::new();
        let t0 = Instant::now();
        let mut out = vec![];
        for (k, seq) in [0u16, 1, 2, 4, 5, 6].iter().enumerate() {
            out.extend(a.push(*seq, *seq as u32 * 960, &[1], t0 + Duration::from_millis(k as u64 * 20)));
        }
        out.extend(a.tick(t0 + Duration::from_millis(400)));
        out.extend(a.push(3, 3 * 960, &[1], t0 + Duration::from_millis(420)));
        assert!(out.windows(2).all(|w| newer(w[1].ts, w[0].ts)));
        assert_eq!(out.len(), 6);
        let s = a.stats();
        assert_eq!((s.lost, s.late), (1, 1));
    }
}
