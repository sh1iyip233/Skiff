//! `skiff` command line frontend.
//!
//! Argument parsing is hand written on purpose: the network in the build environment blocks the
//! `windows-sys` crates that `clap` transitively needs, so this keeps the build offline-capable.

use skiff_core::api::{Api, GameEntry};
use skiff_core::config::{Config, Concurrency};
use skiff_core::download::Progress;
use skiff_core::error::{Error, Result};
use skiff_core::http::Client;
use skiff_core::install::{self, InstallOptions};
use skiff_core::library;
use skiff_core::manifest::Manifest;
use skiff_core::{auth, util};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

mod web;

const USAGE: &str = "\
skiff - third-party Epic Games library downloader

USAGE:
  skiff auth login [--device-code | --code <authorizationCode>]
  skiff auth logout | status
  skiff library list [--search <text>] [--all] [--refresh] [--json]
  skiff info <appName> [--json]
  skiff install <appName> [-j <n>] [--write-threads <n>] [--per-host <n>]
                         [--limit-rate <50M>] [--dir <path>] [--platform <p>]
                         [--label <l>] [--no-dlc] [--verify] [--repair]
                         [--register-launcher] [-y]
  skiff dlc list <appName> [--dir <path>] [--json]
  skiff dlc add <appName> [<dlcName>...] [-j <n>] [--write-threads <n>] [--per-host <n>]
                          [--limit-rate <50M>] [--dir <path>] [--register-launcher]
                          [--dry-run] [-y]
  skiff verify <installDir> [--repair]
  skiff uninstall <installDir> [--keep-files]
  skiff config get | set <key> <value> | list
  skiff serve [--port <n>] [--open]
  skiff debug manifest <file> [--json]
  skiff debug cdn-probe <appName>

GLOBAL:
  --json          machine readable output
  --no-progress   disable the live progress line
  -v              verbose (unused, reserved)

Exit codes: 0 success, 1 runtime error, 2 usage error, 130 interrupted.";

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(Error::Usage(msg)) => {
            eprintln!("error: {msg}");
            eprintln!("\n{USAGE}");
            2
        }
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    };
    std::process::exit(code);
}

#[derive(Debug, Default)]
struct Args {
    positional: Vec<String>,
    flags: HashMap<String, String>,
    switches: HashSet<String>,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Args> {
        let mut args = Args::default();
        let mut iter = raw.iter().peekable();
        while let Some(token) = iter.next() {
            if token == "--" {
                for rest in iter.by_ref() {
                    args.positional.push(rest.clone());
                }
                break;
            }
            if let Some(rest) = token.strip_prefix("--") {
                if let Some((name, value)) = rest.split_once('=') {
                    args.flags.insert(name.to_string(), value.to_string());
                } else if matches!(
                    rest,
                    "json" | "no-progress" | "verify" | "repair" | "register-launcher" | "keep-files"
                        | "device-code" | "yes" | "verbose" | "auto-concurrency" | "open"
                        | "refresh" | "all" | "no-dlc" | "dir-is-exact" | "dry-run"
                ) || rest == "y"
                {
                    args.switches.insert(rest.to_string());
                } else {
                    let value = iter.next().ok_or_else(|| {
                        Error::usage(format!("--{rest} requires a value"))
                    })?;
                    args.flags.insert(rest.to_string(), value.clone());
                }
            } else if let Some(rest) = token.strip_prefix("-j") {
                if rest.is_empty() {
                    let value = iter
                        .next()
                        .ok_or_else(|| Error::usage("-j requires a value"))?;
                    args.flags.insert("concurrency".into(), value.clone());
                } else {
                    args.flags.insert("concurrency".into(), rest.to_string());
                }
            } else if token == "-y" {
                args.switches.insert("yes".into());
            } else if token == "-v" {
                args.switches.insert("verbose".into());
            } else {
                args.positional.push(token.clone());
            }
        }
        Ok(args)
    }

    fn has(&self, name: &str) -> bool {
        self.switches.contains(name)
    }

    fn flag(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(|s| s.as_str())
    }

    fn positional_at(&self, index: usize) -> Option<&str> {
        self.positional.get(index).map(|s| s.as_str())
    }

    fn required(&self, index: usize, what: &str) -> Result<&str> {
        self.positional_at(index)
            .ok_or_else(|| Error::usage(format!("missing {what}")))
    }

    fn usize_flag(&self, name: &str) -> Result<Option<usize>> {
        match self.flag(name) {
            None => Ok(None),
            Some(text) => text
                .parse::<usize>()
                .map(Some)
                .map_err(|_| Error::usage(format!("--{name} expects a number, got '{text}'"))),
        }
    }
}

fn run() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    let args = Args::parse(&raw)?;
    let json = args.has("json");
    let cfg = Config::load()?;

    match args.positional_at(0) {
        Some("auth") => cmd_auth(&args, &cfg, json),
        Some("library") => cmd_library(&args, &cfg, json),
        Some("info") => cmd_info(&args, &cfg, json),
        Some("install") => cmd_install(&args, &cfg, json),
        Some("dlc") => cmd_dlc(&args, &cfg, json),
        Some("verify") => cmd_verify(&args, &cfg),
        Some("uninstall") => cmd_uninstall(&args, &cfg),
        Some("config") => cmd_config(&args, &cfg),
        Some("serve") => cmd_serve(&args, &cfg),
        Some("debug") => cmd_debug(&args, &cfg, json),
        Some("help") | Some("-h") | Some("--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(Error::usage(format!("unknown command '{other}'"))),
        None => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

fn client() -> Client {
    Client::new()
}

fn api<'a>(client: &'a Client, cfg: &'a Config, tokens: &'a auth::TokenSet) -> Api<'a> {
    Api::new(client, cfg, &tokens.access_token)
}

// ---------------------------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------------------------

