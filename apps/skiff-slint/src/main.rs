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

mod covers;

use covers::CoverLoader;
use skiff_core::api::{Api, GameEntry, StageHook};
use skiff_core::auth;
use skiff_core::config::Config;
use skiff_core::disk;
use skiff_core::download::{Phase, Progress};
use skiff_core::http::Client;
use skiff_core::install::{self, InstallOptions};
use skiff_core::library::{self, GameGroup};
use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Browser page that hands out a one-time `authorizationCode` for the launcher client.
const AUTH_REDIRECT_URL: &str = "https://www.epicgames.com/id/api/redirect?clientId=34a02cf8f4414e29b15921876da36f9a&responseType=code";

/// How often state is copied into the UI. Also drives the indeterminate sweep.
const TICK: Duration = Duration::from_millis(60);

/// Upper bound on how long the drawer waits for a size estimate.
///
/// A game with many add-ons resolves one build + manifest per component; without a cap a stalled
/// request would keep the install button disabled for the request timeout (two minutes). A timeout
/// is reported as "unknown", which never blocks the download.
const SPACE_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the cached free-space reading is refreshed while the drawer is open.
const SPACE_REFRESH_INTERVAL: Duration = Duration::from_millis(1_500);

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

/// Whether the confirmation switches should keep the game files. A partial install has no usable
/// game to keep, so it always deletes its chunks even if the switch was left off.
fn keep_files_for_uninstall(status: InstallStatus, delete_files: bool) -> bool {
    status != InstallStatus::Partial && !delete_files
}

/// One-line result of an uninstall, plus the `NoticeBanner` tone (1 = ok, 2 = warn).
fn format_uninstall_report(
    report: &install::UninstallReport,
    title: &str,
    install_dir: &Path,
    was_partial: bool,
) -> (String, i32) {
    const NOTICE_OK: i32 = 1;
    const NOTICE_WARN: i32 = 2;

    let mut tone = NOTICE_OK;
    let mut parts: Vec<String> = Vec::new();

    if was_partial {
        parts.push(format!("已清理《{title}》的未完成安装"));
    } else if report.keep_files {
        parts.push(format!("已从库里移除《{title}》"));
    } else {
        parts.push(format!("已卸载《{title}》"));
    }

    if report.keep_files {
        parts.push(format!("游戏文件保留在 {}", install_dir.display()));
    } else {
        parts.push(format!(
            "删除 {} 个文件 · {}",
            report.files_removed,
            human_bytes(report.bytes_removed)
        ));
        if report.install_dir_removed {
            parts.push("安装目录已移除".to_string());
        } else if report.leftover_entries > 0 {
            tone = NOTICE_WARN;
            parts.push(format!("安装目录仍在（{} 项未列入清单）", report.leftover_entries));
        }
    }

    if report.failed_count > 0 {
        tone = NOTICE_WARN;
        parts.push(format!("{} 个文件未能删除（可能正在运行或被占用）", report.failed_count));
    }
    if !report.metadata_removed {
        tone = NOTICE_WARN;
        parts.push("安装记录未能完全删除，可稍后重试".to_string());
    }
    if let Some(err) = &report.launcher_error {
        tone = NOTICE_WARN;
        parts.push(format!("Epic 启动器记录未清理：{err}"));
    } else if report.launcher_running {
        tone = NOTICE_WARN;
        parts.push(
            "Epic 启动器正在运行，请从托盘完全退出后重新打开，卸载才会在官方库里生效"
                .to_string(),
        );
    } else if report.launcher_unregistered {
        parts.push("Epic 启动器记录已清理".to_string());
    }

    (parts.join(" · "), tone)
}

/// First character of the account name, for the avatar circle.
fn initial_of(name: &str) -> String {
    name.chars()
        .find(|c| !c.is_whitespace())
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Deterministic placeholder colour for a game without a cover image.
///
/// The same app name always gets the same tone, so a card does not change colour when the library
/// is refreshed or the search is edited.
fn tone_for(seed: &str) -> Color {
    const TONES: [(u8, u8, u8); 10] = [
        (38, 52, 78),
        (58, 42, 78),
        (30, 66, 66),
        (78, 46, 56),
        (46, 58, 38),
        (68, 54, 34),
        (36, 54, 72),
        (62, 38, 64),
        (28, 60, 54),
        (52, 48, 74),
    ];
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let (r, g, b) = TONES[(hash as usize) % TONES.len()];
    Color::from_rgb_u8(r, g, b)
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
/// The core emits UI-neutral ids so the CLI can ignore them; the wording and the
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

/// Local installation state for a main game, mirrored as an integer into Slint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum InstallStatus {
    #[default]
    NotInstalled = 0,
    Installed = 1,
    Partial = 2,
}

/// Which section a library row belongs to, mirrored as an integer into the Slint model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// A main game (the install unit).
    Main = 1,
    /// A label above the add-ons of one main game, or above the unlinked add-ons.
    GroupLabel = 2,
    /// An add-on owned for a main game in this library.
    Addon = 3,
    /// An add-on whose main game is not in the library; it cannot be installed alone.
    Orphan = 4,
}

/// One row of either library section.
#[derive(Clone)]
struct Row {
    kind: RowKind,
    title: String,
    sub: String,
    badge: String,
    /// Main-game app name this row activates; empty for orphan labels and orphan rows.
    app: String,
    selectable: bool,
    /// Catalog key-image URL; empty means a generated placeholder.
    cover_url: String,
    /// Store developer, shown on cards and in the detail pane.
    developer: String,
    /// Stable placeholder tone used while the cover is missing.
    tone: Color,
    /// First visible character of the title, for the placeholder.
    initial: String,
    /// Local install state under `install_root`.
    install_status: InstallStatus,
}

/// Store titles are sometimes missing, in which case the app name is all we have.
fn display_title(entry: &GameEntry) -> String {
    if entry.title.is_empty() { entry.app_name.clone() } else { entry.title.clone() }
}

/// Extract a quoted JSON string field value (e.g. `"AppName": "Ginger"`) without pulling in extra deps.
fn extract_json_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let pos = text.find(&needle)?;
    let rest = &text[pos + needle.len()..];
    let (_, after_colon) = rest.split_once(':')?;
    let value = after_colon.split('"').nth(1)?.trim();
    if value.is_empty() { None } else { Some(value.to_string()) }
}

/// Fast metadata read for `.egstore`: collects installed app names without parsing multi-MB manifests.
fn read_installed_app_names(dir: &Path) -> Vec<String> {
    let egstore = dir.join(".egstore");
    let Ok(entries) = std::fs::read_dir(&egstore) else {
        return Vec::new();
    };
    let mut apps = Vec::new();
    let mut saw_item = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "item") {
            continue;
        }
        saw_item = true;
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let guid = extract_json_string(&text, "InstallationGuid")
            .or_else(|| extract_json_string(&text, "HostInstallationGuid"))
            .or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_default();
        if guid.is_empty() || !egstore.join(format!("{guid}.manifest")).is_file() {
            continue;
        }
        if let Some(main_app) = extract_json_string(&text, "MainGameAppName") {
            apps.push(main_app);
        }
        if let Some(app) = extract_json_string(&text, "AppName") {
            apps.push(app);
        }
    }
    if !apps.is_empty() || saw_item {
        return apps;
    }
    // Fallback for legacy installs that carry a `.manifest` without an `.item`.
    install::load_installed_components(dir)
        .map(|components| {
            components
                .into_iter()
                .flat_map(|c| [c.main_app_name, c.app_name])
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Scan `root` once and map lower-cased app names to their local install status and directory.
///
/// Running a single directory pass avoids O(N*M) filesystem scans on every refresh or timer tick.
fn scan_local_installs(
    root: &Path,
    games: &[GameEntry],
) -> HashMap<String, (InstallStatus, PathBuf)> {
    let mut found: HashMap<String, (InstallStatus, PathBuf)> = HashMap::new();
    if root.as_os_str().is_empty() {
        return found;
    }

    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join(".egstore").is_dir() {
                continue;
            }
            let app_names = read_installed_app_names(&dir);
            if app_names.is_empty() {
                continue;
            }
            let status = if install::has_partial_install(&dir) {
                InstallStatus::Partial
            } else {
                InstallStatus::Installed
            };
            for name in app_names {
                found.entry(name.to_lowercase()).or_insert_with(|| (status, dir.clone()));
            }
        }
    }

    // Also detect brand-new partial downloads whose `.egstore/*.item` has not been written yet.
    for game in games.iter().filter(|game| !game.is_dlc) {
        let key = game.app_name.to_lowercase();
        if found.contains_key(&key) {
            continue;
        }
        let default_dir = install::default_install_dir(root, game);
        if install::has_partial_install(&default_dir) {
            found.insert(key, (InstallStatus::Partial, default_dir));
        }
    }

    found
}

/// The "主游戏" section: one row per main game, with owned add-on count and local status.
fn build_main_rows(
    groups: &[GameGroup],
    local_status: &HashMap<String, (InstallStatus, PathBuf)>,
) -> Vec<Row> {
    groups
        .iter()
        .map(|group| {
            let game = &group.game;
            let title = display_title(game);
            let install_status = local_status
                .get(&game.app_name.to_lowercase())
                .map(|(status, _)| *status)
                .unwrap_or(InstallStatus::NotInstalled);
            Row {
                kind: RowKind::Main,
                title: title.clone(),
                sub: game.app_name.clone(),
                badge: if group.dlc.is_empty() {
                    String::new()
                } else {
                    format!("DLC {}", group.dlc.len())
                },
                app: game.app_name.clone(),
                selectable: true,
                cover_url: game.cover_url.clone(),
                developer: game.developer.clone(),
                tone: tone_for(&game.app_name),
                initial: initial_of(&title),
                install_status,
            }
        })
        .collect()
}

