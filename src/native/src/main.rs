// smartpc-native: local backend sidecar for the Smart PC Electron app.
//
// It serves a small JSON API on 127.0.0.1 (see api.rs), stores data in a
// local SQLite file, and speaks to heavy native libs (enigo, whisper-rs…)
// in later iterations — all in-process here, out of Electron's way.
//
// Wiring: Electron spawns this binary with --port/--token/--db, waits for
// the READY line on stdout, and hands url+token to the renderer bridge.
// The per-launch token gates every /v1/* route; /health stays open.
mod ai;
mod api;
mod auth;
mod chat;
mod diagnostics;
mod harness;
mod pi;
mod platform;
mod secrets;
mod stt;
mod tts;

use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::auth::store::Store;

fn print_usage() {
    eprintln!("usage: smartpc-native --port <n|0=auto> --token <min-16-chars> [--db <path>]");
}

struct Args {
    port: u16,
    token: Option<String>,
    db: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        port: 0,
        token: None,
        db: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => {
                args.port = it
                    .next()
                    .ok_or("--port needs a value")?
                    .parse()
                    .map_err(|_| "bad --port (want 0-65535)")?;
            }
            "--token" => {
                args.token = Some(it.next().ok_or("--token needs a value")?);
            }
            "--db" => {
                args.db = Some(it.next().ok_or("--db needs a value")?);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            print_usage();
            std::process::exit(2);
        }
    };
    let token = match args.token {
        Some(t) if t.len() >= 16 => t,
        _ => {
            eprintln!("error: --token is required (min 16 chars, random per launch)");
            print_usage();
            std::process::exit(2);
        }
    };
    let db_path = args.db.unwrap_or_else(|| "smartpc-native.db".into());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Per-launch JWT secret derived from the sidecar token: short-lived
    // access tokens die with the process, the refresh chain re-issues them.
    // Nothing secret ever travels on the command line except the token
    // itself (same trust domain: the spawning Electron app).
    let mut hasher = Sha256::new();
    hasher.update(b"smartpc-native-jwt-v1:");
    hasher.update(token.as_bytes());
    let jwt_secret = hasher.finalize().to_vec();

    let store = Store::open(&db_path)?;
    let chat_store = crate::chat::store::ChatStore::open(&db_path)?;
    // Whisper models live next to the db (<db-dir>/models), overridable
    // with WHISPER_MODEL_DIR. No new CLI flag: Electron already passes --db.
    let data_dir = std::path::Path::new(&db_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let models_dir = std::env::var("WHISPER_MODEL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("models"));
    // pi system prompt baked once per boot; per-turn facts ride in the
    // message (see turn_context). A missing prompt file fails pi turns
    // loudly at spawn, never silently.
    let prompt_path = data_dir.join("pi-system-prompt.txt");
    {
        let prompt = crate::harness::prompt::static_prompt(
            &crate::harness::context::gather(),
        );
        if let Err(e) = std::fs::write(&prompt_path, prompt) {
            eprintln!("warning: pi system prompt unwritable: {e}");
        }
    }
    let bridge_path = match crate::pi::supervisor::resolve_bridge() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warning: {e}");
            std::path::PathBuf::from("pi-bridge/smartpc.ts (missing — set PI_BRIDGE)")
        }
    };

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await?;
    let port = listener.local_addr()?.port();
    let pi = crate::pi::PiSupervisor::new(crate::pi::supervisor::PiConfig {
        bridge_path,
        data_dir: data_dir.clone(),
        system_prompt_path: prompt_path,
        sidecar_url: format!("http://127.0.0.1:{port}"),
        sidecar_token: token.clone(),
        tool_allowlist: crate::harness::tools::catalog()
            .iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
            .join(","),
    });
    let state = api::AppState {
        store: Arc::new(Mutex::new(store)),
        chat: Arc::new(Mutex::new(chat_store)),
        voice: crate::stt::VoiceService::new(models_dir),
        tts: crate::tts::TtsManager::new(data_dir.clone(), crate::tts::resolve_voice_ext()),
        pi,
        jwt_secret: Arc::new(jwt_secret),
        access_ttl_secs: 15 * 60,
        refresh_ttl_secs: 30 * 24 * 3600,
        sidecar_token: Arc::new(token),
    };

    // READY is the spawn contract with Electron: one line, then flush.
    println!("READY port={port}");
    {
        use std::io::Write as _;
        std::io::stdout().flush()?;
    }

    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
