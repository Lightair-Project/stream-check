//! What the lab reads out of a stream and how it judges it: video bitstreams (BPM and caption
//! SEI, AV1 and VP9 headers, level limits, colour tags), Twitch Dual Format over Enhanced RTMP
//! multitrack, WHIP depacketisation, files, RTSP pulls, uncompressed pictures and SRT link
//! statistics. Fixtures are made by ffmpeg where a real encoder's bytes matter; those tests
//! are skipped without it.

mod codecs;
mod colour;
mod depay;
mod files;
mod levels;
mod multitrack;
mod picture;
mod rtmp_wire;
mod rtsp;
mod sei;
mod srt_stats;
mod support;
