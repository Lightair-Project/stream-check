//! Starting everything: the hub, the ingests and the web server.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::http::{App, router};
use crate::hub::{Config, Hub, Proto};
use crate::net::{Ports, addresses, lan_ip};
use crate::whip::WhipState;

pub struct Running {
    pub hub: Arc<Hub>,
    pub http: SocketAddr,
    pub rtmp: SocketAddr,
    pub rtmps: Option<SocketAddr>,
    pub srt: SocketAddr,
    pub cert: Option<std::path::PathBuf>,
}

/// How much an ingest connection's kernel receive buffer holds. A throttle is applied to what
/// the lab reads, so whatever the kernel accepts beyond it hides the slow network from the
/// sender: macOS grows a localhost receive buffer to megabytes, which swallowed seconds of a
/// throttled stream (the sender saw no backlog and never adapted, and the picture arrived late
/// and then in a burst). A slow uplink keeps that backlog in the sender's own buffer, as this
/// does; 64 KB is still far more than a normal stream needs at local round-trip times.
const INGEST_RECV_BUFFER: u32 = 64 * 1024;

/// A TCP listener for an ingest (RTMP, RTMPS) with the receive buffer above, which the
/// connections it accepts inherit.
fn ingest_listener(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let socket = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    socket.set_reuseaddr(true)?;
    socket.set_recv_buffer_size(INGEST_RECV_BUFFER)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// Binds every port (port 0 picks a free one) and starts serving.
pub async fn start(cfg: Config) -> Result<Running, String> {
    let hub = Hub::new(cfg.clone());
    let bind = |p: u16| SocketAddr::from(([0, 0, 0, 0], p));
    let rtmp = ingest_listener(bind(cfg.rtmp_port)).map_err(|e| format!("RTMP port {}: {e}", cfg.rtmp_port))?;
    let rtmp_addr = rtmp.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(crate::rtmp::listen(hub.clone(), rtmp, Proto::Rtmp, None));

    let (mut rtmps_addr, mut cert) = (None, None);
    if let Some(port) = cfg.rtmps_port {
        let mut names = vec!["127.0.0.1".to_string(), "10.0.2.2".to_string()];
        if let Some(ip) = lan_ip() {
            names.push(ip.to_string());
        }
        let (acceptor, path) = crate::tls::acceptor(&cfg.data_dir.join("tls"), names)?;
        let l = ingest_listener(bind(port)).map_err(|e| format!("RTMPS port {port}: {e}"))?;
        rtmps_addr = l.local_addr().ok();
        cert = Some(path);
        tokio::spawn(crate::rtmp::listen(hub.clone(), l, Proto::Rtmps, Some(acceptor)));
    }

    // SRT: UDP, so a port number is free to take even when the same number is used by TCP
    let srt_port = if cfg.srt_port == 0 { free_udp_port() } else { cfg.srt_port };
    let srt_addr = bind(srt_port);
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::srt::listen(hub, srt_addr).await {
                eprintln!("SRT listener: {e}");
            }
        });
    }

    let http = tokio::net::TcpListener::bind(bind(cfg.http_port)).await.map_err(|e| format!("HTTP port {}: {e}", cfg.http_port))?;
    let http_addr = http.local_addr().map_err(|e| e.to_string())?;
    let app = App { hub: hub.clone(), whip: WhipState::new(hub.clone()) };
    tokio::spawn(async move {
        let _ = axum::serve(http, router(app).into_make_service_with_connect_info::<SocketAddr>()).await;
    });

    *hub.addresses.lock() = addresses(
        &Ports { http: http_addr.port(), rtmp: rtmp_addr.port(), rtmps: rtmps_addr.map(|a| a.port()), srt: srt_port },
        cfg.srt_passphrase.as_deref(),
        cfg.whip_token.as_deref(),
    );
    Ok(Running { hub, http: http_addr, rtmp: rtmp_addr, rtmps: rtmps_addr, srt: srt_addr, cert })
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| s.local_addr()).map_or(9000, |a| a.port())
}