fn cmd_auth(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    let client = client();
    match args.positional_at(1) {
        Some("login") => {
            auth::ensure_store_writable()?;
            if let Some(code) = args.flag("code") {
                let code = extract_auth_code(code);
                let tokens = auth::login_with_auth_code(cfg, &client, &code)?;
                auth::save_tokens(&tokens)?;
                print_account(&tokens, json);
                return Ok(());
            }

            // Interactive path: print the URL, then wait for the user to paste the code. Doing the
            // prompt first removes the "the code expires while I type the command" race.
            eprintln!("步骤 1：在浏览器登录 https://www.epicgames.com （已登录可跳过）");
            eprintln!("步骤 2：打开下面的地址，页面会返回一段 JSON：");
            eprintln!();
            eprintln!(
                "    https://www.epicgames.com/id/api/redirect?clientId={}&responseType=code",
                cfg.client_id
            );
            eprintln!();
            eprintln!("步骤 3：复制其中 authorizationCode 的值，粘贴到这里（也可整段 JSON 粘进来）。");
            eprintln!("        直接回车则回退到设备码流程。");
            eprint!("authorizationCode> ");
            std::io::Write::flush(&mut std::io::stdout()).ok();

            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let code = extract_auth_code(&line);
            if !code.is_empty() {
                let tokens = auth::login_with_auth_code(cfg, &client, &code)?;
                auth::save_tokens(&tokens)?;
                print_account(&tokens, json);
                return Ok(());
            }

            // Device-code fallback. Epic currently rejects this grant for the launcher client, so
            // translate the 401 into actionable advice instead of a raw API error.
            let prompt = auth::request_device_code(cfg, &client).map_err(|e| match e {
                Error::Api { status: 401, .. } => Error::Auth(
                    "Epic 未对该客户端开放设备码登录（HTTP 401）；请改用上面的 authorizationCode 方式"
                        .to_string(),
                ),
                other => other,
            })?;
            eprintln!("Open {} and confirm code {}", prompt.verification_uri, prompt.user_code);
            let tokens = auth::poll_device_code(cfg, &client, &prompt, |remaining| {
                eprintln!("  still waiting, {remaining}s left");
            })?;
            auth::save_tokens(&tokens)?;
            print_account(&tokens, json);
            Ok(())
        }
        Some("logout") => {
            if let Some(tokens) = auth::load_tokens()? {
                auth::logout(cfg, &client, &tokens)?;
                println!("logged out {}", tokens.display_name);
            } else {
                println!("not logged in");
            }
            Ok(())
        }
        Some("status") | None => {
            let accounts = auth::accounts()?;
            if accounts.is_empty() {
                if json {
                    println!("{}", serde_json::json!({ "logged_in": false }));
                } else {
                    println!("not logged in; run `skiff auth login`");
                }
                return Ok(());
            }
            if json {
                let payload: Vec<serde_json::Value> = accounts
                    .iter()
                    .map(|(id, name)| serde_json::json!({ "account_id": id, "display_name": name }))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "logged_in": true, "accounts": payload })
                );
            } else {
                for (id, name) in accounts {
                    println!("{name}  ({id})");
                }
            }
            Ok(())
        }
        Some(other) => Err(Error::usage(format!("unknown auth command '{other}'"))),
    }
}

fn print_account(tokens: &auth::TokenSet, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "account_id": tokens.account_id,
                "display_name": tokens.display_name,
                "expires_at": tokens.expires_at,
            })
        );
    } else {
        println!(
            "logged in as {} ({})",
            if tokens.display_name.is_empty() { "unknown" } else { &tokens.display_name },
            tokens.account_id
        );
    }
}

// ---------------------------------------------------------------------------------------------
// library
// ---------------------------------------------------------------------------------------------

fn cmd_library(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    // Cache-first: a full fetch costs ~18s (library pagination + catalog titles for 120+ namespaces)
    // while the library itself changes rarely. `--refresh` forces the network path.
    let refresh = args.has("refresh");
    let cached = if refresh {
        Vec::new()
    } else {
        install::load_cached_games().unwrap_or_default()
    };
    let games = if cached.is_empty() {
        let client = client();
        let tokens = require_tokens(cfg, &client)?;
        let api = api(&client, cfg, &tokens);
        let fetched = api.games(&tokens.account_id)?;
        // Caching is an optimisation; a read-only state directory must not break listing.
        if let Err(e) = install::cache_games(&fetched) {
            eprintln!("warning: could not write the game cache ({e})");
        }
        fetched
    } else {
        if !json {
            eprintln!(
                "note: showing the cached library ({} game(s)); use --refresh to fetch",
                cached.len()
            );
        }
        cached
    };

    let search = args.flag("search").unwrap_or_default().to_lowercase();
    let view = library::library_view(&games).filtered(&search);
    let show_all = args.has("all");

    if json {
        let mut payload = serde_json::to_value(&view).unwrap_or(serde_json::Value::Null);
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(view.games.len()));
        }
        println!("{payload}");
        return Ok(());
    }

    if view.games.is_empty() && view.orphan_dlc.is_empty() {
        println!("no games matched");
        return Ok(());
    }

    if show_all {
        println!("{:<42} {:<30} PARENT", "TITLE", "APP NAME");
        for group in &view.games {
            println!(
                "{:<42} {:<30}",
                truncate(&group.game.title, 42),
                truncate(&group.game.app_name, 30)
            );
            for dlc in &group.dlc {
                println!(
                    "  - {:<38} {:<30} {}",
                    truncate(&dlc.title, 38),
                    truncate(&dlc.app_name, 30),
                    truncate(&group.game.title, 30)
                );
            }
        }
        for dlc in &view.orphan_dlc {
            println!(
                "{:<42} {:<30} （未关联 DLC）",
                truncate(&dlc.title, 42),
                truncate(&dlc.app_name, 30)
            );
        }
    } else {
        println!("{:<42} {:<30} DLC", "TITLE", "APP NAME");
        for group in &view.games {
            println!(
                "{:<42} {:<30} {}",
                truncate(&group.game.title, 42),
                truncate(&group.game.app_name, 30),
                group.dlc.len()
            );
        }
        if !view.orphan_dlc.is_empty() {
            println!(
                "\nnote: {} DLC(s) have no main game in the library; use --all to show them",
                view.orphan_dlc.len()
            );
        }
    }

    println!(
        "\n{} game(s) · {} DLC(s)",
        view.games.len(),
        view.dlc_count
    );
    Ok(())
}

