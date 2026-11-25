//! The command line: `serve` (the default) and the commands that talk to a running Stream Check,
//! meant for scripts and agents: `list`, `report`, `wait-for`, `events`, `fault`, `end`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::hub::Config;

pub const USAGE: &str = "\
Stream Check: a local fake streaming platform for testing RTMP, RTMPS, SRT and WHIP senders.

USAGE
  stream-check [serve] [options]        start it (the default)
  stream-check addresses                print the URLs a sender can use
  stream-check list                     the sessions, one line each
  stream-check report <session|key|latest> [--summary]
                                        the JSON report (or a short text), live or finished
  stream-check wait-for <key> [--live] [--ended] [--timeout 30] [--min-seconds N]
                                        wait for a stream; prints its summary, exit 0 on success, 1 on timeout
  stream-check events <session|key> [--kind <kind>]
  stream-check fault <key|session|*> <drop | refuse <s> | freeze <s> | throttle <kbps> | latency <ms> | loss <pct> | clear> [--for <s>]
  stream-check end <session|key>        finish a session now (writes its report)
  stream-check check-file <path> [--profile <name>] [--key <name>] [--json]
                                        read a recording or clip (MP4, MOV, MKV, FLV, TS) as a session; prints
                                        its summary (or the JSON report); exit 3 if problems were found
  stream-check pull <url> [--key <name>] [--profile <name>] [--for <s>] [--wait] [--json]
                                        pull rtsp://host:8554/live, ndi://MACHINE (Source) or
                                        camera://<name>[?fps=30&size=1920x1080] as a session
  stream-check sources <ndi|camera>     the NDI sources on the network, or this Mac's cameras

  check-file, pull and sources go through a running Stream Check (--url) when one answers, so
  the session shows in its UI; otherwise (or with --local) they run here and write the report
  to --data. Running here, pull waits until the source stops, --for runs out or Ctrl-C.

SERVE OPTIONS
  --http-port <n>        web UI, API and WHIP (default 8890)
  --rtmp-port <n>        RTMP (default 1935)
  --rtmps-port <n>       RTMPS, with a self-signed certificate (default 1936)
  --no-rtmps             don't listen for RTMPS
  --srt-port <n>         SRT listener, UDP (default 9000)
  --srt-passphrase <s>   require this SRT passphrase (10 to 79 characters)
  --whip-token <s>       require this WHIP bearer token (default: none)
  --data <dir>           recordings and reports (default ./stream-check-data)
  --strict-keys          platform profiles refuse keys that don't look like their kind (default)
  --any-key              accept any stream key on every platform
  --accept-key <platform>=<key>   accept only these keys for a platform (repeatable)
  --grace <s>            reconnect grace period, overriding the platform profiles
  --keep <n>             keep the recordings of this many sessions (default 30); reports stay

