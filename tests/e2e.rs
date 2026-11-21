//! Real senders against a real Stream Check: ffmpeg over RTMP, RTMPS-less here, SRT and WHIP
//! when this ffmpeg has them. Skipped (with a message) when there's no ffmpeg.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use stream_check::app::{Running, start};
use stream_check::faults::FaultRequest;
use stream_check::hub::{Config, Hub, Session, State};

fn have_ffmpeg() -> bool {
    stream_check::helper::ffmpeg_available()
}

fn has(kind: &str, name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", &format!("-{kind}")])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().any(|w| w == name)))
        .unwrap_or(false)
}

async fn lab(configure: impl FnOnce(&mut Config)) -> (Running, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_test(dir.path().to_path_buf());
    configure(&mut cfg);
    (start(cfg).await.expect("the lab starts"), dir)
}

async fn until(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// ffmpeg sending test picture and a tone for `secs` seconds in real time.
fn send(url: &str, format: &str, secs: u32, video_args: &[&str]) -> Child {
    let mut c = Command::new("ffmpeg");
    c.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        &secs.to_string(),
    ]);
    c.args(if video_args.is_empty() { &["-c:v", "libx264", "-g", "60", "-bf", "0", "-pix_fmt", "yuv420p"][..] } else { video_args });
    c.args(["-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2", "-f", format, url]);
    c.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().expect("ffmpeg starts")
}

fn find(hub: &Arc<Hub>, key: &str) -> Option<Arc<Session>> {
    hub.find(key)
}

fn state(s: &Session) -> State {
    s.inner.lock().state
}

