//! SEI messages of H.264 and HEVC pictures: the broadcast performance metrics (BPM) that
//! Twitch's Enhanced Broadcasting (Amazon IVS multitrack) wants before every IDR, captions
//! (CEA-608 and CEA-708 in ATSC A/53 user data), and the other user data a sender writes.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::nal::{self, VCodec};

/// BPM timestamp message.
pub const UUID_BPM_TS: [u8; 16] = 0x0aec_ffe7_5272_4e2f_a62f_d19c_d61a_93b5u128.to_be_bytes();
/// BPM session metrics message.
pub const UUID_BPM_SM: [u8; 16] = 0xca60_e71c_6a8b_4388_a377_151d_f7bf_8ac2u128.to_be_bytes();
/// BPM encoded rendition metrics message.
pub const UUID_BPM_ERM: [u8; 16] = 0xf1fb_c1d5_101e_4fb5_a61e_b8ce_3c07_b8c0u128.to_be_bytes();

/// SEI payload types.
pub const SEI_REGISTERED: u32 = 4;
pub const SEI_UNREGISTERED: u32 = 5;

/// Whether a NAL unit of this type holds SEI messages.
pub fn is_sei(codec: VCodec, t: u8) -> bool {
    match codec {
        VCodec::H264 => t == 6,
        VCodec::Hevc => t == 39 || t == 40,
        _ => false,
    }
}

/// One SEI message: its payload type and bytes (emulation prevention removed).
pub struct Message {
    pub kind: u32,
    pub payload: Vec<u8>,
}

/// The SEI messages in one SEI NAL unit.
pub fn messages(codec: VCodec, nal: &[u8]) -> Vec<Message> {
    let d = nal::unescape(nal);
    let mut i = if codec == VCodec::Hevc { 2 } else { 1 };
    let mut out = vec![];
    let coded = |i: &mut usize| -> Option<u32> {
        let mut v = 0u32;
        loop {
            let b = *d.get(*i)?;
            *i += 1;
            v += b as u32;
            if b != 0xff {
                return Some(v);
            }
        }
    };
    // stop at the RBSP's trailing bits
    while i < d.len() && !(i + 1 == d.len() && d[i] == 0x80) {
        let Some(kind) = coded(&mut i) else { break };
        let Some(size) = coded(&mut i) else { break };
        let end = i + size as usize;
        if end > d.len() {
            break;
        }
        out.push(Message { kind, payload: d[i..end].to_vec() });
        i = end;
    }
    out
}

/// A name for an SEI payload type, for the report.
pub fn type_name(codec: VCodec, kind: u32) -> String {
    let name = match (codec, kind) {
        (_, 0) => "buffering period",
        (_, 1) => "picture timing",
        (_, 2) => "pan-scan",
        (_, 3) => "filler",
        (_, 4) => "registered user data (T.35)",
        (_, 5) => "unregistered user data",
        (_, 6) => "recovery point",
        (VCodec::H264, 45) => "frame packing",
        (_, 47) => "display orientation",
        (VCodec::Hevc, 129) => "active parameter sets",
        (VCodec::Hevc, 132) => "decoded picture hash",
        (VCodec::Hevc, 136) => "time code",
        (_, 137) => "mastering display colour volume",
        (_, 144) => "content light level",
        (_, 147) => "alternative transfer characteristics",
        _ => return format!("type {kind}"),
    };
    format!("{name} ({kind})")
}

/// Which of the three BPM messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BpmKind {
    Ts,
    Sm,
    Erm,
}

/// One timestamp of a BPM message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BpmTime {
    /// 1 RFC 3339 string, 2 milliseconds since 1970, 3 a delta in nanoseconds.
    pub kind: u8,
    /// 1 composition, 2 encode request, 3 encode request complete, 4 packet interleave.
    pub event: u8,
    /// The time as milliseconds since 1970 (types 1 and 2).
    pub unix_ms: Option<f64>,
    /// The text of an RFC 3339 time.
    pub text: Option<String>,
    pub delta_ns: Option<i64>,
}

/// A BPM message read from user data unregistered.
#[derive(Debug, Clone, PartialEq)]
pub struct Bpm {
    pub kind: BpmKind,
    pub times: Vec<BpmTime>,
    /// (tag, value) pairs: SM 1 rendered, 2 lagged, 3 dropped, 4 output; ERM 1 input, 2 skipped,
    /// 3 output.
    pub counters: Vec<(u8, u32)>,
}