OPTIONS FOR THE OTHER COMMANDS
  --url <url>            where Stream Check runs (default http://127.0.0.1:8890, or $STREAM_CHECK_URL)
  --data <dir>           where to read reports from when it isn't running

Platforms are the first part of the path: rtmp://host:1935/twitch/<key>, srt://host:9000?streamid=twitch/<key>,
http://host:8890/whip/twitch/<key>. twitch, twitch-dual (Twitch Dual Format: Enhanced RTMP multitrack), youtube, kick and open are known;
anything else is 'open'.
";

pub fn serve_config(args: &[String]) -> Result<Config, String> {
    let mut cfg = Config {
        data_dir: PathBuf::from("stream-check-data"),
        http_port: 8890,
        rtmp_port: 1935,
        rtmps_port: Some(1936),
        srt_port: 9000,
        srt_passphrase: None,
        whip_token: None,
        lenient_keys: false,
        accept_keys: HashMap::new(),
        grace_override: None,
        ffmpeg: crate::helper::ffmpeg_available(),
        keep_recordings: 30,
    };
    let mut i = 0;
    let num =
        |v: Option<&String>, name: &str| -> Result<u32, String> { v.and_then(|v| v.parse().ok()).ok_or(format!("{name} needs a number")) };
    while i < args.len() {
        let v = args.get(i + 1);
        // an option and its value, or a word alone
        let mut step = 2;
        match args[i].as_str() {
            "serve" => step = 1,
            "--http-port" => cfg.http_port = num(v, "--http-port")? as u16,
            "--rtmp-port" => cfg.rtmp_port = num(v, "--rtmp-port")? as u16,
            "--rtmps-port" => cfg.rtmps_port = Some(num(v, "--rtmps-port")? as u16),
            "--no-rtmps" => {
                cfg.rtmps_port = None;
                step = 1;
            }
            "--srt-port" => cfg.srt_port = num(v, "--srt-port")? as u16,
            "--srt-passphrase" => cfg.srt_passphrase = Some(v.cloned().ok_or("--srt-passphrase needs a value")?),
            "--whip-token" => cfg.whip_token = Some(v.cloned().ok_or("--whip-token needs a value")?),
            "--data" => cfg.data_dir = PathBuf::from(v.ok_or("--data needs a folder")?),
            "--strict-keys" => {
                cfg.lenient_keys = false;
                step = 1;
            }
            "--any-key" => {
                cfg.lenient_keys = true;
                step = 1;
            }
            "--accept-key" => {
                let (p, k) = v.and_then(|v| v.split_once('=')).ok_or("--accept-key needs platform=key")?;
                cfg.accept_keys.entry(p.to_string()).or_default().push(k.to_string());
            }
            "--grace" => cfg.grace_override = Some(num(v, "--grace")?),
            "--keep" => cfg.keep_recordings = num(v, "--keep")? as usize,
            other => return Err(format!("unknown option '{other}'\n\n{USAGE}")),
        }
        i += step;
    }
    if let Some(p) = &cfg.srt_passphrase {
        if !(10..=79).contains(&p.len()) {
            return Err("an SRT passphrase has to be 10 to 79 characters".into());
        }
    }
    Ok(cfg)
}

struct Client {
    base: String,
    data: PathBuf,
    agent: ureq::Agent,
}

impl Client {
    fn new(args: &[String]) -> Client {
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let base = flag("--url").or_else(|| std::env::var("STREAM_CHECK_URL").ok()).unwrap_or_else(|| "http://127.0.0.1:8890".into());
        let agent: ureq::Agent =
            ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(20))).build().into();
        Client {
            base: base.trim_end_matches('/').to_string(),
            data: PathBuf::from(flag("--data").unwrap_or_else(|| "stream-check-data".into())),
            agent,
        }
    }

    fn get(&self, path: &str) -> Result<(u16, String), String> {
        let mut r = self
            .agent
            .get(&format!("{}{path}", self.base))
            .call()
            .map_err(|e| format!("can't reach Stream Check at {}: {e}", self.base))?;
        let code = r.status().as_u16();
        Ok((code, r.body_mut().read_to_string().map_err(|e| e.to_string())?))
    }

    fn post(&self, path: &str, body: &Value) -> Result<(u16, String), String> {
        let mut r = self
            .agent
            .post(&format!("{}{path}", self.base))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| format!("can't reach Stream Check at {}: {e}", self.base))?;
        let code = r.status().as_u16();
        Ok((code, r.body_mut().read_to_string().map_err(|e| e.to_string())?))
    }

    fn json(&self, path: &str) -> Result<Value, String> {
        let (code, text) = self.get(path)?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        if code >= 400 {
            return Err(v["error"].as_str().unwrap_or("error").to_string());
        }
        Ok(v)
    }
}