#[tokio::test(flavor = "multi_thread")]
async fn an_rtmp_stream_is_measured_recorded_and_reported() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let (lab, _dir) = lab(|c| c.grace_override = Some(1)).await;
    let mut ff = send(&format!("rtmp://127.0.0.1:{}/open/e2e-rtmp", lab.rtmp.port()), "flv", 6, &[]);
    until(10, "the session to be live", || find(&lab.hub, "e2e-rtmp").is_some_and(|s| s.inner.lock().analyzer.stats.video.frames > 30))
        .await;
    let s = find(&lab.hub, "e2e-rtmp").unwrap();
    // a live look
    let live = s.summary(true);
    assert_eq!(live["state"], "live");
    ff.wait().unwrap();
    until(10, "the session to end", || state(&s) == State::Ended).await;
    let report = s.inner.lock().report.clone().expect("a report");
    let v = &report["stats"]["video"];
    assert_eq!(v["codec"], "h264");
    assert_eq!((v["sps"]["width"].as_u64(), v["sps"]["height"].as_u64()), (Some(640), Some(360)));
    assert!((v["fps"].as_f64().unwrap() - 30.0).abs() < 1.5, "fps {}", v["fps"]);
    assert!((v["keyint_avg_s"].as_f64().unwrap() - 2.0).abs() < 0.2, "keyint {}", v["keyint_avg_s"]);
    assert_eq!(v["b_frames"], false);
    let a = &report["stats"]["audio"];
    assert_eq!((a["codec"].as_str(), a["sample_rate"].as_u64(), a["channels"].as_u64()), (Some("aac"), Some(48000), Some(2)));
    assert_eq!(report["ok"], true, "{}", report["problems"]);
    // the recording plays
    let seg = &report["segments"][0];
    assert_eq!(seg["container"], "flv");
    let streams = seg["probe"]["streams"].as_array().expect("ffprobe read the recording");
    assert!(streams.iter().any(|s| s["codec"] == "h264") && streams.iter().any(|s| s["codec"] == "aac"));
    assert!(s.dir.join("report.json").exists());
    // decoded by the helper: a tone is loud enough to measure, and there are thumbnails
    assert!(report["stats"]["levels"]["available"] == true, "levels");
    assert!(report["stats"]["levels"]["integrated_lufs"].as_f64().unwrap() > -40.0);
    assert!(!report["thumbs"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_key_is_refused_and_a_twitch_stream_with_slow_keyframes_is_flagged() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let (lab, _dir) = lab(|c| c.lenient_keys = false).await;
    let mut ff = send(&format!("rtmp://127.0.0.1:{}/twitch/wrong", lab.rtmp.port()), "flv", 3, &[]);
    let status = ff.wait().unwrap();
    assert!(!status.success(), "ffmpeg should have been refused");
    until(5, "the refusal to be recorded", || lab.hub.rejections.lock().iter().any(|r| r.code == "bad-key")).await;
    // a good key, but keyframes every 4 s are too slow for a Twitch-like platform
    let mut ff = send(
        &format!("rtmp://127.0.0.1:{}/twitch/live_123456789_e2ekey", lab.rtmp.port()),
        "flv",
        7,
        &["-c:v", "libx264", "-g", "120", "-bf", "0", "-pix_fmt", "yuv420p"],
    );
    until(12, "a keyframe-interval violation", || {
        find(&lab.hub, "live_123456789_e2ekey").is_some_and(|s| s.inner.lock().violations.iter().any(|v| v.rule == "keyframe-interval"))
    })
    .await;
    let _ = ff.kill();
    let _ = ff.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sender_that_comes_back_within_the_grace_period_continues_the_session() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let (lab, _dir) = lab(|c| c.grace_override = Some(4)).await;
    let url = format!("rtmp://127.0.0.1:{}/open/e2e-grace", lab.rtmp.port());
    send(&url, "flv", 3, &[]).wait().unwrap();
    let first = find(&lab.hub, "e2e-grace").expect("a session").id.clone();
    until(5, "grace", || state(&find(&lab.hub, "e2e-grace").unwrap()) == State::Grace).await;
    send(&url, "flv", 3, &[]).wait().unwrap();
    let s = find(&lab.hub, "e2e-grace").unwrap();
    assert_eq!(s.id, first, "same session");
    assert_eq!(s.inner.lock().reconnects, 1);
    assert_eq!(s.inner.lock().segments.len(), 2);
    until(10, "the grace period to run out", || state(&s) == State::Ended).await;
    let report = s.inner.lock().report.clone().unwrap();
    assert_eq!(report["grace_uses"], 1);
    assert_eq!(report["segments"].as_array().unwrap().len(), 2);
    // after the end, the same key is a new session
    let mut ff = send(&url, "flv", 2, &[]);
    until(10, "a new session", || find(&lab.hub, "e2e-grace").is_some_and(|n| n.id != first)).await;
    let _ = ff.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_injected_drop_and_refusal_break_the_connection() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let (lab, _dir) = lab(|c| c.grace_override = Some(30)).await;
    let url = format!("rtmp://127.0.0.1:{}/open/e2e-fault", lab.rtmp.port());
    let mut ff = send(&url, "flv", 20, &[]);
    until(10, "live", || find(&lab.hub, "e2e-fault").is_some_and(|s| s.inner.lock().analyzer.stats.video.frames > 20)).await;
    let said = lab.hub.fault("e2e-fault", &FaultRequest { drop: true, ..Default::default() }).unwrap();
    assert!(said.iter().any(|m| m.contains("dropped")));
    let s = find(&lab.hub, "e2e-fault").unwrap();
    until(5, "the session to wait in grace", || state(&s) == State::Grace).await;
    assert!(s.inner.lock().events.iter().any(|e| e.kind == "fault"));
    let _ = ff.wait();
    // refusing: a sender is turned away, and the event log says so
    lab.hub.fault("e2e-fault", &FaultRequest { refuse_s: Some(3.0), ..Default::default() }).unwrap();
    let mut ff = send(&url, "flv", 2, &[]);
    assert!(!ff.wait().unwrap().success());
    assert!(lab.hub.rejections.lock().iter().any(|r| r.code == "refused-fault"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_srt_stream_is_recorded_as_a_transport_stream() {
    if !have_ffmpeg() || !has("protocols", "srt") {
        eprintln!("no ffmpeg with SRT: skipped");
        return;
    }
    let (lab, _dir) = lab(|c| c.grace_override = Some(1)).await;
    let mut ff = send(&format!("srt://127.0.0.1:{}?streamid=open/e2e-srt&mode=caller&latency=120", lab.srt.port()), "mpegts", 5, &[]);
    until(12, "the SRT session", || find(&lab.hub, "e2e-srt").is_some_and(|s| s.inner.lock().analyzer.stats.video.frames > 30)).await;
    let _ = ff.wait();
    let s = find(&lab.hub, "e2e-srt").unwrap();
    until(12, "the end", || state(&s) == State::Ended).await;
    let report = s.inner.lock().report.clone().unwrap();
    assert_eq!(report["segments"][0]["container"], "ts");
    assert_eq!(report["stats"]["video"]["codec"], "h264");
    assert_eq!(report["stats"]["audio"]["codec"], "aac");
    assert!(report["segments"][0]["probe"]["streams"].as_array().is_some_and(|s| s.len() >= 2), "{}", report["segments"][0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_api_serves_the_ui_streams_and_reports() {
    let (lab, _dir) = lab(|_| {}).await;
    let base = format!("http://127.0.0.1:{}", lab.http.port());
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut r = agent.get(&format!("{base}/")).call().unwrap();
    assert!(r.body_mut().read_to_string().unwrap().contains("Stream Check"));
    let mut r = agent.get(&format!("{base}/api/status")).call().unwrap();
    let v: serde_json::Value = serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap();
    assert!(v["addresses"]["hosts"].as_array().unwrap().iter().any(|h| h["host"] == "10.0.2.2"));
    let r = agent.get(&format!("{base}/api/sessions/nothing/report")).call().unwrap();
    assert_eq!(r.status().as_u16(), 404);
    // WHIP: a wrong token is refused before anything else
    let r = agent
        .post(&format!("{base}/whip/open/key"))
        .header("Authorization", "Bearer wrong")
        .header("Content-Type", "application/sdp")
        .send("v=0")
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let r = agent.get(&format!("{base}/api/qr?text=hello")).call().unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_waits_for_a_live_stream_and_reports() {
    if !have_ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let (lab, dir) = lab(|c| c.grace_override = Some(1)).await;
    let url = format!("http://127.0.0.1:{}", lab.http.port());
    let mut ff = send(&format!("rtmp://127.0.0.1:{}/open/e2e-cli", lab.rtmp.port()), "flv", 4, &[]);
    let args: Vec<String> = ["wait-for", "e2e-cli", "--live", "--timeout", "15", "--url", &url].iter().map(|s| s.to_string()).collect();
    let code = tokio::task::spawn_blocking(move || stream_check::cli::run(&args)).await.unwrap();
    assert_eq!(code, 0);
    let _ = ff.wait();
    let args: Vec<String> = ["wait-for", "never-there", "--timeout", "1", "--url", &url].iter().map(|s| s.to_string()).collect();
    assert_eq!(tokio::task::spawn_blocking(move || stream_check::cli::run(&args)).await.unwrap(), 1);
    let s = find(&lab.hub, "e2e-cli").unwrap();
    until(10, "the end", || state(&s) == State::Ended).await;
    let args: Vec<String> =
        ["report", "e2e-cli", "--summary", "--url", &url, "--data", dir.path().to_str().unwrap()].iter().map(|s| s.to_string()).collect();
    assert_eq!(tokio::task::spawn_blocking(move || stream_check::cli::run(&args)).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn hevc_over_enhanced_rtmp_is_accepted_by_youtube_and_flagged_by_twitch() {
    if !have_ffmpeg() || !has("encoders", "libx265") {
        eprintln!("no ffmpeg with x265: skipped");
        return;
    }
    let (lab, _dir) = lab(|c| {
        c.grace_override = Some(1);
        c.lenient_keys = false;
    })
    .await;
    let hevc = ["-c:v", "libx265", "-x265-params", "keyint=60:bframes=0:log-level=error", "-pix_fmt", "yuv420p"];
    let mut ff = send(&format!("rtmp://127.0.0.1:{}/youtube/abcd-efgh-ijkl-mnop-qrst", lab.rtmp.port()), "flv", 4, &hevc);
    until(15, "an HEVC picture", || {
        find(&lab.hub, "abcd-efgh-ijkl-mnop-qrst").is_some_and(|s| s.inner.lock().analyzer.stats.video.frames > 20)
    })
    .await;
    let s = find(&lab.hub, "abcd-efgh-ijkl-mnop-qrst").unwrap();
    {
        let g = s.inner.lock();
        let v = &g.analyzer.stats.video;
        assert_eq!(v.codec, Some(stream_check::nal::VCodec::Hevc));
        let sps = v.sps.as_ref().unwrap();
        assert_eq!((sps.width, sps.height), (640, 360), "{sps:?}");
        assert!(sps.codec_string.starts_with("hvc1."), "{}", sps.codec_string);
        assert!(g.violations.is_empty(), "YouTube-like takes HEVC: {:?}", g.violations);
    }
    let _ = ff.wait();
    until(10, "the end", || state(&s) == State::Ended).await;
    let report = s.inner.lock().report.clone().unwrap();
    assert!(
        report["segments"][0]["probe"]["streams"].as_array().unwrap().iter().any(|s| s["codec"] == "hevc"),
        "{}",
        report["segments"][0]
    );

    let mut ff = send(&format!("rtmp://127.0.0.1:{}/twitch/live_123456789_hevckey", lab.rtmp.port()), "flv", 4, &hevc);
    until(15, "the video-codec rule", || {
        find(&lab.hub, "live_123456789_hevckey").is_some_and(|s| s.inner.lock().violations.iter().any(|v| v.rule == "video-codec"))
    })
    .await;
    let _ = ff.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn rtmps_works_with_the_self_signed_certificate() {
    if !have_ffmpeg() || !has("protocols", "tls") {
        eprintln!("no ffmpeg with TLS: skipped");
        return;
    }
    let (lab, dir) = lab(|c| {
        c.rtmps_port = Some(0);
        c.grace_override = Some(1);
    })
    .await;
    let port = lab.rtmps.expect("an RTMPS port").port();
    assert!(dir.path().join("tls/cert.pem").exists());
    let mut ff = send_tls(&format!("rtmps://127.0.0.1:{port}/open/e2e-tls"));
    until(15, "a picture over TLS", || find(&lab.hub, "e2e-tls").is_some_and(|s| s.inner.lock().analyzer.stats.video.frames > 20)).await;
    let s = find(&lab.hub, "e2e-tls").unwrap();
    assert_eq!(s.inner.lock().protocols, vec![stream_check::hub::Proto::Rtmps]);
    let _ = ff.wait();
}

/// ffmpeg over RTMPS, told to accept the lab's self-signed certificate.
fn send_tls(url: &str) -> Child {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "4",
        ])
        .args(["-c:v", "libx264", "-g", "60", "-c:a", "aac", "-f", "flv", "-tls_verify", "0", url])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ffmpeg starts")
}
