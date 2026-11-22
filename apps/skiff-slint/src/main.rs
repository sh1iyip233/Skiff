// Epic game library downloader — Slint desktop UI.
//
// A real native window with a self-drawn toolkit: no WebView2, no administrator rights, no runtime
// install. All protocol/download logic stays in skiff-core; this file owns the small amount of UI
// state, mirrors it into Slint properties on a timer, and forwards user actions to worker threads.
//
// State flows one way: workers mutate `State`, the timer copies `State` into the Slint properties,
// and the Slint model is rebuilt only when a revision counter changes. That keeps the UI thread free
// of locks around rendering code and means no callback has to marshal its own UI update.

slint::include_modules!();

use skiff_core::api::{Api, GameEntry, StageHook};
use skiff_core::auth;
use skiff_core::config::Config;
use skiff_core::download::Progress;
use skiff_core::http::Client;
use skiff_core::install::{self, InstallOptions};
use skiff_core::library::{self, GameGroup};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Browser page that hands out a one-time `authorizationCode` for the launcher client.
const AUTH_REDIRECT_URL: &str = "https://www.epicgames.com/id/api/redirect?clientId=34a02cf8f4414e29b15921876da36f9a&responseType=code";

/// How often state is copied into the UI. Also drives the indeterminate sweep.
const TICK: Duration = Duration::from_millis(60);

// ---------------------------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------------------------

fn human_bytes(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.2} {}", units[unit])
    }
}

/// `3 分 12 秒` style durations; the progress card has no room for `195.4s`.
fn human_duration(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "—".to_string();
    }
    let total = secs.round() as u64;
    if total >= 3600 {
        format!("{} 小时 {} 分", total / 3600, (total % 3600) / 60)
    } else if total >= 60 {
        format!("{} 分 {} 秒", total / 60, total % 60)
    } else {
        format!("{total} 秒")
    }
}

/// First character of the account name, for the avatar circle.
fn initial_of(name: &str) -> String {
    name.chars()
        .find(|c| !c.is_whitespace())
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// `ID af83ec…56be66` — enough to tell accounts apart without printing the whole id.
fn masked_account(account_id: &str) -> String {
    if account_id.is_ascii() && account_id.len() > 18 {
        format!("ID {}…{}", &account_id[..6], &account_id[account_id.len() - 6..])
    } else if account_id.is_empty() {
        String::new()
    } else {
        format!("ID {account_id}")
    }
}

/// Accept a bare code, a quoted value, or a whole JSON blob pasted from the browser.
fn extract_auth_code(input: &str) -> String {
    let text = input.trim();
    if let Some(pos) = text.find("authorizationCode") {
        let rest = &text[pos + "authorizationCode".len()..];
        if let Some((_, after_colon)) = rest.split_once(':') {
            if let Some(value) = after_colon.split('"').nth(1) {
                return value.trim().to_string();
            }
        }
    }
    text.trim_matches('"').trim().to_string()
}

/// Human label and coarse fraction for a stage id reported by `skiff-core`.
///
/// The core emits UI-neutral ids so the CLI and the web page can ignore them; the wording and the
/// weighting of the bar live here.
fn stage_label(stage: &str) -> (&'static str, f32) {
    match stage {
        skiff_core::api::stage::LIBRARY => ("读取游戏库列表", 0.30),
        skiff_core::api::stage::ENTITLEMENTS => ("读取账号权益", 0.38),
        skiff_core::api::stage::ASSETS => ("读取账号资产", 0.42),
        skiff_core::api::stage::CATALOG => ("解析商店目录", 0.58),
        skiff_core::api::stage::TITLES => ("补全游戏标题", 0.80),
        skiff_core::api::stage::MERGE => ("合并游戏列表", 0.94),
        _ => ("正在读取", 0.12),
    }
}

fn row_of(group: &GameGroup) -> GameRow {
    let game = &group.game;
    let title = if game.title.is_empty() { game.app_name.clone() } else { game.title.clone() };
    GameRow {
        title: SharedString::from(title),
        app: SharedString::from(game.app_name.clone()),
        namespace: SharedString::from(game.namespace.clone()),
        dlc_count: group.dlc.len() as i32,
    }
}

/// Tone of the one-line refresh status, mapped to a colour by the UI.
const TONE_MUTED: i32 = 0;
const TONE_OK: i32 = 1;
const TONE_ERR: i32 = 2;

// ---------------------------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Snapshot {
    // account
    logged_in: bool,
    account_name: String,
    account_initial: String,
    account_id: String,
    // job
    running: bool,
    phase: String,
    phase_kind: i32,
    progress: f32,
    indeterminate: bool,
    percent: String,
    speed: String,
    eta: String,
    bytes: String,
    chunks: String,
    inflight: String,
    elapsed: String,
    retries: String,
    note: String,
    note_error: bool,
    // selection
    selected_title: String,
    selected_app: String,
    target_dir: String,
    /// Number of add-ons attached to the selected main game.
    selected_dlc_count: usize,
    /// Newline-separated add-on titles for the detail pane.
    selected_dlc_text: String,
    /// A paused or interrupted download exists for the selected game.
    can_resume: bool,
    // library refresh
    refresh_busy: bool,
    refresh_progress: f32,
    refresh_detail: String,
    refresh_kind: i32,
    refresh_started: Option<Instant>,
    // dialogs
    login_busy: bool,
    login_error: String,
    config_note: String,
    config_note_error: bool,
    setup_note: String,
    clear_code_input: bool,
}

