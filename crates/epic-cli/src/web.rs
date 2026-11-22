//! `epicd serve`: a dependency-free local web UI.
//!
//! One thread per connection, HTTP/1.1, bound to `127.0.0.1` only. It serves a single embedded
//! page plus a small JSON API; there is no framework, no npm and no extra crate.
//!
//! Security posture: loopback bind, and every `/api/*` call must carry the per-run token that is
//! injected into the page. Nothing leaves the machine.

use epic_core::api::{Api, GameEntry};
use epic_core::auth;
use epic_core::config::Config;
use epic_core::download::Progress;
use epic_core::error::{Error, Result};
use epic_core::http::Client;
use epic_core::install::{self, InstallOptions, InstallOutcome};
use epic_core::library;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PAGE: &str = include_str!("web/index.html");

struct State {
    login: Mutex<LoginState>,
    job: Mutex<JobState>,
    games: Mutex<Vec<GameEntry>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
}

impl Default for State {
    fn default() -> State {
        State {
            login: Mutex::new(LoginState::default()),
            job: Mutex::new(JobState::default()),
            games: Mutex::new(Vec::new()),
            cancel: Mutex::new(None),
        }
    }
}

#[derive(Default)]
struct LoginState {
    stage: String,
    user_code: String,
    url: String,
    error: String,
}

#[derive(Default, Clone)]
struct JobState {
    running: bool,
    finished: bool,
    app: String,
    title: String,
    error: String,
    summary: String,
    progress: ProgressSnapshot,
}

#[derive(Default, Clone)]
struct ProgressSnapshot {
    chunks_total: usize,
    chunks_done: usize,
    bytes_total: u64,
    bytes_done: u64,
    inflight: usize,
    peak_inflight: usize,
    speed_bps: f64,
    eta_secs: f64,
}

impl From<&Progress> for ProgressSnapshot {
    fn from(p: &Progress) -> Self {
        ProgressSnapshot {
            chunks_total: p.chunks_total,
            chunks_done: p.chunks_done,
            bytes_total: p.bytes_total,
            bytes_done: p.bytes_done,
            inflight: p.inflight,
            peak_inflight: p.peak_inflight,
            speed_bps: p.speed_bps,
            eta_secs: p.eta_secs.unwrap_or(0.0),
        }
    }
}

/// Run the UI server until the process exits.
pub fn serve(cfg: &Config, port: u16, open_browser: bool) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| Error::net(format!("cannot bind 127.0.0.1:{port}: {e}")))?;
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let token = random_token();
    let state = Arc::new(State::default());

    {
        let state = state.clone();
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            if let Ok(games) = load_games(&cfg) {
                *state.games.lock().unwrap() = games;
            }
        });
    }

    let url = format!("http://127.0.0.1:{actual_port}/?t={token}");
    println!("epicd web ui: {url}");
    println!("press Ctrl-C to stop");
    if open_browser {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .spawn();
    }

    for stream in listener.incoming().flatten() {
        let state = state.clone();
        let token = token.clone();
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, &state, &token, &cfg);
        });
    }
    Ok(())
}

fn random_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}", nanos ^ (std::process::id() as u64).rotate_left(17))
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn html(body: String) -> Response {
        Response { status: 200, content_type: "text/html; charset=utf-8", body: body.into_bytes() }
    }

    fn json(value: serde_json::Value) -> Response {
        Response::json_status(200, value)
    }

    fn json_status(status: u16, value: serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn write_to(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        let status_text = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        };
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            self.status,
            status_text,
            self.content_type,
            self.body.len()
        )?;
        stream.write_all(&self.body)?;
        stream.flush()
    }
}

