//! Tauri shell for Skiff.
//!
//! All logic lives in `skiff-core`; this crate only exposes commands and keeps the small amount of
//! UI state (login progress, current job, cached game list). The frontend polls `state` on a timer
//! instead of using events, which keeps the IPC surface to five commands.

use skiff_core::api::{Api, GameEntry};
use skiff_core::auth;
use skiff_core::config::Config;
use skiff_core::download::Progress;
use skiff_core::http::Client;
use skiff_core::install::{self, InstallOptions, InstallOutcome};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, State};

#[derive(Default)]
pub struct LoginInfo {
    stage: String,
    user_code: String,
    url: String,
    error: String,
}

#[derive(Default, Clone)]
pub struct JobInfo {
    running: bool,
    finished: bool,
    app: String,
    title: String,
    error: String,
    summary: String,
    chunks_total: usize,
    chunks_done: usize,
    bytes_total: u64,
    bytes_done: u64,
    inflight: usize,
    peak_inflight: usize,
    speed_bps: f64,
    eta_secs: f64,
}

impl JobInfo {
    fn from_progress(progress: &Progress) -> JobInfo {
        JobInfo {
            chunks_total: progress.chunks_total,
            chunks_done: progress.chunks_done,
            bytes_total: progress.bytes_total,
            bytes_done: progress.bytes_done,
            inflight: progress.inflight,
            peak_inflight: progress.peak_inflight,
            speed_bps: progress.speed_bps,
            eta_secs: progress.eta_secs.unwrap_or(0.0),
            ..Default::default()
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "running": self.running,
            "finished": self.finished,
            "app": self.app,
            "title": self.title,
            "error": self.error,
            "summary": self.summary,
            "chunks_total": self.chunks_total,
            "chunks_done": self.chunks_done,
            "bytes_total": self.bytes_total,
            "bytes_done": self.bytes_done,
            "inflight": self.inflight,
            "peak_inflight": self.peak_inflight,
            "speed_bps": self.speed_bps,
            "eta_secs": self.eta_secs,
        })
    }
}

pub struct UiState {
    login: Mutex<LoginInfo>,
    job: Mutex<JobInfo>,
    games: Mutex<Vec<GameEntry>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
}

impl Default for UiState {
    fn default() -> UiState {
        UiState {
            login: Mutex::new(LoginInfo::default()),
            job: Mutex::new(JobInfo::default()),
            games: Mutex::new(Vec::new()),
            cancel: Mutex::new(None),
        }
    }
}

fn load_games(cfg: &Config) -> skiff_core::Result<Vec<GameEntry>> {
    if let Ok(cached) = install::load_cached_games() {
        if !cached.is_empty() {
            return Ok(cached);
        }
    }
    let client = Client::new();
    let tokens = auth::load_tokens()?
        .ok_or_else(|| skiff_core::Error::Auth("not logged in".into()))?;
    let tokens = auth::ensure_fresh(cfg, &client, tokens)?;
    let api = Api::new(&client, cfg, &tokens.access_token);
    let games = api.games(&tokens.account_id)?;
    let _ = install::cache_games(&games);
    Ok(games)
}

#[tauri::command]
fn state(state: State<'_, Arc<UiState>>) -> Value {
    let cfg = Config::load().unwrap_or_default();
    let current = auth::load_tokens().ok().flatten();
    let game_count = state.games.lock().unwrap().len();
    let job = state.job.lock().unwrap().to_json();
    let login = state.login.lock().unwrap();
    json!({
        "logged_in": current.is_some(),
        "display_name": current.as_ref().map(|t| t.display_name.clone()).unwrap_or_default(),
        "games": game_count,
        "install_root": cfg.install_root,
        "default_concurrency": cfg.concurrency.downloads,
        "platform": cfg.platform,
        "login": {
            "stage": login.stage,
            "user_code": login.user_code,
            "url": login.url,
            "error": login.error,
        },
        "job": job,
    })
}

