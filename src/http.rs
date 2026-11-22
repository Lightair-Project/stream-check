//! The web UI, the JSON API, the preview WebSocket and the WHIP endpoint, on one port.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::faults::FaultRequest;
use crate::hub::{Hub, State as SessionState};
use crate::whip::WhipState;

#[derive(Clone)]
pub struct App {
    pub hub: Arc<Hub>,
    pub whip: Arc<WhipState>,
}

const UI: &str = include_str!("ui.html");

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], UI) }))
        .route("/api/status", get(status))
        .route("/api/profiles", get(|| async { Json(json!(crate::profiles::PROFILES)) }))
        .route("/api/sessions", get(sessions))
        .route("/api/streams", get(sessions))
        .route("/api/past", get(past))
        .route("/api/sessions/{id}", get(session))
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/report", get(report))
        .route("/api/sessions/{id}/fault", post(fault_session))
        .route("/api/sessions/{id}/end", post(end_session))
        .route("/api/faults", get(faults))
        .route("/api/faults/{target}", post(fault_target))
        .route("/api/rejections", get(rejections))
        .route("/api/qr", get(qr))
        .route("/media/{id}/{*file}", get(media))
        .route("/ws/state", get(ws_state))
        .route("/ws/preview/{id}", get(ws_preview))
        .route("/api/v3/GetClientConfiguration", post(client_configuration))
        .route("/whip/session/{id}", delete(whip_delete).options(whip_options))
        .route("/whip/{*path}", post(whip_post).options(whip_options))
        .with_state(app)
}

/// Twitch's Enhanced Broadcasting configuration service, standing in: a fixed Dual Format plan
/// (each of the client's two canvases as one track, 2 s keyframes, AAC) whose ingest is this
/// lab, so a client's whole Dual Format path can be tried without Twitch. The field names follow
/// OBS's reading of Twitch's answer; the values are the lab's, not Twitch's.
async fn client_configuration(State(a): State<App>, headers: HeaderMap, body: String) -> Response {
    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let config_id = format!("stream-check-{:x}", crate::hub::now_ms());
    let fail = |msg: &str| Json(json!({ "meta": { "config_id": config_id }, "status": { "result": "error", "html_en_us": msg } })).into_response();
    if req.is_null() {
        return fail("Stream Check: the request isn't JSON.");
    }
    if req["authentication"].as_str().is_none_or(str::is_empty) {
        return fail("Stream Check: no stream key in <code>authentication</code>.");
    }
    let canvases = req["preferences"]["canvases"].as_array().cloned().unwrap_or_default();
    if canvases.len() < 2 {
        return fail("Stream Check: Dual Format needs two canvases (landscape and vertical).");
    }
    // the client's own encoder kind on its system (libobs encoder ids, as Twitch answers)
    let apple = req["capabilities"]["system"]["name"].as_str() == Some("macOS");
    let kind = if apple { "com.apple.videotoolbox.videoencoder.ave.avc" } else { "obs_x264" };
    let track = |i: usize, c: &Value, kbps: u32| {
        let (w, h) = (c["width"].as_u64().unwrap_or(1920).min(1920), c["height"].as_u64().unwrap_or(1080).min(1920));
        let fr = if c["framerate"].is_object() { c["framerate"].clone() } else { json!({ "numerator": 30, "denominator": 1 }) };
        json!({
            "type": kind, "width": w, "height": h, "framerate": fr,
            "gpu_scale_type": "OBS_SCALE_BICUBIC", "colorspace": "OBS_VIDEO_CS_709", "range": "OBS_VIDEO_RANGE_PARTIAL",
            "format": "OBS_VIDEO_FORMAT_NV12", "bitrate_interpolation_points": null,
            "settings": { "bitrate": kbps, "keyint_sec": 2, "rate_control": "CBR", "profile": "high", "bframes": 0 },
            "canvas_index": i,
        })
    };
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| h.rsplit_once(':').map_or(h, |(n, _)| n).to_string())
        .unwrap_or_else(|| "127.0.0.1".into());
    let (rtmp, rtmps) = *a.hub.ingest_ports.lock();
    let mut endpoints = vec![json!({ "protocol": "RTMP", "url_template": format!("rtmp://{host}:{rtmp}/twitch-dual/{{stream_key}}") })];
    if let Some(p) = rtmps {
        endpoints.push(json!({ "protocol": "RTMPS", "url_template": format!("rtmps://{host}:{p}/twitch-dual/{{stream_key}}") }));
    }
    Json(json!({
        "meta": { "config_id": config_id, "schema_version": req["schema_version"] },
        "status": { "result": "success", "html_en_us": "Stream Check: a fixed Dual Format plan (two tracks), streaming to this lab." },
        "ingest_endpoints": endpoints,
        "encoder_configurations": [track(0, &canvases[0], 6000), track(1, &canvases[1], 5000)],
        "audio_configurations": { "live": [{ "codec": "aac", "track_id": 1, "channels": 2, "settings": { "bitrate": 160 } }], "vod": null },
    }))
    .into_response()
}