/// Library cache plus the filtered, grouped view the list model is built from.
#[derive(Default)]
struct Library {
    all: Vec<GameEntry>,
    visible: Vec<GameGroup>,
    query: String,
    revision: u64,
}

impl Library {
    /// Replace the whole set and re-apply the current search.
    fn set_games(&mut self, games: Vec<GameEntry>) {
        self.all = games;
        self.refilter();
    }

    /// Apply a search query (case-insensitive title or app name match).
    fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_string();
        self.refilter();
    }

    fn refilter(&mut self) {
        let needle = self.query.trim().to_lowercase();
        let view = library::library_view(&self.all);
        self.visible = if needle.is_empty() { view.games } else { view.filtered(&needle).games };
        self.revision = self.revision.wrapping_add(1);
    }

    fn base_count(&self) -> usize {
        self.all.iter().filter(|game| !game.is_dlc).count()
    }

    fn dlc_count(&self) -> usize {
        self.all.iter().filter(|game| game.is_dlc).count()
    }

    /// Group selected by its main-game app name.
    fn group_for(&self, app_name: &str) -> Option<&GameGroup> {
        self.visible.iter().find(|group| group.game.app_name == app_name)
    }
}

impl Snapshot {
    /// Idle values, matching the placeholders declared in the `.slint` file.
    fn idle() -> Snapshot {
        Snapshot {
            phase: "空闲".to_string(),
            percent: "0.0%".to_string(),
            speed: "—".to_string(),
            eta: "—".to_string(),
            bytes: "—".to_string(),
            chunks: "—".to_string(),
            inflight: "—".to_string(),
            elapsed: "—".to_string(),
            account_name: "未登录".to_string(),
            account_initial: "?".to_string(),
            account_id: "连接后显示账号名称".to_string(),
            ..Default::default()
        }
    }
}

struct State {
    snapshot: Mutex<Snapshot>,
    library: Mutex<Library>,
    config: Mutex<Config>,
    selected_app: Mutex<Option<String>>,
    /// Set after a successful login so the next "save"/"finish" fetches the new account's library.
    needs_refresh: AtomicBool,
    /// True once the user asked the running install to stop (pause or cancel).
    stopping: AtomicBool,
    /// How the running install was asked to stop (pause keeps files, cancel deletes them).
    stop_kind: Mutex<Option<install::StopKind>>,
}

impl State {
    fn new(config: Config) -> State {
        State {
            snapshot: Mutex::new(Snapshot::idle()),
            library: Mutex::new(Library::default()),
            config: Mutex::new(config),
            selected_app: Mutex::new(None),
            needs_refresh: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            stop_kind: Mutex::new(None),
        }
    }
}

/// Update the snapshot under one short lock.
fn edit(state: &State, f: impl FnOnce(&mut Snapshot)) {
    if let Ok(mut snapshot) = state.snapshot.lock() {
        f(&mut snapshot);
    }
}

// ---------------------------------------------------------------------------------------------
// library fetching
// ---------------------------------------------------------------------------------------------

fn fetch_library(cfg: &Config, hook: StageHook) -> skiff_core::Result<Vec<GameEntry>> {
    let client = Client::new();
    let tokens = auth::load_tokens()?
        .ok_or_else(|| skiff_core::Error::Auth("not logged in".into()))?;
    let tokens = auth::ensure_fresh(cfg, &client, tokens)?;
    let api = Api::new(&client, cfg, &tokens.access_token).with_stage(hook);
    let games = api.games(&tokens.account_id)?;
    // Keep a copy so the next start can show the library immediately.
    let _ = install::cache_games(&games);
    Ok(games)
}

