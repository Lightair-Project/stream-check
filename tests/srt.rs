//! An SRT caller (srt-tokio) sending a transport stream made by ffmpeg, against Stream Check's
//! listener: the stream id, the optional passphrase, the MPEG-TS recording and the stats.

use std::process::Command;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::SinkExt;
use srt_tokio::SrtSocket;
use stream_check::app::start;
use stream_check::hub::{Config, State};

fn make_ts(path: &std::path::Path) -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "5",
        ])
        .args(["-c:v", "libx264", "-g", "60", "-bf", "0", "-pix_fmt", "yuv420p", "-c:a", "aac", "-ar", "48000", "-ac", "2", "-f", "mpegts"])
        .arg(path)
        .status()
        .is_ok_and(|s| s.success())
}

/// Sends the file in real time, in SRT-sized chunks.
async fn send(addr: String, stream_id: &str, passphrase: Option<&str>, ts: &[u8]) -> Result<(), String> {
    let mut b = SrtSocket::builder().latency(Duration::from_millis(120));
    if let Some(p) = passphrase {
        b = b.encryption(16, p);
    }
    let mut socket = b.call(addr.as_str(), Some(stream_id)).await.map_err(|e| e.to_string())?;
    let per_chunk = Duration::from_secs_f64(5.0 * 1316.0 / ts.len() as f64);
    for chunk in ts.chunks(1316) {
        socket.send((Instant::now(), Bytes::copy_from_slice(chunk))).await.map_err(|e| e.to_string())?;
        tokio::time::sleep(per_chunk).await;
    }
    socket.close().await.map_err(|e| e.to_string())
}

