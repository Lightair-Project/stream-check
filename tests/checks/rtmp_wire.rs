//! An Enhanced RTMP sender's conversation with the lab's RTMP server, over an in-memory pipe:
//! what it says about itself is reported, its key's query is kept by name only, and ending the
//! stream with deleteStream is the sender's own end.

use std::sync::Arc;

use stream_check::amf::{self, Amf, obj, s};
use stream_check::hub::{Hub, Proto, Session};
use stream_check::rtmp::{ChunkWriter, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::support::{hub, until};

const SECRET: &str = "s3cret-config-0042";

fn command(values: &[Amf]) -> Vec<u8> {
    let mut p = vec![];
    for v in values {
        amf::write(&mut p, v);
    }
    p
}

/// A whole publish: handshake, connect with Enhanced RTMP capabilities, createStream, publish
/// `name`, then deleteStream. Returns once the server has hung up.
async fn publish(hub: &Arc<Hub>, app: &str, name: &str) {
    let (client, server) = tokio::io::duplex(1 << 20);
    let served = tokio::spawn(serve(server, hub.clone(), Proto::Rtmp, "127.0.0.1:50001".parse().unwrap()));
    let (mut r, mut w) = tokio::io::split(client);
    // what the server says is read and dropped
    let drain = tokio::spawn(async move {
        let mut sink = vec![];
        let _ = r.read_to_end(&mut sink).await;
    });
    let cw = ChunkWriter::new();
    let mut out = vec![3u8];
    out.extend([0u8; 1536 * 2]); // C1, then C2
    let connect = obj(&[
        ("app", s(app)),
        ("tcUrl", s(&format!("rtmp://127.0.0.1/{app}"))),
        ("capsEx", Amf::Number(2.0)),
        ("fourCcList", Amf::Array(vec![s("av01"), s("avc1")])),
    ]);
    out.extend(cw.message(3, 20, 0, 0, &command(&[s("connect"), Amf::Number(1.0), connect])));
    out.extend(cw.message(3, 20, 0, 0, &command(&[s("createStream"), Amf::Number(2.0), Amf::Null])));
    out.extend(cw.message(8, 20, 1, 0, &command(&[s("publish"), Amf::Number(3.0), Amf::Null, s(name), s("live")])));
    out.extend(cw.message(3, 20, 0, 0, &command(&[s("deleteStream"), Amf::Number(4.0), Amf::Null, Amf::Number(1.0)])));
    w.write_all(&out).await.unwrap();
    served.await.unwrap();
    drain.await.unwrap();
}

async fn ended_session(hub: &Arc<Hub>, key: &str) -> Arc<Session> {
    let s = hub.find(key).expect("a session");
    until(5, "the connection to be closed", || s.inner.lock().connections.iter().all(|c| c.end_reason.is_some())).await;
    s
}

#[tokio::test]
async fn a_keys_query_is_reported_by_parameter_name_without_its_values() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path());
    publish(&hub, "twitch-dual", &format!("live_123456789_wiretest?clientConfigId={SECRET}&bandwidthtest=true")).await;
    let s = ended_session(&hub, "live_123456789_wiretest").await;
    let q = s.inner.lock().client["key_query"].clone();
    assert_eq!(q["params"], serde_json::json!(["clientConfigId", "bandwidthtest"]), "{q}");
    assert_eq!(q["client_config_id"], true);
    let everything = format!("{}{}", s.summary(true), s.build_report());
    assert!(!everything.contains(SECRET), "the config id's value was kept");
}

#[tokio::test]
async fn an_enhanced_rtmp_senders_capabilities_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path());
    publish(&hub, "open", "caps").await;
    let s = ended_session(&hub, "caps").await;
    let c = s.inner.lock().client["enhanced_rtmp"].clone();
    assert_eq!(c["multitrack"], true, "{c}");
    assert_eq!(c["four_cc_list"], serde_json::json!(["av01", "avc1"]));
}

#[tokio::test]
async fn delete_stream_is_the_sender_ending_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path());
    publish(&hub, "open", "ending").await;
    let s = ended_session(&hub, "ending").await;
    let reasons: Vec<Option<String>> = s.inner.lock().connections.iter().map(|c| c.end_reason.clone()).collect();
    assert_eq!(reasons, vec![Some("the sender ended the stream".to_string())]);
}
