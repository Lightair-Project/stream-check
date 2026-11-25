//! Platform profiles: what a real platform's ingest accepts, and what it would say about a
//! stream that breaks its rules. The numbers are the platforms' published limits as the lab's
//! authors read them; they are a testing aid, not a promise about the platforms.

use serde::Serialize;

use crate::analyze::{Level, Stats};
use crate::levels;
use crate::media::ACodec;
use crate::nal::VCodec;

#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    pub name: &'static str,
    pub label: &'static str,
    pub video_codecs: &'static [VCodec],
    pub max_video_kbps: Option<u32>,
    /// A keyframe at least this often, in seconds.
    pub max_keyint_s: Option<f64>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub max_fps: Option<f64>,
    pub audio_codecs: &'static [ACodec],
    pub audio_rates: &'static [u32],
    pub b_frames_ok: bool,
    pub audio_required: bool,
    /// A stream that comes back within this many seconds continues the same session.
    pub grace_s: u32,
    /// What a good stream key looks like, for the message.
    pub key_hint: &'static str,
    /// Twitch Dual Format's rules across an Enhanced RTMP multitrack stream's tracks.
    pub dual_format: bool,
    /// H.264 profiles taken (profile_idc: 66 Baseline, 77 Main, 100 High…); empty: any.
    pub h264_profiles: &'static [u8],
    /// HEVC profiles taken (1 Main, 2 Main 10…); empty: any.
    pub hevc_profiles: &'static [u8],
    /// The platform expects limited-range video (a full-range tag is a warning).
    pub limited_range: bool,
    /// The platform shows HDR (PQ or HLG); otherwise an HDR transfer is a warning.
    pub hdr_ok: bool,
    /// Every keyframe on every track needs the broadcast performance metrics SEI in front of it
    /// (Twitch Enhanced Broadcasting, Amazon IVS multitrack).
    pub bpm_required: bool,
}

/// H.264 Baseline (and Constrained Baseline), Main, High.
const H264_8BIT: &[u8] = &[66, 77, 100];

pub const PROFILES: &[Profile] = &[
    Profile {
        name: "twitch",
        label: "Twitch-like",
        video_codecs: &[VCodec::H264],
        max_video_kbps: Some(8500),
        max_keyint_s: Some(2.0),
        max_width: Some(1920),
        max_height: Some(1080),
        max_fps: Some(60.5),
        audio_codecs: &[ACodec::Aac],
        audio_rates: &[44100, 48000],
        b_frames_ok: false,
        audio_required: true,
        grace_s: 30,
        key_hint: "live_<digits>_<letters>",
        dual_format: false,
        h264_profiles: H264_8BIT,
        hevc_profiles: &[1],
        limited_range: true,
        hdr_ok: false,
        bpm_required: false,
    },
    Profile {
        name: "twitch-dual",
        label: "Twitch Dual Format",
        video_codecs: &[VCodec::H264, VCodec::Hevc],
        // the total across tracks is held against 12.5 Mbps instead (check_tracks)
        max_video_kbps: None,
        max_keyint_s: Some(2.0),
        max_width: Some(1920),
        max_height: Some(1080),
        max_fps: Some(60.5),
        audio_codecs: &[ACodec::Aac],
        audio_rates: &[44100, 48000],
        b_frames_ok: false,
        audio_required: true,
        grace_s: 30,
        key_hint: "live_<digits>_<letters>?clientConfigId=<id>",
        dual_format: true,
        h264_profiles: H264_8BIT,
        hevc_profiles: &[1],
        limited_range: true,
        hdr_ok: false,
        bpm_required: true,
    },
    Profile {
        name: "youtube",
        label: "YouTube-like",
        video_codecs: &[VCodec::H264, VCodec::Hevc, VCodec::Av1],
        max_video_kbps: Some(51_000),
        max_keyint_s: Some(4.0),
        max_width: Some(3840),
        max_height: Some(2160),
        max_fps: Some(60.5),
        audio_codecs: &[ACodec::Aac, ACodec::Opus],
        audio_rates: &[44100, 48000],
        b_frames_ok: true,
        audio_required: true,
        grace_s: 10,
        key_hint: "xxxx-xxxx-xxxx-xxxx-xxxx",
        dual_format: false,
        h264_profiles: H264_8BIT,
        hevc_profiles: &[1, 2],
        limited_range: true,
        hdr_ok: true,
        bpm_required: false,
    },
    Profile {
        name: "kick",
        label: "Kick-like",
        video_codecs: &[VCodec::H264],
        max_video_kbps: Some(8000),
        max_keyint_s: Some(2.0),
        max_width: Some(1920),
        max_height: Some(1080),
        max_fps: Some(60.5),
        audio_codecs: &[ACodec::Aac],
        audio_rates: &[44100, 48000],
        b_frames_ok: false,
        audio_required: true,
        grace_s: 15,
        key_hint: "sk_<region>_<letters>",
        dual_format: false,
        h264_profiles: H264_8BIT,
        hevc_profiles: &[1],
        limited_range: true,
        hdr_ok: false,
        bpm_required: false,
    },
    Profile {
        name: "open",
        label: "Open (accepts anything)",
        video_codecs: &[VCodec::H264, VCodec::Hevc, VCodec::Av1, VCodec::Vp9, VCodec::Vp8],
        max_video_kbps: None,
        max_keyint_s: None,
        max_width: None,
        max_height: None,
        max_fps: None,
        audio_codecs: &[ACodec::Aac, ACodec::Opus],
        audio_rates: &[],
        b_frames_ok: true,
        audio_required: false,
        grace_s: 10,
        key_hint: "anything",
        dual_format: false,
        h264_profiles: &[],
        hevc_profiles: &[],
        limited_range: false,
        hdr_ok: true,
        bpm_required: false,
    },
];

