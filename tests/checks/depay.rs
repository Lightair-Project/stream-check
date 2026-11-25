//! WHIP's RTP payloads into pictures, for the codecs beyond H.264: HEVC (RFC 7798), AV1 (the AV1
//! RTP payload format), VP9 (RFC 9628) and VP8 (RFC 7741). Packets are written as each format
//! describes them.

use stream_check::av1;
use stream_check::depay::Depay;
use stream_check::nal::VCodec;
use stream_check::vp9;

fn picture(codec: VCodec, packets: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut d = Depay::new(codec);
    for p in packets {
        d.push(p);
    }
    d.take()
}

#[test]
fn an_hevc_nal_unit_in_fragmentation_units_comes_back_whole_with_its_header() {
    // an IDR_N_LP (20) NAL unit: header 0x28 0x01, then its payload
    let payload: Vec<u8> = (0..30u8).collect();
    let fu = |start: bool, end: bool, part: &[u8]| {
        // FU type 49 with the NAL header's layer and temporal id, then the FU header
        let mut p = vec![49 << 1, 0x01, (start as u8) << 7 | (end as u8) << 6 | 20];
        p.extend(part);
        p
    };
    let nals =
        picture(VCodec::Hevc, &[fu(true, false, &payload[..10]), fu(false, false, &payload[10..20]), fu(false, true, &payload[20..])]);
    let mut expected = vec![0x28, 0x01];
    expected.extend(&payload);
    assert_eq!(nals, vec![expected]);
}

#[test]
fn an_hevc_aggregation_packet_gives_each_nal_unit() {
    let (vps, sps) = (vec![0x40u8, 0x01, 0xaa], vec![0x42u8, 0x01, 0xbb, 0xcc]);
    let mut ap = vec![48 << 1, 0x01];
    for n in [&vps, &sps] {
        ap.extend((n.len() as u16).to_be_bytes());
        ap.extend(n);
    }
    assert_eq!(picture(VCodec::Hevc, &[ap]), vec![vps, sps]);
}

#[test]
fn an_av1_obu_fragmented_across_packets_is_joined_and_the_temporal_delimiter_left_out() {
    let seq_payload = vec![0x00u8, 0x00, 0x00, 0x24, 0x4f];
    let frame_payload: Vec<u8> = (0..40u8).collect();
    // packet 1: two elements (W=2): a temporal delimiter with its length, then a sequence header
    let mut p1 = vec![0x20, 1, av1::OBU_TEMPORAL_DELIMITER << 3, av1::OBU_SEQUENCE_HEADER << 3];
    p1.extend(&seq_payload);
    // packets 2 and 3: one frame OBU, continued (Y) into the next packet, which continues it (Z)
    let mut p2 = vec![0x50, av1::OBU_FRAME << 3];
    p2.extend(&frame_payload[..25]);
    let mut p3 = vec![0x90];
    p3.extend(&frame_payload[25..]);
    let tu = picture(VCodec::Av1, &[p1, p2, p3]);
    assert_eq!(tu.len(), 1);
    let obus = av1::obus(&tu[0]);
    let kinds: Vec<u8> = obus.iter().map(|o| o.kind).collect();
    assert_eq!(kinds, vec![av1::OBU_SEQUENCE_HEADER, av1::OBU_FRAME]);
    assert_eq!((obus[0].payload, obus[1].payload), (&seq_payload[..], &frame_payload[..]));
}

#[test]
fn an_av1_fragment_whose_start_was_lost_is_left_out() {
    let mut p = vec![0x90];
    p.extend([1, 2, 3]);
    assert!(picture(VCodec::Av1, &[p]).is_empty());
}

#[test]
fn vp9_spatial_layers_of_one_picture_become_one_superframe() {
    // descriptor: B (0x08) and E (0x04) per layer frame, no picture id or layer indices
    let (low, high) = (vec![0x82u8, 0x49, 0x83, 0x42, 1], vec![0x86u8, 7, 7, 7]);
    let frame = |part: &[u8], b: bool, e: bool| {
        let mut p = vec![(b as u8) << 3 | (e as u8) << 2];
        p.extend(part);
        p
    };
    let packets = [frame(&low[..2], true, false), frame(&low[2..], false, true), frame(&high, true, true)];
    let chunk = picture(VCodec::Vp9, &packets);
    assert_eq!(chunk.len(), 1);
    assert_eq!(vp9::vp9_frames(&chunk[0]), vec![&low[..], &high[..]]);
}

#[test]
fn a_vp8_payload_descriptor_with_a_long_picture_id_is_skipped() {
    let frame: Vec<u8> = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xf0, 0x00, 0xaa, 0xbb];
    // X, S (start of partition 0); then I; then a 15-bit picture id
    let mut first = vec![0x90, 0x80, 0x80 | 0x12, 0x34];
    first.extend(&frame[..6]);
    // the rest, without the extension
    let mut second = vec![0x00];
    second.extend(&frame[6..]);
    let out = picture(VCodec::Vp8, &[first, second]);
    assert_eq!(out, vec![frame.clone()]);
    let f = vp9::vp8_frame(&out[0]).unwrap();
    assert!(f.key);
    assert_eq!((f.width, f.height), (320, 240));
}