fn cmd_info(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    let app = args.required(1, "app name")?;
    let client = client();
    let tokens = require_tokens(cfg, &client)?;
    let api = api(&client, cfg, &tokens);
    let games = load_games(&api, &tokens)?;
    let choice = select_bundle(&games, app)?;
    let game = choice.selected.clone();
    let build = api.build(&game)?;

    if json {
        let payload = serde_json::json!({
            "game": game,
            "build": build,
            "parent": if choice.orphan {
                serde_json::Value::Null
            } else if choice.selected.is_dlc {
                serde_json::json!(choice.base)
            } else {
                serde_json::Value::Null
            },
            "dlc": if choice.selected.is_dlc { serde_json::json!([]) } else { serde_json::json!(choice.dlc) },
        });
        println!("{payload}");
        return Ok(());
    }
    println!("title      : {}", game.title);
    println!("app name   : {}", game.app_name);
    println!("namespace  : {}", game.namespace);
    println!("catalog id : {}", game.catalog_item_id);
    println!("folder     : {}", game.folder_name);
    if choice.selected.is_dlc {
        if choice.orphan {
            println!("dlc of     : （未找到主游戏）");
        } else {
            println!("dlc of     : {} ({})", choice.base.title, choice.base.app_name);
        }
    } else {
        println!("dlc        : {} owned", choice.dlc.len());
        for dlc in &choice.dlc {
            println!("             - {} ({})", dlc.title, dlc.app_name);
        }
    }
    println!("build      : {}", build.build_version);
    println!("label      : {}", build.label_name);
    println!("manifest   : {}", build.manifest_uri);
    println!("cdn hosts  : {}", build.base_urls.len());
    let raw = api.manifest(&build.manifest_uri)?;
    let manifest = Manifest::parse(&raw)?;
    println!("manifest   : level {} ({} bytes)", manifest.version, raw.len());
    println!("chunks     : {}", manifest.chunks.len());
    println!("files      : {}", manifest.files.len());
    println!("install    : {}", util::human_bytes(manifest.install_size()));
    println!("download   : {}", util::human_bytes(manifest.download_size()));
    println!("launch exe : {}", manifest.meta.launch_exe);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------------------------

fn cmd_install(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    let app = args.required(1, "app name")?;
    // Validate the concurrency contract before touching the network: an out-of-range value must be
    // a usage error regardless of login state.
    let concurrency = concurrency_from(args, cfg)?;
    let client = client();
    let tokens = require_tokens(cfg, &client)?;
    let api = api(&client, cfg, &tokens);
    let games = load_games(&api, &tokens)?;
    let choice = select_bundle(&games, app)?;
    if choice.orphan {
        return Err(Error::usage(format!(
            "'{}' is a DLC but its main game is not in this library; it cannot be installed alone",
            choice.selected.title
        )));
    }
    let game = choice.base.clone();
    let include_dlc = !args.has("no-dlc");
    let dlcs = if include_dlc { choice.dlc.clone() } else { Vec::new() };

    let limit = match args.flag("limit-rate").or(cfg.limit_rate.as_deref()) {
        Some(text) => util::parse_rate(text)?,
        None => None,
    };
    let install_dir = match args.flag("dir") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if args.has("dir-is-exact") {
                path
            } else {
                install::default_install_dir(&path, &game)
            }
        }
        None => {
            let root = PathBuf::from(&cfg.install_root);
            install::find_existing_install(&root, &game.app_name)
                .unwrap_or_else(|| install::default_install_dir(&root, &game))
        }
    };

    if !json {
        if choice.selected.is_dlc {
            println!(
                "note: {} is DLC of {} ({}); installing the main game",
                choice.selected.title, game.title, game.app_name
            );
        }
        println!("installing {} -> {}", game.title, install_dir.display());
        if include_dlc {
            println!(
                "dlc: {} owned{}",
                dlcs.len(),
                if dlcs.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ({}) -- use --no-dlc to install the main game only",
                        dlcs.iter()
                            .map(|dlc| dlc.title.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
        } else {
            println!("dlc: skipped (--no-dlc)");
        }
        println!(
            "concurrency: {} downloads, {} writers, verify {}, per-host {}",
            concurrency.downloads,
            concurrency.writers,
            concurrency.verify_threads(),
            concurrency
                .per_host
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unlimited".to_string())
        );
    }

    let interactive = !json && !args.has("no-progress") && std::io::stdout().is_terminal();
    let ui = Arc::new(std::sync::Mutex::new(ProgressUi::new(interactive)));
    let callback = {
        let ui = ui.clone();
        Arc::new(move |progress: Progress| {
            if let Ok(mut ui) = ui.lock() {
                ui.render(&progress);
            }
        }) as Arc<dyn Fn(Progress) + Send + Sync>
    };

    let options = InstallOptions {
        install_dir: install_dir.clone(),
        concurrency: concurrency.clone(),
        limit_bytes_per_sec: limit,
        verify: true,
        repair: args.has("repair"),
        register_launcher: args.has("register-launcher") || cfg.register_launcher,
    };

    let started = Instant::now();
    let outcome =
        install::install_bundle(&client, &api, &game, &dlcs, &options, Some(callback))?;
    if interactive {
        let mut ui = ui.lock().unwrap();
        ui.finish();
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "app_name": outcome.app_name,
                "build_version": outcome.build_version,
                "install_dir": install_dir.to_string_lossy(),
                "files": outcome.files,
                "install_bytes": outcome.install_bytes,
                "components": outcome.components,
                "dlc": outcome.dlc_apps,
                "skipped_dlc": outcome.skipped_dlc,
                "conflicts": outcome.conflicts,
                "chunks_downloaded": outcome.chunks_downloaded,
                "resumed_chunks": outcome.resumed_chunks,
                "repaired_chunks": outcome.repaired_chunks,
                "peak_inflight": outcome.peak_inflight,
                "elapsed_secs": outcome.elapsed_secs,
                "launcher_registered": outcome.launcher_registered,
                "launcher_running": outcome.launcher_running,
                "launcher_error": outcome.launcher_error,
            })
        );
        return Ok(());
    }

    println!();
    println!("installed {}", outcome.app_name);
    println!("  version     : {}", outcome.build_version);
    println!("  location    : {}", install_dir.display());
    println!("  components  : {}", outcome.components);
    if !outcome.dlc_apps.is_empty() {
        println!("  dlc         : {}", outcome.dlc_apps.join(", "));
    }
    if !outcome.skipped_dlc.is_empty() {
        println!("  dlc skipped : {}", outcome.skipped_dlc.join(", "));
    }
    if outcome.conflicts > 0 {
        println!("  conflicts   : {} file(s) (kept the main game version)", outcome.conflicts);
    }
    println!("  files       : {}", outcome.files);
    println!("  size        : {}", util::human_bytes(outcome.install_bytes));
    println!("  downloaded  : {} chunks", outcome.chunks_downloaded);
    if outcome.resumed_chunks > 0 {
        println!("  resumed     : {} chunks", outcome.resumed_chunks);
    }
    if outcome.repaired_chunks > 0 {
        println!("  repaired    : {} chunks", outcome.repaired_chunks);
    }
    println!(
        "  peak inflight: {} (configured {})",
        outcome.peak_inflight, concurrency.downloads
    );
    println!("  elapsed     : {:.1}s", started.elapsed().as_secs_f64());
    if outcome.launcher_registered {
        println!("  launcher    : registered with Epic Games Launcher");
        if outcome.launcher_running {
            println!(
                "  提示        : Epic 启动器正在运行。它只在启动时读取安装清单，\
请从托盘菜单完全退出（关闭窗口不算）后重新打开，游戏才会出现在库里。"
            );
        } else {
            println!("  提示        : 下次打开 Epic 启动器就能在库里看到这个游戏。");
        }
    } else if let Some(error) = &outcome.launcher_error {
        println!("  launcher    : not registered ({error})");
        println!("  提示        : 游戏文件本身已装好，只是官方启动器不会显示为已安装。");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// dlc
// ---------------------------------------------------------------------------------------------

fn cmd_dlc(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    match args.positional_at(1) {
        Some("list") => cmd_dlc_list(args, cfg, json),
        Some("add") => cmd_dlc_add(args, cfg, json),
        Some(other) => Err(Error::usage(format!("unknown dlc command '{other}'"))),
        None => Err(Error::usage("dlc needs a subcommand: list | add")),
    }
}

/// Where a game lives: `--dir`, else the existing install under the configured root, else the
/// folder name the game would get on a fresh install.
///
/// `--dir` accepts either the install directory itself or a parent that contains it; an add-on can
/// only be added to an existing install, so an exact match wins over folder-name construction.
fn resolve_install_dir(args: &Args, cfg: &Config, game: &GameEntry) -> PathBuf {
    if let Some(dir) = args.flag("dir") {
        let path = PathBuf::from(dir);
        if args.has("dir-is-exact") {
            return path;
        }
        let is_this_install = install::load_installed_components(&path)
            .map(|components| {
                components
                    .iter()
                    .any(|component| component.app_name.eq_ignore_ascii_case(&game.app_name))
            })
            .unwrap_or(false);
        if is_this_install {
            return path;
        }
        if let Some(found) = install::find_existing_install(&path, &game.app_name) {
            return found;
        }
        return install::default_install_dir(&path, game);
    }
    let root = PathBuf::from(&cfg.install_root);
    install::find_existing_install(&root, &game.app_name)
        .unwrap_or_else(|| install::default_install_dir(&root, game))
}

fn cmd_dlc_list(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    let app = args.required(2, "app name")?;
    let client = client();
    let tokens = require_tokens(cfg, &client)?;
    let api = api(&client, cfg, &tokens);
    let games = load_games(&api, &tokens)?;
    let choice = select_bundle(&games, app)?;
    if choice.orphan || choice.base.is_dlc {
        return Err(Error::usage(format!(
            "'{}' has no main game in this library; its add-ons cannot be installed alone",
            choice.selected.title
        )));
    }
    let dir = resolve_install_dir(args, cfg, &choice.base);
    let installed_components = install::load_installed_components(&dir).unwrap_or_default();
    let installed: HashSet<String> = installed_components
        .iter()
        .map(|component| component.app_name.to_lowercase())
        .collect();
    let missing = choice
        .dlc
        .iter()
        .filter(|dlc| !installed.contains(&dlc.app_name.to_lowercase()))
        .count();

    if json {
        let rows: Vec<serde_json::Value> = choice
            .dlc
            .iter()
            .map(|dlc| {
                serde_json::json!({
                    "app_name": dlc.app_name,
                    "title": dlc.title,
                    "installed": installed.contains(&dlc.app_name.to_lowercase()),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "base_app_name": choice.base.app_name,
                "base_title": choice.base.title,
                "install_dir": dir.to_string_lossy(),
                "installed": !installed_components.is_empty(),
                "missing": missing,
                "dlc": rows,
            })
        );
        return Ok(());
    }

    println!("{} ({})", choice.base.title, choice.base.app_name);
    println!("install dir : {}", dir.display());
    println!(
        "installed   : {}",
        if installed_components.is_empty() {
            "no (main game not found)".to_string()
        } else {
            format!("yes ({} component(s))", installed_components.len())
        }
    );
    if choice.dlc.is_empty() {
        println!("add-ons     : none owned for this game");
        return Ok(());
    }
    println!();
    println!("{:<46} {:<34} STATE", "TITLE", "APP NAME");
    for dlc in &choice.dlc {
        let state = if installed.contains(&dlc.app_name.to_lowercase()) { "installed" } else { "missing" };
        println!(
            "{:<46} {:<34} {}",
            truncate(&dlc.title, 46),
            truncate(&dlc.app_name, 34),
            state
        );
    }
    println!();
    println!("{} add-on(s) · {} missing", choice.dlc.len(), missing);
    if missing > 0 {
        println!("install them with: skiff dlc add {app}");
    }
    Ok(())
}

fn cmd_dlc_add(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    let app = args.required(2, "app name")?;
    let concurrency = concurrency_from(args, cfg)?;
    let client = client();
    let tokens = require_tokens(cfg, &client)?;
    let api = api(&client, cfg, &tokens);
    let games = load_games(&api, &tokens)?;
    let choice = select_bundle(&games, app)?;
    if choice.orphan || choice.base.is_dlc {
        return Err(Error::usage(format!(
            "'{}' has no main game in this library; install the main game first",
            choice.selected.title
        )));
    }
    let base = choice.base.clone();
    let install_dir = resolve_install_dir(args, cfg, &base);
    let installed: HashSet<String> = install::load_installed_components(&install_dir)
        .map_err(|_| {
            Error::usage(format!(
                "{} is not installed in {}; run `skiff install {}` first",
                base.title,
                install_dir.display(),
                base.app_name
            ))
        })?
        .into_iter()
        .map(|component| component.app_name.to_lowercase())
        .collect();

    // Explicit selectors win; with none, every owned add-on that is not installed yet is added.
    // positional = ["dlc", "add", <appName>, <dlcName>...].
    let selectors: Vec<&str> = args.positional.iter().skip(3).map(String::as_str).collect();
    let mut picked: Vec<GameEntry> = Vec::new();
    if selectors.is_empty() {
        for dlc in &choice.dlc {
            if !installed.contains(&dlc.app_name.to_lowercase()) {
                picked.push(dlc.clone());
            }
        }
    } else {
        for selector in selectors {
            // Exact title or app name first; otherwise accept a unique substring so that
            // "Phantom Liberty" selects "Cyberpunk 2077: Phantom Liberty".
            let needle = selector.trim().to_lowercase();
            let chosen = match choice.dlc.iter().find(|dlc| {
                dlc.app_name.eq_ignore_ascii_case(selector)
                    || dlc.title.eq_ignore_ascii_case(selector)
            }) {
                Some(dlc) => dlc,
                None => {
                    let partial: Vec<&GameEntry> = choice
                        .dlc
                        .iter()
                        .filter(|dlc| {
                            !needle.is_empty()
                                && (dlc.title.to_lowercase().contains(&needle)
                                    || dlc.app_name.to_lowercase().contains(&needle))
                        })
                        .collect();
                    match partial.len() {
                        1 => partial[0],
                        0 => {
                            return Err(Error::usage(format!(
                                "'{selector}' is not an add-on of {}; see `skiff dlc list {app}`",
                                base.title
                            )))
                        }
                        _ => {
                            return Err(Error::usage(format!(
                                "'{selector}' matches {} add-ons of {} ({}); use a full title or app name",
                                partial.len(),
                                base.title,
                                partial
                                    .iter()
                                    .map(|dlc| dlc.title.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )))
                        }
                    }
                }
            };
            if !picked
                .iter()
                .any(|existing| existing.app_name.eq_ignore_ascii_case(&chosen.app_name))
            {
                picked.push(chosen.clone());
            }
        }
    }

    // Add-ons already in the directory need no work; say so instead of silently doing nothing.
    let already: Vec<String> = picked
        .iter()
        .filter(|dlc| installed.contains(&dlc.app_name.to_lowercase()))
        .map(|dlc| dlc.title.clone())
        .collect();
    picked.retain(|dlc| !installed.contains(&dlc.app_name.to_lowercase()));

    if picked.is_empty() {
        if !already.is_empty() {
            if !json {
                println!("already installed: {}", already.join(", "));
            }
            return Ok(());
        }
        return Err(Error::usage(format!(
            "no add-on of {} is left to install; see `skiff dlc list {app}`",
            base.title
        )));
    }

    let limit = match args.flag("limit-rate").or(cfg.limit_rate.as_deref()) {
        Some(text) => util::parse_rate(text)?,
        None => None,
    };

    // Resolving each add-on is exactly what `add_dlc` will do next, so a dry run proves the whole
    // plan without writing a byte (an add-on can be tens of gigabytes).
    if args.has("dry-run") {
        println!("dry run: nothing will be written");
        println!("location    : {}", install_dir.display());
        // Files already present in the main game, by their manifest path.
        let installed_hashes: HashMap<String, [u8; 20]> =
            install::load_installed_files(&install_dir)
                .unwrap_or_default()
                .into_iter()
                .map(|(path, meta)| (path, meta.hash))
                .collect();
        // Printed so "no overlap" can be told apart from "nothing was compared".
        println!("main game   : {} file(s) recorded on disk", installed_hashes.len());
        for dlc in &picked {
            match api.build(dlc) {
                Ok(build) => {
                    let parsed = api
                        .manifest(&build.manifest_uri)
                        .and_then(|raw| Manifest::parse(&raw));
                    println!("  + {} ({})", dlc.title, dlc.app_name);
                    println!("    build    : {}", build.build_version);
                    match parsed {
                        Ok(manifest) => {
                            let bytes: u64 = manifest.files.iter().map(|file| file.size).sum();
                            println!(
                                "    contents : {} chunk(s), {} file(s), {}",
                                manifest.chunks.len(),
                                manifest.files.len(),
                                util::human_bytes(bytes)
                            );
                            // Same path but different content means the add-on expects a newer
                            // main game than the one on disk; the merge keeps the main game copy.
                            let overlap = manifest
                                .files
                                .iter()
                                .filter(|file| {
                                    installed_hashes
                                        .get(&file.path)
                                        .map(|hash| hash != &file.hash)
                                        .unwrap_or(false)
                                })
                                .count();
                            if overlap > 0 {
                                println!(
                                    "    overlap  : {overlap} file(s) already exist in the main game with different content"
                                );
                                println!(
                                    "               本体可能落后于这个 DLC；建议先 `skiff install {}` 把本体更新到同一构建",
                                    base.app_name
                                );
                            }
                        }
                        Err(e) => println!("    contents : manifest unavailable ({e})"),
                    }
                }
                Err(e) => println!("  ! {} ({}) cannot be resolved: {e}", dlc.title, dlc.app_name),
            }
        }
        for title in &already {
            println!("  = {title} (already installed)");
        }
        return Ok(());
    }

    if !json {
        println!("adding add-on(s) to {}", base.title);
        println!("location    : {}", install_dir.display());
        for dlc in &picked {
            println!("  + {} ({})", dlc.title, dlc.app_name);
        }
        for title in &already {
            println!("  = {title} (already installed)");
        }
        println!(
            "concurrency: {} downloads, {} writers, verify {}, per-host {}",
            concurrency.downloads,
            concurrency.writers,
            concurrency.verify_threads(),
            concurrency
                .per_host
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unlimited".to_string())
        );
        println!("note        : the main game is not re-downloaded or re-verified");
    }

    let interactive = !json && !args.has("no-progress") && std::io::stdout().is_terminal();
    let ui = Arc::new(std::sync::Mutex::new(ProgressUi::new(interactive)));
    let callback = {
        let ui = ui.clone();
        Arc::new(move |progress: Progress| {
            if let Ok(mut ui) = ui.lock() {
                ui.render(&progress);
            }
        }) as Arc<dyn Fn(Progress) + Send + Sync>
    };

    let options = InstallOptions {
        install_dir: install_dir.clone(),
        concurrency: concurrency.clone(),
        limit_bytes_per_sec: limit,
        verify: true,
        repair: args.has("repair"),
        register_launcher: args.has("register-launcher") || cfg.register_launcher,
    };

    let started = Instant::now();
    let outcome = install::add_dlc(&client, &api, &base, &picked, &options, Some(callback))?;
    if interactive {
        let mut ui = ui.lock().unwrap();
        ui.finish();
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "app_name": outcome.app_name,
                "install_dir": install_dir.to_string_lossy(),
                "added": outcome.dlc_apps,
                "skipped": outcome.skipped_dlc,
                "conflicts": outcome.conflicts,
                "files": outcome.files,
                "chunks_downloaded": outcome.chunks_downloaded,
                "repaired_chunks": outcome.repaired_chunks,
                "peak_inflight": outcome.peak_inflight,
                "elapsed_secs": outcome.elapsed_secs,
                "launcher_registered": outcome.launcher_registered,
                "launcher_running": outcome.launcher_running,
                "launcher_error": outcome.launcher_error,
            })
        );
        return Ok(());
    }

    println!();
    println!("added to {}", base.title);
    println!("  location    : {}", install_dir.display());
    println!("  components  : {}", outcome.components);
    if !outcome.dlc_apps.is_empty() {
        println!("  added       : {}", outcome.dlc_apps.join(", "));
    }
    if !outcome.skipped_dlc.is_empty() {
        println!("  skipped     : {}", outcome.skipped_dlc.join(", "));
    }
    println!("  files       : {} ({} in this install)", outcome.files, install_dir.join(".egstore").display());
    println!("  downloaded  : {} chunks", outcome.chunks_downloaded);
    if outcome.repaired_chunks > 0 {
        println!("  repaired    : {} chunks", outcome.repaired_chunks);
    }
    if outcome.conflicts > 0 {
        println!(
            "  conflicts   : {} file(s) kept the main game version",
            outcome.conflicts
        );
        println!(
            "  提示        : 有 {} 个文件和本体重叠但内容不同，通常意味着本体版本落后于这个 DLC；\
建议先 `skiff install {}` 把本体更新到同一构建，否则 DLC 可能缺文件。",
            outcome.conflicts, base.app_name
        );
    }
    println!(
        "  peak inflight: {} (configured {})",
        outcome.peak_inflight, concurrency.downloads
    );
    println!("  elapsed     : {:.1}s", started.elapsed().as_secs_f64());
    if outcome.launcher_registered {
        println!("  launcher    : registered with Epic Games Launcher");
        if outcome.launcher_running {
            println!(
                "  提示        : Epic 启动器正在运行。它只在启动时读取安装清单，\
请从托盘菜单完全退出（关闭窗口不算）后重新打开，DLC 才会被识别。"
            );
        }
    } else if let Some(error) = &outcome.launcher_error {
        println!("  launcher    : not registered ({error})");
    }
    Ok(())
}