fn handle(stream: TcpStream, state: &Arc<State>, token: &str, cfg: &Config) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length.min(64 * 1024)];
    if !body.is_empty() {
        reader.read_exact(&mut body)?;
    }

    let (path, query) = match raw_path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (raw_path.clone(), String::new()),
    };
    let params = parse_query(&query);

    let response = if path == "/" || path == "/index.html" {
        Response::html(PAGE.replace("__TOKEN__", token))
    } else if path.starts_with("/api/") {
        if params.get("t").map(String::as_str) != Some(token) {
            Response::json_status(403, serde_json::json!({ "error": "invalid token" }))
        } else {
            dispatch(&method, &path, &body, state, cfg)
        }
    } else {
        Response::json_status(404, serde_json::json!({ "error": "not found" }))
    };

    response.write_to(&mut stream.try_clone()?)
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        map.insert(percent_decode(key), percent_decode(value));
    }
    map
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&text[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn dispatch(method: &str, path: &str, body: &[u8], state: &Arc<State>, cfg: &Config) -> Response {
    match (method, path) {
        ("GET", "/api/state") => Response::json(state_json(state)),
        ("GET", "/api/progress") => {
            let job = state.job.lock().unwrap().clone();
            Response::json(job_json(&job))
        }
        ("GET", "/api/library") => Response::json(library_json(state)),
        ("POST", "/api/library/refresh") => match load_games(cfg) {
            Ok(games) => {
                let count = games.len();
                *state.games.lock().unwrap() = games;
                Response::json(serde_json::json!({ "ok": true, "count": count }))
            }
            Err(e) => Response::json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        },
        ("POST", "/api/login") => {
            start_login(cfg, state);
            Response::json(serde_json::json!({ "ok": true }))
        }
        ("POST", "/api/install") => {
            let request: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            match start_install(cfg, state, &request) {
                Ok(()) => Response::json(serde_json::json!({ "ok": true })),
                Err(e) => Response::json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            }
        }
        ("POST", "/api/cancel") => {
            // The web page has only a stop button; keep the partial download resumable.
            install::request_stop(install::StopKind::Pause);
            Response::json(serde_json::json!({ "ok": true }))
        }
        _ => Response::json_status(404, serde_json::json!({ "error": "unknown endpoint" })),
    }
}

fn job_json(job: &JobState) -> serde_json::Value {
    let p = &job.progress;
    serde_json::json!({
        "running": job.running,
        "finished": job.finished,
        "app": job.app,
        "title": job.title,
        "error": job.error,
        "summary": job.summary,
        "chunks_total": p.chunks_total,
        "chunks_done": p.chunks_done,
        "bytes_total": p.bytes_total,
        "bytes_done": p.bytes_done,
        "inflight": p.inflight,
        "peak_inflight": p.peak_inflight,
        "speed_bps": p.speed_bps,
        "eta_secs": p.eta_secs,
    })
}

fn library_json(state: &Arc<State>) -> serde_json::Value {
    let games = state.games.lock().unwrap().clone();
    let view = library::library_view(&games);
    serde_json::json!({
        "count": view.games.len(),
        "dlc_count": view.dlc_count,
        "games": view.games,
        "orphan_dlc": view.orphan_dlc,
    })
}

fn state_json(state: &Arc<State>) -> serde_json::Value {
    let accounts = auth::accounts().unwrap_or_default();
    let current = auth::load_tokens().ok().flatten();
    let view = library::library_view(&state.games.lock().unwrap().clone());
    let game_count = view.games.len();
    let dlc_count = view.dlc_count;
    let job = state.job.lock().unwrap().clone();
    let login = state.login.lock().unwrap();
    serde_json::json!({
        "logged_in": current.is_some(),
        "display_name": current.as_ref().map(|t| t.display_name.clone()).unwrap_or_default(),
        "accounts": accounts.len(),
        "games": game_count,
        "dlc": dlc_count,
        "orphan_dlc": view.orphan_dlc.len(),
        "login": {
            "stage": login.stage,
            "user_code": login.user_code,
            "url": login.url,
            "error": login.error,
        },
        "job": job_json(&job),
    })
}

fn load_games(cfg: &Config) -> Result<Vec<GameEntry>> {
    if let Ok(cached) = install::load_cached_games() {
        if !cached.is_empty() {
            return Ok(cached);
        }
    }
    let client = Client::new();
    let tokens = auth::load_tokens()?.ok_or_else(|| Error::Auth("not logged in".into()))?;
    let tokens = auth::ensure_fresh(cfg, &client, tokens)?;
    let api = Api::new(&client, cfg, &tokens.access_token);
    let games = api.games(&tokens.account_id)?;
    let _ = install::cache_games(&games);
    Ok(games)
}

fn start_login(cfg: &Config, state: &Arc<State>) {
    {
        let mut login = state.login.lock().unwrap();
        *login = LoginState { stage: "starting".into(), ..Default::default() };
    }
    let cfg = cfg.clone();
    let state = state.clone();
    std::thread::spawn(move || {
        let client = Client::new();
        let prompt = match auth::request_device_code(&cfg, &client) {
            Ok(prompt) => prompt,
            Err(e) => {
                let mut login = state.login.lock().unwrap();
                login.stage = "failed".into();
                login.error = e.to_string();
                return;
            }
        };
        {
            let mut login = state.login.lock().unwrap();
            login.stage = "waiting".into();
            login.user_code = prompt.user_code.clone();
            login.url = if prompt.verification_uri_complete.is_empty() {
                prompt.verification_uri.clone()
            } else {
                prompt.verification_uri_complete.clone()
            };
        }
        match auth::poll_device_code(&cfg, &client, &prompt, |_| {}) {
            Ok(tokens) => {
                if let Err(e) = auth::save_tokens(&tokens) {
                    let mut login = state.login.lock().unwrap();
                    login.stage = "failed".into();
                    login.error = e.to_string();
                    return;
                }
                state.login.lock().unwrap().stage = "done".into();
                if let Ok(games) = load_games(&cfg) {
                    *state.games.lock().unwrap() = games;
                }
            }
            Err(e) => {
                let mut login = state.login.lock().unwrap();
                login.stage = "failed".into();
                login.error = e.to_string();
            }
        }
    });
}

fn start_install(cfg: &Config, state: &Arc<State>, request: &serde_json::Value) -> Result<()> {
    if state.job.lock().unwrap().running {
        return Err(Error::usage("an install is already running"));
    }
    let app = request
        .get("app")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::usage("missing app"))?
        .to_string();
    let downloads = request
        .get("concurrency")
        .and_then(|v| v.as_u64())
        .unwrap_or(cfg.concurrency.downloads as u64) as usize;
    let dir = request.get("dir").and_then(|v| v.as_str()).map(str::to_string);
    let include_dlc = request
        .get("include_dlc")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut concurrency = cfg.concurrency.clone();
    concurrency.downloads = downloads;
    concurrency.validate()?;

    let games = state.games.lock().unwrap().clone();
    let view = library::library_view(&games);
    let selection = view
        .select(&app)
        .ok_or_else(|| Error::usage(format!("unknown app {app}")))?;
    if selection.orphan {
        return Err(Error::usage(format!(
            "{} is DLC but its main game is not in the library",
            selection.selected.title
        )));
    }
    let game = selection.base.clone();
    let dlcs = if include_dlc { selection.dlc.clone() } else { Vec::new() };

    let install_dir = match dir {
        Some(dir) => std::path::PathBuf::from(dir),
        None => {
            let root = std::path::Path::new(&cfg.install_root);
            install::find_existing_install(root, &game.app_name)
                .unwrap_or_else(|| install::default_install_dir(root, &game))
        }
    };

    let cancel = Arc::new(AtomicBool::new(false));
    *state.cancel.lock().unwrap() = Some(cancel.clone());
    // The install workers learn about a stop through this registration.
    install::set_cancel_flag(cancel);
    {
        let mut job = state.job.lock().unwrap();
        *job = JobState {
            running: true,
            app: game.app_name.clone(),
            title: game.title.clone(),
            ..Default::default()
        };
    }

    let cfg = cfg.clone();
    let state = state.clone();
    std::thread::spawn(move || {
        let client = Client::new();
        let result: Result<InstallOutcome> = (|| {
            let tokens = auth::load_tokens()?.ok_or_else(|| Error::Auth("not logged in".into()))?;
            let tokens = auth::ensure_fresh(&cfg, &client, tokens)?;
            let api = Api::new(&client, &cfg, &tokens.access_token);
            let options = InstallOptions {
                install_dir: install_dir.clone(),
                concurrency,
                limit_bytes_per_sec: None,
                verify: true,
                repair: true,
                register_launcher: cfg.register_launcher,
            };
            let progress_state = state.clone();
            let callback = Arc::new(move |p: Progress| {
                progress_state.job.lock().unwrap().progress = ProgressSnapshot::from(&p);
            }) as Arc<dyn Fn(Progress) + Send + Sync>;
            install::install_bundle(&client, &api, &game, &dlcs, &options, Some(callback))
        })();

        let mut job = state.job.lock().unwrap();
        job.running = false;
        job.finished = true;
        match result {
            Ok(outcome) => {
                let dlc = if outcome.dlc_apps.is_empty() {
                    String::new()
                } else {
                    format!(", {} DLC", outcome.dlc_apps.len())
                };
                job.summary = format!(
                    "{} files{dlc}, {} chunks downloaded, peak inflight {}",
                    outcome.files, outcome.chunks_downloaded, outcome.peak_inflight
                );
            }
            Err(e) => job.error = e.to_string(),
        }
    });

    Ok(())
}