pub fn find(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

impl Profile {
    /// Whether this platform would take the key.
    pub fn key_ok(&self, key: &str) -> bool {
        match self.name {
            "twitch" | "twitch-dual" => key.starts_with("live_") && key.len() >= 12,
            "youtube" => {
                let parts: Vec<&str> = key.split('-').collect();
                parts.len() == 5 && parts.iter().all(|p| p.len() == 4 && p.chars().all(|c| c.is_ascii_alphanumeric()))
            }
            "kick" => key.starts_with("sk_") && key.len() >= 10,
            _ => !key.is_empty(),
        }
    }

    /// An example key that passes.
    pub fn example_key(&self) -> &'static str {
        match self.name {
            "twitch" | "twitch-dual" => "live_123456789_labtestkey",
            "youtube" => "abcd-efgh-ijkl-mnop-qrst",
            "kick" => "sk_us-west-2_labtestkey",
            _ => "test",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Violation {
    pub rule: &'static str,
    pub level: Level,
    pub msg: String,
}

/// Every rule the stream breaks right now.
pub fn check(p: &Profile, s: &Stats, seconds_live: f64) -> Vec<Violation> {
    let mut out = vec![];
    let mut v = |rule, level, msg: String| out.push(Violation { rule, level, msg });
    if let Some(c) = s.video.codec {
        if !p.video_codecs.contains(&c) {
            v("video-codec", Level::Error, format!("{} refuses {} video", p.label, c.name()));
        }
    }
    if let Some(max) = p.max_video_kbps {
        if s.video.kbps > max as f64 {
            v("video-bitrate", Level::Error, format!("video at {:.0} kbps is over {}'s {max} kbps", s.video.kbps, p.label));
        }
    }
    if let Some(max) = p.max_keyint_s {
        let worst = s.video.since_key_s.unwrap_or(0.0).max(s.video.keyint_last_s.unwrap_or(0.0));
        if worst > max + 0.25 {
            v("keyframe-interval", Level::Error, format!("a keyframe every {worst:.1} s; {} wants one at least every {max:.0} s", p.label));
        }
    }
    if let Some(sps) = &s.video.sps {
        if p.max_width.is_some_and(|w| sps.width > w) || p.max_height.is_some_and(|h| sps.height > h) {
            v("resolution", Level::Error, format!("{}x{} is bigger than {} takes", sps.width, sps.height, p.label));
        }
    }
    if let Some(max) = p.max_fps {
        if s.video.fps > max {
            v("frame-rate", Level::Error, format!("{:.1} fps is over {}'s {max:.0}", s.video.fps, p.label));
        }
    }
    if s.video.b_frames && !p.b_frames_ok {
        v("b-frames", Level::Warn, format!("B-frames in the stream; {} asks for none", p.label));
    }
    if let Some(c) = s.audio.codec {
        if !p.audio_codecs.contains(&c) {
            v("audio-codec", Level::Error, format!("{} refuses {} audio", p.label, c.name()));
        }
        if !p.audio_rates.is_empty() && c == ACodec::Aac && !p.audio_rates.contains(&s.audio.sample_rate) {
            v("audio-rate", Level::Error, format!("{} Hz audio; {} takes 44100 or 48000", s.audio.sample_rate, p.label));
        }
    } else if p.audio_required && s.has_video && seconds_live > 5.0 {
        v("audio-missing", Level::Warn, format!("no audio after {seconds_live:.0} s; {} expects it", p.label));
    }
    video_rules(p, s, None, &mut out);
    out
}

/// The rules on what the video bitstream says: profile, level, range, HDR, BPM. `track`: the
/// multitrack track these stats are of (its rules are named `track-…`), or `None` for track 0.
fn video_rules(p: &Profile, s: &Stats, track: Option<u8>, out: &mut Vec<Violation>) {
    let (Some(codec), Some(sps)) = (s.video.codec, &s.video.sps) else { return };
    let who = track.map_or(String::new(), |t| format!("track {t}: "));
    let mut v = |rule: (&'static str, &'static str), level, msg: String| {
        out.push(Violation { rule: if track.is_some() { rule.1 } else { rule.0 }, level, msg: format!("{who}{msg}") })
    };
    let taken = match codec {
        VCodec::H264 => p.h264_profiles,
        VCodec::Hevc => p.hevc_profiles,
        _ => &[],
    };
    if p.video_codecs.contains(&codec) && !taken.is_empty() && !taken.contains(&sps.profile_idc) {
        v(
            ("video-profile", "track-video-profile"),
            Level::Error,
            format!("{} doesn't take {} {} video", p.label, codec.name(), sps.profile),
        );
    }
    if let Some(first) = s.video.bitstream.level_warnings.first() {
        let more = s.video.bitstream.level_warnings.len() - 1;
        v(
            ("video-level", "track-video-level"),
            Level::Warn,
            format!("level {} is too low: {first}{}", sps.level, if more > 0 { format!(" (and {more} more)") } else { String::new() }),
        );
    }
    if p.limited_range && sps.colour.full_range == Some(true) {
        v(
            ("video-range", "track-video-range"),
            Level::Warn,
            format!("the picture is tagged full range; {} expects limited range", p.label),
        );
    }
    if !p.hdr_ok && levels::is_hdr(sps) {
        v(
            ("video-hdr", "track-video-hdr"),
            Level::Warn,
            format!("an HDR transfer ({}); {} shows SDR", sps.colour.transfer_name.as_deref().unwrap_or("?"), p.label),
        );
    }
    let b = &s.video.bpm;
    if p.bpm_required && codec.is_nal() && b.keyframes_checked > 0 && !b.last_missing.is_empty() {
        v(
            ("bpm-missing", "track-bpm-missing"),
            Level::Error,
            format!(
                "the latest keyframe had no BPM {} SEI in front of it ({} of {} keyframes); {} drops streams without the broadcast performance metrics before every IDR",
                b.last_missing.join(", "),
                b.keyframes_missing,
                b.keyframes_checked,
                p.label
            ),
        );
    }
}

/// The frame rates Twitch Enhanced Broadcasting (Amazon IVS multitrack) takes.
pub const DUAL_FRAME_RATES: &[f64] = &[24.0, 25.0, 30.0, 48.0, 50.0, 60.0];

/// Twitch Dual Format's own total: picture across every track.
pub const DUAL_MAX_TOTAL_KBPS: f64 = 12_500.0;

/// The rules across an Enhanced RTMP multitrack stream's tracks (`s0` is track 0's stats,
/// `tracks` the others'): each track's keyframes, and for Twitch Dual Format a landscape and a
/// vertical picture, the vertical at 1080x1920, track 0 the largest landscape one, the total
/// against 12.5 Mbps.
pub fn check_tracks(p: &Profile, s0: &Stats, tracks: &[(u8, &Stats)], seconds_live: f64) -> Vec<Violation> {
    let mut out = vec![];
    let mut v = |rule, level, msg: String| out.push(Violation { rule, level, msg });
    if let Some(max) = p.max_keyint_s {
        let worst = tracks
            .iter()
            .filter(|(_, s)| s.has_video)
            .map(|(id, s)| (*id, s.video.since_key_s.unwrap_or(0.0).max(s.video.keyint_last_s.unwrap_or(0.0))))
            .fold(None, |a: Option<(u8, f64)>, b| if a.is_none_or(|a| b.1 > a.1) { Some(b) } else { a });
        if let Some((id, w)) = worst.filter(|w| w.1 > max + 0.25) {
            v(
                "track-keyframe-interval",
                Level::Error,
                format!("track {id}: a keyframe every {w:.1} s; {} wants one at least every {max:.0} s", p.label),
            );
        }
    }
    if !p.dual_format {
        return out;
    }
    for (id, s) in tracks {
        video_rules(p, s, Some(*id), &mut out);
    }
    let mut v = |rule, level, msg: String| out.push(Violation { rule, level, msg });
    // a declared rate the service doesn't take (59.94 and 29.97 among them)
    let odd: Vec<String> = std::iter::once((0u8, s0))
        .chain(tracks.iter().map(|(id, s)| (*id, *s)))
        .filter_map(|(id, s)| s.video.sps.as_ref()?.declared_fps.map(|f| (id, f)))
        .filter(|(_, f)| !DUAL_FRAME_RATES.iter().any(|r| (f - r).abs() < 0.01))
        .map(|(id, f)| format!("track {id} at {f:.3} fps"))
        .collect();
    if !odd.is_empty() {
        v("dual-frame-rate", Level::Warn, format!("{}; {} takes 24, 25, 30, 48, 50 or 60", odd.join(", "), p.label));
    }
    // captions copied onto every track, so every rendition's viewers get them
    let c0 = s0.video.captions.sei_messages;
    let bare: Vec<String> = tracks
        .iter()
        .filter(|(_, s)| s.has_video && (c0 >= 10) != (s.video.captions.sei_messages >= 10))
        .map(|(id, _)| format!("track {id}"))
        .collect();
    if !bare.is_empty() {
        v(
            "dual-captions",
            Level::Warn,
            if c0 >= 10 {
                format!("captions on track 0 but not on {}; {} wants them on every track", bare.join(", "), p.label)
            } else {
                format!("captions on {} but not on track 0; {} wants them on every track", bare.join(", "), p.label)
            },
        );
    }
    // every picture with its size, track 0 first
    let sizes: Vec<(u8, u32, u32, f64)> = std::iter::once((0u8, s0))
        .chain(tracks.iter().map(|(id, s)| (*id, *s)))
        .filter_map(|(id, s)| s.video.sps.as_ref().map(|sps| (id, sps.width, sps.height, s.video.kbps)))
        .collect();
    if sizes.is_empty() || seconds_live < 5.0 {
        return out;
    }
    let landscape: Vec<_> = sizes.iter().filter(|t| t.1 > t.2).collect();
    let vertical: Vec<_> = sizes.iter().filter(|t| t.2 > t.1).collect();
    if landscape.is_empty() || vertical.is_empty() {
        v(
            "dual-tracks",
            Level::Error,
            format!(
                "{} wants a 16:9 and a 9:16 picture; this stream has {} landscape and {} vertical",
                p.label,
                landscape.len(),
                vertical.len()
            ),
        );
    }
    if let Some(big) = vertical.iter().max_by_key(|t| t.1 as u64 * t.2 as u64) {
        if (big.1, big.2) != (1080, 1920) {
            v(
                "dual-vertical-size",
                Level::Error,
                format!("the largest vertical track (track {}) is {}x{}; {} wants 1080x1920", big.0, big.1, big.2, p.label),
            );
        }
    }
    if let Some(big) = landscape.iter().max_by_key(|t| t.1 as u64 * t.2 as u64) {
        if big.0 != 0 {
            v(
                "dual-track0",
                Level::Error,
                format!("track 0 should be the largest landscape picture; track {} ({}x{}) is larger", big.0, big.1, big.2),
            );
        }
    } else if !vertical.is_empty() && sizes.first().is_some_and(|t| t.0 == 0 && t.2 > t.1) {
        v("dual-track0", Level::Error, "track 0 is vertical; it should be the largest landscape picture".into());
    }
    let total: f64 = sizes.iter().map(|t| t.3).sum();
    if total > DUAL_MAX_TOTAL_KBPS {
        v(
            "dual-bitrate",
            Level::Error,
            format!("{total:.0} kbps of picture across the tracks is over {}'s {DUAL_MAX_TOTAL_KBPS:.0}", p.label),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::Sps;

    #[test]
    fn keys_are_checked_per_platform() {
        for p in PROFILES {
            assert!(p.key_ok(p.example_key()), "{}", p.name);
        }
        assert!(!find("twitch").unwrap().key_ok("wrong"));
        assert!(!find("youtube").unwrap().key_ok("live_123456789_x"));
        assert!(find("open").unwrap().key_ok("anything"));
    }

    #[test]
    fn a_twitch_stream_with_hevc_and_slow_keyframes_breaks_rules() {
        let mut s = Stats { has_video: true, ..Default::default() };
        s.video.codec = Some(VCodec::Hevc);
        s.video.keyint_last_s = Some(4.0);
        s.video.kbps = 9000.0;
        s.video.sps = Some(Sps { width: 1280, height: 720, ..Default::default() });
        let rules: Vec<_> = check(find("twitch").unwrap(), &s, 10.0).into_iter().map(|v| v.rule).collect();
        assert!(rules.contains(&"video-codec") && rules.contains(&"keyframe-interval") && rules.contains(&"video-bitrate"));
        assert!(rules.contains(&"audio-missing"));
        // the same stream is fine for YouTube, except for the keyframes it's right at the limit of
        let y: Vec<_> = check(find("youtube").unwrap(), &s, 10.0).into_iter().map(|v| v.rule).collect();
        assert!(!y.contains(&"video-codec") && !y.contains(&"keyframe-interval"));
    }
}