fn cmd_verify(args: &Args, cfg: &Config) -> Result<()> {
    let dir = PathBuf::from(args.required(1, "install directory")?);
    if args.has("repair") {
        let client = client();
        let tokens = require_tokens(cfg, &client)?;
        let api = api(&client, cfg, &tokens);
        let components = install::load_installed_components(&dir)?;
        let base_app = components
            .first()
            .map(|component| component.main_app_name.clone())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::integrity("cannot determine the installed main game"))?;
        let games = load_games(&api, &tokens)?;
        let choice = select_bundle(&games, &base_app)?;
        if choice.orphan {
            return Err(Error::usage(format!(
                "{} is a DLC without a main game; install the main game instead",
                choice.selected.title
            )));
        }
        if choice.selected.is_dlc && !choice.base.app_name.eq_ignore_ascii_case(&base_app) {
            return Err(Error::usage(format!(
                "this directory holds a standalone DLC install ({}); reinstall {} to repair it",
                base_app, choice.base.title
            )));
        }
        let installed: HashSet<String> =
            components.iter().map(|component| component.app_name.to_lowercase()).collect();
        let dlcs: Vec<GameEntry> = choice
            .dlc
            .iter()
            .filter(|dlc| installed.contains(&dlc.app_name.to_lowercase()))
            .cloned()
            .collect();
        let options = InstallOptions {
            install_dir: dir.clone(),
            concurrency: concurrency_from(args, cfg)?,
            limit_bytes_per_sec: None,
            verify: true,
            repair: true,
            register_launcher: false,
        };
        let outcome = install::install_bundle(&client, &api, &choice.base, &dlcs, &options, None)?;
        println!(
            "repaired {} chunks in {} ({} component(s))",
            outcome.repaired_chunks,
            dir.display(),
            outcome.components
        );
        return Ok(());
    }

    let components = install::load_installed_components(&dir)?;
    let files = install::load_installed_files(&dir)?;
    let threads = concurrency_from(args, cfg)?.verify_threads();
    let report = install::verify_files(&dir, &files, threads)?;
    println!("checked {} file(s) in {} component(s)", report.checked, components.len());
    println!("  ok      : {}", report.ok);
    println!("  bad     : {}", report.bad.len());
    for path in report.bad.iter().take(20) {
        println!("    - {path}");
    }
    println!("  missing : {}", report.missing.len());
    for path in report.missing.iter().take(20) {
        println!("    - {path}");
    }
    if report.is_clean() {
        println!("install is clean");
        Ok(())
    } else {
        Err(Error::integrity("install is damaged; re-run with --repair".to_string()))
    }
}