/// The "附加内容" section: add-ons grouped under their main game, then the unlinked ones.
///
/// Both group headers and add-on rows point at their main game because an add-on cannot be
/// installed on its own.
fn build_addon_rows(groups: &[GameGroup], orphans: &[GameEntry]) -> Vec<Row> {
    let mut rows = Vec::new();

    for group in groups.iter().filter(|group| !group.dlc.is_empty()) {
        let parent = &group.game;
        let parent_title = display_title(parent);
        rows.push(Row {
            kind: RowKind::GroupLabel,
            title: parent_title.clone(),
            sub: format!("{} 项", group.dlc.len()),
            badge: String::new(),
            app: parent.app_name.clone(),
            selectable: true,
            cover_url: String::new(),
            developer: String::new(),
            tone: tone_for(&parent.app_name),
            initial: initial_of(&parent_title),
            install_status: InstallStatus::NotInstalled,
        });
        for dlc in &group.dlc {
            let title = display_title(dlc);
            rows.push(Row {
                kind: RowKind::Addon,
                title: title.clone(),
                sub: dlc.app_name.clone(),
                badge: String::new(),
                app: parent.app_name.clone(),
                selectable: true,
                cover_url: dlc.cover_url.clone(),
                developer: dlc.developer.clone(),
                tone: tone_for(&dlc.app_name),
                initial: initial_of(&title),
                install_status: InstallStatus::NotInstalled,
            });
        }
    }

    if !orphans.is_empty() {
        rows.push(Row {
            kind: RowKind::GroupLabel,
            title: "未关联主游戏".to_string(),
            sub: format!("{} 项", orphans.len()),
            badge: String::new(),
            app: String::new(),
            selectable: false,
            cover_url: String::new(),
            developer: String::new(),
            tone: tone_for("orphan"),
            initial: "?".to_string(),
            install_status: InstallStatus::NotInstalled,
        });
        for orphan in orphans {
            let title = display_title(orphan);
            rows.push(Row {
                kind: RowKind::Orphan,
                title: title.clone(),
                sub: orphan.app_name.clone(),
                badge: "未关联".to_string(),
                app: String::new(),
                selectable: false,
                cover_url: orphan.cover_url.clone(),
                developer: orphan.developer.clone(),
                tone: tone_for(&orphan.app_name),
                initial: initial_of(&title),
                install_status: InstallStatus::NotInstalled,
            });
        }
    }

    rows
}

fn row_to_ui(row: &Row, covers: &CoverLoader) -> LibraryRow {
    let cover = covers.image_for(&row.cover_url);
    LibraryRow {
        kind: row.kind as i32,
        title: SharedString::from(row.title.clone()),
        sub: SharedString::from(row.sub.clone()),
        badge: SharedString::from(row.badge.clone()),
        app: SharedString::from(row.app.clone()),
        selectable: row.selectable,
        cover_image: cover.clone().unwrap_or_default(),
        has_cover: cover.is_some(),
        tone: row.tone,
        initial: SharedString::from(row.initial.clone()),
        developer: SharedString::from(row.developer.clone()),
        install_status: row.install_status as i32,
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
    // Labels of the telemetry tiles: the same six tiles describe the download, verification and
    // repair stages, so their captions follow the active phase.
    speed_label: String,
    bytes_label: String,
    chunks_label: String,
    inflight_label: String,
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
    /// Local install state for the selected game (0 = none, 1 = installed, 2 = partial).
    selected_install_status: i32,
    /// Error scoped to the detail drawer (kept separate from active download job `note`).
    detail_error: String,
    /// Non-error result of the last detail action (uninstall summary, warnings).
    detail_notice: String,
    /// `NoticeBanner` tone for `detail_notice` (1 = ok, 2 = warn).
    detail_notice_tone: i32,
    /// Uninstall confirmation dialog state.
    uninstall_open: bool,
    uninstall_busy: bool,
    uninstall_error: String,
    // job identity, so the download page keeps showing the right game even when another library
    // entry is selected
    job_started: bool,
    job_app: String,
    job_title: String,
    job_cover_url: String,
    job_target_dir: String,
    job_include_dlc: bool,
    job_can_resume: bool,
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
    collapse_account_edit: bool,
}

/// Library cache plus the filtered rows of both sections.
#[derive(Default)]
struct Library {
    all: Vec<GameEntry>,
    visible: Vec<GameGroup>,
    /// Rows of the "主游戏" section.
    main_rows: Vec<Row>,
    /// Rows of the "附加内容" section, group labels included.
    addon_rows: Vec<Row>,
    /// Lower-cased app name -> (local install status, install directory).
    local_status: HashMap<String, (InstallStatus, PathBuf)>,
    /// Total main games that are installed or have partial progress on disk.
    installed_count: usize,
    /// Total main games that own at least one add-on.
    with_dlc_count: usize,
    /// Quick filter for the main-game grid: 0 = all, 1 = installed/partial, 2 = with add-ons.
    filter_mode: i32,
    query: String,
    revision: u64,
}

impl Library {
    /// Replace the whole set, scan local installs under `install_root`, and re-apply filters.
    fn set_games(&mut self, games: Vec<GameEntry>, install_root: &Path) {
        self.all = games;
        self.local_status = scan_local_installs(install_root, &self.all);
        self.refilter();
    }

    /// Re-scan local install status after config changes or install completion/pause/cancel.
    fn rescan_local(&mut self, install_root: &Path) {
        self.local_status = scan_local_installs(install_root, &self.all);
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

    /// Apply a main-game quick filter (0 = all, 1 = installed/partial, 2 = with DLC).
    fn set_filter(&mut self, mode: i32) {
        let mode = if (0..=2).contains(&mode) { mode } else { 0 };
        if self.filter_mode == mode {
            return;
        }
        self.filter_mode = mode;
        self.refilter();
    }

    fn refilter(&mut self) {
        let full_view = library::library_view(&self.all);
        self.installed_count = full_view
            .games
            .iter()
            .filter(|group| self.status_of(&group.game.app_name) != InstallStatus::NotInstalled)
            .count();
        self.with_dlc_count =
            full_view.games.iter().filter(|group| !group.dlc.is_empty()).count();

        let needle = self.query.trim().to_lowercase();
        // Group-level matching: a group survives when its main game *or* any of its add-ons
        // matches, and then the whole group is shown (same rule the library list uses).
        let view = if needle.is_empty() { full_view } else { full_view.filtered(&needle) };

        let filtered_games: Vec<GameGroup> = view
            .games
            .iter()
            .filter(|group| match self.filter_mode {
                1 => self.status_of(&group.game.app_name) != InstallStatus::NotInstalled,
                2 => !group.dlc.is_empty(),
                _ => true,
            })
            .cloned()
            .collect();

        self.main_rows = build_main_rows(&filtered_games, &self.local_status);
        // The Add-ons sub-view follows the search query only (main-game filter pills don't hide it).
        self.addon_rows = build_addon_rows(&view.games, &view.orphan_dlc);
        self.visible = filtered_games;
        self.revision = self.revision.wrapping_add(1);
    }

    fn status_of(&self, app_name: &str) -> InstallStatus {
        self.local_status
            .get(&app_name.to_lowercase())
            .map(|(status, _)| *status)
            .unwrap_or(InstallStatus::NotInstalled)
    }

    fn local_dir_of(&self, app_name: &str) -> Option<&Path> {
        self.local_status
            .get(&app_name.to_lowercase())
            .map(|(_, path)| path.as_path())
    }

    fn base_count(&self) -> usize {
        self.all.iter().filter(|game| !game.is_dlc).count()
    }

    fn dlc_count(&self) -> usize {
        self.all.iter().filter(|game| game.is_dlc).count()
    }

    /// True when the account owns any add-on at all, regardless of the current search.
    fn has_addons(&self) -> bool {
        self.all.iter().any(|game| game.is_dlc)
    }

    /// Group selected by its main-game app name, even when the current search or filter hides it.
    fn group_any(&self, app_name: &str) -> Option<GameGroup> {
        if let Some(group) = self.group_for(app_name) {
            return Some(group.clone());
        }
        library::library_view(&self.all)
            .games
            .into_iter()
            .find(|group| group.game.app_name == app_name)
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
            speed_label: "下载速度".to_string(),
            bytes_label: "已下载 / 总计".to_string(),
            chunks_label: "分块进度".to_string(),
            inflight_label: "在途 / 峰值并发".to_string(),
            elapsed: "—".to_string(),
            account_name: "未登录".to_string(),
            account_initial: "?".to_string(),
            account_id: "连接后显示账号名称".to_string(),
            ..Default::default()
        }
    }
}

/// A resolved size estimate plus the volume reading taken next to it.
struct ReadySpace {
    preview: install::SpacePreview,
    volume: Option<disk::VolumeSpace>,
}

/// Cached result of one game's pre-download estimate.
enum SpaceEntry {
    Ready(ReadySpace),
    Failed(String),
}

/// Per-session estimates and the single probe / volume refresh allowed to run at a time.
#[derive(Default)]
struct SpaceCache {
    /// Bumped whenever cached estimates stop being valid (library refresh, settings save, install
    /// finished). A probe started under an older generation drops its result instead of publishing
    /// a stale one.
    generation: u64,
    /// Probe currently running and the flag that cancels it.
    probing: Option<(String, Arc<AtomicBool>)>,
    /// A background free-space refresh is in flight.
    volume_busy: bool,
    /// Lower-cased app name -> estimate.
    entries: HashMap<String, SpaceEntry>,
}

impl SpaceCache {
    fn key(app: &str) -> String {
        app.to_lowercase()
    }

    fn remove(&mut self, app: &str) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.remove(&Self::key(app));
    }

    fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
    }
}

struct State {
    snapshot: Mutex<Snapshot>,
    library: Mutex<Library>,
    config: Mutex<Config>,
    covers: Arc<CoverLoader>,
    space: Mutex<SpaceCache>,
    selected_app: Mutex<Option<String>>,
    /// Set after a successful login so the next "save"/"finish" fetches the new account's library.
    needs_refresh: AtomicBool,
    /// True once the user asked the running install to stop (pause or cancel).
    stopping: AtomicBool,
    /// How the running install was asked to stop (pause keeps files, cancel deletes them).
    stop_kind: Mutex<Option<install::StopKind>>,
    /// One uninstall at a time; also keeps a double click from starting two delete workers.
    uninstalling: AtomicBool,
}

