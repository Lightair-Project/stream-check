//! SRT ingest: a listener that accepts callers publishing an MPEG transport stream, with an
//! optional passphrase and the stream id naming `app/key` (plain, or in the access-control form
//! `#!::r=app/key,m=publish`).

use std::sync::Arc;

use futures::StreamExt;
use srt_protocol::options::KeySize;
use srt_protocol::settings::{KeySettings, Passphrase};
use srt_tokio::SrtListener;

use crate::analyze::Level;
use crate::hub::{Hub, Proto};
use crate::srt_stats;
use crate::ts::TsDemux;

/// `(app, key, mode)` out of a stream id.
pub fn parse_stream_id(id: &str) -> (String, String, Option<String>) {
    let mut resource = id.to_string();
    let mut mode = None;
    if let Some(rest) = id.strip_prefix("#!::") {
        resource = String::new();
        for kv in rest.split(',') {
            match kv.split_once('=') {
                Some(("r", v)) => resource = v.to_string(),
                Some(("m", v)) => mode = Some(v.to_string()),
                _ => {}
            }
        }
    }
    let parts: Vec<&str> = resource.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => (String::new(), String::new(), mode),
        [k] => (String::new(), k.to_string(), mode),
        [a, .., k] => (a.to_string(), k.to_string(), mode),
    }
}

pub async fn listen(hub: Arc<Hub>, bind: std::net::SocketAddr) -> std::io::Result<()> {
    let (listener, mut incoming) = SrtListener::builder().bind(bind).await?;
    let lab_latency_ms = listener.settings().recv_latency.as_millis() as u64;
    while let Some(request) = incoming.incoming().next().await {
        let id = request.stream_id().map(|s| s.to_string()).unwrap_or_default();
        let remote = request.remote();
        let (app, key, mode) = parse_stream_id(&id);
        if hub.refusing_all() {
            continue;
        }
        if mode.as_deref().is_some_and(|m| m != "publish") {
            let _ = hub.authorize(Proto::Srt, &app, "", remote); // recorded as a refusal
            continue;
        }
        let publisher = match hub.authorize(Proto::Srt, &app, &key, remote) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let key_settings =
            hub.cfg.srt_passphrase.as_ref().map(|p| KeySettings { key_size: KeySize::AES128, passphrase: Passphrase::from(p.as_str()) });
        let hub2 = hub.clone();
        tokio::spawn(async move {
            let _ = &hub2;
            let mut publisher = publisher;
            let mut socket = match request.accept(key_settings).await {
                Ok(s) => s,
                Err(e) => {
                    publisher.session.event(Level::Error, "srt", format!("the SRT handshake failed: {e} (a wrong passphrase?)"));
                    publisher.end("the SRT handshake failed");
                    return;
                }
            };
            publisher.session.event(Level::Info, "srt", format!("stream id '{id}'"));
            let link = srt_stats::link(publisher.conn, socket.settings(), lab_latency_ms, publisher.session.t0.elapsed().as_secs_f64());
            publisher.session.event(
                Level::Info,
                "srt",
                format!(
                    "SRT link: {} ms receive latency (the lab's {} ms or the sender's, whichever is larger), {:.0} ms handshake round trip, flow window {} packets{}",
                    link.latency_ms,
                    link.lab_latency_ms,
                    link.handshake_rtt_ms,
                    link.flow_window_pkts,
                    link.encryption_bits.map_or(String::new(), |b| format!(", AES-{b}"))
                ),
            );
            let stats = srt_stats::for_session(&publisher.session);
            stats.lock().begin(link);
            let mut socket_stats = socket.statistics().clone();
            let mut fault_dropped = 0u64;
            let mut demux = TsDemux::new();
            let mut pending: Vec<u8> = vec![];
            let mut reported = (0u64, 0u64);
            let mut rng = 0x9e37_79b9_7f4a_7c15u64;
            let reason = loop {
                let item = tokio::select! {
                    _ = publisher.killed() => break "dropped by a fault (injected)".to_string(),
                    i = socket.next() => i,
                    Some(st) = socket_stats.next() => {
                        let notes = stats.lock().update(publisher.session.t0.elapsed().as_secs_f64(), &st, fault_dropped);
                        for (level, msg) in notes {
                            publisher.session.event(level, "srt", msg);
                        }
                        continue;
                    }
                };
                let data = match item {
                    Some(Ok((_, d))) => d,
                    Some(Err(e)) => break format!("SRT error: {e}"),
                    None => break "the sender closed the connection".to_string(),
                };
                if !publisher.gate(data.len()).await {
                    break "dropped by a fault (injected)".to_string();
                }
                let loss = publisher.effective().loss_pct;
                if loss > 0.0 {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    if ((rng % 10_000) as f32) < loss * 100.0 {
                        fault_dropped += 1;
                        continue;
                    }
                }
                pending.extend_from_slice(&data);
                let n = pending.len() / 188 * 188;
                if n > 0 {
                    publisher.raw(&pending[..n]);
                }
                let mut out = vec![];
                demux.push(&data, &mut out);
                for w in demux.warnings.drain(..) {
                    publisher.session.event(Level::Warn, "unsupported", w);
                }
                if (demux.discontinuities, demux.sync_losses) != reported {
                    let (d, s) = (demux.discontinuities - reported.0, demux.sync_losses - reported.1);
                    reported = (demux.discontinuities, demux.sync_losses);
                    if d > 0 {
                        publisher.session.event(
                            Level::Warn,
                            "ts-loss",
                            format!("{d} transport stream continuity jump(s): packets were lost"),
                        );
                    }
                    if s > 0 {
                        publisher.session.event(
                            Level::Warn,
                            "ts-sync",
                            format!("{s} byte(s) skipped to find the next transport stream packet"),
                        );
                    }
                }
                for m in out {
                    publisher.media(m);
                }
                pending.drain(..n);
            };
            stats.lock().finish();
            let mut out = vec![];
            demux.finish(&mut out);
            for m in out {
                publisher.media(m);
            }
            publisher.end(&reason);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ids_come_in_several_forms() {
        assert_eq!(parse_stream_id("twitch/live_1_abc"), ("twitch".into(), "live_1_abc".into(), None));
        assert_eq!(parse_stream_id("mykey"), ("".into(), "mykey".into(), None));
        assert_eq!(parse_stream_id("#!::r=youtube/abcd-efgh,m=publish"), ("youtube".into(), "abcd-efgh".into(), Some("publish".into())));
        assert_eq!(parse_stream_id("#!::r=live/test,m=request").2.as_deref(), Some("request"));
    }
}