#[tauri::command]
fn library(state: State<'_, Arc<UiState>>) -> Value {
    let games = state.games.lock().unwrap().clone();
    json!({ "games": games })
}

#[tauri::command]
fn library_refresh(state: State<'_, Arc<UiState>>) -> std::result::Result<Value, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    match load_games(&cfg) {
        Ok(games) => {
            let count = games.len();
            *state.games.lock().unwrap() = games;
            Ok(json!({ "ok": true, "count": count }))
        }
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
fn login_start(state: State<'_, Arc<UiState>>) -> std::result::Result<Value, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    {
        let mut login = state.login.lock().unwrap();
        *login = LoginInfo { stage: "starting".into(), ..Default::default() };
    }
    let shared = state.inner().clone();
    std::thread::spawn(move || {
        let client = Client::new();
        let prompt = match auth::request_device_code(&cfg, &client) {
            Ok(prompt) => prompt,
            Err(e) => {
                let mut login = shared.login.lock().unwrap();
                login.stage = "failed".into();
                login.error = e.to_string();
                return;
            }
        };
        {
            let mut login = shared.login.lock().unwrap();
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
                    let mut login = shared.login.lock().unwrap();
                    login.stage = "failed".into();
                    login.error = e.to_string();
                    return;
                }
                shared.login.lock().unwrap().stage = "done".into();
                if let Ok(games) = load_games(&cfg) {
                    *shared.games.lock().unwrap() = games;
                }
            }
            Err(e) => {
                let mut login = shared.login.lock().unwrap();
                login.stage = "failed".into();
                login.error = e.to_string();
            }
        }
    });
    Ok(json!({ "ok": true }))
}

/// Log in with a browser `authorizationCode` pasted by the user.
///
/// This is the flow Epic actually allows for the launcher client: the device-code grant is
/// rejected with HTTP 401 for it, so the GUI needs this path to be self-sufficient.
/// Open a URL in the user's default browser.
///
/// WebView2 swallows `target="_blank"` links, so external navigation has to be explicit. The host
/// allow-list keeps a compromised page from launching arbitrary local handlers.
#[tauri::command]
fn open_url(url: String) -> std::result::Result<(), String> {
    let allowed = url.starts_with("https://www.epicgames.com/")
        || url.starts_with("https://epicgames.com/")
        || url.starts_with("https://www.epicgames.dev/");
    if !allowed {
        return Err(format!("refusing to open unexpected URL: {url}"));
    }
    std::process::Command::new("cmd")
        .args(["/c", "start", "", &url])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}
