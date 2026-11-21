//! A small Matroska writer: H.264, HEVC, AAC and Opus in clusters of known size. It makes the
//! recording of a WHIP stream, and the input the lab's ffmpeg helper decodes for loudness and
//! thumbnails. Written so that a file that stops anywhere still plays, and so that a closing
//! file can be given its true size and duration.

use crate::media::ACodec;
use crate::nal::VCodec;

pub struct VideoTrack {
    pub codec: VCodec,
    pub record: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct AudioTrack {
    pub codec: ACodec,
    pub config: Vec<u8>,
    pub rate: u32,
    pub channels: u8,
}

fn id(out: &mut Vec<u8>, id: u32) {
    let b = id.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(3);
    out.extend(&b[skip..]);
}

fn size(out: &mut Vec<u8>, n: u64) {
    let mut len = 1;
    while len < 8 && n >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    let v = n | (1u64 << (7 * len));
    out.extend(&v.to_be_bytes()[8 - len..]);
}

fn elem(out: &mut Vec<u8>, i: u32, body: &[u8]) {
    id(out, i);
    size(out, body.len() as u64);
    out.extend(body);
}

fn uint(out: &mut Vec<u8>, i: u32, v: u64) {
    let b = v.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(7);
    elem(out, i, &b[skip..]);
}

fn float(out: &mut Vec<u8>, i: u32, v: f64) {
    elem(out, i, &v.to_be_bytes());
}

fn text(out: &mut Vec<u8>, i: u32, v: &str) {
    elem(out, i, v.as_bytes());
}

/// Where the header's patchable fields sit, for [`patch`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Offsets {
    pub segment_size: usize,
    pub segment_start: usize,
    pub duration: usize,
}

pub struct Mkv {
    video: Option<VideoTrack>,
    audio: Option<AudioTrack>,
    pub offsets: Offsets,
    cluster: Vec<u8>,
    cluster_start_ms: i64,
    in_cluster: bool,
    pub last_ms: i64,
    base_us: Option<i64>,
}

impl Mkv {
    pub fn new(video: Option<VideoTrack>, audio: Option<AudioTrack>) -> (Mkv, Vec<u8>) {
        let mut m = Mkv {
            video,
            audio,
            offsets: Offsets::default(),
            cluster: vec![],
            cluster_start_ms: 0,
            in_cluster: false,
            last_ms: 0,
            base_us: None,
        };
        let header = m.header();
        (m, header)
    }