async fn run(passphrase: Option<&'static str>, key: &str) {
    if !stream_check::helper::ffmpeg_available() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("t.ts");
    assert!(make_ts(&file));
    let ts = std::fs::read(&file).unwrap();
    let mut cfg = Config::for_test(dir.path().join("data"));
    cfg.grace_override = Some(1);
    cfg.srt_passphrase = passphrase.map(str::to_string);
    let lab = start(cfg).await.unwrap();
    let addr = format!("127.0.0.1:{}", lab.srt.port());
    let id = format!("#!::r=open/{key},m=publish");
    let sender = tokio::spawn({
        let id = id.clone();
        async move { send(addr, &id, passphrase, &ts).await }
    });
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if lab.hub.find(key).is_some_and(|s| s.inner.lock().analyzer.stats.video.frames > 30) {
            break;
        }
        assert!(Instant::now() < end, "no SRT media arrived");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    sender.await.unwrap().expect("the sender finished");
    let s = lab.hub.find(key).unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    while s.inner.lock().state != State::Ended {
        assert!(Instant::now() < end, "the session didn't end");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let report = s.inner.lock().report.clone().unwrap();
    let st = &report["stats"];
    assert_eq!(report["protocols"][0], "srt");
    assert_eq!((st["video"]["codec"].as_str(), st["audio"]["codec"].as_str()), (Some("h264"), Some("aac")));
    assert_eq!((st["video"]["sps"]["width"].as_u64(), st["audio"]["sample_rate"].as_u64()), (Some(640), Some(48000)));
    assert!((st["video"]["fps"].as_f64().unwrap() - 30.0).abs() < 1.5, "{}", st["video"]["fps"]);
    assert!((st["video"]["keyint_avg_s"].as_f64().unwrap() - 2.0).abs() < 0.2);
    assert_eq!(report["segments"][0]["container"], "ts");
    // the recording is the transport stream as sent, and it plays
    let seg = s.dir.join("seg01.ts");
    assert_eq!(std::fs::metadata(&seg).unwrap().len() % 188, 0);
    let codecs: Vec<&str> =
        report["segments"][0]["probe"]["streams"].as_array().expect("plays").iter().filter_map(|c| c["codec"].as_str()).collect();
    assert!(codecs.contains(&"h264") && codecs.contains(&"aac"), "{codecs:?}");
    assert_eq!(report["ok"], true, "{}", report["problems"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_srt_stream_is_ingested_and_recorded() {
    run(None, "srt-plain").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_srt_stream_with_a_passphrase_is_ingested() {
    run(Some("a-test-passphrase"), "srt-secret").await;
}

/// A two-second stream sent to a lab where the `loss` fault is already set for its key; the
/// session once it has ended.
async fn lossy(key: &str) -> Option<(stream_check::app::Running, std::sync::Arc<stream_check::hub::Session>, tempfile::TempDir)> {
    if !stream_check::helper::ffmpeg_available() {
        eprintln!("no ffmpeg: skipped");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("t.ts");
    assert!(make_ts(&file));
    // the first two seconds of it, sent over two seconds
    let ts = std::fs::read(&file).unwrap();
    let ts = ts[..ts.len() * 2 / 5 / 188 * 188].to_vec();
    let mut cfg = Config::for_test(dir.path().join("data"));
    cfg.grace_override = Some(1);
    let lab = start(cfg).await.unwrap();
    lab.hub.fault(key, &stream_check::faults::FaultRequest { loss_pct: Some(10.0), ..Default::default() }).unwrap();
    let addr = format!("127.0.0.1:{}", lab.srt.port());
    let mut socket =
        SrtSocket::builder().latency(Duration::from_millis(120)).call(addr.as_str(), Some(&format!("open/{key}"))).await.unwrap();
    let per_chunk = Duration::from_secs_f64(2.0 * 1316.0 / ts.len() as f64);
    for chunk in ts.chunks(1316) {
        socket.send((Instant::now(), Bytes::copy_from_slice(chunk))).await.unwrap();
        tokio::time::sleep(per_chunk).await;
    }
    socket.close().await.unwrap();
    let s = lab.hub.find(key).expect("a session");
    let end = Instant::now() + Duration::from_secs(20);
    while s.inner.lock().state != State::Ended {
        assert!(Instant::now() < end, "the session didn't end");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Some((lab, s, dir))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_loss_fault_throws_datagrams_away_and_the_gaps_are_said() {
    let Some((_lab, s, _dir)) = lossy("srt-loss").await else { return };
    let stats = stream_check::srt_stats::stats_json(&s, 0);
    assert!(stats["total"]["fault_dropped"].as_u64() > Some(0), "{}", stats["total"]);
    let g = s.inner.lock();
    assert!(g.events.iter().any(|e| e.kind == "ts-loss"), "the transport stream's continuity jumps are said");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_and_the_sessions_json_carry_the_srt_link_statistics() {
    let Some((_lab, s, _dir)) = lossy("srt-loss-report").await else { return };
    let report = s.inner.lock().report.clone().unwrap();
    assert!(report["srt"]["total"]["fault_dropped"].as_u64() > Some(0), "{}", report["srt"]);
    assert!(s.summary(false)["srt"].is_object(), "the web UI's `srt`");
}

/// A sender that writes the stream in two halves, each at once (hundreds of datagrams back to
/// back, like a big keyframe handed over whole), with a send buffer that holds them (srt-tokio's
/// default holds 32 datagrams and stalls the sender for good when more wait). The lab's UDP
/// receive buffer takes the bursts whole: nothing goes missing, nothing needs retransmitting.
#[tokio::test(flavor = "multi_thread")]
async fn bursts_of_hundreds_of_datagrams_arrive_whole() {
    if !stream_check::helper::ffmpeg_available() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("t.ts");
    assert!(make_ts(&file));
    let ts = std::fs::read(&file).unwrap();
    let mut cfg = Config::for_test(dir.path().join("data"));
    cfg.grace_override = Some(1);
    let lab = start(cfg).await.unwrap();
    let addr = format!("127.0.0.1:{}", lab.srt.port());
    let key = "srt-bursts";
    let mut socket = SrtSocket::builder()
        .latency(Duration::from_millis(120))
        .set(|o| o.sender.buffer_size = srt_tokio::options::ByteCount(16 * 1024 * 1024))
        .call(addr.as_str(), Some(&format!("open/{key}")))
        .await
        .unwrap();
    let chunks: Vec<&[u8]> = ts.chunks(1316).collect();
    let per_burst = chunks.len() / 2 + 1;
    for burst in chunks.chunks(per_burst) {
        for chunk in burst {
            socket.send((Instant::now(), Bytes::copy_from_slice(chunk))).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(2500)).await;
    }
    socket.close().await.unwrap();
    let s = lab.hub.find(key).expect("a session");
    let end = Instant::now() + Duration::from_secs(20);
    while s.inner.lock().state != State::Ended {
        assert!(Instant::now() < end, "the session didn't end");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let stats = stream_check::srt_stats::stats_json(&s, 0);
    assert!(per_burst > 200, "bursts of {per_burst}");
    assert_eq!(stats["total"]["unique"].as_u64(), Some(chunks.len() as u64), "{}", stats["total"]);
    assert_eq!(stats["total"]["missing"].as_u64(), Some(0), "{}", stats["total"]);
    let report = s.inner.lock().report.clone().unwrap();
    assert!(report["stats"]["video"]["frames"].as_u64() > Some(140), "{}", report["stats"]["video"]["frames"]);
}

/// An srt-tokio sender with its default send buffer (32 datagrams) handed a burst of hundreds:
/// srt-protocol 0.4 drops the unsent ones without moving its send pointer and never sends data
/// again, though the connection stays open. The lab says so and the report fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_sender_that_stops_sending_on_an_open_connection_is_a_problem() {
    if !stream_check::helper::ffmpeg_available() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("t.ts");
    assert!(make_ts(&file));
    let ts = std::fs::read(&file).unwrap();
    let mut cfg = Config::for_test(dir.path().join("data"));
    cfg.grace_override = Some(1);
    let lab = start(cfg).await.unwrap();
    let addr = format!("127.0.0.1:{}", lab.srt.port());
    let key = "srt-stalled";
    let mut socket =
        SrtSocket::builder().latency(Duration::from_millis(120)).call(addr.as_str(), Some(&format!("open/{key}"))).await.unwrap();
    for chunk in ts.chunks(1316).take(300) {
        socket.send((Instant::now(), Bytes::copy_from_slice(chunk))).await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let s = lab.hub.find(key).expect("a session");
    assert!(s.inner.lock().events.iter().any(|e| e.kind == "srt" && e.msg.contains("stopped sending")), "the stall is said");
    drop(socket);
    let end = Instant::now() + Duration::from_secs(30);
    while s.inner.lock().state != State::Ended {
        assert!(Instant::now() < end, "the session didn't end");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let report = s.inner.lock().report.clone().unwrap();
    assert_eq!(report["ok"], false);
    assert!(
        report["problems"].as_array().unwrap().iter().any(|p| p.as_str().is_some_and(|p| p.contains("stopped sending"))),
        "{}",
        report["problems"]
    );
}