#[tauri::command]
fn login_code(state: State<'_, Arc<UiState>>, code: String) -> std::result::Result<Value, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    auth::ensure_store_writable().map_err(|e| e.to_string())?;
    let client = Client::new();

    // Accept a bare code or a whole pasted JSON blob.
    let raw = code.trim();
    let trimmed = if let Some(pos) = raw.find("authorizationCode") {
        let rest = &raw[pos + "authorizationCode".len()..];
        rest.split_once(':')
            .and_then(|(_, after)| after.split('"').nth(1))
            .unwrap_or(raw)
            .to_string()
    } else {
        raw.trim_matches('"').to_string()
    };
    if trimmed.is_empty() {
        return Err("请先粘贴 authorizationCode".to_string());
    }

    let tokens = auth::login_with_auth_code(&cfg, &client, &trimmed).map_err(|e| e.to_string())?;
    auth::save_tokens(&tokens).map_err(|e| e.to_string())?;
    state.login.lock().unwrap().stage = "done".into();

    let shared = state.inner().clone();
    std::thread::spawn(move || {
        if let Ok(games) = load_games(&cfg) {
            *shared.games.lock().unwrap() = games;
        }
    });

    Ok(json!({ "ok": true, "display_name": tokens.display_name }))
}
#[tauri::command]
fn install_game(
    app_handle: tauri::AppHandle,
    state: State<'_, Arc<UiState>>,
    app: String,
    concurrency: usize,
    dir: Option<String>,
) -> std::result::Result<Value, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    if state.job.lock().unwrap().running {
        return Err("已有下载任务在运行".into());
    }
    let mut concurrency_cfg = cfg.concurrency.clone();
    concurrency_cfg.downloads = concurrency;
    concurrency_cfg.validate().map_err(|e| e.to_string())?;

    let game = state
        .games
        .lock()
        .unwrap()
        .iter()
        .find(|g| g.app_name.eq_ignore_ascii_case(&app))
        .cloned()
        .ok_or_else(|| format!("未知游戏 {app}"))?;

    let install_dir = match dir {
        Some(dir) if !dir.trim().is_empty() => std::path::PathBuf::from(dir),
        _ => install::default_install_dir(std::path::Path::new(&cfg.install_root), &game),
    };

    let cancel = Arc::new(AtomicBool::new(false));
    *state.cancel.lock().unwrap() = Some(cancel.clone());
    {
        let mut job = state.job.lock().unwrap();
        *job = JobInfo {
            running: true,
            app: game.app_name.clone(),
            title: game.title.clone(),
            ..Default::default()
        };
    }

    let install_dir_thread = install_dir.clone();
    let shared = state.inner().clone();
    let handle = app_handle.clone();
    std::thread::spawn(move || {
        let client = Client::new();
        let result: skiff_core::Result<InstallOutcome> = (|| {
            let tokens = auth::load_tokens()?
                .ok_or_else(|| skiff_core::Error::Auth("not logged in".into()))?;
            let tokens = auth::ensure_fresh(&cfg, &client, tokens)?;
            let api = Api::new(&client, &cfg, &tokens.access_token);
            let options = InstallOptions {
                install_dir: install_dir_thread.clone(),
                concurrency: concurrency_cfg,
                limit_bytes_per_sec: None,
                verify: true,
                repair: true,
                register_launcher: cfg.register_launcher,
            };
            let progress_state = shared.clone();
            let progress_handle = handle.clone();
            let callback = Arc::new(move |p: Progress| {
                let snapshot = JobInfo::from_progress(&p);
                if let Ok(mut job) = progress_state.job.lock() {
                    job.chunks_total = snapshot.chunks_total;
                    job.chunks_done = snapshot.chunks_done;
                    job.bytes_total = snapshot.bytes_total;
                    job.bytes_done = snapshot.bytes_done;
                    job.inflight = snapshot.inflight;
                    job.peak_inflight = snapshot.peak_inflight;
                    job.speed_bps = snapshot.speed_bps;
                    job.eta_secs = snapshot.eta_secs;
                }
                // Nudge the UI; harmless if the window is gone.
                let _ = progress_handle.emit("skiff://progress", ());
            }) as Arc<dyn Fn(Progress) + Send + Sync>;
            install::install(&client, &api, &game, &options, Some(callback))
        })();

        let mut job = shared.job.lock().unwrap();
        job.running = false;
        job.finished = true;
        match result {
            Ok(outcome) => {
                job.summary = format!(
                    "{} 个文件 · {} 个分块 · 峰值并发 {}",
                    outcome.files, outcome.chunks_downloaded, outcome.peak_inflight
                );
            }
            Err(e) => job.error = e.to_string(),
        }
        drop(job);
        let _ = handle.emit("skiff://progress", ());
    });

    Ok(json!({ "ok": true, "install_dir": install_dir.to_string_lossy() }))
}

#[tauri::command]
fn cancel(state: State<'_, Arc<UiState>>) -> Value {
    if let Some(flag) = state.cancel.lock().unwrap().as_ref() {
        flag.store(true, Ordering::Relaxed);
    }
    json!({ "ok": true })
}

pub fn run() {
    tauri::Builder::default()
        .manage(Arc::new(UiState::default()))
        .invoke_handler(tauri::generate_handler![
            state,
            library,
            library_refresh,
            login_start,
            login_code,
            open_url,
            install_game,
            cancel
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Skiff GUI");
}