/// Which BPM message a user data unregistered payload is, if any.
pub fn bpm_kind(payload: &[u8]) -> Option<BpmKind> {
    let uuid = payload.get(..16)?;
    if uuid == UUID_BPM_TS {
        Some(BpmKind::Ts)
    } else if uuid == UUID_BPM_SM {
        Some(BpmKind::Sm)
    } else if uuid == UUID_BPM_ERM {
        Some(BpmKind::Erm)
    } else {
        None
    }
}

/// Reads a BPM message (a user data unregistered payload, UUID first). `None`: not BPM or cut
/// short.
pub fn bpm(payload: &[u8]) -> Option<Bpm> {
    let kind = bpm_kind(payload)?;
    let p = &payload[16..];
    let mut i = 0;
    let n = (*p.get(i)? & 0x0f) as usize + 1;
    i += 1;
    let mut times = vec![];
    for _ in 0..n {
        let (ty, event) = (*p.get(i)?, *p.get(i + 1)?);
        i += 2;
        let mut t = BpmTime { kind: ty, event, unix_ms: None, text: None, delta_ns: None };
        match ty {
            1 => {
                let len = p.get(i..)?.iter().position(|&b| b == 0)?;
                let text = String::from_utf8_lossy(&p[i..i + len]).into_owned();
                t.unix_ms = rfc3339_ms(&text);
                t.text = Some(text);
                i += len + 1;
            }
            2 => {
                t.unix_ms = Some(u64::from_be_bytes(p.get(i..i + 8)?.try_into().ok()?) as f64);
                i += 8;
            }
            3 => {
                t.delta_ns = Some(i64::from_be_bytes(p.get(i..i + 8)?.try_into().ok()?));
                i += 8;
            }
            _ => return None,
        }
        times.push(t);
    }
    let mut counters = vec![];
    if kind != BpmKind::Ts {
        let n = (*p.get(i)? & 0x0f) as usize + 1;
        i += 1;
        for _ in 0..n {
            let tag = *p.get(i)?;
            let value = u32::from_be_bytes(p.get(i + 1..i + 5)?.try_into().ok()?);
            counters.push((tag, value));
            i += 5;
        }
    }
    Some(Bpm { kind, times, counters })
}

/// An RFC 3339 time ("2024-03-25T15:10:34.489Z", or with an offset) as milliseconds since 1970.
pub fn rfc3339_ms(s: &str) -> Option<f64> {
    let s = s.trim();
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if !matches!(s.as_bytes().get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut i = 19;
    let mut frac = 0.0;
    if s.as_bytes().get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while s.as_bytes().get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        frac = format!("0.{}", s.get(start..i)?).parse().ok()?;
    }
    let offset_min = match s.as_bytes().get(i) {
        Some(b'Z' | b'z') | None => 0,
        Some(b'+') => num(i + 1, i + 3)? * 60 + num(i + 4, i + 6)?,
        Some(b'-') => -(num(i + 1, i + 3)? * 60 + num(i + 4, i + 6)?),
        _ => return None,
    };
    // days since 1970-01-01 of a Gregorian date
    let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    Some((secs as f64 + frac) * 1000.0)
}

/// The caption data of an ATSC A/53 registered user data payload (T.35 country 0xB5, provider
/// 0x0031, "GA94", type 3): each construct as (valid, cc_type, byte 1, byte 2).
pub fn a53_cc(payload: &[u8]) -> Option<Vec<(bool, u8, u8, u8)>> {
    if payload.get(..8)? != [0xb5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03] {
        return None;
    }
    let flags = *payload.get(8)?;
    if flags & 0x40 == 0 {
        return Some(vec![]);
    }
    let count = (flags & 0x1f) as usize;
    let data = payload.get(10..10 + count * 3)?;
    Some(data.chunks(3).map(|c| (c[0] & 0x04 != 0, c[0] & 0x03, c[1], c[2])).collect())
}

/// What the captions in a stream look like.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CaptionStats {
    pub present: bool,
    /// A/53 caption SEI messages seen.
    pub sei_messages: u64,
    /// CEA-608 byte pairs with content (not padding), field 1 (CC1, CC2) and field 2 (CC3, CC4).
    pub cea608_field1: u64,
    pub cea608_field2: u64,
    /// CEA-708 (DTVCC) packets put together.
    pub cea708_packets: u64,
    /// CEA-608 channels named by control codes: "CC1"…"CC4".
    pub channels_608: Vec<String>,
    /// CEA-708 service numbers that carried data.
    pub services_708: Vec<u8>,
    /// The latest CEA-608 field-1 text, up to 80 characters.
    pub recent_text: String,
    /// Seconds into the session of the latest caption data.
    pub last_at_s: Option<f64>,
}