async fn status(State(a): State<App>) -> Json<Value> {
    Json(a.hub.snapshot())
}

async fn sessions(State(a): State<App>) -> Json<Value> {
    let v: Vec<Value> = a.hub.sessions.lock().iter().rev().map(|s| s.summary(false)).collect();
    Json(json!(v))
}

/// Reports of earlier runs, from disk.
async fn past(State(a): State<App>) -> Json<Value> {
    let dir = a.hub.cfg.data_dir.join("sessions");
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut names: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        names.sort();
        for p in names.into_iter().rev().take(100) {
            if let Ok(text) = std::fs::read_to_string(p.join("report.json")) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    out.push(json!({ "id": v["id"], "platform": v["platform"], "key": v["key"], "ok": v["ok"], "problems": v["problems"], "duration_s": v["duration_s"], "started_ms": v["started_ms"], "protocols": v["protocols"] }));
                }
            }
        }
    }
    Json(json!(out))
}

#[allow(clippy::result_large_err)]
fn find(a: &App, id: &str) -> Result<Arc<crate::hub::Session>, Response> {
    a.hub.find(id).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no session '{id}'")))
}

async fn session(State(a): State<App>, Path(id): Path<String>) -> Response {
    match find(&a, &id) {
        Ok(s) => Json(s.summary(true)).into_response(),
        Err(e) => e,
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<f64>,
    kind: Option<String>,
}

async fn events(State(a): State<App>, Path(id): Path<String>, Query(q): Query<EventsQuery>) -> Response {
    let s = match find(&a, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let g = s.inner.lock();
    let v: Vec<_> = g.events.iter().filter(|e| e.t > q.since.unwrap_or(-1.0) && q.kind.as_deref().is_none_or(|k| e.kind == k)).collect();
    Json(json!(v)).into_response()
}

async fn report(State(a): State<App>, Path(id): Path<String>) -> Response {
    if let Some(s) = a.hub.find(&id) {
        let ended = s.inner.lock().report.clone();
        return Json(ended.unwrap_or_else(|| {
            let mut r = s.build_report();
            r["live"] = json!(true);
            r
        }))
        .into_response();
    }
    // an earlier run
    let dir = a.hub.cfg.data_dir.join("sessions");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut names: Vec<_> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        let hit = if id == "latest" { names.last().cloned() } else { names.iter().rev().find(|n| **n == id || n.contains(&id)).cloned() };
        if let Some(n) = hit {
            if let Ok(text) = std::fs::read_to_string(dir.join(n).join("report.json")) {
                return ([(header::CONTENT_TYPE, "application/json")], text).into_response();
            }
        }
    }
    err(StatusCode::NOT_FOUND, format!("no session or report '{id}'"))
}

async fn fault_session(State(a): State<App>, Path(id): Path<String>, Json(r): Json<FaultRequest>) -> Response {
    apply_fault(&a, &id, r)
}

async fn fault_target(State(a): State<App>, Path(t): Path<String>, Json(r): Json<FaultRequest>) -> Response {
    apply_fault(&a, &t, r)
}

fn apply_fault(a: &App, target: &str, r: FaultRequest) -> Response {
    match a.hub.fault(target, &r) {
        Ok(said) => Json(json!({ "ok": true, "applied": said })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn faults(State(a): State<App>) -> Json<Value> {
    Json(a.hub.faults_json())
}

async fn rejections(State(a): State<App>) -> Json<Value> {
    Json(json!(a.hub.rejections.lock().iter().rev().cloned().collect::<Vec<_>>()))
}

async fn end_session(State(a): State<App>, Path(id): Path<String>) -> Response {
    let s = match find(&a, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    s.kill();
    a.hub.finalize(s, "ended from the UI or API".into()).await;
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct QrQuery {
    text: String,
}

async fn qr(Query(q): Query<QrQuery>) -> Response {
    ([(header::CONTENT_TYPE, "image/svg+xml")], crate::net::qr_svg(&q.text)).into_response()
}

async fn media(State(a): State<App>, Path((id, file)): Path<(String, String)>) -> Response {
    let Some(s) = a.hub.find(&id) else {
        // an earlier run
        let p = a.hub.cfg.data_dir.join("sessions").join(&id).join(&file);
        return serve_file(p, a.hub.cfg.data_dir.join("sessions").join(&id)).await;
    };
    serve_file(s.dir.join(&file), s.dir.clone()).await
}

async fn serve_file(path: PathBuf, root: PathBuf) -> Response {
    let (Ok(p), Ok(r)) = (path.canonicalize(), root.canonicalize()) else { return err(StatusCode::NOT_FOUND, "no such file") };
    if !p.starts_with(&r) {
        return err(StatusCode::FORBIDDEN, "outside the session");
    }
    match tokio::fs::read(&p).await {
        Ok(bytes) => {
            let ct = match p.extension().and_then(|e| e.to_str()) {
                Some("jpg") => "image/jpeg",
                Some("json") => "application/json",
                Some("flv") => "video/x-flv",
                Some("ts") => "video/mp2t",
                Some("mkv") => "video/x-matroska",
                _ => "application/octet-stream",
            };
            Response::builder().header(header::CONTENT_TYPE, ct).header(header::CACHE_CONTROL, "no-cache").body(Body::from(bytes)).unwrap()
        }
        Err(_) => err(StatusCode::NOT_FOUND, "no such file"),
    }
}

async fn ws_state(State(a): State<App>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let mut every = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = every.tick() => {
                    let text = a.hub.snapshot().to_string();
                    if socket.send(Message::Text(text.into())).await.is_err() { break }
                }
                m = socket.recv() => if m.is_none() { break },
            }
        }
    })
}

async fn ws_preview(State(a): State<App>, Path(id): Path<String>, ws: WebSocketUpgrade) -> Response {
    let s = match find(&a, &id) {
        Ok(s) => s,
        Err(e) => return e,
    };
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let mut rx = s.preview.subscribe();
        for m in s.preview_start() {
            if socket.send(Message::Binary(m.to_vec().into())).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Ok(m) => if socket.send(Message::Binary(m.to_vec().into())).await.is_err() { break },
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                },
                c = socket.recv() => if c.is_none() { break },
            }
            if s.inner.lock().state == SessionState::Ended {
                break;
            }
        }
    })
}

fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("POST, DELETE, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("location"));
    r
}

async fn whip_options() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

async fn whip_post(
    State(a): State<App>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(|s| s.trim().to_string());
    match a.whip.offer(&path, bearer.as_deref(), body, remote).await {
        Ok((sdp, id)) => cors(
            Response::builder()
                .status(StatusCode::CREATED)
                .header(header::CONTENT_TYPE, "application/sdp")
                .header(header::LOCATION, format!("/whip/session/{id}"))
                .body(Body::from(sdp))
                .unwrap(),
        ),
        Err((code, msg)) => cors(err(StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST), msg)),
    }
}

async fn whip_delete(State(a): State<App>, Path(id): Path<String>) -> Response {
    cors(if a.whip.delete(&id) { StatusCode::OK.into_response() } else { StatusCode::NOT_FOUND.into_response() })
}