fn cmd_uninstall(args: &Args, cfg: &Config) -> Result<()> {
    let _ = cfg;
    let dir = PathBuf::from(args.required(1, "install directory")?);
    let app = install::load_installed_components(&dir)
        .ok()
        .and_then(|components| components.first().map(|component| component.main_app_name.clone()))
        .unwrap_or_default();
    install::uninstall(&dir, args.has("keep-files"), &app)?;
    println!("removed install metadata from {}", dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------------------------

/// Start the local web UI (loopback only).
fn cmd_serve(args: &Args, cfg: &Config) -> Result<()> {
    let port = args.usize_flag("port")?.unwrap_or(8790);
    if port == 0 || port > 65535 {
        return Err(Error::usage("--port must be between 1 and 65535"));
    }
    web::serve(cfg, port as u16, args.has("open"))
}

fn cmd_config(args: &Args, cfg: &Config) -> Result<()> {
    let mut cfg = cfg.clone();
    match args.positional_at(1) {
        Some("get") => {
            let key = args.required(2, "config key")?;
            println!("{}", config_get(&cfg, key)?);
            Ok(())
        }
        Some("set") => {
            let key = args.required(2, "config key")?;
            let value = args.required(3, "config value")?;
            config_set(&mut cfg, key, value)?;
            cfg.concurrency.validate()?;
            cfg.save()?;
            println!("{key} = {value}");
            Ok(())
        }
        _ => {
            println!("config file: {}", Config::path().display());
            println!("state dir  : {}", Config::dir().display());
            println!("install_root     = {}", cfg.install_root);
            println!("platform         = {}", cfg.platform);
            println!("label            = {}", cfg.label);
            println!("concurrency      = {}", cfg.concurrency.downloads);
            println!("write_threads    = {}", cfg.concurrency.writers);
            println!("verify_threads   = {} (0 = auto)", cfg.concurrency.verify);
            println!(
                "per_host         = {}",
                cfg.concurrency
                    .per_host
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unlimited".to_string())
            );
            println!(
                "limit_rate       = {}",
                cfg.limit_rate.clone().unwrap_or_else(|| "unlimited".to_string())
            );
            println!("preferred_cdn    = {}", cfg.preferred_cdn.clone().unwrap_or_default());
            println!("register_launcher= {}", cfg.register_launcher);
            Ok(())
        }
    }
}

fn config_get(cfg: &Config, key: &str) -> Result<String> {
    Ok(match key {
        "install_root" | "install-root" => cfg.install_root.clone(),
        "platform" => cfg.platform.clone(),
        "label" => cfg.label.clone(),
        "concurrency" => cfg.concurrency.downloads.to_string(),
        "write_threads" | "write-threads" => cfg.concurrency.writers.to_string(),
        "verify_threads" | "verify-threads" => cfg.concurrency.verify.to_string(),
        "per_host" | "per-host" => cfg
            .concurrency
            .per_host
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unlimited".to_string()),
        "preferred_cdn" | "preferred-cdn" => cfg.preferred_cdn.clone().unwrap_or_else(|| "none".into()),
        "limit_rate" | "limit-rate" => cfg.limit_rate.clone().unwrap_or_else(|| "unlimited".into()),
        other => return Err(Error::usage(format!("unknown config key '{other}'"))),
    })
}

fn config_set(cfg: &mut Config, key: &str, value: &str) -> Result<()> {
    match key {
        "install_root" | "install-root" => cfg.install_root = value.to_string(),
        "platform" => cfg.platform = value.to_string(),
        "label" => cfg.label = value.to_string(),
        "concurrency" => {
            cfg.concurrency.downloads = value
                .parse()
                .map_err(|_| Error::usage("concurrency must be a number"))?
        }
        "write_threads" | "write-threads" => {
            cfg.concurrency.writers = value
                .parse()
                .map_err(|_| Error::usage("write_threads must be a number"))?
        }
        "verify_threads" | "verify-threads" => {
            cfg.concurrency.verify = value
                .parse()
                .map_err(|_| Error::usage("verify_threads must be a number"))?
        }
        "per_host" | "per-host" => {
            cfg.concurrency.per_host = if value == "unlimited" || value == "0" {
                None
            } else {
                Some(
                    value
                        .parse()
                        .map_err(|_| Error::usage("per_host must be a number"))?,
                )
            }
        }
        "limit_rate" | "limit-rate" => {
            cfg.limit_rate = if value == "unlimited" || value == "0" {
                None
            } else {
                Some(value.to_string())
            }
        }
        "preferred_cdn" | "preferred-cdn" => {
            cfg.preferred_cdn = if value == "none" || value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        other => return Err(Error::usage(format!("unknown config key '{other}'"))),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// debug
// ---------------------------------------------------------------------------------------------

fn cmd_debug(args: &Args, cfg: &Config, json: bool) -> Result<()> {
    match args.positional_at(1) {
        Some("titles") => {
            // Probe entitlement route variants with the stored user token.
            let client = client();
            let tokens = auth::load_tokens()?.ok_or_else(|| Error::Auth("not logged in".into()))?;
            let tokens = auth::ensure_fresh(cfg, &client, tokens)?;
            let base = "https://entitlement-public-service-prod08.ol.epicgames.com";
            let account = tokens.account_id.clone();
            let variants = [
                format!("{base}/entitlement/api/accounts/{account}/entitlements?start=0&count=1"),
                format!("{base}/entitlement/api/accounts/{account}/entitlements?start=0&count=1&includeRedeemed=true"),
                format!("{base}/entitlement/api/public/accounts/{account}/entitlements?start=0&count=1"),
                format!("{base}/entitlement/api/public/accounts/{account}/entitlements?start=0&count=1&country=US"),
                format!("{base}/entitlement/api/accounts/{account}/entitlements?start=0&count=1&country=US&locale=en-US"),
            ];
            for url in variants {
                let request = skiff_core::http::Request::get(&url)
                    .header("Authorization", format!("bearer {}", tokens.access_token));
                match client.request_vec(&request) {
                    Ok((head, body)) => println!(
                        "{} -> HTTP {}  {}",
                        &url[base.len()..],
                        head.status,
                        String::from_utf8_lossy(&body).chars().take(110).collect::<String>()
                    ),
                    Err(err) => println!("{} -> {err}", &url[base.len()..]),
                }
            }
            Ok(())
        }        Some("net") => {
            let client = client();
            let hosts = [
                ("account", "https://account-public-service-prod.ol.epicgames.com/account/api/oauth/verify"),
                ("entitlement", "https://entitlement-public-service-prod.ol.epicgames.com/entitlement/api/accounts/00000000000000000000000000000000/entitlements?start=0&count=1"),
                ("catalog", "https://catalog-public-service-prod.ol.epicgames.com/catalog/api/shared/namespace/fn/bulk/items?id=0000"),
                ("launcher", "https://launcher-public-service-prod.ol.epicgames.com/launcher/api/public/assets/Windows?label=Live"),
                ("library", "https://library-service.live.use1a.on.epicgames.com/library/api/public/items?count=1"),
            ];
            for (name, url) in hosts {
                let req = skiff_core::http::Request::get(url).timeout(20);
                let mut sink = std::io::sink();
                match client.request(&req, &mut sink) {
                    Ok(head) => println!("{name:<14} reachable (HTTP {})", head.status),
                    Err(e) => println!("{name:<14} FAILED: {e}"),
                }
            }
            Ok(())
        }        Some("manifest") => {
            let path = args.required(2, "manifest file")?;
            let raw = std::fs::read(path)?;
            let manifest = Manifest::parse(&raw)?;
            if json {
                let chunks: Vec<serde_json::Value> = manifest
                    .chunks
                    .iter()
                    .take(5)
                    .map(|c| {
                        serde_json::json!({
                            "guid": c.guid_hex(),
                            "group": c.group_num,
                            "window": c.window_size,
                            "file_size": c.file_size,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "version": manifest.version,
                        "compressed": manifest.compressed,
                        "app_name": manifest.meta.app_name,
                        "build_version": manifest.meta.build_version,
                        "launch_exe": manifest.meta.launch_exe,
                        "chunks": manifest.chunks.len(),
                        "files": manifest.files.len(),
                        "install_size": manifest.install_size(),
                        "download_size": manifest.download_size(),
                        "custom_fields": manifest.custom,
                        "sample_chunks": chunks,
                    })
                );
            } else {
                println!("format      : binary level {}", manifest.version);
                println!("compressed  : {}", manifest.compressed);
                println!("app name    : {}", manifest.meta.app_name);
                println!("build       : {}", manifest.meta.build_version);
                println!("launch exe  : {}", manifest.meta.launch_exe);
                println!("chunks      : {}", manifest.chunks.len());
                println!("files       : {}", manifest.files.len());
                println!("install     : {}", util::human_bytes(manifest.install_size()));
                println!("download    : {}", util::human_bytes(manifest.download_size()));
                for (key, value) in &manifest.custom {
                    println!("custom      : {key} = {value}");
                }
            }
            Ok(())
        }
        Some("cdn-probe") => {
            let app = args.required(2, "app name")?;
            let client = client();
            let tokens = require_tokens(cfg, &client)?;
            let api = api(&client, cfg, &tokens);
            let game = resolve_game(&api, &tokens, app)?;
            let build = api.build(&game)?;
            let raw = api.manifest(&build.manifest_uri)?;
            let manifest = Manifest::parse(&raw)?;
            if build.base_urls.is_empty() {
                println!("no CDN base URLs returned by the asset API");
                return Ok(());
            }
            println!("base urls:");
            for base in &build.base_urls {
                println!("  {base}");
            }
            let first = manifest
                .chunks
                .first()
                .ok_or_else(|| Error::integrity("manifest has no chunks"))?;
            let path = skiff_core::cdn::chunk_path(
                manifest.version,
                first.group_num,
                first.rolling_hash,
                &first.guid,
                first.secret_guid.as_ref(),
            );
            for base in &build.base_urls {
                let url = skiff_core::cdn::chunk_url(base, &path);
                let req = skiff_core::http::Request::get(&url).timeout(30);
                match client.request_vec(&req) {
                    Ok((head, body)) => {
                        println!("  HTTP {} ({} bytes) {}", head.status, body.len(), base);
                    }
                    Err(e) => println!("  failed ({e}) {base}"),
                }
            }
            Ok(())
        }
        Some(other) => Err(Error::usage(format!("unknown debug command '{other}'"))),
        None => Err(Error::usage("debug needs a subcommand")),
    }
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn require_tokens(cfg: &Config, client: &Client) -> Result<auth::TokenSet> {
    let tokens = auth::load_tokens()?.ok_or_else(|| {
        Error::Auth("not logged in; run `skiff auth login`".to_string())
    })?;
    auth::ensure_fresh(cfg, client, tokens)
}

fn load_games(api: &Api<'_>, tokens: &auth::TokenSet) -> Result<Vec<GameEntry>> {
    if let Ok(cached) = install::load_cached_games() {
        if !cached.is_empty() {
            return Ok(cached);
        }
    }
    let games = api.games(&tokens.account_id)?;
    // Caching is an optimisation; a read-only state directory must not break listing.
    if let Err(e) = install::cache_games(&games) {
        eprintln!("warning: could not write the game cache ({e})");
    }
    Ok(games)
}

fn entry_matches(game: &GameEntry, key: &str) -> bool {
    game.app_name.eq_ignore_ascii_case(key) || game.title.eq_ignore_ascii_case(key)
}

/// A selected install target: the main game plus every add-on owned for it.
struct BundleChoice {
    base: GameEntry,
    dlc: Vec<GameEntry>,
    selected: GameEntry,
    /// The selection is an add-on whose main game is missing from the library.
    orphan: bool,
}

fn select_bundle(games: &[GameEntry], key: &str) -> Result<BundleChoice> {
    if key.trim().is_empty() {
        return Err(Error::usage("missing app name"));
    }
    let view = library::library_view(games);
    if let Some(selection) = view.select(key) {
        return Ok(BundleChoice {
            base: selection.base,
            dlc: selection.dlc,
            selected: selection.selected,
            orphan: selection.orphan,
        });
    }
    // Defensive fallback: a flat list that has not been classified yet (e.g. a partial cache).
    if let Some(game) = games.iter().find(|game| entry_matches(game, key)) {
        return Ok(BundleChoice {
            base: game.clone(),
            dlc: Vec::new(),
            selected: game.clone(),
            orphan: game.is_dlc,
        });
    }
    Err(Error::usage(format!("no owned game matches '{key}'")))
}

/// Resolution used by `debug cdn-probe`: one entry, exact app name or title.
fn resolve_game(api: &Api<'_>, tokens: &auth::TokenSet, app: &str) -> Result<GameEntry> {
    let games = load_games(api, tokens)?;
    games
        .into_iter()
        .find(|game| entry_matches(game, app))
        .ok_or_else(|| Error::usage(format!("no owned game matches '{app}'")))
}

fn concurrency_from(args: &Args, cfg: &Config) -> Result<Concurrency> {
    if args.has("auto-concurrency") {
        return Err(Error::usage(
            "automatic concurrency tuning is not implemented; pass an explicit -j/--concurrency",
        ));
    }
    let mut concurrency = cfg.concurrency.clone();
    if let Some(value) = args.usize_flag("concurrency")? {
        concurrency.downloads = value;
    }
    if let Some(value) = args.usize_flag("write-threads")? {
        concurrency.writers = value;
    }
    if let Some(value) = args.usize_flag("verify-threads")? {
        concurrency.verify = value;
    }
    if let Some(value) = args.usize_flag("per-host")? {
        concurrency.per_host = Some(value);
    }
    concurrency.auto = false;
    concurrency.validate()?;
    Ok(concurrency)
}

fn truncate(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    let mut out: String = chars[..width.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

/// Minimal terminal progress renderer (no external crate).
struct ProgressUi {
    interactive: bool,
    last_render: Instant,
    started: Instant,
}

impl ProgressUi {
    fn new(interactive: bool) -> ProgressUi {
        ProgressUi { interactive, last_render: Instant::now(), started: Instant::now() }
    }

    fn render(&mut self, progress: &Progress) {
        if !self.interactive {
            return;
        }
        if self.last_render.elapsed().as_millis() < 200 {
            return;
        }
        self.last_render = Instant::now();
        let fraction = if progress.bytes_total > 0 {
            (progress.bytes_done as f64 / progress.bytes_total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let width = 32usize;
        let filled = (fraction * width as f64).round() as usize;
        let bar: String = "=".repeat(filled) + &" ".repeat(width - filled);
        let eta = progress
            .eta_secs
            .map(|s| format!("{}:{:02}", (s as u64) / 60, (s as u64) % 60))
            .unwrap_or_else(|| "--:--".to_string());
        print!(
            "\r[{bar}] {:5.1}%  {}/{}  {}/s  inflight {}/{}  eta {}  retry {}   ",
            fraction * 100.0,
            util::human_bytes(progress.bytes_done),
            util::human_bytes(progress.bytes_total),
            util::human_bytes(progress.speed_bps as u64),
            progress.inflight,
            progress.peak_inflight,
            eta,
            progress.retries
        );
        let _ = std::io::stdout().flush();
    }

    fn finish(&mut self) {
        if self.interactive {
            println!();
            let _ = std::io::stdout().flush();
        }
        let _ = self.started;
    }
}

/// Accept either a bare authorization code, a quoted value, or a pasted JSON blob containing
/// `authorizationCode`. Copies are often made from a browser page, so all three occur in practice.
fn extract_auth_code(input: &str) -> String {
    let text = input.trim();
    if let Some(pos) = text.find("authorizationCode") {
        let rest = &text[pos + "authorizationCode".len()..];
        if let Some(colon) = rest.find(':') {
            let after_colon = &rest[colon + 1..];
            if let Some(first_quote) = after_colon.find('"') {
                let after_open = &after_colon[first_quote + 1..];
                if let Some(close) = after_open.find('"') {
                    return after_open[..close].to_string();
                }
            }
        }
    }
    text.trim_matches('"').trim().to_string()
}