fn positional(args: &[String], skip: usize) -> Vec<&String> {
    let mut out = vec![];
    let mut i = skip;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += if matches!(args[i].as_str(), "--live" | "--ended" | "--summary" | "--json" | "--local" | "--wait") { 1 } else { 2 };
        } else {
            out.push(&args[i]);
            i += 1;
        }
    }
    out
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// A few lines for a person (or an agent that prefers text) about a report or a live summary.
pub fn summary_text(r: &Value) -> String {
    let st = &r["stats"];
    let v = &st["video"];
    let a = &st["audio"];
    let mut out = format!(
        "{} on {} (key {}): {}, {:.1} s{}\n",
        r["id"].as_str().unwrap_or("?"),
        r["platform"].as_str().unwrap_or("?"),
        r["key"].as_str().unwrap_or("?"),
        r["state"].as_str().unwrap_or(if r["ok"].as_bool() == Some(true) { "ok" } else { "problems" }),
        r["duration_s"].as_f64().unwrap_or(0.0),
        r["end_reason"].as_str().map_or(String::new(), |e| format!(" ({e})")),
    );
    out += &source_text(r);
    if let (Some(s), true) = (v["sps"].as_object(), v["codec"].is_null()) {
        out += &format!(
            "  video  {} {}x{}, {:.1} fps (declared {}), arrival {:.1} fps\n",
            s["profile"].as_str().unwrap_or("?"),
            s["width"],
            s["height"],
            v["fps"].as_f64().unwrap_or(0.0),
            s["declared_fps"].as_f64().map_or("?".into(), |f| format!("{f:.2}")),
            v["fps_arrival"].as_f64().unwrap_or(0.0),
        );
    } else if let Some(s) = v["sps"].as_object() {
        out += &format!(
            "  video  {} {} level {}, {}x{}, {:.1} fps, {:.0} kbps, keyframe every {} s, GOP {}{}\n",
            v["codec"].as_str().map_or("?".into(), |c| c.to_uppercase()),
            s["profile"].as_str().unwrap_or("?"),
            s["level"].as_str().unwrap_or("?"),
            s["width"],
            s["height"],
            v["fps"].as_f64().unwrap_or(0.0),
            v["kbps"].as_f64().unwrap_or(0.0),
            v["keyint_avg_s"].as_f64().map_or("?".into(), |k| format!("{k:.2}")),
            v["gop_pattern"].as_str().unwrap_or("?"),
            if v["b_frames"] == true { ", B-frames" } else { "" },
        );
    }
    if a["frames"].as_u64().unwrap_or(0) > 0 {
        out += &format!(
            "  audio  {} {} Hz, {} ch, {:.0} kbps; loudness {} LUFS integrated, peak {} dB\n",
            a["codec"].as_str().map_or("?".into(), |c| c.to_uppercase()),
            a["sample_rate"],
            a["channels"],
            a["kbps"].as_f64().unwrap_or(0.0),
            st["levels"]["integrated_lufs"].as_f64().map_or("?".into(), |l| format!("{l:.1}")),
            st["levels"]["max_peak_db"].as_f64().map_or("?".into(), |l| format!("{l:.1}")),
        );
    }
    out += &format!(
        "  timing A/V offset {} ms, drift {} ms; {} stall(s); {} timestamp gap(s); {} backwards\n",
        st["av"]["offset_ms"].as_f64().map_or("?".into(), |o| format!("{o:.0}")),
        st["av"]["drift_ms"].as_f64().map_or("?".into(), |o| format!("{o:.0}")),
        st["stalls"]["count"],
        v["gaps"].as_u64().unwrap_or(0) + a["gaps"].as_u64().unwrap_or(0),
        v["dts_backwards"].as_u64().unwrap_or(0) + a["pts_backwards"].as_u64().unwrap_or(0),
    );
    // Enhanced RTMP multitrack: the tracks beyond the first, and the total
    for t in r["tracks"]["tracks"].as_array().into_iter().flatten() {
        let tv = &t["video"];
        if let Some(s) = tv["sps"].as_object() {
            out += &format!(
                "  track {} video {}x{}, {:.1} fps, {:.0} kbps, keyframe every {} s{}; keyframes aligned {}, not {}; starts {} ms from track 0\n",
                t["track"],
                s["width"],
                s["height"],
                tv["fps"].as_f64().unwrap_or(0.0),
                tv["kbps"].as_f64().unwrap_or(0.0),
                tv["keyint_avg_s"].as_f64().map_or("?".into(), |k| format!("{k:.2}")),
                if tv["b_frames"] == true { ", B-frames" } else { "" },
                t["keyframes_aligned"],
                t["keyframes_misaligned"],
                t["start_offset_ms"].as_f64().map_or("?".into(), |o| format!("{o:.0}")),
            );
        }
        if t["has_audio"] == true {
            out += &format!(
                "  track {} audio {} {} Hz\n",
                t["track"],
                t["audio"]["codec"].as_str().unwrap_or("?").to_uppercase(),
                t["audio"]["sample_rate"]
            );
        }
    }
    if let Some(total) = r["tracks"]["total_video_kbps"].as_f64() {
        out += &format!("  tracks {}, picture {total:.0} kbps in all\n", r["tracks"]["count"]);
    }
    if r["client"]["key_query"]["client_config_id"] == true {
        out += "  key query: clientConfigId is there\n";
    }
    out += &format!("  reconnects {}, grace used {}\n", r["reconnects"], r["grace_uses"]);
    for p in r["violations"].as_array().into_iter().flatten() {
        out += &format!("  rule {} [{}x]: {}\n", p["rule"].as_str().unwrap_or("?"), p["count"], p["msg"].as_str().unwrap_or(""));
    }
    for p in r["problems"].as_array().into_iter().flatten() {
        out += &format!("  PROBLEM: {}\n", p.as_str().unwrap_or(""));
    }
    if let Some(ok) = r["ok"].as_bool() {
        out += &format!("  verdict: {}\n", if ok { "ok" } else { "problems found" });
    }
    out
}

