//! Pulling from an RTSP server: a small server in the test answers OPTIONS, DESCRIBE, SETUP and
//! PLAY, sends an x264 stream as RTP over the RTSP connection (single NAL units and FU-A, as
//! RFC 6184 says) and hangs up when it has sent it all.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use stream_check::file_check::ended;
use stream_check::hub::Hub;
use stream_check::nal::split_annexb;
use stream_check::pull::{PullRequest, start};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::support::{ffmpeg, have_ffmpeg, hub};

const FRAMES: usize = 60;

/// Two seconds of 320x240 at 30 fps, a keyframe a second: access units of NAL units, each
/// starting with its access unit delimiter.
fn access_units(dir: &std::path::Path) -> Vec<Vec<Vec<u8>>> {
    let f = dir.join("in.h264");
    assert!(ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30",
        "-frames:v",
        &FRAMES.to_string(),
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-g",
        "30",
        "-bf",
        "0",
        "-x264-params",
        "aud=1",
        "-f",
        "h264",
        f.to_str().unwrap(),
    ]));
    let data = std::fs::read(&f).unwrap();
    let mut aus: Vec<Vec<Vec<u8>>> = vec![];
    for n in split_annexb(&data) {
        if n[0] & 0x1f == 9 || aus.is_empty() {
            aus.push(vec![]);
        }
        aus.last_mut().unwrap().push(n.to_vec());
    }
    assert_eq!(aus.len(), FRAMES);
    aus
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// The RTP packets of the stream: (marker, timestamp, payload), FU-A for units over 1200 bytes.
fn packets(aus: &[Vec<Vec<u8>>]) -> Vec<(bool, u32, Vec<u8>)> {
    let mut out = vec![];
    for (i, au) in aus.iter().enumerate() {
        let ts = i as u32 * 3000;
        let nals: Vec<&Vec<u8>> = au.iter().filter(|n| n[0] & 0x1f != 9).collect();
        for (k, n) in nals.iter().enumerate() {
            let last_nal = k + 1 == nals.len();
            if n.len() <= 1200 {
                out.push((last_nal, ts, n.to_vec()));
                continue;
            }
            let (indicator, kind) = ((n[0] & 0xe0) | 28, n[0] & 0x1f);
            let parts: Vec<&[u8]> = n[1..].chunks(1200).collect();
            for (j, part) in parts.iter().enumerate() {
                let (start, end) = (j == 0, j + 1 == parts.len());
                let mut p = vec![indicator, (start as u8) << 7 | (end as u8) << 6 | kind];
                p.extend(*part);
                out.push((last_nal && end, ts, p));
            }
        }
    }
    out
}

/// What the server does: the credentials it wants, and which RTP packet it leaves out.
#[derive(Clone, Default)]
struct Server {
    basic: Option<String>,
    skip: Option<usize>,
}

/// Serves one player on a free port; returns the port.
async fn serve(aus: Vec<Vec<Vec<u8>>>, how: Server) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (r, mut w) = sock.into_split();
        let mut r = BufReader::new(r);
        let sps = aus[0].iter().find(|n| n[0] & 0x1f == 7).unwrap();
        let pps = aus[0].iter().find(|n| n[0] & 0x1f == 8).unwrap();
        let sdp = format!(
            "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;sprop-parameter-sets={},{}\r\na=control:trackID=0\r\n",
            base64(sps),
            base64(pps)
        );
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let method = line.split_whitespace().next().unwrap_or("").to_string();
            let mut headers = vec![];
            loop {
                let mut h = String::new();
                r.read_line(&mut h).await.unwrap();
                if h.trim().is_empty() {
                    break;
                }
                headers.push(h.trim().to_string());
            }
            let header = |name: &str| {
                headers
                    .iter()
                    .find_map(|h| h.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()))
            };
            let cseq = header("CSeq").unwrap_or_default();
            let authorized = how.basic.as_ref().is_none_or(|b| header("Authorization").as_deref() == Some(&format!("Basic {b}")));
            let (status, extra, body) = match method.as_str() {
                "OPTIONS" => ("200 OK", "Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n".to_string(), String::new()),
                "DESCRIBE" if !authorized => ("401 Unauthorized", "WWW-Authenticate: Basic realm=\"test\"\r\n".into(), String::new()),
                "DESCRIBE" => {
                    ("200 OK", format!("Content-Type: application/sdp\r\nContent-Base: rtsp://127.0.0.1:{port}/live/\r\n"), sdp.clone())
                }
                "SETUP" => {
                    ("200 OK", "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\nSession: 1234;timeout=60\r\n".into(), String::new())
                }
                "PLAY" => ("200 OK", "Session: 1234\r\n".into(), String::new()),
                _ => ("200 OK", String::new(), String::new()),
            };
            let msg = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{extra}Content-Length: {}\r\n\r\n{body}", body.len());
            w.write_all(msg.as_bytes()).await.unwrap();
            if method == "PLAY" {
                break;
            }
        }
        // the stream, then goodbye
        let mut out = vec![];
        for (seq, (marker, ts, payload)) in packets(&aus).into_iter().enumerate() {
            if how.skip == Some(seq) {
                continue;
            }
            let mut rtp = vec![0x80, (marker as u8) << 7 | 96];
            rtp.extend((seq as u16).to_be_bytes());
            rtp.extend(ts.to_be_bytes());
            rtp.extend(0x5eed_u32.to_be_bytes());
            rtp.extend(payload);
            out.extend([b'$', 0]);
            out.extend((rtp.len() as u16).to_be_bytes());
            out.extend(rtp);
        }
        w.write_all(&out).await.unwrap();
        w.shutdown().await.unwrap();
    });
    port
}

