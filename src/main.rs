use stream_check::{app, cli};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(String::as_str).unwrap_or("serve");
    if matches!(first, "-h" | "--help" | "help") {
        print!("{}", cli::USAGE);
        return;
    }
    if matches!(first, "addresses" | "list" | "report" | "wait-for" | "events" | "fault" | "end") {
        let code = tokio::task::spawn_blocking(move || cli::run(&args)).await.unwrap_or(2);
        std::process::exit(code);
    }
    let cfg = match cli::serve_config(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let running = match app::start(cfg.clone()).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Stream Check couldn't start: {e}");
            std::process::exit(1);
        }
    };
    print_banner(&running, &cfg);
    let _ = tokio::signal::ctrl_c().await;
    println!("\nstopping: finishing the sessions and writing their reports…");
    running.hub.kill_all();
    let sessions: Vec<_> = running.hub.sessions.lock().iter().cloned().collect();
    for s in sessions {
        running.hub.finalize(s, "Stream Check stopped".into()).await;
    }
}

fn print_banner(r: &app::Running, cfg: &stream_check::hub::Config) {
    let a = r.hub.addresses.lock().clone();
    println!("Stream Check is running.  Data (recordings, reports): {}", cfg.data_dir.display());
    if !cfg.ffmpeg {
        println!("  (no ffmpeg found: no thumbnails or loudness; ingest, recording and timing still work)");
    }
    for h in a["hosts"].as_array().into_iter().flatten() {
        println!();
        println!("  {}: open {}", h["label"].as_str().unwrap_or(""), h["ui"].as_str().unwrap_or(""));
        let streams = h["streams"].as_array().cloned().unwrap_or_default();
        if let Some(st) = streams.iter().find(|s| s["platform"] == "open") {
            println!(
                "    RTMP   {}/<platform>   (key: any; platforms: twitch, youtube, kick, open)",
                st["rtmp"]["server"].as_str().unwrap_or("").trim_end_matches("/open")
            );
            if let Some(u) = st["rtmps"]["server"].as_str() {
                println!(
                    "    RTMPS  {}/<platform>   (self-signed certificate: {})",
                    u.trim_end_matches("/open"),
                    r.cert.as_ref().map_or(String::new(), |c| c.display().to_string())
                );
            }
            println!("    SRT    {}", st["srt"].as_str().unwrap_or(""));
            println!("    WHIP   {}", st["whip"].as_str().unwrap_or(""));
        }
        if !cfg.lenient_keys {
            for st in streams.iter().filter(|s| s["platform"] != "open") {
                println!("    {:<8} key like {}", st["platform"].as_str().unwrap_or(""), st["key"].as_str().unwrap_or(""));
            }
        }
    }
    println!();
}