/// The lines a file or pull session adds: what was read, its tracks, its picture.
fn source_text(r: &Value) -> String {
    let src = &r["source"];
    let mut out = String::new();
    match src["kind"].as_str() {
        Some("file") => {
            out += &format!(
                "  file   {} ({}), {} bytes, {} s, {:.0} kbps in all{}{}\n",
                src["path"].as_str().unwrap_or("?"),
                src["format_long"].as_str().or(src["format"].as_str()).unwrap_or("?"),
                src["bytes"],
                src["duration_s"].as_f64().map_or("?".into(), |d| format!("{d:.2}")),
                src["average_kbps"].as_f64().unwrap_or(0.0),
                src["container"]["layout"].as_str().map_or(String::new(), |l| format!(", {l} MP4")),
                src["chapters"].as_array().filter(|c| !c.is_empty()).map_or(String::new(), |c| format!(", {} chapter(s)", c.len())),
            );
            if let Some(o) = src["av_start_offset_ms"].as_f64() {
                let when = if o >= 0.0 { format!("{o:.0} ms after") } else { format!("{:.0} ms before", -o) };
                out += &format!(
                    "  start  sound starts {when} the picture; picture {} s, sound {} s\n",
                    src["video_duration_s"].as_f64().map_or("?".into(), |d| format!("{d:.2}")),
                    src["audio_duration_s"].as_f64().map_or("?".into(), |d| format!("{d:.2}"))
                );
            }
            for (i, t) in src["audio_tracks"].as_array().into_iter().flatten().enumerate() {
                out += &format!(
                    "  sound track {i}: {} {} Hz {} ch{}, {:.0} kbps, {} LUFS integrated, peak {} dB{}\n",
                    t["codec"].as_str().unwrap_or("?"),
                    t["sample_rate"].as_str().unwrap_or("?"),
                    t["channels"],
                    t["title"].as_str().map_or(String::new(), |n| format!(" \"{n}\"")),
                    t["measured"]["kbps"].as_f64().unwrap_or(0.0),
                    t["levels"]["integrated_lufs"].as_f64().map_or("?".into(), |l| format!("{l:.1}")),
                    t["levels"]["max_peak_db"].as_f64().map_or("?".into(), |l| format!("{l:.1}")),
                    if t["levels"]["silent"] == true { ", silent at the end" } else { "" },
                );
            }
        }
        Some("rtsp") => {
            out += &format!(
                "  rtsp   {} (server {}), {}\n",
                src["url"].as_str().unwrap_or("?"),
                src["server"].as_str().unwrap_or("?"),
                src["tracks"].as_array().map_or(String::new(), |t| t
                    .iter()
                    .map(|x| format!("{} {}", x["media"].as_str().unwrap_or(""), x["codec"].as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join(", ")),
            );
            let rtp = &r["stats"]["rtp"];
            out += &format!(
                "  rtp    video {} received, {} lost; sound {} received, {} lost\n",
                rtp["video"]["received"], rtp["video"]["lost"], rtp["audio"]["received"], rtp["audio"]["lost"]
            );
        }
        Some(k @ ("ndi" | "camera")) => {
            out += &format!("  {k:<6} {}\n", src["name"].as_str().unwrap_or("?"));
        }
        _ => {}
    }
    if let Some(p) = src["picture"].as_object() {
        out += &format!(
            "  picture mean colour {}, brightness {:.0}/255; black {:.1} s; frozen {} time(s), longest {:.1} s{}\n",
            p["mean_colour"].as_str().unwrap_or("?"),
            p["mean_luma"].as_f64().unwrap_or(0.0),
            p["black_s"].as_f64().unwrap_or(0.0),
            p["frozen_events"],
            p["longest_frozen_s"].as_f64().unwrap_or(0.0),
            if src["picture_never_changed"] == true { "; it never changed" } else { "" },
        );
    }
    out
}

/// `check-file`, `pull` and `sources`: through a running Stream Check when one answers (or
/// `--url` says where), otherwise here, in this process. Returns the exit code.
pub async fn run_lab(args: &[String]) -> i32 {
    let explicit = args.iter().any(|a| a == "--url") || std::env::var("STREAM_CHECK_URL").is_ok();
    let local = args.iter().any(|a| a == "--local");
    let a = args.to_vec();
    let remote = !local
        && (explicit
            || tokio::task::spawn_blocking(move || {
                let mut c = Client::new(&a);
                c.agent =
                    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(2))).build().into();
                c.get("/api/status").is_ok()
            })
            .await
            .unwrap_or(false));
    let result = if remote {
        let a = args.to_vec();
        tokio::task::spawn_blocking(move || run_remote(&a)).await.unwrap_or_else(|e| Err(e.to_string()))
    } else {
        run_local(args).await
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

fn print_report(report: &Value, json_out: bool) -> i32 {
    if json_out {
        println!("{}", serde_json::to_string_pretty(report).unwrap_or_default());
    } else {
        print!("{}", summary_text(report));
    }
    if report["ok"].as_bool() == Some(false) { 3 } else { 0 }
}

fn run_remote(args: &[String]) -> Result<i32, String> {
    let c = Client::new(args);
    let pos = positional(args, 1);
    let json_out = args.iter().any(|a| a == "--json");
    let wait_end = |id: &str, timeout_s: f64| -> Result<Value, String> {
        let start = Instant::now();
        loop {
            let s = c.json(&format!("/api/sessions/{id}"))?;
            if s["state"] == "ended" && s["has_report"] == true {
                return c.json(&format!("/api/sessions/{id}/report"));
            }
            if start.elapsed().as_secs_f64() > timeout_s {
                return Err(format!("timed out after {timeout_s} s waiting for {id} to end"));
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    };
    let post = |path: &str, body: Value| -> Result<Value, String> {
        let (code, text) = c.post(path, &body)?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        if code >= 400 {
            return Err(v["error"].as_str().unwrap_or("error").to_string());
        }
        Ok(v)
    };
    let timeout: f64 = flag_value(args, "--timeout").and_then(|t| t.parse().ok()).unwrap_or(3600.0);
    match args[0].as_str() {
        "check-file" => {
            let path = pos.first().ok_or("check-file needs a file")?;
            let abs = std::fs::canonicalize(path.as_str()).unwrap_or_else(|_| PathBuf::from(path.as_str()));
            let v = post(
                "/api/check-file",
                json!({ "path": abs, "profile": flag_value(args, "--profile"), "key": flag_value(args, "--key") }),
            )?;
            let report = wait_end(v["id"].as_str().unwrap_or(""), timeout)?;
            Ok(print_report(&report, json_out))
        }
        "pull" => {
            let url = pos.first().ok_or("pull needs a source: rtsp://…, ndi://… or camera://…")?;
            let for_s: Option<f64> = flag_value(args, "--for").and_then(|f| f.parse().ok());
            let v = post(
                "/api/pull",
                json!({ "url": url, "key": flag_value(args, "--key"), "profile": flag_value(args, "--profile"), "for_s": for_s }),
            )?;
            let id = v["id"].as_str().unwrap_or("").to_string();
            if args.iter().any(|a| a == "--wait") || for_s.is_some() {
                let report = wait_end(&id, timeout.max(for_s.unwrap_or(0.0) + 30.0))?;
                return Ok(print_report(&report, json_out));
            }
            println!("{}", json!({ "id": id, "key": v["key"] }));
            Ok(0)
        }
        "sources" => {
            let kind = pos.first().map_or("camera", |k| k.as_str());
            let v = c.json(&format!("/api/sources/{kind}"))?;
            Ok(print_sources(&v, json_out))
        }
        other => Err(format!("unknown command '{other}'")),
    }
}

fn print_sources(v: &Value, json_out: bool) -> i32 {
    if json_out {
        println!("{v}");
    } else if v["available"] == true {
        for s in v["sources"].as_array().into_iter().flatten() {
            println!("{}", s.as_str().unwrap_or(""));
        }
    } else {
        eprintln!("{}", v["error"].as_str().unwrap_or("not available"));
    }
    if v["available"] == true { 0 } else { 1 }
}

async fn run_local(args: &[String]) -> Result<i32, String> {
    let mut cfg = serve_config(&[])?;
    if let Some(d) = flag_value(args, "--data") {
        cfg.data_dir = PathBuf::from(d);
    }
    let pos = positional(args, 1);
    let json_out = args.iter().any(|a| a == "--json");
    match args[0].as_str() {
        "sources" => {
            let kind = pos.first().map_or("camera", |k| k.as_str());
            Ok(print_sources(&crate::pull::sources(kind).await, json_out))
        }
        "check-file" => {
            let path = pos.first().ok_or("check-file needs a file")?;
            let hub = crate::hub::Hub::new(cfg);
            let req = crate::file_check::Request {
                path: PathBuf::from(path.as_str()),
                profile: flag_value(args, "--profile").unwrap_or_else(|| "open".into()),
                key: flag_value(args, "--key"),
            };
            let s = crate::file_check::start(&hub, req)?;
            crate::file_check::ended(&s).await;
            let report = s.inner.lock().report.clone().unwrap_or_default();
            Ok(print_report(&report, json_out))
        }
        "pull" => {
            let url = pos.first().ok_or("pull needs a source: rtsp://…, ndi://… or camera://…")?;
            let hub = crate::hub::Hub::new(cfg);
            let req = crate::pull::PullRequest {
                url: url.to_string(),
                key: flag_value(args, "--key"),
                profile: flag_value(args, "--profile"),
                for_s: flag_value(args, "--for").and_then(|f| f.parse().ok()),
            };
            let s = crate::pull::start(&hub, req).await?;
            eprintln!("pulling {url} as session {} (Ctrl-C ends it)", s.id);
            tokio::select! {
                _ = crate::file_check::ended(&s) => {}
                _ = tokio::signal::ctrl_c() => {
                    crate::pull::stop(&hub, &s, "stopped with Ctrl-C".into()).await;
                    crate::file_check::ended(&s).await;
                }
            }
            let report = s.inner.lock().report.clone().unwrap_or_default();
            Ok(print_report(&report, json_out))
        }
        other => Err(format!("unknown command '{other}'")),
    }
}

/// Runs a command that talks to a running Stream Check. Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let c = Client::new(args);
    let cmd = args[0].as_str();
    let pos = positional(args, 1);
    let result: Result<i32, String> = (|| match cmd {
        "addresses" => {
            let s = c.json("/api/status")?;
            let a = &s["addresses"];
            for h in a["hosts"].as_array().into_iter().flatten() {
                println!(
                    "{} ({}): UI {}",
                    h["label"].as_str().unwrap_or(""),
                    h["host"].as_str().unwrap_or(""),
                    h["ui"].as_str().unwrap_or("")
                );
                for st in h["streams"].as_array().into_iter().flatten() {
                    println!(
                        "  {:<8} RTMP {}  key {}",
                        st["platform"].as_str().unwrap_or(""),
                        st["rtmp"]["server"].as_str().unwrap_or(""),
                        st["key"].as_str().unwrap_or("")
                    );
                    if let Some(u) = st["rtmps"]["server"].as_str() {
                        println!("  {:<8} RTMPS {u}", "");
                    }
                    println!("  {:<8} SRT  {}", "", st["srt"].as_str().unwrap_or(""));
                    println!("  {:<8} WHIP {}", "", st["whip"].as_str().unwrap_or(""));
                }
            }
            Ok(0)
        }
        "list" => {
            let v = c.json("/api/sessions")?;
            for s in v.as_array().into_iter().flatten() {
                println!(
                    "{}  {:<6} {:<5} {:.0}s  {}",
                    s["id"].as_str().unwrap_or("?"),
                    s["state"].as_str().unwrap_or("?"),
                    s["protocols"].as_array().map_or(String::new(), |p| p.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("+")),
                    s["duration_s"].as_f64().unwrap_or(0.0),
                    if s["violations"].as_array().is_some_and(|v| v.iter().any(|x| x["active"] == true)) { "RULES BROKEN" } else { "" }
                );
            }
            Ok(0)
        }
        "report" => {
            let id = pos.first().ok_or("report needs a session id, a key or 'latest'")?;
            let report = match c.json(&format!("/api/sessions/{id}/report")) {
                Ok(r) => r,
                Err(e) => {
                    // not running: the files
                    let dir = c.data.join("sessions");
                    let mut names: Vec<String> = std::fs::read_dir(&dir)
                        .map_err(|_| e.clone())?
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    let hit = if id.as_str() == "latest" {
                        names.last().cloned()
                    } else {
                        names.iter().rev().find(|n| n.contains(id.as_str())).cloned()
                    };
                    let n = hit.ok_or(e)?;
                    let text = std::fs::read_to_string(dir.join(n).join("report.json")).map_err(|e| e.to_string())?;
                    serde_json::from_str(&text).map_err(|e| e.to_string())?
                }
            };
            if args.iter().any(|a| a == "--summary") {
                print!("{}", summary_text(&report));
            } else {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            }
            Ok(if report["ok"].as_bool() == Some(false) { 3 } else { 0 })
        }
        "wait-for" => {
            let key = pos.first().ok_or("wait-for needs a stream key")?;
            let timeout: f64 = flag_value(args, "--timeout").and_then(|t| t.parse().ok()).unwrap_or(30.0);
            let min_s: f64 = flag_value(args, "--min-seconds").and_then(|t| t.parse().ok()).unwrap_or(0.0);
            let (live, ended) = (args.iter().any(|a| a == "--live"), args.iter().any(|a| a == "--ended"));
            let start = Instant::now();
            loop {
                let v = c.json("/api/sessions")?;
                let hit = v.as_array().into_iter().flatten().find(|s| {
                    s["key"].as_str() == Some(key.as_str())
                        && (!ended || s["state"] == "ended")
                        && (ended || s["state"] != "ended" || !live)
                });
                if let Some(s) = hit {
                    let flowing =
                        s["stats"]["video"]["frames"].as_u64().unwrap_or(0) + s["stats"]["audio"]["frames"].as_u64().unwrap_or(0) > 0;
                    let ok = if ended {
                        s["state"] == "ended"
                    } else if live {
                        s["state"] == "live" && flowing && s["duration_s"].as_f64().unwrap_or(0.0) >= min_s
                    } else {
                        true
                    };
                    if ok {
                        print!("{}", summary_text(s));
                        println!("{}", json!({ "id": s["id"], "state": s["state"] }));
                        return Ok(0);
                    }
                }
                if start.elapsed().as_secs_f64() > timeout {
                    eprintln!(
                        "timed out after {timeout} s waiting for '{key}'{}",
                        if ended {
                            " to end"
                        } else if live {
                            " to be live"
                        } else {
                            ""
                        }
                    );
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        "events" => {
            let id = pos.first().ok_or("events needs a session id or key")?;
            let q = flag_value(args, "--kind").map_or(String::new(), |k| format!("?kind={k}"));
            let v = c.json(&format!("/api/sessions/{id}/events{q}"))?;
            for e in v.as_array().into_iter().flatten() {
                println!(
                    "{:>8.1}s {:<5} {:<14} {}",
                    e["t"].as_f64().unwrap_or(0.0),
                    e["level"].as_str().unwrap_or(""),
                    e["kind"].as_str().unwrap_or(""),
                    e["msg"].as_str().unwrap_or("")
                );
            }
            Ok(0)
        }
        "fault" => {
            let target = pos.first().ok_or("fault needs a key, a session id or *")?;
            let what = pos
                .get(1)
                .ok_or("fault needs an action: drop, refuse <s>, freeze <s>, throttle <kbps>, latency <ms>, loss <pct> or clear")?;
            let n: Option<f64> = pos.get(2).and_then(|x| x.parse().ok());
            let need = |n: Option<f64>| n.ok_or(format!("'{what}' needs a number"));
            let mut body = match what.as_str() {
                "drop" => json!({ "drop": true }),
                "refuse" => json!({ "refuse_s": need(n)? }),
                "freeze" => json!({ "freeze_s": need(n)? }),
                "throttle" => json!({ "throttle_kbps": need(n)? as u32 }),
                "latency" => json!({ "latency_ms": need(n)? as u32 }),
                "loss" => json!({ "loss_pct": need(n)? }),
                "clear" => json!({ "clear": true }),
                other => return Err(format!("unknown fault '{other}'")),
            };
            if let Some(f) = flag_value(args, "--for").and_then(|f| f.parse::<f64>().ok()) {
                body["for_s"] = json!(f);
            }
            let (code, text) = c.post(&format!("/api/faults/{target}"), &body)?;
            println!("{text}");
            Ok(if code < 400 { 0 } else { 1 })
        }
        "end" => {
            let id = pos.first().ok_or("end needs a session id or key")?;
            let (code, text) = c.post(&format!("/api/sessions/{id}/end"), &json!({}))?;
            println!("{text}");
            Ok(if code < 400 { 0 } else { 1 })
        }
        _ => Err(format!("unknown command '{cmd}'\n\n{USAGE}")),
    })();
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_options_parse() {
        let a: Vec<String> = ["--http-port", "9999", "--no-rtmps", "--accept-key", "twitch=live_x", "--grace", "5", "--any-key"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c = serve_config(&a).unwrap();
        assert_eq!((c.http_port, c.rtmps_port, c.grace_override, c.lenient_keys), (9999, None, Some(5), true));
        assert_eq!(c.accept_keys["twitch"], vec!["live_x"]);
        assert!(serve_config(&["--nope".to_string()]).is_err());
        assert!(serve_config(&["--srt-passphrase".into(), "short".into()]).is_err());
    }

    #[test]
    fn a_summary_reads_a_report() {
        let r = json!({"id": "x", "platform": "twitch", "key": "k", "ok": false, "problems": ["rule broken: keyframe-interval"], "duration_s": 12.0, "stats": {"video": {"codec": "h264", "fps": 30.0, "kbps": 3000.0, "sps": {"profile": "High", "level": "4.1", "width": 1920, "height": 1080}}, "audio": {}, "av": {}, "stalls": {"count": 0}, "levels": {}}, "reconnects": 0, "grace_uses": 0, "violations": []});
        let t = summary_text(&r);
        assert!(t.contains("H264 High level 4.1, 1920x1080") && t.contains("PROBLEM"), "{t}");
    }
}
