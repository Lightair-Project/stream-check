//! A WHIP publisher (a WebRTC peer from the webrtc crate, sending H.264 and Opus made by ffmpeg)
//! against Stream Check: the HTTP exchange, the media, the Matroska recording and the report.

use std::fs::File;
use std::io::BufReader;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media::io::h26x_reader::H26xSampleReader;
use rtc::media::io::ogg_reader::OggReader;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use stream_check::app::start;
use stream_check::hub::{Config, State};
use tokio::sync::mpsc;
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc::runtime::default_runtime;

struct Handler {
    gathered: mpsc::Sender<()>,
    connected: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            let _ = self.connected.try_send(());
        }
    }
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().is_ok_and(|s| s.success())
}

fn track(kind: RtpCodecKind, codec: &RTCRtpCodec, ssrc: u32) -> Arc<TrackLocalStaticSample> {
    Arc::new(
        TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "test".into(),
                format!("test-{kind}"),
                format!("test-{kind}"),
                kind,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                    codec: codec.clone(),
                    ..Default::default()
                }],
            ),
        )
        .unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whip_publisher_is_ingested_recorded_and_reported() {
    if !stream_check::helper::ffmpeg_available() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (h264, ogg) = (dir.path().join("v.h264"), dir.path().join("a.ogg"));
    if !ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-t",
        "5",
        "-c:v",
        "libx264",
        "-profile:v",
        "baseline",
        "-level",
        "3.1",
        "-g",
        "30",
        "-bf",
        "0",
        "-pix_fmt",
        "yuv420p",
        "-x264-params",
        "aud=1",
        "-f",
        "h264",
        h264.to_str().unwrap(),
    ]) || !ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "5",
        "-c:a",
        "libopus",
        "-b:a",
        "64k",
        "-frame_duration",
        "20",
        "-page_duration",
        "20000",
        "-ac",
        "2",
        "-f",
        "ogg",
        ogg.to_str().unwrap(),
    ]) {
        eprintln!("this ffmpeg can't make H.264 and Opus: skipped");
        return;
    }
    let mut cfg = Config::for_test(dir.path().join("data"));
    cfg.grace_override = Some(1);
    let lab = start(cfg).await.unwrap();
    let hub = lab.hub.clone();

    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
    };
    let audio_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
    };
    let mut media = MediaEngine::default();
    media.register_codec(video_codec.clone(), RtpCodecKind::Video).unwrap();
    media.register_codec(audio_codec.clone(), RtpCodecKind::Audio).unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
    let (gathered_tx, mut gathered) = mpsc::channel(1);
    let (connected_tx, mut connected) = mpsc::channel(1);
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler { gathered: gathered_tx, connected: connected_tx }))
        .with_runtime(default_runtime().unwrap())
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await
        .unwrap();
    let (vt, at) = (track(RtpCodecKind::Video, &video_codec.rtp_codec, 1111), track(RtpCodecKind::Audio, &audio_codec.rtp_codec, 2222));
    let vs = pc.add_track(vt.clone() as Arc<dyn TrackLocal>).await.unwrap();
    let as_ = pc.add_track(at.clone() as Arc<dyn TrackLocal>).await.unwrap();
    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), gathered.recv()).await;
    let offer_sdp = pc.local_description().await.unwrap().sdp;

    // the WHIP exchange
    let url = format!("http://127.0.0.1:{}/whip/open/e2e-whip", lab.http.port());
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let (a2, u2, o2) = (agent.clone(), url.clone(), offer_sdp.clone());
    let refused = tokio::task::spawn_blocking(move || {
        a2.post(&u2).header("Authorization", "Bearer nope").header("Content-Type", "application/sdp").send(o2).unwrap().status().as_u16()
    })
    .await
    .unwrap();
    assert_eq!(refused, 401);
    let (a2, u2) = (agent.clone(), url.clone());
    let (status, location, answer) = tokio::task::spawn_blocking(move || {
        let mut r =
            a2.post(&u2).header("Authorization", "Bearer secret").header("Content-Type", "application/sdp").send(offer_sdp).unwrap();
        let loc = r.headers().get("location").and_then(|l| l.to_str().ok()).map(str::to_owned);
        (r.status().as_u16(), loc, r.body_mut().read_to_string().unwrap())
    })
    .await
    .unwrap();
    assert_eq!(status, 201, "{answer}");
    let location = location.expect("a Location");
    pc.set_remote_description(RTCSessionDescription::answer(answer).unwrap()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), connected.recv()).await.expect("connected");

    // stream: pictures at 30 fps and Opus pages every 20 ms
    let vpt = vs.get_parameters().await.unwrap().rtp_parameters.codecs[0].payload_type;
    let apt = as_.get_parameters().await.unwrap().rtp_parameters.codecs[0].payload_type;
    let v = tokio::spawn({
        let (vt, h264) = (vt.clone(), h264.clone());
        async move {
            let mut r = H26xSampleReader::new(BufReader::new(File::open(h264).unwrap()), 1_048_576, false);
            let ssrc = vt.ssrcs().await[0];
            let mut tick = tokio::time::interval(Duration::from_millis(33));
            while let Ok(s) = r.next_sample() {
                let d = if s.timed { Duration::from_millis(33) } else { Duration::ZERO };
                let _ =
                    vt.sample_writer(ssrc, vpt).write_sample(&Sample { data: s.data, duration: d, ..Sample::new(Instant::now()) }).await;
                if s.timed {
                    tick.tick().await;
                }
            }
        }
    });
    let a = tokio::spawn({
        let (at, ogg) = (at.clone(), ogg.clone());
        async move {
            let (mut r, _) = OggReader::new(BufReader::new(File::open(ogg).unwrap()), true).unwrap();
            let ssrc = at.ssrcs().await[0];
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            let mut last = 0u64;
            while let Ok((page, header)) = r.parse_next_page() {
                let n = header.granule_position.saturating_sub(last);
                last = header.granule_position;
                let _ = at
                    .sample_writer(ssrc, apt)
                    .write_sample(&Sample {
                        data: page.freeze(),
                        duration: Duration::from_millis(n * 1000 / 48000),
                        ..Sample::new(Instant::now())
                    })
                    .await;
                tick.tick().await;
            }
        }
    });
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        let ok = hub.find("e2e-whip").is_some_and(|s| {
            let g = s.inner.lock();
            g.analyzer.stats.video.frames > 40 && g.analyzer.stats.audio.frames > 100
        });
        if ok {
            break;
        }
        assert!(Instant::now() < end, "no media arrived: {:?}", hub.find("e2e-whip").map(|s| s.summary(false)["stats"].to_string()));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(8), async {
        let _ = v.await;
        let _ = a.await;
    })
    .await;

    // the sender ends the session
    let (a2, del) = (agent.clone(), format!("http://127.0.0.1:{}{location}", lab.http.port()));
    assert_eq!(tokio::task::spawn_blocking(move || a2.delete(&del).call().unwrap().status().as_u16()).await.unwrap(), 200);
    let _ = pc.close().await;
    let s = hub.find("e2e-whip").unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    while s.inner.lock().state != State::Ended {
        assert!(Instant::now() < end, "the session didn't end");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let report = s.inner.lock().report.clone().unwrap();
    let st = &report["stats"];
    assert_eq!(st["video"]["codec"], "h264");
    assert_eq!((st["video"]["sps"]["width"].as_u64(), st["video"]["sps"]["height"].as_u64()), (Some(640), Some(360)));
    assert_eq!((st["audio"]["codec"].as_str(), st["audio"]["sample_rate"].as_u64()), (Some("opus"), Some(48000)));
    assert!((st["video"]["fps"].as_f64().unwrap() - 30.0).abs() < 3.0, "fps {}", st["video"]["fps"]);
    assert_eq!(report["protocols"][0], "whip");
    // RTP statistics are reported for WebRTC, and on a clean local link nothing is lost
    assert_eq!(st["rtp"]["active"], true);
    assert_eq!(st["rtp"]["video"]["lost"], 0);
    assert!(st["rtp"]["video"]["pictures"].as_u64().unwrap() > 40);
    let seg = &report["segments"][0];
    assert_eq!(seg["container"], "mkv");
    let codecs: Vec<&str> =
        seg["probe"]["streams"].as_array().expect("the recording plays").iter().filter_map(|s| s["codec"].as_str()).collect();
    assert!(codecs.contains(&"h264") && codecs.contains(&"opus"), "{codecs:?}");
    let _ = &report["problems"];
}
