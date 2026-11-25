//! SEI: broadcast performance metrics as Amazon IVS defines them, CEA-608/708 captions in A/53
//! user data, and SEI messages out of escaped NAL units.

use std::sync::Arc;
use std::time::{Duration, Instant};

use stream_check::analyze::Analyzer;
use stream_check::media::Media;
use stream_check::nal::VCodec;
use stream_check::sei::{self, BpmKind, Captions};

use crate::support::{bpm_erm, bpm_nals, bpm_sm, bpm_ts, h264_sei};

#[test]
fn a_bpm_timestamp_reads_as_the_moment_it_names() {
    let m = sei::bpm(&bpm_ts("2024-03-25T15:10:34.489Z")).expect("a BPM message");
    assert_eq!(m.kind, BpmKind::Ts);
    assert_eq!(m.times.len(), 1, "{m:?}");
    let t = &m.times[0];
    assert_eq!((t.kind, t.event), (1, 4), "an RFC 3339 time of packet interleave");
    assert_eq!(t.unix_ms, Some(1_711_379_434_489.0));
    assert!(m.counters.is_empty());
}

#[test]
fn an_rfc3339_time_with_an_offset_is_the_same_instant_as_in_utc() {
    assert_eq!(sei::rfc3339_ms("2024-03-25T17:10:34.489+02:00"), sei::rfc3339_ms("2024-03-25T15:10:34.489Z"));
    assert_eq!(sei::rfc3339_ms("1970-01-01T00:00:01Z"), Some(1000.0));
}

#[test]
fn bpm_metrics_messages_give_their_counters_by_tag() {
    let sm = sei::bpm(&bpm_sm("2024-03-25T15:10:34.489Z", [60, 1, 2, 57])).unwrap();
    assert_eq!(sm.kind, BpmKind::Sm);
    assert_eq!(sm.counters, vec![(1, 60), (2, 1), (3, 2), (4, 57)]);
    let erm = sei::bpm(&bpm_erm("2024-03-25T15:10:34.489Z", [60, 3, 57])).unwrap();
    assert_eq!(erm.kind, BpmKind::Erm);
    assert_eq!(erm.counters, vec![(1, 60), (2, 3), (3, 57)]);
}

#[test]
fn a_bpm_message_cut_short_or_with_another_uuid_reads_as_nothing() {
    let sm = bpm_sm("2024-03-25T15:10:34.489Z", [60, 1, 2, 57]);
    assert!(sei::bpm(&sm[..sm.len() - 3]).is_none(), "cut inside the last counter");
    let mut other = sm.clone();
    other[0] ^= 0xff;
    assert!(sei::bpm(&other).is_none());
    assert!(sei::bpm_kind(&other).is_none());
}

#[test]
fn an_sei_message_comes_out_of_its_nal_unit_without_emulation_prevention() {
    // bytes that need escaping inside a NAL unit
    let payload: Vec<u8> = [&sei::UUID_BPM_TS[..], &[0, 0, 1, 0, 0, 0, 0, 3, 9]].concat();
    let nal = h264_sei(5, &payload);
    assert_ne!(&nal[3..3 + payload.len()], &payload[..], "the NAL unit is escaped");
    let msgs = sei::messages(VCodec::H264, &nal);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind, sei::SEI_UNREGISTERED);
    assert_eq!(msgs[0].payload, payload);
}

/// An ATSC A/53 caption payload (T.35 United States, ATSC, "GA94", cc_data) of these
/// constructs: (cc_type, byte 1, byte 2), all valid.
fn a53(cc: &[(u8, u8, u8)]) -> Vec<u8> {
    let mut p = vec![0xb5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03, 0x40 | cc.len() as u8, 0xff];
    for &(ty, b1, b2) in cc {
        p.extend([0xf8 | 0x04 | ty, b1, b2]);
    }
    p.push(0xff);
    p
}