impl State {
    fn new(config: Config, covers: Arc<CoverLoader>) -> State {
        State {
            snapshot: Mutex::new(Snapshot::idle()),
            library: Mutex::new(Library::default()),
            config: Mutex::new(config),
            covers,
            space: Mutex::new(SpaceCache::default()),
            selected_app: Mutex::new(None),
            needs_refresh: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            stop_kind: Mutex::new(None),
            uninstalling: AtomicBool::new(false),
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
                let install_root = worker_state
                    .config
                    .lock()
                    .map(|cfg| PathBuf::from(&cfg.install_root))
                    .unwrap_or_default();
                worker_state
                    .covers
                    .request_all(games.iter().map(|game| game.cover_url.clone()));
                let shown = {
                    let mut library = worker_state.library.lock().unwrap();
                    library.set_games(games, &install_root);
                    library.visible.len()
                };
                // Builds may have changed: every cached size estimate is stale now.
                if let Ok(mut space) = worker_state.space.lock() {
                    space.clear();
                }
                if let Ok(mut snapshot) = worker_state.snapshot.lock() {
                    snapshot.refresh_busy = false;
                    snapshot.refresh_progress = 1.0;
                    snapshot.refresh_detail = if shown == bases {
                        format!("已更新 {bases} 款游戏 · {dlcs} 个附加内容 · {elapsed:.1} 秒")
                    } else {
                        format!(
                            "已更新 {bases} 款游戏 · {dlcs} 个附加内容，当前显示 {shown} 款 · {elapsed:.1} 秒"
                        )
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

    let (group, cached_local) = selected
        .as_ref()
        .and_then(|app_name| {
            state.library.lock().ok().and_then(|library| {
                let group = library.group_any(app_name)?;
                let local = library
                    .local_dir_of(app_name)
                    .map(|dir| (library.status_of(app_name), dir.to_path_buf()));
                Some((Some(group), local))
            })
        })
        .unwrap_or((None, None));

    let Some(group) = group else {
        // Nothing selected, or the game no longer exists after a library refresh: clear detail.
        if selected.is_some() {
            if let Ok(mut slot) = state.selected_app.lock() {
                *slot = None;
            }
        }
        ui.set_selected_title(SharedString::default());
        ui.set_selected_app(SharedString::default());
        ui.set_target_dir(SharedString::default());
        ui.set_can_resume(false);
        ui.set_selected_install_status(InstallStatus::NotInstalled as i32);
        ui.set_selected_dlc_count(0);
        ui.set_selected_dlc_text(SharedString::default());
        ui.set_selected_cover(slint::Image::default());
        ui.set_selected_has_cover(false);
        ui.set_selected_tone(tone_for(""));
        ui.set_selected_initial(SharedString::from("?"));
        ui.set_selected_developer(SharedString::default());
        edit(state, |snapshot| {
            snapshot.selected_title.clear();
            snapshot.selected_app.clear();
            snapshot.target_dir.clear();
            snapshot.selected_dlc_count = 0;
            snapshot.selected_dlc_text.clear();
            snapshot.can_resume = false;
            snapshot.selected_install_status = InstallStatus::NotInstalled as i32;
            snapshot.detail_error.clear();
            snapshot.detail_notice.clear();
            snapshot.uninstall_open = false;
            snapshot.uninstall_busy = false;
            snapshot.uninstall_error.clear();
        });
        return;
    };
    let game = &group.game;

    let base = state
        .config
        .lock()
        .map(|cfg| PathBuf::from(&cfg.install_root))
        .unwrap_or_default();
    let (install_status, target) = match cached_local {
        Some((status, dir)) => (status, dir),
        None => {
            let dir = install::find_existing_install(&base, &game.app_name)
                .unwrap_or_else(|| install::default_install_dir(&base, game));
            let status = if install::has_partial_install(&dir) {
                InstallStatus::Partial
            } else if dir.join(".egstore").is_dir()
                && install::load_installed_components(&dir).is_ok()
            {
                InstallStatus::Installed
            } else {
                InstallStatus::NotInstalled
            };
            (status, dir)
        }
    };
    let title = display_title(game);
    let can_resume = install_status == InstallStatus::Partial || install::has_partial_install(&target);
    let dlc_text = group
        .dlc
        .iter()
        .map(|dlc| format!("· {}", display_title(dlc)))
        .collect::<Vec<_>>()
        .join("\n");

    let cover = state.covers.image_for(&game.cover_url);
    ui.set_selected_title(SharedString::from(title.clone()));
    ui.set_selected_app(SharedString::from(game.app_name.clone()));
    ui.set_target_dir(SharedString::from(target.to_string_lossy().to_string()));
    ui.set_can_resume(can_resume);
    ui.set_selected_install_status(install_status as i32);
    ui.set_selected_dlc_count(group.dlc.len() as i32);
    ui.set_selected_dlc_text(SharedString::from(dlc_text.clone()));
    ui.set_selected_cover(cover.clone().unwrap_or_default());
    ui.set_selected_has_cover(cover.is_some());
    ui.set_selected_tone(tone_for(&game.app_name));
    ui.set_selected_initial(SharedString::from(initial_of(&title)));
    ui.set_selected_developer(SharedString::from(game.developer.clone()));
    edit(state, |snapshot| {
        snapshot.selected_title = title;
        snapshot.selected_app = game.app_name.clone();
        snapshot.target_dir = target.to_string_lossy().to_string();
        snapshot.selected_dlc_count = group.dlc.len();
        snapshot.selected_dlc_text = dlc_text;
        snapshot.can_resume = can_resume;
        snapshot.selected_install_status = install_status as i32;
    });
}

// ---------------------------------------------------------------------------------------------
// pre-download space estimate
// ---------------------------------------------------------------------------------------------

/// What the drawer shows before an install can start.
struct SpaceView {
    /// An estimate is being computed; the install button waits for it.
    pending: bool,
    /// An estimate exists (the volume reading may still be unknown).
    known: bool,
    /// The target volume can hold what the install still needs.
    fits: bool,
    required_text: String,
    free_text: String,
    detail_text: String,
    note: String,
    note_tone: i32,
    install_enabled: bool,
}

impl SpaceView {
    /// Nothing known and nothing being computed: no gate.
    fn unknown() -> SpaceView {
        SpaceView {
            pending: false,
            known: false,
            fits: true,
            required_text: "—".to_string(),
            free_text: "—".to_string(),
            detail_text: String::new(),
            note: String::new(),
            note_tone: 0,
            install_enabled: true,
        }
    }
}

/// Render the space card for `app` from the cache and the current add-on choice.
///
/// Pure so the gate rules stay testable: an unreadable volume or a failed estimate never disables
/// the button, while a known shortfall always does.
fn space_view(cache: &SpaceCache, app: &str, include_dlc: bool) -> SpaceView {
    if app.is_empty() {
        return SpaceView::unknown();
    }
    let key = SpaceCache::key(app);
    let pending = cache
        .probing
        .as_ref()
        .map(|(running, _)| running == &key)
        .unwrap_or(false);

    let Some(entry) = cache.entries.get(&key) else {
        if !pending {
            return SpaceView::unknown();
        }
        return SpaceView {
            pending: true,
            known: false,
            fits: true,
            required_text: "…".to_string(),
            free_text: "…".to_string(),
            detail_text: String::new(),
            note: "正在解析构建与清单以计算所需空间…".to_string(),
            note_tone: 0,
            install_enabled: false,
        };
    };

    match entry {
        SpaceEntry::Failed(reason) => SpaceView {
            pending: false,
            known: false,
            fits: true,
            required_text: "—".to_string(),
            free_text: "—".to_string(),
            detail_text: String::new(),
            note: format!("无法获取所需空间（{reason}），仍可尝试安装。"),
            note_tone: 2,
            install_enabled: true,
        },
        SpaceEntry::Ready(ready) => {
            let sizes = if include_dlc {
                &ready.preview.with_dlc
            } else {
                &ready.preview.base_only
            };
            let needed = sizes.needed_bytes;
            let required_text = if needed == 0 {
                "无需额外空间".to_string()
            } else {
                human_bytes(needed)
            };

            let (free_text, fits, detail_text, mut note, mut tone) = match ready.volume {
                Some(volume) => {
                    let detail = format!(
                        "安装后占用 {} · 目标盘共 {}",
                        human_bytes(sizes.install_bytes),
                        human_bytes(volume.total_bytes)
                    );
                    if volume.free_bytes < needed {
                        (
                            human_bytes(volume.free_bytes),
                            false,
                            detail,
                            format!(
                                "剩余空间不足，还差 {}。请清理磁盘或更换安装目录。",
                                human_bytes(needed.saturating_sub(volume.free_bytes))
                            ),
                            3,
                        )
                    } else if needed == 0 {
                        (
                            human_bytes(volume.free_bytes),
                            true,
                            detail,
                            "现有文件已覆盖安装所需空间，无需额外空间。".to_string(),
                            1,
                        )
                    } else {
                        (
                            human_bytes(volume.free_bytes),
                            true,
                            detail,
                            "空间充足，可以开始下载。".to_string(),
                            1,
                        )
                    }
                }
                None => (
                    "—".to_string(),
                    true,
                    format!("安装后占用 {}", human_bytes(sizes.install_bytes)),
                    "尚未读取到目标盘剩余空间，暂不做空间检查。".to_string(),
                    0,
                ),
            };

            if !ready.preview.skipped_dlc.is_empty() {
                note = format!(
                    "{note} {} 个附加内容无法解析，未计入预估。",
                    ready.preview.skipped_dlc.len()
                );
                tone = tone.max(2);
            }

            SpaceView {
                pending: false,
                known: true,
                fits,
                required_text,
                free_text,
                detail_text,
                note,
                note_tone: tone,
                install_enabled: fits,
            }
        }
    }
}

/// Compute `app`'s size estimate in the background.
///
/// Only the open drawer asks for this, and only one probe runs at a time: selecting another game
/// cancels the running one so the newest selection is not stuck behind a slow manifest fetch.
fn ensure_space_probe(state: &Arc<State>, app: &str, target_dir: &str) {
    if app.is_empty() || target_dir.trim().is_empty() {
        return;
    }
    let key = SpaceCache::key(app);

    // Cheap precheck on every tick: a cached estimate or a probe for this game needs no work, and a
    // probe for another game is cancelled so this selection wins as soon as it exits.
    {
        let Ok(cache) = state.space.lock() else { return };
        if cache.entries.contains_key(&key) {
            return;
        }
        match &cache.probing {
            Some((running, _)) if running == &key => return,
            Some((_, running_cancel)) => {
                running_cancel.store(true, Ordering::Relaxed);
                return;
            }
            None => {}
        }
    }

    // Read what the worker needs before registering: no lock is held across these clones.
    let Some(group) = state
        .library
        .lock()
        .ok()
        .and_then(|library| library.group_any(app))
    else {
        return;
    };
    let cfg = state.config.lock().map(|cfg| cfg.clone()).unwrap_or_default();
    let dir = PathBuf::from(target_dir);

    let cancel = Arc::new(AtomicBool::new(false));
    let generation = {
        let Ok(mut cache) = state.space.lock() else { return };
        // Re-check after the clones: another tick may have registered a probe or published a result.
        if cache.entries.contains_key(&key) {
            return;
        }
        match &cache.probing {
            Some((running, _)) if running == &key => return,
            Some((_, running_cancel)) => {
                running_cancel.store(true, Ordering::Relaxed);
                return;
            }
            None => {}
        }
        cache.probing = Some((key.clone(), cancel.clone()));
        cache.generation
    };

    let worker_state = state.clone();
    std::thread::spawn(move || {
        let timed_out = Arc::new(AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            let timed_out = timed_out.clone();
            // Detached watchdog: it only raises the flags, and the probe thread is never joined.
            std::thread::spawn(move || {
                std::thread::sleep(SPACE_PROBE_TIMEOUT);
                timed_out.store(true, Ordering::Relaxed);
                cancel.store(true, Ordering::Relaxed);
            });
        }

        let result = (|| -> skiff_core::Result<install::SpacePreview> {
            let client = Client::new();
            let tokens = auth::load_tokens()?
                .ok_or_else(|| skiff_core::Error::Auth("not logged in".into()))?;
            let tokens = auth::ensure_fresh(&cfg, &client, tokens)?;
            let api = Api::new(&client, &cfg, &tokens.access_token).with_cancel(cancel.clone());
            install::preview_install_sizes(&api, &group.game, &group.dlc, &dir)
        })();

        let cancelled = cancel.load(Ordering::Relaxed);
        let timed_out = timed_out.load(Ordering::Relaxed);
        if let Ok(mut cache) = worker_state.space.lock() {
            if cache
                .probing
                .as_ref()
                .map(|(running, _)| running == &key)
                .unwrap_or(false)
            {
                cache.probing = None;
            }
            if cache.generation == generation {
                let entry = match result {
                    Ok(preview) => Some(SpaceEntry::Ready(ReadySpace { preview, volume: None })),
                    Err(_) if cancelled && timed_out => {
                        Some(SpaceEntry::Failed("计算超时（网络较慢）".to_string()))
                    }
                    // Cancelled because another game was selected: caching the failure would leave
                    // a stale error behind when the user comes back.
                    Err(_) if cancelled => None,
                    Err(e) => Some(SpaceEntry::Failed(e.to_string())),
                };
                if let Some(entry) = entry {
                    cache.entries.insert(key.clone(), entry);
                }
            }
        }

        // The volume query can block for a long time on a disconnected share, so it runs after the
        // estimate is published: the drawer is never stuck waiting for it.
        if let Some(volume) = disk::volume_space(&dir).ok() {
            if let Ok(mut cache) = worker_state.space.lock() {
                if cache.generation == generation {
                    if let Some(SpaceEntry::Ready(ready)) = cache.entries.get_mut(&key) {
                        ready.volume = Some(volume);
                    }
                }
            }
        }
    });
}

/// Re-read the target volume in the background while the drawer stays open.
fn refresh_space_volume(state: &Arc<State>, app: &str) {
    let key = SpaceCache::key(app);
    let (dir, generation) = {
        let Ok(mut cache) = state.space.lock() else { return };
        if cache.volume_busy {
            return;
        }
        let Some(SpaceEntry::Ready(ready)) = cache.entries.get(&key) else {
            return;
        };
        let dir = ready.preview.target_dir.clone();
        cache.volume_busy = true;
        (dir, cache.generation)
    };

    let worker_state = state.clone();
    std::thread::spawn(move || {
        let volume = disk::volume_space(&dir).ok();
        if let Ok(mut cache) = worker_state.space.lock() {
            cache.volume_busy = false;
            if cache.generation == generation {
                if let Some(SpaceEntry::Ready(ready)) = cache.entries.get_mut(&key) {
                    ready.volume = volume;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// install job
// ---------------------------------------------------------------------------------------------

/// Resolve the selected game's install directory from the cached scan, falling back to the target
/// shown in the detail drawer and then to a disk scan.
fn selected_install_target(state: &State) -> Option<(String, PathBuf, InstallStatus, String)> {
    let app = state.selected_app.lock().ok().and_then(|slot| slot.clone())?;
    let (status, dir, title) = {
        let library = state.library.lock().ok()?;
        let group = library.group_any(&app)?;
        (
            library.status_of(&app),
            library.local_dir_of(&app).map(Path::to_path_buf),
            display_title(&group.game),
        )
    };
    let dir = dir.or_else(|| {
        let target = state
            .snapshot
            .lock()
            .ok()
            .map(|snapshot| snapshot.target_dir.clone())
            .unwrap_or_default();
        if !target.is_empty() {
            let path = PathBuf::from(target);
            if path.join(".egstore").is_dir() || install::has_partial_install(&path) {
                return Some(path);
            }
        }
        let root = state
            .config
            .lock()
            .ok()
            .map(|cfg| PathBuf::from(&cfg.install_root))
            .unwrap_or_default();
        install::find_existing_install(&root, &app)
    })?;
    Some((app, dir, status, title))
}

fn ratio_of(done: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (done as f32 / total as f32).clamp(0.0, 1.0)
    }
}

fn format_percent(ratio: f32) -> String {
    format!("{:.1}%", ratio * 100.0)
}

/// Copy one core progress snapshot into the UI state.
///
/// The download page has a single progress surface but three stages (download, verification and
/// repair), so the numbers and the telemetry captions follow `progress.phase`. `job_started` keeps
/// "已用时间" a total for the whole task instead of resetting when verification starts.
fn apply_progress(snapshot: &mut Snapshot, progress: &Progress, job_started: Instant) {
    snapshot.elapsed = human_duration(job_started.elapsed().as_secs_f64());
    snapshot.indeterminate = false;
    match progress.phase {
        Phase::Download => apply_download_progress(snapshot, progress),
        Phase::Verify => apply_verify_progress(snapshot, progress),
        Phase::Repair => apply_repair_progress(snapshot, progress),
    }
}

fn apply_download_progress(snapshot: &mut Snapshot, progress: &Progress) {
    let ratio = ratio_of(progress.bytes_done, progress.bytes_total);
    snapshot.phase = "下载中".to_string();
    snapshot.phase_kind = 1;
    snapshot.progress = ratio;
    snapshot.percent = format_percent(ratio);
    snapshot.speed = format!("{}/s", human_bytes(progress.speed_bps as u64));
    snapshot.bytes = format!(
        "{} / {}",
        human_bytes(progress.bytes_done),
        human_bytes(progress.bytes_total)
    );
    snapshot.chunks = format!("{} / {}", progress.chunks_done, progress.chunks_total);
    snapshot.inflight = format!("{} / {}", progress.inflight, progress.peak_inflight);
    snapshot.eta = progress.eta_secs.map(human_duration).unwrap_or_else(|| "—".to_string());
    snapshot.retries = if progress.retries > 0 {
        format!("已重试 {} 次（网络抖动，属正常）", progress.retries)
    } else {
        String::new()
    };
    snapshot.speed_label = "下载速度".to_string();
    snapshot.bytes_label = "已下载 / 总计".to_string();
    snapshot.chunks_label = "分块进度".to_string();
    snapshot.inflight_label = "在途 / 峰值并发".to_string();
    if ratio > 0.0 {
        // The "解析构建 / 探测 CDN" hint is no longer true.
        snapshot.note.clear();
    }
}

fn apply_verify_progress(snapshot: &mut Snapshot, progress: &Progress) {
    let verify = &progress.verify;
    // Zero-byte files still leave a meaningful file-count progress.
    let ratio = if verify.bytes_total > 0 {
        ratio_of(verify.bytes_done, verify.bytes_total)
    } else {
        ratio_of(verify.files_done as u64, verify.files_total as u64)
    };
    snapshot.phase = "校验中".to_string();
    snapshot.phase_kind = 5;
    snapshot.progress = ratio;
    snapshot.percent = format_percent(ratio);
    snapshot.speed = format!("{}/s", human_bytes(verify.speed_bps as u64));
    snapshot.eta = verify.eta_secs.map(human_duration).unwrap_or_else(|| "—".to_string());
    snapshot.bytes = format!(
        "已校验 {} / {}",
        human_bytes(verify.bytes_done),
        human_bytes(verify.bytes_total)
    );
    snapshot.chunks = format!("{} / {}", verify.files_done, verify.files_total);
    snapshot.inflight = format!("{} 线程", verify.threads);
    snapshot.retries.clear();
    snapshot.speed_label = "校验速度".to_string();
    snapshot.bytes_label = "已校验 / 总计".to_string();
    snapshot.chunks_label = "文件进度".to_string();
    snapshot.inflight_label = "校验线程".to_string();
    snapshot.note = "文件已下载完成，正在逐文件校验完整性…".to_string();
    snapshot.note_error = false;
}

fn apply_repair_progress(snapshot: &mut Snapshot, progress: &Progress) {
    let ratio = ratio_of(progress.bytes_done, progress.bytes_total);
    snapshot.phase = "修复中".to_string();
    snapshot.phase_kind = 1;
    snapshot.progress = ratio;
    snapshot.percent = format_percent(ratio);
    snapshot.speed = format!("{}/s", human_bytes(progress.speed_bps as u64));
    snapshot.eta = progress.eta_secs.map(human_duration).unwrap_or_else(|| "—".to_string());
    snapshot.bytes = format!(
        "已修复 {} / {}",
        human_bytes(progress.bytes_done),
        human_bytes(progress.bytes_total)
    );
    snapshot.chunks = format!("{} / {}", progress.chunks_done, progress.chunks_total);
    snapshot.inflight = format!("{} / {}", progress.inflight, progress.peak_inflight);
    snapshot.retries = if progress.retries > 0 {
        format!("修复重试 {} 次", progress.retries)
    } else {
        String::new()
    };
    snapshot.speed_label = "修复速度".to_string();
    snapshot.bytes_label = "已修复 / 总计".to_string();
    snapshot.chunks_label = "分块进度".to_string();
    snapshot.inflight_label = "在途 / 峰值并发".to_string();
    snapshot.note = "校验发现损坏的分块，正在重新下载…".to_string();
    snapshot.note_error = false;
}

/// Start installing one main game and mirror the job identity into the snapshot.
///
/// The download/verify work stays in `skiff-core`; this function only prepares the worker and the
/// visible job state. It uses the saved configuration instead of the settings form, so an unsaved
/// edit cannot silently change the install location or concurrency. The caller decides whether to
/// switch to the download page.
fn begin_install(
    state: &Arc<State>,
    app_name: &str,
    include_dlc: bool,
) -> Result<(), String> {
    if state.uninstalling.load(Ordering::Relaxed) {
        return Err("正在卸载游戏，请稍候".to_string());
    }
    let group = state
        .library
        .lock()
        .map_err(|_| "游戏库状态不可用".to_string())?
        .group_any(app_name)
        .ok_or_else(|| "找不到这个游戏，请刷新游戏库".to_string())?;
    let game = group.game.clone();
    let dlcs = if include_dlc { group.dlc.clone() } else { Vec::new() };
    let cfg = state
        .config
        .lock()
        .map_err(|_| "配置状态不可用".to_string())?
        .clone();

    let base = PathBuf::from(&cfg.install_root);
    let install_dir = install::find_existing_install(&base, &game.app_name)
        .unwrap_or_else(|| install::default_install_dir(&base, &game));

    // The core keeps the flag; stop requests go through `install::request_stop`.
    install::set_cancel_flag(Arc::new(AtomicBool::new(false)));
    if let Ok(mut slot) = state.stop_kind.lock() {
        *slot = None;
    }
    state.stopping.store(false, Ordering::Relaxed);

    let title = display_title(&game);
    let target = install_dir.to_string_lossy().to_string();
    edit(state, |snapshot| {
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
        snapshot.speed_label = "下载速度".to_string();
        snapshot.bytes_label = "已下载 / 总计".to_string();
        snapshot.chunks_label = "分块进度".to_string();
        snapshot.inflight_label = "在途 / 峰值并发".to_string();
        snapshot.elapsed = "0 秒".to_string();
        snapshot.retries.clear();
        snapshot.note = if dlcs.is_empty() {
            "正在解析构建、探测 CDN 并拉取清单…".to_string()
        } else {
            format!("正在解析构建（含 {} 个附加内容）、探测 CDN 并拉取清单…", dlcs.len())
        };
        snapshot.note_error = false;
        snapshot.job_started = true;
        snapshot.job_app = game.app_name.clone();
        snapshot.job_title = title.clone();
        snapshot.job_cover_url = game.cover_url.clone();
        snapshot.job_target_dir = target;
        snapshot.job_include_dlc = include_dlc;
        snapshot.job_can_resume = false;
        if snapshot.selected_app == game.app_name {
            snapshot.can_resume = false;
        }
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

            let job_started = Instant::now();
            let progress_state = worker_state.clone();
            let callback = Arc::new(move |progress: Progress| {
                if let Ok(mut snapshot) = progress_state.snapshot.lock() {
                    // A stop is in flight; keep the 暂停中/取消中 state instead of overwriting it
                    // with fresh progress numbers.
                    if progress_state.stopping.load(Ordering::Relaxed) {
                        return;
                    }
                    apply_progress(&mut snapshot, &progress, job_started);
                }
            }) as Arc<dyn Fn(Progress) + Send + Sync>;

            install::install_bundle(&client, &api, &game, &dlcs, &options, Some(callback))
        };

        let result = run();
        let stop_kind = worker_state.stop_kind.lock().ok().and_then(|mut slot| slot.take());
        edit(&worker_state, |snapshot| {
            snapshot.running = false;
            snapshot.indeterminate = false;
            snapshot.eta = "—".to_string();
            snapshot.speed_label = "下载速度".to_string();
            snapshot.bytes_label = "已下载 / 总计".to_string();
            snapshot.chunks_label = "分块进度".to_string();
            snapshot.inflight_label = "在途 / 峰值并发".to_string();
            match result {
                Ok(outcome) => {
                    snapshot.phase = "完成".to_string();
                    snapshot.phase_kind = 2;
                    snapshot.progress = 1.0;
                    snapshot.percent = "100.0%".to_string();
                    snapshot.speed = "—".to_string();
                    let dlc = if outcome.dlc_apps.is_empty() {
                        String::new()
                    } else {
                        format!(" · {} 个附加内容", outcome.dlc_apps.len())
                    };
                    let skipped = if outcome.skipped_dlc.is_empty() {
                        String::new()
                    } else {
                        format!(" · {} 个附加内容跳过", outcome.skipped_dlc.len())
                    };
                    // The launcher only re-reads `Manifests\*.item` on start, so a running
                    // launcher keeps showing the game as not installed until it is exited from
                    // the tray; say so instead of leaving the user guessing.
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
                                "已暂停 · 进度已保存，点「继续下载」即可接着下。".to_string();
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
            // Whatever happened, the resume state is whatever is on disk now (a failed install
            // keeps its ledger, a completed one has none).
            let job_target = snapshot.job_target_dir.clone();
            let job_can_resume =
                !job_target.is_empty() && install::has_partial_install(Path::new(&job_target));
            snapshot.job_can_resume = job_can_resume;
            if snapshot.selected_app == snapshot.job_app {
                snapshot.can_resume = job_can_resume;
            } else if !snapshot.target_dir.is_empty() {
                snapshot.can_resume =
                    install::has_partial_install(Path::new(&snapshot.target_dir));
            }
        });
        // Refresh cached local install badges & "已安装" filter count after the job ends.
        let install_root = worker_state
            .config
            .lock()
            .map(|c| PathBuf::from(&c.install_root))
            .unwrap_or_default();
        if let Ok(mut library) = worker_state.library.lock() {
            library.rescan_local(&install_root);
        }
        // The install changed what is on disk and how much room is left.
        if let Ok(mut space) = worker_state.space.lock() {
            space.remove(&game.app_name);
        }
    });
    Ok(())
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
    // The install root may have moved, so the size estimates were computed for another volume.
    if let Ok(mut space) = state.space.lock() {
        space.clear();
    }
    if let Ok(mut library) = state.library.lock() {
        library.rescan_local(Path::new(&cfg.install_root));
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
    let covers = Arc::new(CoverLoader::new(Config::dir().join("covers")));
    let state = Arc::new(State::new(cfg, covers));

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
    let mut cached_coverless = false;
    if let Ok(cached) = install::load_cached_games() {
        if !cached.is_empty() {
            cached_count = cached.len();
            cached_coverless = cached.iter().all(|game| game.cover_url.is_empty());
            let install_root = state
                .config
                .lock()
                .map(|cfg| PathBuf::from(&cfg.install_root))
                .unwrap_or_default();
            state
                .covers
                .request_all(cached.iter().map(|game| game.cover_url.clone()));
            if let Ok(mut library) = state.library.lock() {
                library.set_games(cached, &install_root);
            }
        }
    }

    // ---- first-run refresh ---------------------------------------------------------------
    let logged_in = state.snapshot.lock().map(|s| s.logged_in).unwrap_or(false);
    // A cache written by an older build has no cover URLs. Refresh once automatically so the new
    // library grid fills in without asking the user to press 刷新库; offline, the placeholders stay.
    if logged_in && (cached_count == 0 || cached_coverless) {
        start_refresh(state.clone());
    }

    // ---- library refresh -----------------------------------------------------------------
    {
        let state = state.clone();
        ui.on_refresh(move || start_refresh(state.clone()));
    }

    // ---- search & quick filter -----------------------------------------------------------
    {
        let state = state.clone();
        ui.on_filter_games(move |text: SharedString| {
            if let Ok(mut library) = state.library.lock() {
                library.set_query(text.as_str());
            }
        });
    }
    {
        let state = state.clone();
        ui.on_set_library_filter(move |mode: i32| {
            if let Ok(mut library) = state.library.lock() {
                library.set_filter(mode);
            }
        });
    }

    // ---- selection -----------------------------------------------------------------------
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_select(move |app_name: SharedString| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let app_name = app_name.to_string();
            if app_name.is_empty() {
                return;
            }
            // Both main games and add-on rows (or group headers) carry the main game's app name.
            // Checking `group_any` ensures add-ons stay clickable even when a main-game filter is active.
            let selectable = state
                .library
                .lock()
                .ok()
                .map(|library| library.group_any(&app_name).is_some())
                .unwrap_or(false);
            if !selectable {
                return;
            }
            if let Ok(mut slot) = state.selected_app.lock() {
                *slot = Some(app_name);
            }
            edit(&state, |snapshot| {
                snapshot.detail_error.clear();
                snapshot.detail_notice.clear();
                snapshot.uninstall_open = false;
                snapshot.uninstall_busy = false;
                snapshot.uninstall_error.clear();
            });
            ui.set_uninstall_open(false);
            ui.set_uninstall_error(SharedString::default());
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
                            snapshot.collapse_account_edit = true;
                            snapshot.config_note = "授权成功，账号已连接。".to_string();
                            snapshot.config_note_error = false;
                            snapshot.setup_note = "账号已连接，请点击「下一步」设置安装目录。".to_string();
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

    // ---- settings / first run ------------------------------------------------------------
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_finish_setup(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match apply_settings(&ui, &state) {
                Ok(_) => {
                    ui.set_setup_open(false);
                    ui.set_setup_step(0);
                    ui.set_page(0);
                    ui.set_detail_open(false);
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
                    snapshot.detail_error = "请先选择一款游戏".to_string();
                });
                return;
            };
            let include_dlc = ui.get_include_dlc();
            match begin_install(&state, &app_name, include_dlc) {
                Ok(()) => {
                    edit(&state, |snapshot| {
                        snapshot.detail_error.clear();
                        snapshot.detail_notice.clear();
                        snapshot.uninstall_open = false;
                        snapshot.uninstall_error.clear();
                    });
                    ui.set_page(1);
                    ui.set_detail_open(false);
                }
                Err(message) => edit(&state, |snapshot| {
                    snapshot.detail_error = message;
                }),
            }
        });
    }

    // ---- uninstall -----------------------------------------------------------------------
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_request_uninstall(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if state.uninstalling.load(Ordering::Relaxed) {
                return;
            }
            if state.snapshot.lock().map(|snapshot| snapshot.running).unwrap_or(false) {
                edit(&state, |snapshot| {
                    snapshot.detail_error = "有下载任务正在运行，请先完成或暂停后再卸载。".to_string();
                });
                return;
            }
            let Some((_, dir, status, _title)) = selected_install_target(&state) else {
                edit(&state, |snapshot| {
                    snapshot.detail_error = "没有找到本地安装目录，请刷新游戏库后重试。".to_string();
                });
                return;
            };
            if status == InstallStatus::NotInstalled
                && !dir.join(".egstore").is_dir()
                && !install::has_partial_install(&dir)
            {
                edit(&state, |snapshot| {
                    snapshot.detail_error = "没有检测到本地安装。".to_string();
                });
                return;
            }
            edit(&state, |snapshot| {
                snapshot.uninstall_open = true;
                snapshot.uninstall_busy = false;
                snapshot.uninstall_error.clear();
                snapshot.detail_error.clear();
                snapshot.detail_notice.clear();
            });
            ui.set_uninstall_open(true);
            ui.set_uninstall_busy(false);
            ui.set_uninstall_error(SharedString::default());
            ui.set_uninstall_delete_files(true);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_cancel_uninstall(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if state.snapshot.lock().map(|snapshot| snapshot.uninstall_busy).unwrap_or(false) {
                return;
            }
            edit(&state, |snapshot| {
                snapshot.uninstall_open = false;
                snapshot.uninstall_error.clear();
            });
            ui.set_uninstall_open(false);
            ui.set_uninstall_error(SharedString::default());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_confirm_uninstall(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if state.snapshot.lock().map(|snapshot| snapshot.running).unwrap_or(false) {
                edit(&state, |snapshot| {
                    snapshot.uninstall_error = "有下载任务正在运行，请先完成或暂停。".to_string();
                });
                return;
            }
            if state.uninstalling.swap(true, Ordering::SeqCst) {
                return;
            }

            let Some((app, dir, status, title)) = selected_install_target(&state) else {
                state.uninstalling.store(false, Ordering::SeqCst);
                edit(&state, |snapshot| {
                    snapshot.uninstall_error = "没有找到本地安装目录，请关闭后重试。".to_string();
                });
                return;
            };
            if status == InstallStatus::NotInstalled
                && !dir.join(".egstore").is_dir()
                && !install::has_partial_install(&dir)
            {
                state.uninstalling.store(false, Ordering::SeqCst);
                edit(&state, |snapshot| {
                    snapshot.uninstall_error = "没有检测到本地安装，无法卸载。".to_string();
                });
                return;
            }

            // A partial install always deletes its chunks; for a finished install the confirmation
            // switch decides whether the game files survive.
            let was_partial = status == InstallStatus::Partial;
            let keep_files = keep_files_for_uninstall(status, ui.get_uninstall_delete_files());
            edit(&state, |snapshot| {
                snapshot.uninstall_busy = true;
                snapshot.uninstall_error.clear();
            });
            ui.set_uninstall_busy(true);

            let worker = state.clone();
            std::thread::spawn(move || {
                let result = install::uninstall(&dir, keep_files, &app);
                let install_root = worker
                    .config
                    .lock()
                    .map(|cfg| PathBuf::from(&cfg.install_root))
                    .unwrap_or_default();
                if let Ok(mut library) = worker.library.lock() {
                    library.rescan_local(&install_root);
                }

                match result {
                    Ok(report) => {
                        let (notice, tone) =
                            format_uninstall_report(&report, &title, &dir, was_partial);
                        edit(&worker, |snapshot| {
                            snapshot.uninstall_busy = false;
                            snapshot.uninstall_open = false;
                            snapshot.detail_error.clear();
                            snapshot.detail_notice = notice;
                            snapshot.detail_notice_tone = tone;
                        });
                    }
                    Err(err) => edit(&worker, |snapshot| {
                        snapshot.uninstall_busy = false;
                        snapshot.uninstall_error = format!("卸载失败：{err}");
                    }),
                }
                worker.uninstalling.store(false, Ordering::SeqCst);
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_resume_job(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let pending = state
                .snapshot
                .lock()
                .ok()
                .map(|snapshot| (snapshot.job_app.clone(), snapshot.job_include_dlc));
            let Some((app_name, include_dlc)) = pending else { return };
            if app_name.is_empty() {
                return;
            }
            match begin_install(&state, &app_name, include_dlc) {
                Ok(()) => {
                    ui.set_page(1);
                }
                Err(message) => edit(&state, |snapshot| {
                    snapshot.note = message;
                    snapshot.note_error = true;
                }),
            }
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
                    snapshot.note = "正在停止当前任务，已完成的进度会保留…".to_string();
                    snapshot.note_error = false;
                }
            });
        });
    }
    {
        let state = state.clone();
        ui.on_cancel_job(move || {
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

            // Nothing is running: delete the partial files of the paused job.
            let target = state
                .snapshot
                .lock()
                .ok()
                .map(|snapshot| snapshot.job_target_dir.clone())
                .unwrap_or_default();
            if target.is_empty() {
                return;
            }
            let worker_state = state.clone();
            std::thread::spawn(move || {
                let report = install::discard_partial_install(std::path::Path::new(&target));
                edit(&worker_state, |snapshot| {
                    snapshot.job_can_resume = false;
                    if snapshot.selected_app == snapshot.job_app {
                        snapshot.can_resume = false;
                    }
                    snapshot.note_error = false;
                    snapshot.note = if report.kept_existing_install {
                        "已取消 · 目录里已有完整安装，未删除原有文件。".to_string()
                    } else if report.removed > 0 {
                        format!("已取消 · 已删除 {} 个未完成的文件。", report.removed)
                    } else {
                        "已取消 · 没有需要删除的文件。".to_string()
                    };
                });
                let install_root = worker_state
                    .config
                    .lock()
                    .map(|c| PathBuf::from(&c.install_root))
                    .unwrap_or_default();
                if let Ok(mut library) = worker_state.library.lock() {
                    library.rescan_local(&install_root);
                }
            });
        });
    }

    // ---- retry the space estimate --------------------------------------------------------
    {
        let state = state.clone();
        ui.on_retry_space(move || {
            let app = state.selected_app.lock().ok().and_then(|slot| slot.clone());
            if let Some(app) = app {
                if let Ok(mut space) = state.space.lock() {
                    space.remove(&app);
                }
            }
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
        let mut shown_cover_generation = u64::MAX;
        let mut last_cover_rebuild = Instant::now() - Duration::from_secs(1);
        let mut shown_job_cover_target = String::new();
        let mut shown_job_cover_loaded = false;
        // Free space changes while the window is open; keep the gate fresh without blocking the UI
        // thread on a possibly disconnected volume.
        let mut last_space_refresh = Instant::now() - SPACE_REFRESH_INTERVAL;

        timer.start(slint::TimerMode::Repeated, TICK, move || {
            let Some(ui) = ui_weak.upgrade() else { return };

            pulse += 0.028;
            if pulse > 1.0 {
                pulse = 0.0;
            }
            ui.set_pulse(pulse);

            // ---- library rows (library revision or a newly downloaded cover) ----
            let revision = state.library.lock().map(|library| library.revision).unwrap_or(0);
            let cover_generation = state.covers.generation();
            // Covers arrive a few at a time; rebuild at most four times a second so a large
            // library does not re-create its model for every finished image.
            let covers_changed = cover_generation != shown_cover_generation
                && last_cover_rebuild.elapsed() >= Duration::from_millis(250);
            if revision != shown_revision || covers_changed {
                let (
                    main_rows,
                    addon_rows,
                    base_count,
                    dlc_count,
                    installed_count,
                    with_dlc_count,
                    matched_games,
                    addon_shown,
                    query,
                    filter_mode,
                    loaded,
                    has_addons,
                ) = {
                    let Ok(library) = state.library.lock() else { return };
                    (
                        library.main_rows.clone(),
                        library.addon_rows.clone(),
                        library.base_count(),
                        library.dlc_count(),
                        library.installed_count,
                        library.with_dlc_count,
                        library.main_rows.len(),
                        library
                            .addon_rows
                            .iter()
                            .filter(|row| matches!(row.kind, RowKind::Addon | RowKind::Orphan))
                            .count(),
                        library.query.clone(),
                        library.filter_mode,
                        !library.all.is_empty(),
                        library.has_addons(),
                    )
                };
                // Decode outside the library lock; a cold cover can take a few milliseconds and the
                // refresh worker should never wait on the UI thread.
                let main_rows: Vec<LibraryRow> =
                    main_rows.iter().map(|row| row_to_ui(row, &state.covers)).collect();
                let addon_rows: Vec<LibraryRow> =
                    addon_rows.iter().map(|row| row_to_ui(row, &state.covers)).collect();

                ui.set_main_rows(ModelRc::new(VecModel::from(main_rows)));
                ui.set_addon_rows(ModelRc::new(VecModel::from(addon_rows)));
                ui.set_base_count(base_count as i32);
                ui.set_addon_total_count(dlc_count as i32);
                ui.set_installed_count(installed_count as i32);
                ui.set_with_dlc_count(with_dlc_count as i32);
                ui.set_main_count_text(SharedString::from(format!("{matched_games}")));
                ui.set_addon_count_text(SharedString::from(format!("{addon_shown}")));
                ui.set_library_loaded(loaded);
                ui.set_has_addons(has_addons);
                ui.set_games_count_text(SharedString::from(
                    if query.trim().is_empty() && filter_mode == 0 {
                        if installed_count > 0 {
                            format!(
                                "{base_count} 款主游戏 · {dlc_count} 个附加内容 · {installed_count} 款已安装"
                            )
                        } else {
                            format!("{base_count} 款主游戏 · {dlc_count} 个附加内容")
                        }
                    } else {
                        format!(
                            "当前显示 {matched_games} / {base_count} 款主游戏 · {dlc_count} 个附加内容"
                        )
                    },
                ));

                // Drop the selection only when the game no longer exists in the library at all
                // (e.g. after switching accounts).
                let selected = state.selected_app.lock().ok().and_then(|slot| slot.clone());
                let still_exists = selected.as_ref().is_some_and(|app| {
                    state
                        .library
                        .lock()
                        .map(|library| library.group_any(app).is_some())
                        .unwrap_or(false)
                });
                if selected.is_some() && !still_exists {
                    if let Ok(mut slot) = state.selected_app.lock() {
                        *slot = None;
                    }
                    ui.set_detail_open(false);
                }
                push_selection(&ui, &state);
                shown_revision = revision;
                shown_cover_generation = cover_generation;
                last_cover_rebuild = Instant::now();
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
            ui.set_speed_label(SharedString::from(snapshot.speed_label.clone()));
            ui.set_bytes_label(SharedString::from(snapshot.bytes_label.clone()));
            ui.set_chunks_label(SharedString::from(snapshot.chunks_label.clone()));
            ui.set_inflight_label(SharedString::from(snapshot.inflight_label.clone()));
            ui.set_elapsed_text(SharedString::from(snapshot.elapsed.clone()));
            ui.set_retries_text(SharedString::from(snapshot.retries.clone()));
            ui.set_note(SharedString::from(snapshot.note.clone()));
            ui.set_note_error(snapshot.note_error);

            ui.set_job_started(snapshot.job_started);
            ui.set_job_app(SharedString::from(snapshot.job_app.clone()));
            ui.set_job_title(SharedString::from(snapshot.job_title.clone()));
            ui.set_job_target_dir(SharedString::from(snapshot.job_target_dir.clone()));
            ui.set_job_can_resume(snapshot.job_can_resume);
            ui.set_job_tone(tone_for(&snapshot.job_app));
            ui.set_job_initial(SharedString::from(initial_of(&snapshot.job_title)));

            // The job cover may arrive after the download started; check the in-memory decoded map
            // instead of touching the disk on every tick.
            let job_cover_ready = state.covers.is_decoded(&snapshot.job_cover_url);
            if snapshot.job_cover_url != shown_job_cover_target
                || (job_cover_ready && !shown_job_cover_loaded)
            {
                let image = state.covers.image_for(&snapshot.job_cover_url);
                shown_job_cover_loaded = image.is_some();
                ui.set_job_has_cover(shown_job_cover_loaded);
                ui.set_job_cover(image.unwrap_or_default());
                shown_job_cover_target = snapshot.job_cover_url.clone();
            }

            ui.set_selected_title(SharedString::from(snapshot.selected_title.clone()));
            ui.set_selected_app(SharedString::from(snapshot.selected_app.clone()));
            ui.set_target_dir(SharedString::from(snapshot.target_dir.clone()));
            ui.set_can_resume(snapshot.can_resume);
            ui.set_selected_install_status(snapshot.selected_install_status);
            ui.set_selected_dlc_count(snapshot.selected_dlc_count as i32);
            ui.set_selected_dlc_text(SharedString::from(snapshot.selected_dlc_text.clone()));
            ui.set_detail_error(SharedString::from(snapshot.detail_error.clone()));
            ui.set_detail_notice(SharedString::from(snapshot.detail_notice.clone()));
            ui.set_detail_notice_tone(snapshot.detail_notice_tone);
            ui.set_uninstall_open(snapshot.uninstall_open);
            ui.set_uninstall_busy(snapshot.uninstall_busy);
            ui.set_uninstall_error(SharedString::from(snapshot.uninstall_error.clone()));

            // ---- pre-download space gate ----
            // Only the open drawer pays for the estimate; the volume reading is refreshed in the
            // background so a slow or disconnected drive never blocks this tick.
            let include_dlc = ui.get_include_dlc();
            if ui.get_detail_open() && !snapshot.selected_app.is_empty() {
                ensure_space_probe(&state, &snapshot.selected_app, &snapshot.target_dir);
                if last_space_refresh.elapsed() >= SPACE_REFRESH_INTERVAL {
                    refresh_space_volume(&state, &snapshot.selected_app);
                    last_space_refresh = Instant::now();
                }
            }
            let space = {
                let cache = state.space.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                space_view(&cache, &snapshot.selected_app, include_dlc)
            };
            ui.set_space_pending(space.pending);
            ui.set_space_known(space.known);
            ui.set_space_fits(space.fits);
            ui.set_install_enabled(space.install_enabled);
            ui.set_space_required_text(SharedString::from(space.required_text));
            ui.set_space_free_text(SharedString::from(space.free_text));
            ui.set_space_detail_text(SharedString::from(space.detail_text));
            ui.set_space_note(SharedString::from(space.note));
            ui.set_space_note_tone(space.note_tone);

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
            if snapshot.collapse_account_edit {
                ui.set_account_edit_open(false);
                edit(&state, |snapshot| snapshot.collapse_account_edit = false);
            }
        });
    }
    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(app: &str, title: &str) -> GameEntry {
        GameEntry {
            app_name: app.into(),
            namespace: "ns".into(),
            catalog_item_id: format!("cid-{app}"),
            title: title.into(),
            ..Default::default()
        }
    }

    fn dlc(app: &str, title: &str, parent: &str) -> GameEntry {
        GameEntry {
            is_dlc: true,
            parent_app_name: parent.into(),
            ..game(app, title)
        }
    }

    fn kinds(rows: &[Row]) -> Vec<RowKind> {
        rows.iter().map(|row| row.kind).collect()
    }

    #[test]
    fn main_rows_keep_order_and_carry_addon_counts() {
        let games = vec![
            game("Ginger", "Cyberpunk 2077"),
            dlc("phantom", "Phantom Liberty", "Ginger"),
            game("Other", "Other Game"),
        ];
        let view = library::library_view(&games);
        let rows = build_main_rows(&view.games, &HashMap::new());

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].app, "Ginger");
        assert_eq!(rows[0].badge, "DLC 1");
        assert!(rows[0].selectable);
        assert_eq!(rows[0].install_status, InstallStatus::NotInstalled);
        assert_eq!(rows[1].app, "Other");
        assert_eq!(rows[1].badge, "");
        assert_eq!(rows[1].title, "Other Game");
    }

    #[test]
    fn addon_rows_group_by_parent_and_skip_games_without_addons() {
        let games = vec![
            game("A", "Game A"),
            dlc("a2", "A Two", "A"),
            dlc("a1", "A One", "A"),
            game("B", "Game B"),
            dlc("b1", "B One", "B"),
            game("C", "Game C"),
        ];
        let view = library::library_view(&games);
        let rows = build_addon_rows(&view.games, &view.orphan_dlc);

        assert_eq!(
            kinds(&rows),
            vec![
                RowKind::GroupLabel,
                RowKind::Addon,
                RowKind::Addon,
                RowKind::GroupLabel,
                RowKind::Addon,
            ]
        );
        assert_eq!(rows[0].title, "Game A");
        assert_eq!(rows[0].sub, "2 项");
        assert_eq!(rows[0].app, "A");
        assert!(rows[0].selectable);
        // Add-ons are sorted by title inside their group by the core view.
        assert_eq!(rows[1].title, "A One");
        assert_eq!(rows[1].app, "A");
        assert!(rows[1].selectable);
        assert_eq!(rows[3].title, "Game B");
        assert_eq!(rows[3].app, "B");
        assert_eq!(rows[4].app, "B");
        assert!(!rows.iter().any(|row| row.title == "Game C"));
    }

    #[test]
    fn orphans_are_listed_but_not_selectable() {
        let games = vec![
            game("Ginger", "Cyberpunk 2077"),
            dlc("lost", "Lost Add-on", "Missing"),
        ];
        let view = library::library_view(&games);
        let rows = build_addon_rows(&view.games, &view.orphan_dlc);

        assert_eq!(kinds(&rows), vec![RowKind::GroupLabel, RowKind::Orphan]);
        assert_eq!(rows[0].title, "未关联主游戏");
        assert!(!rows[0].selectable);
        assert_eq!(rows[1].title, "Lost Add-on");
        assert_eq!(rows[1].badge, "未关联");
        assert!(!rows[1].selectable);
    }

    #[test]
    fn games_without_addons_produce_no_addon_rows() {
        let games = vec![game("Ginger", "Cyberpunk 2077")];
        let view = library::library_view(&games);
        assert!(build_addon_rows(&view.games, &view.orphan_dlc).is_empty());
    }

    #[test]
    fn display_title_falls_back_to_app_name() {
        assert_eq!(display_title(&game("Ginger", "")), "Ginger");
        assert_eq!(display_title(&game("Ginger", "Cyberpunk 2077")), "Cyberpunk 2077");
    }

    #[test]
    fn rows_carry_cover_and_developer_from_the_entry() {
        let mut base = game("Ginger", "Cyberpunk 2077");
        base.cover_url = "https://cdn.example/base".into();
        base.developer = "CD PROJEKT RED".into();
        let mut addon = dlc("phantom", "Phantom Liberty", "Ginger");
        addon.cover_url = "https://cdn.example/dlc".into();
        addon.developer = "CD PROJEKT RED".into();

        let view = library::library_view(&[base, addon]);
        let main = build_main_rows(&view.games, &HashMap::new());
        let addons = build_addon_rows(&view.games, &view.orphan_dlc);

        assert_eq!(main[0].cover_url, "https://cdn.example/base");
        assert_eq!(main[0].developer, "CD PROJEKT RED");
        assert_eq!(main[0].initial, "C");
        assert_eq!(addons[1].cover_url, "https://cdn.example/dlc");
        assert_eq!(addons[1].developer, "CD PROJEKT RED");
    }

    #[test]
    fn library_filters_by_install_status_and_dlc_while_preserving_addon_view() {
        let games = vec![
            game("A", "Alpha Installed"),
            dlc("a1", "Alpha Pack", "A"),
            game("B", "Beta Partial"),
            game("C", "Gamma Cloud Only"),
            dlc("c1", "Gamma Expansion", "C"),
        ];
        let mut lib = Library::default();
        lib.all = games;
        lib.local_status.insert("a".into(), (InstallStatus::Installed, PathBuf::from("D:/Games/A")));
        lib.local_status.insert("b".into(), (InstallStatus::Partial, PathBuf::from("D:/Games/B")));
        lib.refilter();

        assert_eq!(lib.installed_count, 2);
        assert_eq!(lib.with_dlc_count, 2);
        assert_eq!(lib.main_rows.len(), 3);

        // Filter = 1 (installed / partial)
        lib.set_filter(1);
        let apps: Vec<&str> = lib.main_rows.iter().map(|r| r.app.as_str()).collect();
        assert_eq!(apps, vec!["A", "B"]);
        assert_eq!(lib.main_rows[0].install_status, InstallStatus::Installed);
        assert_eq!(lib.main_rows[1].install_status, InstallStatus::Partial);
        // Add-on view stays intact for C's DLC even when main-game filter is set to installed.
        assert!(lib.addon_rows.iter().any(|r| r.app == "C"));

        // Filter = 2 (with DLC)
        lib.set_filter(2);
        let apps: Vec<&str> = lib.main_rows.iter().map(|r| r.app.as_str()).collect();
        assert_eq!(apps, vec!["A", "C"]);

        // Combined with search query
        lib.set_query("gamma");
        assert_eq!(lib.main_rows.len(), 1);
        assert_eq!(lib.main_rows[0].app, "C");
    }

    #[test]
    fn scan_local_installs_detects_installed_and_partial_games() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("skiff-ui-scan-test-{nonce}"));
        let _ = std::fs::remove_dir_all(&root);

        // Installed game with `.egstore/*.item` + matching `.manifest`
        let installed_eg = root.join("GameA").join(".egstore");
        std::fs::create_dir_all(&installed_eg).unwrap();
        std::fs::write(
            installed_eg.join("AAA.item"),
            r#"{"AppName":"AppA","MainGameAppName":"AppA","InstallationGuid":"AAA","DisplayName":"Game A"}"#,
        )
        .unwrap();
        std::fs::write(installed_eg.join("AAA.manifest"), b"stub-manifest").unwrap();

        // Partial download with `skiff.resume` (no `.item` yet)
        let partial_eg = root.join("AppB").join(".egstore");
        std::fs::create_dir_all(&partial_eg).unwrap();
        std::fs::write(partial_eg.join("skiff.resume"), b"{}").unwrap();

        // Broken `.item` without a matching `.manifest` must not count as installed
        let broken_eg = root.join("GameC").join(".egstore");
        std::fs::create_dir_all(&broken_eg).unwrap();
        std::fs::write(
            broken_eg.join("CCC.item"),
            r#"{"AppName":"AppC","MainGameAppName":"AppC","InstallationGuid":"CCC"}"#,
        )
        .unwrap();

        let games = vec![game("AppA", "Game A"), game("AppB", "Game B"), game("AppC", "Game C")];
        let map = scan_local_installs(&root, &games);

        assert_eq!(map.get("appa").map(|(s, _)| *s), Some(InstallStatus::Installed));
        assert_eq!(map.get("appb").map(|(s, _)| *s), Some(InstallStatus::Partial));
        assert!(!map.contains_key("appc"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn placeholder_tone_is_stable() {
        assert_eq!(tone_for("Ginger"), tone_for("Ginger"));
        // Different names usually get different tones, but the palette is intentionally small;
        // only determinism is a contract here.
        let _ = tone_for("Other");
    }

    #[test]
    fn download_progress_maps_bytes_to_the_bar() {
        let mut snapshot = Snapshot::idle();
        let progress = Progress {
            phase: Phase::Download,
            chunks_total: 10,
            chunks_done: 4,
            bytes_total: 200,
            bytes_done: 50,
            inflight: 3,
            peak_inflight: 8,
            speed_bps: 1024.0,
            eta_secs: Some(120.0),
            ..Default::default()
        };
        apply_progress(&mut snapshot, &progress, Instant::now());

        assert_eq!(snapshot.phase, "下载中");
        assert_eq!(snapshot.phase_kind, 1);
        assert!((snapshot.progress - 0.25).abs() < 0.001);
        assert_eq!(snapshot.percent, "25.0%");
        assert_eq!(snapshot.chunks, "4 / 10");
        assert_eq!(snapshot.inflight, "3 / 8");
        assert_eq!(snapshot.speed_label, "下载速度");
        assert_eq!(snapshot.bytes_label, "已下载 / 总计");
        assert_eq!(snapshot.chunks_label, "分块进度");
        assert_eq!(snapshot.inflight_label, "在途 / 峰值并发");
        assert_eq!(snapshot.eta, "2 分 0 秒");
        assert!(snapshot.note.is_empty());
    }

    #[test]
    fn verify_progress_uses_verify_counters_and_labels() {
        let mut snapshot = Snapshot::idle();
        let progress = Progress {
            phase: Phase::Verify,
            verify: skiff_core::download::VerifyProgress {
                files_total: 8,
                files_done: 2,
                bytes_total: 1000,
                bytes_done: 250,
                speed_bps: 2048.0,
                eta_secs: Some(1.5),
                threads: 4,
            },
            ..Default::default()
        };
        apply_progress(&mut snapshot, &progress, Instant::now());

        assert_eq!(snapshot.phase, "校验中");
        assert_eq!(snapshot.phase_kind, 5);
        assert!((snapshot.progress - 0.25).abs() < 0.001);
        assert_eq!(snapshot.percent, "25.0%");
        assert_eq!(snapshot.chunks, "2 / 8");
        assert_eq!(snapshot.inflight, "4 线程");
        assert_eq!(snapshot.speed_label, "校验速度");
        assert_eq!(snapshot.bytes_label, "已校验 / 总计");
        assert_eq!(snapshot.chunks_label, "文件进度");
        assert_eq!(snapshot.inflight_label, "校验线程");
        assert!(snapshot.retries.is_empty());
        assert!(snapshot.note.contains("校验"));
    }

    #[test]
    fn zero_byte_verification_falls_back_to_file_counts() {
        let mut snapshot = Snapshot::idle();
        let progress = Progress {
            phase: Phase::Verify,
            verify: skiff_core::download::VerifyProgress {
                files_total: 4,
                files_done: 1,
                threads: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        apply_progress(&mut snapshot, &progress, Instant::now());

        assert!((snapshot.progress - 0.25).abs() < 0.001);
        assert_eq!(snapshot.percent, "25.0%");
    }

    #[test]
    fn repair_progress_reads_like_a_repair() {
        let mut snapshot = Snapshot::idle();
        let progress = Progress {
            phase: Phase::Repair,
            chunks_total: 5,
            chunks_done: 1,
            bytes_total: 100,
            bytes_done: 20,
            ..Default::default()
        };
        apply_progress(&mut snapshot, &progress, Instant::now());

        assert_eq!(snapshot.phase, "修复中");
        assert_eq!(snapshot.phase_kind, 1);
        assert_eq!(snapshot.percent, "20.0%");
        assert_eq!(snapshot.speed_label, "修复速度");
        assert_eq!(snapshot.bytes_label, "已修复 / 总计");
        assert!(snapshot.note.contains("重新下载"));
    }

    #[test]
    fn uninstall_switch_keeps_files_only_for_a_finished_install() {
        assert!(!keep_files_for_uninstall(InstallStatus::Installed, true));
        assert!(keep_files_for_uninstall(InstallStatus::Installed, false));
        assert!(
            !keep_files_for_uninstall(InstallStatus::Partial, false),
            "a partial install always deletes its chunks"
        );
    }

    #[test]
    fn uninstall_report_summarizes_a_full_uninstall() {        let report = install::UninstallReport {
            components: 2,
            files_removed: 12,
            bytes_removed: 2048,
            metadata_removed: true,
            install_dir_removed: true,
            launcher_unregistered: true,
            ..Default::default()
        };

        let (text, tone) =
            format_uninstall_report(&report, "Brotato", Path::new("D:/Games/Brotato"), false);

        assert_eq!(tone, 1);
        assert!(text.contains("已卸载《Brotato》"));
        assert!(text.contains("删除 12 个文件"));
        assert!(text.contains("2.00 KiB"));
        assert!(text.contains("安装目录已移除"));
        assert!(text.contains("Epic 启动器记录已清理"));
        assert!(!text.contains("未能删除"));
    }

    #[test]
    fn uninstall_report_names_the_kept_directory_and_launcher_restart() {
        let report = install::UninstallReport {
            keep_files: true,
            metadata_removed: true,
            launcher_unregistered: true,
            launcher_running: true,
            leftover_entries: 4,
            ..Default::default()
        };

        let (text, tone) =
            format_uninstall_report(&report, "Brotato", Path::new("D:/Games/Brotato"), false);

        assert_eq!(tone, 2);
        assert!(text.contains("已从库里移除《Brotato》"));
        assert!(text.contains("游戏文件保留在 D:/Games/Brotato"));
        assert!(text.contains("从托盘完全退出"));
    }

    #[test]
    fn uninstall_report_warns_about_failed_files_and_leftovers() {
        let report = install::UninstallReport {
            files_removed: 9,
            failed_count: 2,
            metadata_removed: true,
            install_dir_removed: false,
            leftover_entries: 3,
            launcher_unregistered: true,
            ..Default::default()
        };

        let (text, tone) =
            format_uninstall_report(&report, "Brotato", Path::new("D:/Games/Brotato"), false);

        assert_eq!(tone, 2);
        assert!(text.contains("2 个文件未能删除"));
        assert!(text.contains("安装目录仍在（3 项未列入清单）"));
    }

    #[test]
    fn partial_uninstall_report_reads_as_cleanup() {
        let report = install::UninstallReport {
            files_removed: 5,
            metadata_removed: true,
            install_dir_removed: true,
            launcher_unregistered: true,
            ..Default::default()
        };

        let (text, tone) =
            format_uninstall_report(&report, "Brotato", Path::new("D:/Games/Brotato"), true);

        assert_eq!(tone, 1);
        assert!(text.contains("已清理《Brotato》的未完成安装"));
    }

    // ---- pre-download space gate ---------------------------------------------------------

    fn sizes(install: u64, needed: u64) -> install::InstallSizes {
        install::InstallSizes { install_bytes: install, needed_bytes: needed }
    }

    fn ready(needed: u64, free: Option<u64>) -> SpaceEntry {
        SpaceEntry::Ready(ReadySpace {
            preview: install::SpacePreview {
                target_dir: PathBuf::from("D:\\Games\\Test"),
                base_only: sizes(needed + 10, needed),
                with_dlc: sizes(needed + 100, needed + 90),
                skipped_dlc: Vec::new(),
            },
            volume: free.map(|free_bytes| disk::VolumeSpace {
                free_bytes,
                total_bytes: 1_000,
            }),
        })
    }

    fn cache_with(entry: SpaceEntry) -> SpaceCache {
        let mut cache = SpaceCache::default();
        cache.entries.insert("game".to_string(), entry);
        cache
    }

    #[test]
    fn space_gate_enables_the_button_when_the_volume_fits() {
        let view = space_view(&cache_with(ready(100, Some(400))), "Game", false);
        assert!(view.known);
        assert!(view.fits);
        assert!(view.install_enabled);
        assert_eq!(view.required_text, human_bytes(100));
        assert_eq!(view.free_text, human_bytes(400));
        assert_eq!(view.note_tone, 1);
        assert!(view.note.contains("空间充足"), "{}", view.note);
    }

    #[test]
    fn space_gate_blocks_the_button_when_the_volume_is_short() {
        let view = space_view(&cache_with(ready(100, Some(40))), "Game", false);
        assert!(view.known);
        assert!(!view.fits);
        assert!(!view.install_enabled, "a known shortfall must disable the download button");
        assert_eq!(view.note_tone, 3);
        assert!(view.note.contains("还差"), "{}", view.note);
        assert!(view.note.contains(&human_bytes(60)), "{}", view.note);
    }

    #[test]
    fn space_gate_uses_the_add_on_total_when_dlc_are_included() {
        let cache = cache_with(ready(100, Some(150)));
        let base = space_view(&cache, "Game", false);
        let bundle = space_view(&cache, "Game", true);
        assert_eq!(base.required_text, human_bytes(100));
        assert_eq!(bundle.required_text, human_bytes(190));
        // 190 > 150: only the bundle exceeds the volume.
        assert!(base.install_enabled);
        assert!(!bundle.install_enabled);
    }

    #[test]
    fn space_gate_waits_while_the_estimate_runs() {
        let mut cache = SpaceCache::default();
        cache.probing = Some(("game".to_string(), Arc::new(AtomicBool::new(false))));
        let view = space_view(&cache, "Game", false);
        assert!(view.pending);
        assert!(!view.known);
        assert!(!view.install_enabled, "the button waits for the estimate");
        assert!(view.note.contains("正在解析"), "{}", view.note);
    }

    #[test]
    fn space_gate_stays_usable_when_the_estimate_failed() {
        let view = space_view(&cache_with(SpaceEntry::Failed("network down".into())), "Game", false);
        assert!(!view.known);
        assert!(view.install_enabled, "an unknown estimate must not block the download");
        assert_eq!(view.note_tone, 2);
        assert!(view.note.contains("network down"), "{}", view.note);
    }

    #[test]
    fn space_gate_reports_an_unreadable_volume_without_blocking() {
        let view = space_view(&cache_with(ready(100, None)), "Game", false);
        assert!(view.known);
        assert!(view.install_enabled, "a failed volume query is unknown, not a shortfall");
        assert_eq!(view.free_text, "—");
        assert!(view.note.contains("暂不做空间检查"), "{}", view.note);
    }

    #[test]
    fn space_gate_treats_a_provisioned_install_as_free() {
        let view = space_view(&cache_with(ready(0, Some(0))), "Game", false);
        assert!(view.known);
        assert!(view.install_enabled);
        assert_eq!(view.required_text, "无需额外空间");
        assert!(view.note.contains("无需额外空间"), "{}", view.note);
    }

    #[test]
    fn space_gate_is_open_when_nothing_is_selected() {
        let view = space_view(&SpaceCache::default(), "", false);
        assert!(!view.pending);
        assert!(view.install_enabled);
        assert_eq!(view.required_text, "—");
    }
}