    fn header(&mut self) -> Vec<u8> {
        let mut out = vec![];
        let mut ebml = vec![];
        uint(&mut ebml, 0x4286, 1);
        uint(&mut ebml, 0x42F7, 1);
        uint(&mut ebml, 0x42F2, 4);
        uint(&mut ebml, 0x42F3, 8);
        text(&mut ebml, 0x4282, "matroska");
        uint(&mut ebml, 0x4287, 4);
        uint(&mut ebml, 0x4285, 2);
        elem(&mut out, 0x1A45DFA3, &ebml);
        // Segment, size unknown until the file is closed
        id(&mut out, 0x18538067);
        self.offsets.segment_size = out.len();
        out.extend([0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        self.offsets.segment_start = out.len();

        let mut info = vec![];
        uint(&mut info, 0x2AD7B1, 1_000_000);
        text(&mut info, 0x4D80, "stream-check");
        text(&mut info, 0x5741, "stream-check");
        // Duration: a float of 8 bytes, patched at the end
        id(&mut info, 0x4489);
        info.push(0x88);
        let duration_in_info = info.len();
        info.extend(0f64.to_be_bytes());
        let mut seg = vec![];
        id(&mut seg, 0x1549A966);
        size(&mut seg, info.len() as u64);
        let info_start = seg.len();
        seg.extend(&info);
        self.offsets.duration = out.len() + info_start + duration_in_info;
        out.extend(seg);

        let mut tracks = vec![];
        let mut n = 1;
        if let Some(v) = &self.video {
            let mut t = vec![];
            uint(&mut t, 0xD7, n);
            uint(&mut t, 0x73C5, n);
            uint(&mut t, 0x83, 1);
            text(&mut t, 0x86, if v.codec == VCodec::H264 { "V_MPEG4/ISO/AVC" } else { "V_MPEGH/ISO/HEVC" });
            elem(&mut t, 0x63A2, &v.record);
            let mut video = vec![];
            uint(&mut video, 0xB0, v.width as u64);
            uint(&mut video, 0xBA, v.height as u64);
            elem(&mut t, 0xE0, &video);
            elem(&mut tracks, 0xAE, &t);
            n += 1;
        }
        if let Some(a) = &self.audio {
            let mut t = vec![];
            uint(&mut t, 0xD7, n);
            uint(&mut t, 0x73C5, n);
            uint(&mut t, 0x83, 2);
            text(&mut t, 0x86, if a.codec == ACodec::Aac { "A_AAC" } else { "A_OPUS" });
            if a.codec == ACodec::Aac {
                elem(&mut t, 0x63A2, &a.config);
            } else {
                let mut head = b"OpusHead".to_vec();
                head.extend([1, a.channels.max(1)]);
                head.extend(0u16.to_le_bytes());
                head.extend(a.rate.to_le_bytes());
                head.extend(0u16.to_le_bytes());
                head.push(0);
                elem(&mut t, 0x63A2, &head);
                uint(&mut t, 0x56AA, 6_500_000);
                uint(&mut t, 0x56BB, 80_000_000);
            }
            let mut audio = vec![];
            float(&mut audio, 0xB5, a.rate as f64);
            uint(&mut audio, 0x9F, a.channels.max(1) as u64);
            elem(&mut t, 0xE1, &audio);
            elem(&mut tracks, 0xAE, &t);
        }
        elem(&mut out, 0x1654AE6B, &tracks);
        out
    }

    fn block(&mut self, track: u64, us: i64, key: bool, data: &[u8]) -> Vec<u8> {
        let base = *self.base_us.get_or_insert(us);
        let ms = (us - base) / 1000;
        let mut out = vec![];
        // a cluster ends on a video keyframe after at least a second, or at 5 s
        let long = self.in_cluster && ms - self.cluster_start_ms >= 1000 && key && track == 1;
        let very_long = self.in_cluster && ms - self.cluster_start_ms >= 5000;
        if long || very_long {
            out = self.flush();
        }
        if !self.in_cluster {
            self.in_cluster = true;
            self.cluster_start_ms = ms.max(0);
            uint(&mut self.cluster, 0xE7, self.cluster_start_ms as u64);
        }
        self.last_ms = self.last_ms.max(ms);
        let rel = (ms - self.cluster_start_ms).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        let mut body = vec![0x80 | track as u8];
        body.extend(rel.to_be_bytes());
        body.push(if key { 0x80 } else { 0 });
        body.extend(data);
        elem(&mut self.cluster, 0xA3, &body);
        out
    }

    pub fn video(&mut self, pts_us: i64, key: bool, data: &[u8]) -> Vec<u8> {
        self.block(1, pts_us, key, data)
    }

    pub fn audio(&mut self, pts_us: i64, data: &[u8]) -> Vec<u8> {
        let t = if self.video.is_some() { 2 } else { 1 };
        self.block(t, pts_us, true, data)
    }

    /// The cluster so far, if there is one.
    pub fn flush(&mut self) -> Vec<u8> {
        if !self.in_cluster {
            return vec![];
        }
        self.in_cluster = false;
        let body = std::mem::take(&mut self.cluster);
        let mut out = vec![];
        elem(&mut out, 0x1F43B675, &body);
        out
    }

    pub fn track_info(&self) -> (bool, bool) {
        (self.video.is_some(), self.audio.is_some())
    }

    pub fn duration_ms(&self) -> i64 {
        self.last_ms
    }
}

/// Gives a finished file its segment size and duration.
pub fn patch(file: &mut std::fs::File, o: Offsets, total_len: u64, duration_ms: i64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let seg = total_len - o.segment_start as u64;
    file.seek(SeekFrom::Start(o.segment_size as u64))?;
    file.write_all(&((1u64 << 56) | seg).to_be_bytes())?;
    file.seek(SeekFrom::Start(o.duration as u64))?;
    file.write_all(&(duration_ms as f64).to_be_bytes())?;
    file.seek(SeekFrom::Start(total_len))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_sizes_are_minimal_and_valid() {
        let mut o = vec![];
        size(&mut o, 5);
        assert_eq!(o, [0x85]);
        o.clear();
        size(&mut o, 126);
        assert_eq!(o, [0xfe]);
        o.clear();
        size(&mut o, 127);
        assert_eq!(o, [0x40, 0x7f]);
    }

    #[test]
    fn a_file_with_clusters_gets_its_size_and_duration() {
        let (mut m, mut file) = Mkv::new(
            Some(VideoTrack { codec: VCodec::H264, record: vec![1, 2, 3], width: 640, height: 360 }),
            Some(AudioTrack { codec: ACodec::Opus, config: vec![], rate: 48000, channels: 2 }),
        );
        let o = m.offsets;
        for i in 0..90 {
            file.extend(m.video(i * 33_333, i % 30 == 0, &[0, 0, 0, 1, 0x65]));
            file.extend(m.audio(i * 33_333, &[0xfc, 0xff]));
        }
        file.extend(m.flush());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mkv");
        std::fs::write(&path, &file).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        patch(&mut f, o, file.len() as u64, m.duration_ms()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), file.len());
        assert_eq!(bytes[o.segment_size], 0x01);
        let seg = u64::from_be_bytes(bytes[o.segment_size..o.segment_size + 8].try_into().unwrap()) & 0x00ff_ffff_ffff_ffff;
        assert_eq!(seg as usize, bytes.len() - o.segment_start);
    }
}