#[test]
fn cea608_caption_text_and_its_channels_are_read() {
    let mut c = Captions::default();
    // CC1: erase displayed memory, then "HELLO WORLD", then end of caption
    let mut cc = vec![(0, 0x14, 0x2c)];
    cc.extend("HELLO WORLD".as_bytes().chunks(2).map(|p| (0, p[0], *p.get(1).unwrap_or(&0))));
    cc.push((0, 0x14, 0x2f));
    // CC3 (field 2) says something too
    cc.push((1, 0x15, 0x2c));
    c.feed(&sei::a53_cc(&a53(&cc)).expect("A/53 caption data"), 1.0);
    let s = &c.stats;
    assert!(s.present);
    assert!(s.recent_text.contains("HELLO WORLD"), "{:?}", s.recent_text);
    assert_eq!(s.channels_608, vec!["CC1".to_string(), "CC3".to_string()]);
    assert!(s.cea608_field2 > 0);
}

#[test]
fn cea708_packets_name_their_services() {
    let mut c = Captions::default();
    // a DTVCC packet of 4 bytes: header (size code 2), service 1 block of 2 bytes, "AB"
    let cc = [(3, 0x02, (1 << 5) | 2), (2, b'A', b'B')];
    c.feed(&sei::a53_cc(&a53(&cc)).unwrap(), 2.0);
    assert_eq!(c.stats.cea708_packets, 1);
    assert_eq!(c.stats.services_708, vec![1]);
}

#[test]
fn user_data_that_is_not_captions_is_not_read_as_captions() {
    // the same T.35 header with another provider's identifier
    let mut p = a53(&[(0, 0x14, 0x2c)]);
    p[3..7].copy_from_slice(b"DTG1");
    assert!(sei::a53_cc(&p).is_none());
}

/// One H.264 IDR picture: `before` NAL units (length-prefixed), the slice, then `after`.
fn idr(before: &[u8], after: &[u8]) -> Arc<Vec<u8>> {
    let slice = [0x65u8, 0x88, 0x84, 0x00];
    let mut d = before.to_vec();
    d.extend((slice.len() as u32).to_be_bytes());
    d.extend(slice);
    d.extend(after);
    Arc::new(d)
}

#[test]
fn bpm_after_the_slice_does_not_count_as_in_front_of_the_keyframe() {
    let t0 = Instant::now();
    let mut a = Analyzer::new(t0);
    a.on_media(&Media::Video { pts_us: 0, dts_us: 0, key: true, data: idr(&bpm_nals(0), &[]) }, t0);
    let at = t0 + Duration::from_secs(2);
    a.on_media(&Media::Video { pts_us: 2_000_000, dts_us: 2_000_000, key: true, data: idr(&[], &bpm_nals(60)) }, at);
    let b = &a.stats.video.bpm;
    assert!(b.present);
    assert_eq!((b.keyframes_checked, b.keyframes_with_all, b.keyframes_missing), (2, 1, 1), "{b:?}");
    assert_eq!(b.last_missing, vec!["ts", "sm", "erm"]);
    assert_eq!(b.every_keyframe, Some(false));
    assert!(b.after_slice > 0);
}

#[test]
fn bpm_session_and_rendition_counters_add_up_over_keyframes() {
    let t0 = Instant::now();
    let mut a = Analyzer::new(t0);
    for k in 0..3u64 {
        let at = t0 + Duration::from_secs(2 * k);
        let us = 2_000_000 * k as i64;
        a.on_media(&Media::Video { pts_us: us, dts_us: us, key: true, data: idr(&bpm_nals(60), &[]) }, at);
    }
    let b = &a.stats.video.bpm;
    assert_eq!((b.ts_messages, b.sm_messages, b.erm_messages), (3, 3, 3));
    assert_eq!((b.session_total.rendered, b.session_total.output), (180, 180));
    assert_eq!(b.rendition_total.output, 180);
    assert_eq!(b.every_keyframe, Some(true));
    assert_eq!(b.malformed, 0);
}
