//! Platform profiles: what a real platform's ingest accepts, and what it would say about a
//! stream that breaks its rules. The numbers are the platforms' published limits as the lab's
//! authors read them; they are a testing aid, not a promise about the platforms.

use serde::Serialize;

use crate::analyze::{Level, Stats};
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
}

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
    },
    Profile {
        name: "youtube",
        label: "YouTube-like",
        video_codecs: &[VCodec::H264, VCodec::Hevc],
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
    },
    Profile {
        name: "open",
        label: "Open (accepts anything)",
        video_codecs: &[VCodec::H264, VCodec::Hevc],
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
    },
];

pub fn find(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

impl Profile {
    /// Whether this platform would take the key.
    pub fn key_ok(&self, key: &str) -> bool {
        match self.name {
            "twitch" => key.starts_with("live_") && key.len() >= 12,
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
            "twitch" => "live_123456789_labtestkey",
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