/// Fetch the library in the background, driving the visible refresh progress.
fn start_refresh(state: Arc<State>) {
    {
        let Ok(mut snapshot) = state.snapshot.lock() else { return };
        if snapshot.refresh_busy {
            return;
        }
        if !snapshot.logged_in {
            snapshot.refresh_detail = "请先在设置里连接 Epic 账号".to_string();
            snapshot.refresh_kind = TONE_ERR;
            return;
        }
        snapshot.refresh_busy = true;
        snapshot.refresh_progress = 0.04;
        snapshot.refresh_detail = "正在检查登录状态…".to_string();
        snapshot.refresh_kind = TONE_MUTED;
        snapshot.refresh_started = Some(Instant::now());
    }

    let worker_state = state.clone();
    std::thread::spawn(move || {
        let hook: StageHook = {
            let hook_state = worker_state.clone();
            Arc::new(move |stage: &str| {
                let (label, progress) = stage_label(stage);
                if let Ok(mut snapshot) = hook_state.snapshot.lock() {
                    // Never walk the bar backwards when a fallback path re-reports an earlier stage.
                    snapshot.refresh_progress = snapshot.refresh_progress.max(progress);
                    snapshot.refresh_detail = format!("{label}…");
                }
            })
        };

        let started = worker_state
            .snapshot
            .lock()
            .ok()
            .and_then(|snapshot| snapshot.refresh_started)
            .unwrap_or_else(Instant::now);
        let outcome = Config::load().and_then(|cfg| fetch_library(&cfg, hook));
        let elapsed = started.elapsed().as_secs_f64();

        match outcome {
            Ok(games) => {
                let bases = games.iter().filter(|game| !game.is_dlc).count();
                let dlcs = games.iter().filter(|game| game.is_dlc).count();
                let shown = {
                    let mut library = worker_state.library.lock().unwrap();
                    library.set_games(games);
                    library.visible.len()
                };
                if let Ok(mut snapshot) = worker_state.snapshot.lock() {
                    snapshot.refresh_busy = false;
                    snapshot.refresh_progress = 1.0;
                    snapshot.refresh_detail = if shown == bases {
                        format!("已更新 {bases} 款游戏 · {dlcs} 个 DLC · {elapsed:.1} 秒")
                    } else {
                        format!("已更新 {bases} 款游戏 · {dlcs} 个 DLC，匹配 {shown} 款 · {elapsed:.1} 秒")
                    };
                    snapshot.refresh_kind = TONE_OK;
                    snapshot.refresh_started = None;
                }
            }
            Err(e) => {
                eprintln!("warning: library refresh failed: {e}");
                if let Ok(mut snapshot) = worker_state.snapshot.lock() {
                    snapshot.refresh_busy = false;
                    snapshot.refresh_progress = 0.0;
                    snapshot.refresh_detail = format!("读取游戏库失败：{e}");
                    snapshot.refresh_kind = TONE_ERR;
                    snapshot.refresh_started = None;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// selection
// ---------------------------------------------------------------------------------------------

/// Mirror the selected game (and its install target) into the UI.
fn push_selection(ui: &AppWindow, state: &State) {
    let selected = state
        .selected_app
        .lock()
        .ok()
        .and_then(|slot| slot.clone());

    let Some(app_name) = selected else {
        ui.set_selected(-1);
        ui.set_selected_title(SharedString::default());
        ui.set_selected_app(SharedString::default());
        ui.set_target_dir(SharedString::default());
        ui.set_can_resume(false);
        ui.set_selected_dlc_count(0);
        ui.set_selected_dlc_text(SharedString::default());
        edit(state, |snapshot| {
            snapshot.selected_title.clear();
            snapshot.selected_app.clear();
            snapshot.target_dir.clear();
            snapshot.selected_dlc_count = 0;
            snapshot.selected_dlc_text.clear();
            snapshot.can_resume = false;
        });
        return;
    };

    let group = state
        .library
        .lock()
        .ok()
        .and_then(|library| library.group_for(&app_name).cloned());

    let Some(group) = group else {
        ui.set_selected(-1);
        return;
    };
    let game = &group.game;

    let install_root = ui.get_install_root().to_string();
    let cfg_root = state
        .config
        .lock()
        .map(|cfg| cfg.install_root.clone())
        .unwrap_or_default();
    let base = if install_root.trim().is_empty() {
        PathBuf::from(cfg_root)
    } else {
        PathBuf::from(install_root.trim())
    };
    // Reuse an existing install if the catalog enriched `FolderName` after the game was first
    // installed, otherwise fall back to the normal default directory.
    let target = install::find_existing_install(&base, &game.app_name)
        .unwrap_or_else(|| install::default_install_dir(&base, game));
    let title = if game.title.is_empty() { game.app_name.clone() } else { game.title.clone() };
    let can_resume = install::has_partial_install(&target);
    let dlc_text = group
        .dlc
        .iter()
        .map(|dlc| {
            let name = if dlc.title.is_empty() { &dlc.app_name } else { &dlc.title };
            format!("· {name}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    ui.set_selected_title(SharedString::from(title.clone()));
    ui.set_selected_app(SharedString::from(game.app_name.clone()));
    ui.set_target_dir(SharedString::from(target.to_string_lossy().to_string()));
    ui.set_can_resume(can_resume);
    ui.set_selected_dlc_count(group.dlc.len() as i32);
    ui.set_selected_dlc_text(SharedString::from(dlc_text.clone()));
    edit(state, |snapshot| {
        snapshot.selected_title = title;
        snapshot.selected_app = game.app_name.clone();
        snapshot.target_dir = target.to_string_lossy().to_string();
        snapshot.selected_dlc_count = group.dlc.len();
        snapshot.selected_dlc_text = dlc_text;
        snapshot.can_resume = can_resume;
    });
}

// ---------------------------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------------------------

/// Validate the form, persist it to `config.toml` and update the cached config.
fn apply_settings(ui: &AppWindow, state: &State) -> Result<Config, String> {
    let text = ui.get_concurrency_text().trim().to_string();
    let concurrency: usize = text
        .parse()
        .map_err(|_| "并发线程必须是 1–128 之间的整数".to_string())?;
    if !(1..=128).contains(&concurrency) {
        return Err("并发线程必须在 1–128 之间".to_string());
    }

    let mut cfg = state
        .config
        .lock()
        .map_err(|_| "配置状态不可用".to_string())?
        .clone();
    cfg.concurrency.downloads = concurrency;
    cfg.concurrency.auto = false;

    let root = ui.get_install_root().trim().to_string();
    if !root.is_empty() {
        cfg.install_root = root;
    }
    cfg.register_launcher = ui.get_register_launcher();
    cfg.save().map_err(|e| format!("无法保存配置：{e}"))?;

    if let Ok(mut cached) = state.config.lock() {
        *cached = cfg.clone();
    }
    // Echo the normalized values back into the form.
    ui.set_concurrency_text(SharedString::from(cfg.concurrency.downloads.to_string()));
    ui.set_install_root(SharedString::from(cfg.install_root.clone()));
    Ok(cfg)
}

fn account_display(tokens: &auth::TokenSet) -> (String, String, String) {
    let name = tokens.account_name();
    let display = if name.is_empty() { "已登录".to_string() } else { name };
    (display.clone(), initial_of(&display), masked_account(&tokens.account_id))
}

// ---------------------------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------------------------

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_auth_url(SharedString::from(AUTH_REDIRECT_URL));

    let cfg = Config::load().unwrap_or_default();
    ui.set_concurrency_text(SharedString::from(cfg.concurrency.downloads.to_string()));
    ui.set_install_root(SharedString::from(cfg.install_root.clone()));
    ui.set_register_launcher(cfg.register_launcher);
    let state = Arc::new(State::new(cfg));

    // ---- account -------------------------------------------------------------------------
    match auth::load_tokens() {
        Ok(Some(tokens)) => {
            let (name, initial, masked) = account_display(&tokens);
            ui.set_logged_in(true);
            ui.set_account_name(SharedString::from(name.clone()));
            ui.set_account_initial(SharedString::from(initial.clone()));
            ui.set_account_id(SharedString::from(masked.clone()));
            edit(&state, |snapshot| {
                snapshot.logged_in = true;
                snapshot.account_name = name;
                snapshot.account_initial = initial;
                snapshot.account_id = masked;
            });
        }
        Ok(None) => {
            ui.set_logged_in(false);
            ui.set_account_name(SharedString::from("未登录"));
            ui.set_account_initial(SharedString::from("?"));
            ui.set_account_id(SharedString::from("连接后显示账号名称"));
            ui.set_setup_open(true);
            edit(&state, |snapshot| {
                snapshot.account_name = "未登录".to_string();
                snapshot.account_initial = "?".to_string();
                snapshot.account_id = "连接后显示账号名称".to_string();
            });
        }
        Err(e) => {
            eprintln!("warning: cannot read the stored token: {e}");
            let message = e.to_string();
            ui.set_account_name(SharedString::from("登录状态不可用"));
            ui.set_account_id(SharedString::from(message.clone()));
            ui.set_setup_open(true);
            edit(&state, |snapshot| {
                snapshot.account_name = "登录状态不可用".to_string();
                snapshot.account_id = message;
            });
        }
    }

    // ---- cached library, so the window is useful on the first frame -----------------------
    let mut cached_count = 0usize;
    if let Ok(cached) = install::load_cached_games() {
        if !cached.is_empty() {
            cached_count = cached.len();
            if let Ok(mut library) = state.library.lock() {
                library.set_games(cached);
            }
        }
    }

    // ---- first-run refresh ---------------------------------------------------------------
    let logged_in = state.snapshot.lock().map(|s| s.logged_in).unwrap_or(false);
    if logged_in && cached_count == 0 {
        start_refresh(state.clone());
    }

    // ---- library refresh -----------------------------------------------------------------
    {
        let state = state.clone();
        ui.on_refresh(move || start_refresh(state.clone()));
    }

    // ---- search --------------------------------------------------------------------------
    {
        let state = state.clone();
        ui.on_filter_games(move |text: SharedString| {
            if let Ok(mut library) = state.library.lock() {
                library.set_query(text.as_str());
            }
        });
    }

    // ---- selection -----------------------------------------------------------------------
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_select(move |index: i32| {
            let Some(ui) = ui_weak.upgrade() else { return };
            if index < 0 {
                return;
            }
            let app = state
                .library
                .lock()
                .ok()
                .and_then(|library| {
                    library.visible.get(index as usize).map(|group| group.game.app_name.clone())
                });
            let Some(app) = app else { return };
            if let Ok(mut slot) = state.selected_app.lock() {
                *slot = Some(app);
            }
            ui.set_selected(index);
            push_selection(&ui, &state);
        });
    }

    // ---- authorization code --------------------------------------------------------------
    {
        let state = state.clone();
        ui.on_login_code_submit(move |text: SharedString| {
            let code = extract_auth_code(text.as_str());
            if code.is_empty() {
                edit(&state, |snapshot| {
                    snapshot.login_error = "请先粘贴授权码".to_string();
                });
                return;
            }
            edit(&state, |snapshot| {
                snapshot.login_busy = true;
                snapshot.login_error.clear();
                snapshot.config_note.clear();
                snapshot.config_note_error = false;
                snapshot.setup_note.clear();
            });

            let worker_state = state.clone();
            std::thread::spawn(move || {
                let outcome = (|| -> skiff_core::Result<auth::TokenSet> {
                    let cfg = Config::load()?;
                    auth::ensure_store_writable()?;
                    let client = Client::new();
                    let tokens = auth::login_with_auth_code(&cfg, &client, &code)?;
                    auth::save_tokens(&tokens)?;
                    Ok(tokens)
                })();

                match outcome {
                    Ok(tokens) => {
                        let (name, initial, masked) = account_display(&tokens);
                        edit(&worker_state, |snapshot| {
                            snapshot.logged_in = true;
                            snapshot.account_name = name;
                            snapshot.account_initial = initial;
                            snapshot.account_id = masked;
                            snapshot.login_busy = false;
                            snapshot.login_error.clear();
                            snapshot.clear_code_input = true;
                            snapshot.config_note = "授权成功，账号已连接。".to_string();
                            snapshot.config_note_error = false;
                            snapshot.setup_note = "账号已连接，点「完成配置」继续。".to_string();
                        });
                        worker_state.needs_refresh.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("warning: authorization failed: {e}");
                        edit(&worker_state, |snapshot| {
                            snapshot.login_busy = false;
                            snapshot.login_error = format!("授权失败：{e}");
                        });
                    }
                }
            });
        });
    }

    // ---- open the authorization page -----------------------------------------------------
    ui.on_open_auth_url(|| {
        // ShellExecute via `start`; the URL is a compile-time constant, not user input.
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", AUTH_REDIRECT_URL])
            .spawn();
    });

    // ---- settings dialogs ----------------------------------------------------------------
    {
        let ui_weak = ui.as_weak();
        ui.on_open_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_open(true);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_close_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_open(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_finish_setup(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match apply_settings(&ui, &state) {
                Ok(_) => {
                    ui.set_setup_open(false);
                    edit(&state, |snapshot| {
                        snapshot.config_note.clear();
                        snapshot.config_note_error = false;
                        snapshot.setup_note.clear();
                    });
                    push_selection(&ui, &state);
                    if state.needs_refresh.swap(false, Ordering::Relaxed)
                        || state.library.lock().map(|l| l.all.is_empty()).unwrap_or(false)
                    {
                        start_refresh(state.clone());
                    }
                }
                Err(message) => {
                    edit(&state, |snapshot| {
                        snapshot.config_note = message.clone();
                        snapshot.config_note_error = true;
                        snapshot.setup_note = message;
                    });
                }
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_save_settings(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match apply_settings(&ui, &state) {
                Ok(_) => {
                    ui.set_settings_open(false);
                    edit(&state, |snapshot| {
                        snapshot.config_note = "设置已保存。".to_string();
                        snapshot.config_note_error = false;
                    });
                    push_selection(&ui, &state);
                    if state.needs_refresh.swap(false, Ordering::Relaxed)
                        || state.library.lock().map(|l| l.all.is_empty()).unwrap_or(false)
                    {
                        start_refresh(state.clone());
                    }
                }
                Err(message) => {
                    edit(&state, |snapshot| {
                        snapshot.config_note = message;
                        snapshot.config_note_error = true;
                    });
                }
            }
        });
    }

    // ---- install / pause / cancel --------------------------------------------------------
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_install(move || {
            let Some(ui) = ui_weak.upgrade() else { return };

            let Some(app_name) = state.selected_app.lock().ok().and_then(|slot| slot.clone())
            else {
                edit(&state, |snapshot| {
                    snapshot.note = "请先在左侧选择一款游戏".to_string();
                    snapshot.note_error = true;
                });
                return;
            };
            let Some(group) = state
                .library
                .lock()
                .ok()
                .and_then(|library| library.group_for(&app_name).cloned())
            else {
                return;
            };
            let game = group.game.clone();
            let include_dlc = ui.get_include_dlc();
            let dlcs = if include_dlc { group.dlc.clone() } else { Vec::new() };

            let concurrency_text = ui.get_concurrency_text().trim().to_string();
            let concurrency = match concurrency_text.parse::<usize>() {
                Ok(value) if (1..=128).contains(&value) => value,
                _ => {
                    edit(&state, |snapshot| {
                        snapshot.note = "并发线程必须在 1–128 之间，请到设置里修改。".to_string();
                        snapshot.note_error = true;
                    });
                    return;
                }
            };

            let mut cfg = match state.config.lock() {
                Ok(cfg) => cfg.clone(),
                Err(_) => return,
            };
            cfg.concurrency.downloads = concurrency;
            cfg.concurrency.auto = false;

            let root_text = ui.get_install_root().trim().to_string();
            let base = if root_text.is_empty() {
                PathBuf::from(&cfg.install_root)
            } else {
                PathBuf::from(&root_text)
            };
            let install_dir = install::find_existing_install(&base, &game.app_name)
                .unwrap_or_else(|| install::default_install_dir(&base, &game));

            // The core keeps the flag; stop requests go through `install::request_stop`.
            install::set_cancel_flag(Arc::new(AtomicBool::new(false)));
            if let Ok(mut slot) = state.stop_kind.lock() {
                *slot = None;
            }
            state.stopping.store(false, Ordering::Relaxed);

            edit(&state, |snapshot| {
                snapshot.running = true;
                snapshot.phase = "准备中".to_string();
                snapshot.phase_kind = 1;
                snapshot.progress = 0.0;
                snapshot.indeterminate = true;
                snapshot.percent = "0.0%".to_string();
                snapshot.speed = "—".to_string();
                snapshot.eta = "—".to_string();
                snapshot.bytes = "—".to_string();
                snapshot.chunks = "—".to_string();
                snapshot.inflight = "—".to_string();
                snapshot.elapsed = "0 秒".to_string();
                snapshot.retries.clear();
                snapshot.note = if dlcs.is_empty() {
                    "正在解析构建、探测 CDN 并拉取清单…".to_string()
                } else {
                    format!("正在解析构建（含 {} 个 DLC）、探测 CDN 并拉取清单…", dlcs.len())
                };
                snapshot.note_error = false;
            });

            let worker_state = state.clone();
            std::thread::spawn(move || {
                let run = || -> skiff_core::Result<install::InstallOutcome> {
                    let client = Client::new();
                    let tokens = auth::load_tokens()?
                        .ok_or_else(|| skiff_core::Error::Auth("not logged in".into()))?;
                    let tokens = auth::ensure_fresh(&cfg, &client, tokens)?;
                    let api = Api::new(&client, &cfg, &tokens.access_token);
                    let options = InstallOptions {
                        install_dir: install_dir.clone(),
                        concurrency: cfg.concurrency.clone(),
                        limit_bytes_per_sec: None,
                        verify: true,
                        repair: true,
                        register_launcher: cfg.register_launcher,
                    };

                    let progress_state = worker_state.clone();
                    let callback = Arc::new(move |progress: Progress| {
                        if let Ok(mut snapshot) = progress_state.snapshot.lock() {
                            // A stop is in flight; keep the 暂停中/取消中 state instead of
                            // overwriting it with fresh download numbers.
                            if progress_state.stopping.load(Ordering::Relaxed) {
                                return;
                            }
                            let ratio = if progress.bytes_total > 0 {
                                progress.bytes_done as f32 / progress.bytes_total as f32
                            } else {
                                0.0
                            };
                            let ratio = ratio.clamp(0.0, 1.0);
                            snapshot.progress = ratio;
                            snapshot.indeterminate = false;
                            snapshot.percent = format!("{:.1}%", ratio * 100.0);
                            snapshot.speed = format!("{}/s", human_bytes(progress.speed_bps as u64));
                            snapshot.bytes = format!(
                                "{} / {}",
                                human_bytes(progress.bytes_done),
                                human_bytes(progress.bytes_total)
                            );
                            snapshot.chunks =
                                format!("{} / {}", progress.chunks_done, progress.chunks_total);
                            snapshot.inflight =
                                format!("{} / {}", progress.inflight, progress.peak_inflight);
                            snapshot.elapsed = human_duration(progress.elapsed_secs);
                            snapshot.retries = if progress.retries > 0 {
                                format!("已重试 {} 次（网络抖动，属正常）", progress.retries)
                            } else {
                                String::new()
                            };
                            // Two phases share one byte counter: once every byte has landed the
                            // install is verifying files and writing `.egstore`.
                            let verifying = progress.bytes_total > 0
                                && progress.bytes_done >= progress.bytes_total;
                            snapshot.phase = if verifying { "校验中" } else { "下载中" }.to_string();
                            snapshot.phase_kind = 1;
                            snapshot.eta = if verifying {
                                "—".to_string()
                            } else {
                                progress
                                    .eta_secs
                                    .map(human_duration)
                                    .unwrap_or_else(|| "—".to_string())
                            };
                            if ratio > 0.0 {
                                // The "解析构建 / 探测 CDN" hint is no longer true.
                                snapshot.note.clear();
                            }
                        }
                    }) as Arc<dyn Fn(Progress) + Send + Sync>;

                    install::install_bundle(
                        &client,
                        &api,
                        &game,
                        &dlcs,
                        &options,
                        Some(callback),
                    )
                };

                let result = run();
                let stop_kind = worker_state.stop_kind.lock().ok().and_then(|mut slot| slot.take());
                edit(&worker_state, |snapshot| {
                    snapshot.running = false;
                    snapshot.indeterminate = false;
                    snapshot.eta = "—".to_string();
                    match result {
                        Ok(outcome) => {
                            snapshot.phase = "完成".to_string();
                            snapshot.phase_kind = 2;
                            snapshot.progress = 1.0;
                            snapshot.percent = "100.0%".to_string();
                            let dlc = if outcome.dlc_apps.is_empty() {
                                String::new()
                            } else {
                                format!(" · {} 个 DLC", outcome.dlc_apps.len())
                            };
                            let skipped = if outcome.skipped_dlc.is_empty() {
                                String::new()
                            } else {
                                format!(" · {} 个 DLC 跳过", outcome.skipped_dlc.len())
                            };
                            // The launcher only re-reads `Manifests\*.item` on start, so a running
                            // launcher keeps showing the game as not installed until it is exited
                            // from the tray; say so instead of leaving the user guessing.
                            let launcher = if outcome.launcher_registered {
                                if outcome.launcher_running {
                                    " · 已注册到 Epic 启动器；请从托盘完全退出启动器后重新打开，游戏才会显示"
                                } else {
                                    " · 已注册到 Epic 启动器"
                                }
                            } else if outcome.launcher_error.is_some() {
                                " · 未能注册到 Epic 启动器（不影响游戏文件）"
                            } else {
                                ""
                            };
                            snapshot.note = format!(
                                "安装完成 · {} 个文件{dlc}{skipped} · 峰值并发 {}{launcher}",
                                outcome.files, outcome.peak_inflight
                            );
                            snapshot.note_error = false;
                        }
                        // A user stop, not a failure: pause keeps the files, cancel deletes them.
                        Err(skiff_core::Error::Cancelled { removed, kept_existing_install }) => {
                            snapshot.note_error = false;
                            match stop_kind {
                                Some(install::StopKind::Pause) => {
                                    snapshot.phase = "已暂停".to_string();
                                    snapshot.phase_kind = 4;
                                    snapshot.note =
                                        "已暂停 · 进度已保存，点「继续安装」即可接着下。".to_string();
                                }
                                _ => {
                                    snapshot.phase = "已取消".to_string();
                                    snapshot.phase_kind = 0;
                                    snapshot.note = if kept_existing_install {
                                        "已取消 · 目录里已有完整安装，未删除原有文件。".to_string()
                                    } else if removed > 0 {
                                        format!("已取消 · 已删除 {removed} 个未完成的文件。")
                                    } else {
                                        "已取消 · 没有需要删除的文件。".to_string()
                                    };
                                }
                            }
                        }
                        Err(e) => {
                            snapshot.phase = "失败".to_string();
                            snapshot.phase_kind = 3;
                            snapshot.note = format!("安装失败：{e}");
                            snapshot.note_error = true;
                        }
                    }
                    // Whatever happened, the resume state is whatever is on disk now (a failed
                    // install keeps its ledger, a completed one has none).
                    snapshot.can_resume = !snapshot.target_dir.is_empty()
                        && install::has_partial_install(std::path::Path::new(&snapshot.target_dir));
                });
            });
        });
    }
    {
        let state = state.clone();
        ui.on_pause(move || {
            if !state.snapshot.lock().map(|s| s.running).unwrap_or(false) {
                return;
            }
            if let Ok(mut slot) = state.stop_kind.lock() {
                *slot = Some(install::StopKind::Pause);
            }
            state.stopping.store(true, Ordering::Relaxed);
            install::request_stop(install::StopKind::Pause);
            edit(&state, |snapshot| {
                if snapshot.running {
                    snapshot.phase = "暂停中…".to_string();
                    snapshot.phase_kind = 1;
                    snapshot.note = "正在停止下载，已下载的进度会保留…".to_string();
                    snapshot.note_error = false;
                }
            });
        });
    }
    {
        let state = state.clone();
        ui.on_cancel(move || {
            let running = state.snapshot.lock().map(|s| s.running).unwrap_or(false);
            if running {
                if let Ok(mut slot) = state.stop_kind.lock() {
                    *slot = Some(install::StopKind::Cancel);
                }
                state.stopping.store(true, Ordering::Relaxed);
                install::request_stop(install::StopKind::Cancel);
                edit(&state, |snapshot| {
                    snapshot.phase = "取消中…".to_string();
                    snapshot.phase_kind = 1;
                    snapshot.note = "正在停止下载并删除未完成的文件…".to_string();
                    snapshot.note_error = false;
                });
                return;
            }

            // Nothing is running: delete the partial files of a paused/aborted download.
            let target = state
                .snapshot
                .lock()
                .ok()
                .map(|snapshot| snapshot.target_dir.clone())
                .unwrap_or_default();
            if target.is_empty() {
                return;
            }
            let worker_state = state.clone();
            std::thread::spawn(move || {
                let report = install::discard_partial_install(std::path::Path::new(&target));
                edit(&worker_state, |snapshot| {
                    snapshot.can_resume = false;
                    snapshot.note_error = false;
                    snapshot.note = if report.kept_existing_install {
                        "已取消 · 目录里已有完整安装，未删除原有文件。".to_string()
                    } else if report.removed > 0 {
                        format!("已取消 · 已删除 {} 个未完成的文件。", report.removed)
                    } else {
                        "已取消 · 没有需要删除的文件。".to_string()
                    };
                });
            });
        });
    }

    // ---- mirror state into the UI --------------------------------------------------------
    let timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let mut pulse: f32 = 0.0;
        // `u64::MAX` forces the first tick to publish the model, including the cached library.
        let mut shown_revision = u64::MAX;

        timer.start(slint::TimerMode::Repeated, TICK, move || {
            let Some(ui) = ui_weak.upgrade() else { return };

            pulse += 0.028;
            if pulse > 1.0 {
                pulse = 0.0;
            }
            ui.set_pulse(pulse);

            // ---- library model (only rebuilt when the set or the query changed) ----
            let revision = state.library.lock().map(|library| library.revision).unwrap_or(0);
            if revision != shown_revision {
                let (rows, bases, dlcs, shown, query) = {
                    let Ok(library) = state.library.lock() else { return };
                    (
                        library.visible.iter().map(row_of).collect::<Vec<_>>(),
                        library.base_count(),
                        library.dlc_count(),
                        library.visible.len(),
                        library.query.clone(),
                    )
                };
                shown_revision = revision;
                ui.set_games(ModelRc::new(VecModel::from(rows)));
                ui.set_games_count_text(SharedString::from(if query.trim().is_empty() {
                    format!("{bases} 款游戏 · {dlcs} 个 DLC")
                } else {
                    format!("{bases} 款 · {dlcs} 个 DLC · 匹配 {shown}")
                }));

                // The refresh may have reordered or removed the selected row, and the search may
                // have filtered it out; in both cases the selection has to follow the visible list.
                let selected = state.selected_app.lock().ok().and_then(|slot| slot.clone());
                let position = selected.as_ref().and_then(|app| {
                    state.library.lock().ok().and_then(|library| {
                        library
                            .visible
                            .iter()
                            .position(|group| group.game.app_name == *app)
                    })
                });
                if position.is_none() {
                    if let Ok(mut slot) = state.selected_app.lock() {
                        *slot = None;
                    }
                }
                ui.set_selected(position.map(|p| p as i32).unwrap_or(-1));
                push_selection(&ui, &state);
            }

            // ---- snapshot ----
            let snapshot = match state.snapshot.lock() {
                Ok(snapshot) => snapshot.clone(),
                Err(_) => return,
            };

            ui.set_logged_in(snapshot.logged_in);
            ui.set_account_name(SharedString::from(snapshot.account_name.clone()));
            ui.set_account_initial(SharedString::from(snapshot.account_initial.clone()));
            ui.set_account_id(SharedString::from(snapshot.account_id.clone()));

            ui.set_job_running(snapshot.running);
            ui.set_phase(SharedString::from(snapshot.phase.clone()));
            ui.set_phase_kind(snapshot.phase_kind);
            ui.set_progress(snapshot.progress);
            ui.set_indeterminate(snapshot.indeterminate);
            ui.set_percent_text(SharedString::from(snapshot.percent.clone()));
            ui.set_speed(SharedString::from(snapshot.speed.clone()));
            ui.set_eta(SharedString::from(snapshot.eta.clone()));
            ui.set_bytes_text(SharedString::from(snapshot.bytes.clone()));
            ui.set_chunks_text(SharedString::from(snapshot.chunks.clone()));
            ui.set_inflight_text(SharedString::from(snapshot.inflight.clone()));
            ui.set_elapsed_text(SharedString::from(snapshot.elapsed.clone()));
            ui.set_retries_text(SharedString::from(snapshot.retries.clone()));
            ui.set_note(SharedString::from(snapshot.note.clone()));
            ui.set_note_error(snapshot.note_error);

            ui.set_selected_title(SharedString::from(snapshot.selected_title.clone()));
            ui.set_selected_app(SharedString::from(snapshot.selected_app.clone()));
            ui.set_target_dir(SharedString::from(snapshot.target_dir.clone()));
            ui.set_can_resume(snapshot.can_resume);
            ui.set_selected_dlc_count(snapshot.selected_dlc_count as i32);
            ui.set_selected_dlc_text(SharedString::from(snapshot.selected_dlc_text.clone()));

            ui.set_refresh_busy(snapshot.refresh_busy);
            ui.set_refresh_progress(snapshot.refresh_progress);
            ui.set_refresh_kind(snapshot.refresh_kind);
            let detail = if let (true, Some(started)) =
                (snapshot.refresh_busy, snapshot.refresh_started)
            {
                // Keep the elapsed time moving between stage reports.
                format!(
                    "{} · {:.1} 秒",
                    snapshot.refresh_detail.trim_end_matches('…'),
                    started.elapsed().as_secs_f64()
                )
            } else {
                snapshot.refresh_detail.clone()
            };
            ui.set_refresh_detail(SharedString::from(detail));

            ui.set_login_busy(snapshot.login_busy);
            ui.set_login_error(SharedString::from(snapshot.login_error.clone()));
            ui.set_config_note(SharedString::from(snapshot.config_note.clone()));
            ui.set_config_note_error(snapshot.config_note_error);
            ui.set_setup_note(SharedString::from(snapshot.setup_note.clone()));

            if snapshot.clear_code_input {
                ui.set_code_input(SharedString::default());
                edit(&state, |snapshot| snapshot.clear_code_input = false);
            }
        });
    }

    ui.run()
}