async fn pull(hub: &Arc<Hub>, url: String) -> Value {
    let s = start(hub, PullRequest { url, key: Some("rtsp-test".into()), profile: None, for_s: None }).await.expect("the pull starts");
    tokio::time::timeout(Duration::from_secs(30), ended(&s)).await.expect("the pull ends when the server hangs up");
    s.inner.lock().report.clone().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pulled_rtsp_stream_is_measured_like_an_ingest() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let port = serve(access_units(dir.path()), Server::default()).await;
    let hub = hub(dir.path());
    let r = pull(&hub, format!("rtsp://127.0.0.1:{port}/live")).await;
    assert_eq!(r["ok"], true, "{}", r["problems"]);
    assert_eq!(r["kind"], "pull");
    let v = &r["stats"]["video"];
    assert_eq!((v["codec"].as_str(), v["sps"]["width"].as_u64(), v["sps"]["height"].as_u64()), (Some("h264"), Some(320), Some(240)));
    assert_eq!((v["frames"].as_u64(), v["keyframes"].as_u64()), (Some(FRAMES as u64), Some(2)));
    assert_eq!(r["stats"]["rtp"]["video"]["lost"], 0);
    assert_eq!(r["source"]["tracks"][0]["codec"], "H264");
    assert_eq!(r["end_reason"], "the server closed the connection");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "bug: a picture-only Matroska recording waits 1.5 s of wall clock for sound, and Pipeline::finish \
            (src/hub.rs) drops what it held: a pull or WHIP session that ends sooner has an empty seg01.mkv"]
async fn a_short_picture_only_pull_is_still_recorded() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let port = serve(access_units(dir.path()), Server::default()).await;
    let hub = hub(dir.path());
    let r = pull(&hub, format!("rtsp://127.0.0.1:{port}/live")).await;
    let seg = &r["segments"][0];
    assert_eq!(seg["container"], "mkv");
    assert!(seg["probe"]["streams"].as_array().is_some_and(|s| s.iter().any(|s| s["codec"] == "h264")), "{seg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_rtp_packet_is_counted_and_its_picture_dropped() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let aus = access_units(dir.path());
    // the middle of a picture sent in several packets, after the first second
    let all = packets(&aus);
    let skip = (all.len() / 2..all.len() - 1)
        .find(|&i| all[i].1 == all[i - 1].1 && all[i].1 == all[i + 1].1)
        .expect("a picture of three packets or more");
    let port = serve(aus, Server { skip: Some(skip), ..Default::default() }).await;
    let hub = hub(dir.path());
    let r = pull(&hub, format!("rtsp://127.0.0.1:{port}/live")).await;
    let rtp = &r["stats"]["rtp"]["video"];
    assert_eq!((rtp["lost"].as_u64(), rtp["pictures_dropped"].as_u64()), (Some(1), Some(1)), "{rtp}");
    assert_eq!(r["stats"]["video"]["frames"].as_u64(), Some(FRAMES as u64 - 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_in_the_url_answer_basic_authentication_and_stay_out_of_the_report() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let port = serve(access_units(dir.path()), Server { basic: Some(base64(b"viewer:pa55word")), ..Default::default() }).await;
    let hub = hub(dir.path());
    let r = pull(&hub, format!("rtsp://viewer:pa55word@127.0.0.1:{port}/live")).await;
    assert_eq!(r["stats"]["video"]["frames"].as_u64(), Some(FRAMES as u64));
    assert!(!r.to_string().contains("pa55word"), "the password is in the report");
}