/// Puts captions together across pictures.
#[derive(Debug, Default)]
pub struct Captions {
    pub stats: CaptionStats,
    dtvcc: Vec<u8>,
    dtvcc_len: usize,
}

impl Captions {
    /// One A/53 caption payload's constructs, at `t` seconds.
    pub fn feed(&mut self, cc: &[(bool, u8, u8, u8)], t: f64) {
        self.stats.present = true;
        self.stats.sei_messages += 1;
        for &(valid, ty, b1, b2) in cc {
            if !valid {
                continue;
            }
            let s = &mut self.stats;
            s.last_at_s = Some(t);
            match ty {
                0 | 1 => {
                    let (c1, c2) = (b1 & 0x7f, b2 & 0x7f);
                    if c1 == 0 && c2 == 0 {
                        continue;
                    }
                    if ty == 0 {
                        s.cea608_field1 += 1;
                    } else {
                        s.cea608_field2 += 1;
                    }
                    if (0x10..=0x1f).contains(&c1) {
                        // a control code names its channel: bit 3 is the second channel of the field
                        let ch = format!("CC{}", 1 + (c1 & 0x08 != 0) as u8 + 2 * ty);
                        if !s.channels_608.contains(&ch) {
                            s.channels_608.push(ch);
                            s.channels_608.sort();
                        }
                        // carriage return, erase, end of caption: a break in the text
                        if ty == 0 && matches!(c1 & 0x77, 0x14) && matches!(c2, 0x2c | 0x2d | 0x2f) && !s.recent_text.ends_with(' ') {
                            s.recent_text.push(' ');
                        }
                    } else if ty == 0 {
                        for c in [c1, c2] {
                            if (0x20..0x7f).contains(&c) {
                                s.recent_text.push(c as char);
                            }
                        }
                        let n = s.recent_text.chars().count();
                        if n > 80 {
                            s.recent_text = s.recent_text.chars().skip(n - 80).collect();
                        }
                    }
                }
                3 => {
                    self.dtvcc.clear();
                    let code = (b1 & 0x3f) as usize;
                    self.dtvcc_len = if code == 0 { 128 } else { code * 2 };
                    self.dtvcc.extend([b1, b2]);
                    self.dtvcc_done();
                }
                _ => {
                    if !self.dtvcc.is_empty() {
                        self.dtvcc.extend([b1, b2]);
                        self.dtvcc_done();
                    }
                }
            }
        }
    }

    /// A whole DTVCC packet: the services its blocks are for.
    fn dtvcc_done(&mut self) {
        if self.dtvcc.len() < self.dtvcc_len {
            return;
        }
        let p = std::mem::take(&mut self.dtvcc);
        let s = &mut self.stats;
        s.cea708_packets += 1;
        let mut i = 1;
        while i < self.dtvcc_len.min(p.len()) {
            let mut service = p[i] >> 5;
            let size = (p[i] & 0x1f) as usize;
            i += 1;
            if service == 7 && size != 0 {
                service = p.get(i).map_or(0, |b| b & 0x3f);
                i += 1;
            }
            if service == 0 || size == 0 {
                break;
            }
            if !s.services_708.contains(&service) {
                s.services_708.push(service);
                s.services_708.sort();
            }
            i += size;
        }
    }
}

/// A user data unregistered message that isn't BPM: its UUID, how often it came, and its text
/// when it reads as text (encoders write their name and settings this way).
#[derive(Debug, Clone, Serialize)]
pub struct UserData {
    pub uuid: String,
    pub count: u64,
    pub text: Option<String>,
}

/// Notes a user data unregistered payload that isn't BPM (at most 8 different UUIDs are kept).
pub fn note_user_data(list: &mut Vec<UserData>, payload: &[u8]) {
    let Some(uuid) = payload.get(..16) else { return };
    let id = uuid_string(uuid);
    if let Some(u) = list.iter_mut().find(|u| u.uuid == id) {
        u.count += 1;
        return;
    }
    if list.len() >= 8 {
        return;
    }
    let body = &payload[16..];
    let body = &body[..body.iter().position(|&b| b == 0).unwrap_or(body.len())];
    let printable = body.iter().filter(|&&b| (0x20..0x7f).contains(&b) || b == b'\n').count();
    let text = (body.len() >= 4 && printable * 10 >= body.len() * 9).then(|| {
        let t = String::from_utf8_lossy(body);
        if t.chars().count() > 300 { format!("{}…", t.chars().take(300).collect::<String>()) } else { t.into_owned() }
    });
    list.push(UserData { uuid: id, count: 1, text });
}

fn uuid_string(u: &[u8]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// SEI payload type counts by name.
pub type SeiCounts = BTreeMap<String, u64>;
