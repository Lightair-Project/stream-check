//! Sessions the lab starts itself: pulling from an RTSP server, an NDI source or a camera, and
//! checking a file. They are listed, measured, reported and ended like a sender's sessions.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::hub::{Hub, Session, State};

/// What to pull, as the API and the CLI say it.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    /// `rtsp://host:8554/live`, `ndi://MACHINE (Source)`, `camera://Lightair Virtual Camera`.
    pub url: String,
    /// The name the session goes by (from the URL when not given).
    pub key: Option<String>,
    /// The platform profile it's held to ("open" when not given).
    pub profile: Option<String>,
    /// End the session after this many seconds.
    pub for_s: Option<f64>,
}

/// Starts pulling; the session exists when this returns.
pub async fn start(hub: &Arc<Hub>, req: PullRequest) -> Result<Arc<Session>, String> {
    let profile = req.profile.clone().unwrap_or_else(|| "open".into());
    if crate::profiles::find(&profile).is_none() {
        return Err(format!("no profile '{profile}' (twitch, twitch-dual, youtube, kick, open)"));
    }
    let (scheme, rest) = req.url.split_once("://").ok_or("a pull address looks like rtsp://…, ndi://… or camera://…")?;
    let s = match scheme.to_ascii_lowercase().as_str() {
        "rtsp" => crate::rtsp::start(hub, &req.url, req.key.clone(), &profile).await?,
        "ndi" => {
            let name = crate::camera::percent_decode(rest);
            crate::ndi::start(hub, &name, req.key.clone(), &profile).await?
        }
        "camera" => {
            let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
            let query: HashMap<String, String> = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), crate::camera::percent_decode(v)))
                .collect();
            crate::camera::start(hub, &crate::camera::percent_decode(name), &query, req.key.clone(), &profile).await?
        }
        "rtsps" => return Err("RTSP over TLS (rtsps://) isn't supported; use rtsp://".into()),
        other => return Err(format!("Stream Check can't pull '{other}://' (rtsp, ndi and camera are known)")),
    };
    if let Some(secs) = req.for_s.filter(|s| *s > 0.0) {
        let (hub, s) = (hub.clone(), s.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(secs)).await;
            if s.inner.lock().state != State::Ended {
                stop(&hub, &s, format!("the {secs} s asked for are up")).await;
            }
        });
    }
    Ok(s)
}

/// Ends a session the lab reads from: its reader is told to stop and puts its last numbers in,
/// then (if it hasn't ended by itself in a few seconds) the session is ended here.
pub async fn stop(hub: &Arc<Hub>, s: &Arc<Session>, reason: String) {
    s.stop_with(reason.clone());
    for _ in 0..50 {
        if s.inner.lock().state == State::Ended {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hub.finalize(s.clone(), reason).await;
}

/// The sources of a kind on this computer or its network: `ndi` or `camera`.
pub async fn sources(kind: &str) -> Value {
    let kind = kind.to_string();
    tokio::task::spawn_blocking(move || {
        let r = match kind.as_str() {
            "ndi" => crate::ndi::sources(Duration::from_secs(3)),
            "camera" | "cameras" => crate::camera::list(),
            other => Err(format!("no source kind '{other}' (ndi, camera)")),
        };
        match r {
            Ok(list) => json!({ "kind": kind, "available": true, "sources": list }),
            Err(e) => json!({ "kind": kind, "available": false, "error": e, "sources": [] }),
        }
    })
    .await
    .unwrap_or_else(|e| json!({ "available": false, "error": e.to_string(), "sources": [] }))
}
