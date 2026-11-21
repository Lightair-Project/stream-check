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

/// Binds every port (port 0 picks a free one) and starts serving.
pub async fn start(cfg: Config) -> Result<Running, String> {
    let hub = Hub::new(cfg.clone());
    let bind = |p: u16| SocketAddr::from(([0, 0, 0, 0], p));
    let rtmp = tokio::net::TcpListener::bind(bind(cfg.rtmp_port)).await.map_err(|e| format!("RTMP port {}: {e}", cfg.rtmp_port))?;
    let rtmp_addr = rtmp.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(crate::rtmp::listen(hub.clone(), rtmp, Proto::Rtmp, None));

    let (mut rtmps_addr, mut cert) = (None, None);
    if let Some(port) = cfg.rtmps_port {
        let mut names = vec!["127.0.0.1".to_string(), "10.0.2.2".to_string()];
        if let Some(ip) = lan_ip() {
            names.push(ip.to_string());
        }
        let (acceptor, path) = crate::tls::acceptor(&cfg.data_dir.join("tls"), names)?;
        let l = tokio::net::TcpListener::bind(bind(port)).await.map_err(|e| format!("RTMPS port {port}: {e}"))?;
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
