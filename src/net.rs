//! The addresses senders should use: this machine's LAN address, and QR codes for them.

use std::net::{IpAddr, UdpSocket};

use serde_json::{Value, json};

/// The address other devices on the network reach this machine at (no packet is sent).
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

pub fn qr_svg(text: &str) -> String {
    use qrcode::{QrCode, render::svg};
    match QrCode::new(text.as_bytes()) {
        Ok(c) => c.render::<svg::Color>().min_dimensions(180, 180).dark_color(svg::Color("#111")).light_color(svg::Color("#fff")).build(),
        Err(_) => String::new(),
    }
}

pub struct Ports {
    pub http: u16,
    pub rtmp: u16,
    pub rtmps: Option<u16>,
    pub srt: u16,
}

/// Every URL a sender can use, for each host: the LAN address, localhost, and the Android
/// emulator's name for this machine.
pub fn addresses(p: &Ports, passphrase: Option<&str>, whip_token: Option<&str>) -> Value {
    let mut hosts = vec![];
    if let Some(ip) = lan_ip() {
        hosts.push(("lan", ip.to_string()));
    }
    hosts.push(("localhost", "127.0.0.1".to_string()));
    hosts.push(("android-emulator", "10.0.2.2".to_string()));
    let examples = [
        ("twitch", "live_123456789_labtestkey"),
        ("youtube", "abcd-efgh-ijkl-mnop-qrst"),
        ("kick", "sk_us-west-2_labtestkey"),
        ("open", "test"),
    ];
    let list: Vec<Value> = hosts
        .iter()
        .map(|(label, host)| {
            let rtmp = format!("rtmp://{host}:{}", p.rtmp);
            let rtmps = p.rtmps.map(|port| format!("rtmps://{host}:{port}"));
            let srt = |app: &str, key: &str| {
                let mut u = format!("srt://{host}:{}?streamid={app}/{key}", p.srt);
                if let Some(pw) = passphrase {
                    u.push_str(&format!("&passphrase={pw}"));
                }
                u
            };
            let whip = |app: &str, key: &str| format!("http://{host}:{}/whip/{app}/{key}", p.http);
            let streams: Vec<Value> = examples
                .iter()
                .map(|(app, key)| {
                    json!({
                        "platform": app,
                        "key": key,
                        "rtmp": { "server": format!("{rtmp}/{app}"), "key": key, "url": format!("{rtmp}/{app}/{key}") },
                        "rtmps": rtmps.as_ref().map(|r| json!({ "server": format!("{r}/{app}"), "key": key, "url": format!("{r}/{app}/{key}") })),
                        "srt": srt(app, key),
                        "whip": whip(app, key),
                    })
                })
                .collect();
            json!({ "label": label, "host": host, "ui": format!("http://{host}:{}", p.http), "streams": streams })
        })
        .collect();
    json!({
        "hosts": list,
        "srt_passphrase": passphrase,
        "whip_token": whip_token,
        "ports": { "http": p.http, "rtmp": p.rtmp, "rtmps": p.rtmps, "srt": p.srt },
    })
}
