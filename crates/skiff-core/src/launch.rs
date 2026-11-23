//! Launching an installed game the way the official launcher does, Epic account hand-off included.
//!
//! The official launcher never gives a game a password. It asks the account service for a
//! short-lived *exchange code* and appends it to the game's command line:
//!
//! ```text
//! -AUTH_LOGIN=unused -AUTH_PASSWORD=<code> -AUTH_TYPE=exchangecode
//! -epicapp=<AppName> -epicenv=Prod [-epicdeploymentid=<id>] -EpicPortal
//! [-epicusername="<name>"] -epicuserid=<account id> -epiclocale=<locale>
//! [-epicsandboxid=<namespace>]
//! ```
//!
//! The game (or the publisher launcher named by the install's `LaunchExecutable`) redeems that code
//! with its own EOS client at `api.epicgames.dev/epic/oauth/v2/token` (`grant_type=exchange_code`).
//! That is what makes the in-game Epic account appear "already logged in".
//!
//! The code is a one-time credential with a roughly five minute lifetime, so this module validates
//! the install first and only mints it right before the process is started. It is never logged,
//! never serialized and never printed: [`LaunchPlan`]'s `Debug` output and [`masked_args`] replace
//! it with [`PLACEHOLDER_CODE`].

use crate::auth::{self, TokenSet};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::Client;
use crate::install;
use crate::util;
use std::fmt;
use std::path::{Path, PathBuf};

/// Account-service endpoint that mints a one-time exchange code for the logged-in account.
const EXCHANGE_PATH: &str = "/account/api/oauth/exchange";
/// What a redacted exchange code looks like. Never send this value to Epic.
pub const PLACEHOLDER_CODE: &str = "********";
/// Lifetime Epic reports for an exchange code (seconds); only used when the response omits it.
const DEFAULT_TTL: u64 = 299;

/// One minted exchange code.
#[derive(Clone)]
pub struct ExchangeCode {
    /// 32 hexadecimal characters. Redacted by `Debug`.
    pub code: String,
    /// Seconds until the code expires (299 on current Epic responses).
    pub expires_in: u64,
}

impl fmt::Debug for ExchangeCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExchangeCode")
            .field("code", &PLACEHOLDER_CODE)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Everything needed to start one installed game.
#[derive(Debug, Clone)]
pub struct LaunchTarget {
    /// `AppName` from the install's `.item` (falls back to the manifest app name).
    pub app_name: String,
    /// Human readable title, used for reporting (the account name comes from the token set).
    pub display_name: String,
    pub install_dir: PathBuf,
    /// Absolute path of the executable to start; always inside `install_dir`.
    pub executable: PathBuf,
    /// `LaunchCommand` from the `.item`, appended verbatim before the authentication arguments.
    pub launch_command: String,
    /// `CatalogNamespace` from the `.item`; also what the official launcher passes as sandbox id.
    pub namespace: String,
    /// `SidecarDeploymentId` from the `.item`, or a caller override; empty when unknown.
    pub deployment_id: String,
}

/// Knobs for [`plan`].
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Install directory override; `None` means the caller already resolved it.
    pub install_dir: Option<PathBuf>,
    /// Extra game arguments supplied by the user (`skiff launch -- <args>`).
    pub extra_args: Vec<String>,
    /// Mint an exchange code and pass the account arguments. `false` = plain launch.
    pub auth: bool,
    /// Overrides `SidecarDeploymentId` for games whose `.item` does not carry one.
    pub deployment_id: Option<String>,
    /// Overrides `config.locale` for `-epiclocale`.
    pub locale: Option<String>,
    /// Build the plan without contacting Epic and without a real code.
    pub dry_run: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        LaunchOptions {
            install_dir: None,
            extra_args: Vec::new(),
            auth: true,
            deployment_id: None,
            locale: None,
            dry_run: false,
        }
    }
}

/// A resolved, ready-to-spawn launch. Holds the exchange code when `authenticated` is true.
pub struct LaunchPlan {
    target: LaunchTarget,
    /// Authentication arguments in the official launcher's order (the real code inside).
    args: Vec<String>,
    extra_args: Vec<String>,
    authenticated: bool,
    dry_run: bool,
    exchange_expires_in: u64,
}

impl LaunchPlan {
    pub fn target(&self) -> &LaunchTarget {
        &self.target
    }

    /// Real arguments, including the exchange code. Intended for process spawning only.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Arguments safe to print or serialize: the code is replaced by [`PLACEHOLDER_CODE`].
    pub fn masked_args(&self) -> Vec<String> {
        masked_args(&self.args)
    }

    pub fn authenticated(&self) -> bool {
        self.authenticated
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    /// Exchange-code lifetime in seconds; `0` for `--no-auth` plans.
    pub fn exchange_expires_in(&self) -> u64 {
        self.exchange_expires_in
    }

    /// Whole command line for display, with the exchange code masked.
    pub fn display_command_line(&self) -> String {
        let mut text = format!("\"{}\"", self.target.executable.display());
        let tail = self.argument_text(&self.masked_args());
        if !tail.is_empty() {
            text.push(' ');
            text.push_str(&tail);
        }
        for arg in &self.extra_args {
            text.push(' ');
            text.push_str(arg);
        }
        text
    }

    /// `LaunchCommand` followed by the authentication arguments, joined with single spaces.
    fn argument_text(&self, args: &[String]) -> String {
        let mut text = self.target.launch_command.trim().to_string();
        for arg in args {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(arg);
        }
        text
    }
}

impl fmt::Debug for LaunchPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchPlan")
            .field("target", &self.target)
            .field("args", &self.masked_args())
            .field("extra_args", &self.extra_args)
            .field("authenticated", &self.authenticated)
            .field("dry_run", &self.dry_run)
            .finish()
    }
}

/// Replace every `-AUTH_PASSWORD=...` with the placeholder, keeping the rest unchanged.
pub fn masked_args(args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| {
            if arg.starts_with("-AUTH_PASSWORD=") {
                format!("-AUTH_PASSWORD={PLACEHOLDER_CODE}")
            } else {
                arg.clone()
            }
        })
        .collect()
}

/// Read one string field from a JSON object.
fn text_field(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default().trim().to_string()
}

/// Mint a fresh one-time exchange code for `tokens`.
///
/// The code is redeemed by the game with *its* EOS client, not with Skiff's OAuth client, so no
/// `consumingClientId` is sent: the official launcher hands the same account-level code to games.
pub fn mint_exchange_code(client: &Client, tokens: &TokenSet) -> Result<ExchangeCode> {
    if tokens.access_token.is_empty() {
        return Err(Error::Auth("not logged in; run `skiff auth login`".into()));
    }
    let url = format!("{}{EXCHANGE_PATH}", auth::OAUTH_HOST);
    let value = client.get_json(&url, Some(&tokens.bearer()))?;
    let code = value
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if code.len() != 32 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Auth(
            "the exchange-code response did not contain a 32-character hex code".into(),
        ));
    }
    let expires_in = value
        .get("expiresInSeconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TTL);
    Ok(ExchangeCode { code, expires_in })
}

/// Resolve the executable a `.item` describes, rejecting paths that escape the install directory.
fn executable_rel_path(install_dir: &Path, item: &serde_json::Value, manifest_exe: &str) -> Result<String> {
    let from_item = text_field(item, "LaunchExecutable");
    let raw = if from_item.is_empty() { manifest_exe.trim().to_string() } else { from_item };
    if raw.is_empty() {
        return Err(Error::integrity(format!(
            "{} does not record a launch executable; reinstall the game with skiff",
            install_dir.display()
        )));
    }
    util::sanitize_rel_path(&raw).map_err(|e| {
        Error::integrity(format!(
            "{} records an unsafe launch executable '{raw}': {e}",
            install_dir.display()
        ))
    })
}

/// Join sanitised relative path segments onto the install directory.
fn join_rel_path(install_dir: &Path, rel: &str) -> PathBuf {
    let mut path = install_dir.to_path_buf();
    for segment in rel.split('\\') {
        path.push(segment);
    }
    path
}

/// Build a [`LaunchTarget`] from an install's `.egstore/<guid>.item`.
///
/// The main game is the first component returned by
/// [`install::load_installed_components`]; add-on components never carry a launch executable.
pub fn resolve_target(
    install_dir: &Path,
    display_name: &str,
    deployment_override: Option<&str>,
) -> Result<LaunchTarget> {
    let components = install::load_installed_components(install_dir)?;
    let main = components.first().ok_or_else(|| {
        Error::integrity(format!("{} has no install metadata", install_dir.display()))
    })?;
    let item = &main.item;
    let app_name = {
        let name = text_field(item, "AppName");
        if name.is_empty() {
            main.app_name.clone()
        } else {
            name
        }
    };
    let rel = executable_rel_path(install_dir, item, &main.manifest.meta.launch_exe)?;
    let executable = join_rel_path(install_dir, &rel);
    if !executable.is_file() {
        return Err(Error::integrity(format!(
            "{} is missing; run `skiff verify \"{}\"` or reinstall the game",
            executable.display(),
            install_dir.display()
        )));
    }
    let title = {
        let stored = text_field(item, "DisplayName");
        let chosen = if !display_name.trim().is_empty() {
            display_name.trim().to_string()
        } else if !stored.is_empty() {
            stored
        } else {
            app_name.clone()
        };
        chosen
    };
    let deployment_id = deployment_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| text_field(item, "SidecarDeploymentId"));

    Ok(LaunchTarget {
        app_name,
        display_name: title,
        install_dir: install_dir.to_path_buf(),
        executable,
        launch_command: text_field(item, "LaunchCommand"),
        namespace: text_field(item, "CatalogNamespace"),
        deployment_id,
    })
}

/// Display names may not contain quotes or control characters: the value is embedded inside
/// `-epicusername="..."` on a Windows command line.
fn sanitize_display_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '"' && *c != '\\' && !c.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Keep `-epiclocale` to a safe BCP-47-ish token.
fn sanitize_locale(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if cleaned.is_empty() {
        "en-US".to_string()
    } else {
        cleaned
    }
}

/// Build the official launcher's authentication arguments in its exact order.
///
/// Returns an empty vector when `account` or `code` is `None`, which is the `--no-auth` shape: the
/// game starts as a plain executable and decides on its own whether to ask for a login.
pub fn build_auth_args(
    target: &LaunchTarget,
    account: Option<&TokenSet>,
    code: Option<&str>,
    locale: &str,
) -> Vec<String> {
    let (Some(account), Some(code)) = (account, code) else {
        return Vec::new();
    };
    let mut args = vec![
        "-AUTH_LOGIN=unused".to_string(),
        format!("-AUTH_PASSWORD={code}"),
        "-AUTH_TYPE=exchangecode".to_string(),
        format!("-epicapp={}", target.app_name),
        "-epicenv=Prod".to_string(),
    ];
    if !target.deployment_id.is_empty() {
        args.push(format!("-epicdeploymentid={}", target.deployment_id));
    }
    args.push("-EpicPortal".to_string());
    // The official launcher passes the *account* display name here, not the game title.
    let name = sanitize_display_name(&account.account_name());
    if !name.is_empty() {
        args.push(format!("-epicusername=\"{name}\""));
    }
    if !account.account_id.is_empty() {
        args.push(format!("-epicuserid={}", account.account_id));
    }
    args.push(format!("-epiclocale={}", sanitize_locale(locale)));
    if !target.namespace.is_empty() {
        args.push(format!("-epicsandboxid={}", target.namespace));
    }
    args
}

/// Resolve the install, optionally mint a code, and produce a spawnable plan.
///
/// `tokens` is only required when `opts.auth` is true. With `opts.dry_run` the code is replaced by
/// [`PLACEHOLDER_CODE`] and no network request is made, so printing the plan is always safe.
pub fn plan(
    cfg: &Config,
    client: &Client,
    tokens: Option<&TokenSet>,
    install_dir: &Path,
    display_name: &str,
    opts: &LaunchOptions,
) -> Result<LaunchPlan> {
    let dir = opts.install_dir.as_deref().unwrap_or(install_dir);
    let target = resolve_target(dir, display_name, opts.deployment_id.as_deref())?;
    let locale = opts.locale.clone().unwrap_or_else(|| cfg.locale.clone());
    let (account, code, expires_in) = if opts.auth {
        let tokens = tokens.ok_or_else(|| {
            Error::Auth("not logged in; run `skiff auth login` (or pass --no-auth)".into())
        })?;
        if opts.dry_run {
            (Some(tokens), PLACEHOLDER_CODE.to_string(), 0)
        } else {
            let exchange = mint_exchange_code(client, tokens)?;
            (Some(tokens), exchange.code, exchange.expires_in)
        }
    } else {
        (None, String::new(), 0)
    };
    let code = if opts.auth { Some(code.as_str()) } else { None };
    let args = build_auth_args(&target, account, code, &locale);
    Ok(LaunchPlan {
        target,
        args,
        extra_args: opts.extra_args.clone(),
        authenticated: opts.auth,
        dry_run: opts.dry_run,
        exchange_expires_in: expires_in,
    })
}

#[cfg(windows)]
fn push_raw(command: &mut std::process::Command, text: &str) {
    use std::os::windows::process::CommandExt;
    command.raw_arg(text);
}

#[cfg(not(windows))]
fn push_raw(command: &mut std::process::Command, text: &str) {
    // Best effort on non-Windows hosts: the game-facing quoting is Windows-specific anyway.
    for part in text.split_whitespace() {
        command.arg(part);
    }
}

/// Start the process described by `plan` and return its pid.
///
/// The child is not waited for and is not placed in a Job Object, so closing Skiff does not stop
/// the game. `.bat`/`.cmd` entries (the official launcher supports them too) go through `cmd.exe`.
pub fn spawn(plan: &LaunchPlan) -> Result<u32> {
    if plan.dry_run {
        return Err(Error::usage("a --dry-run plan cannot be launched"));
    }
    let target = &plan.target;
    let text = plan.argument_text(&plan.args);
    let extension = target
        .executable
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_batch = cfg!(windows) && matches!(extension.as_str(), "bat" | "cmd");

    let mut command = if is_batch {
        let mut command = std::process::Command::new("cmd.exe");
        command.arg("/d").arg("/s").arg("/c");
        let line = if text.is_empty() {
            format!("\"{}\"", target.executable.display())
        } else {
            format!("\"{}\" {}", target.executable.display(), text)
        };
        push_raw(&mut command, &format!("\"{line}\""));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: a batch shim must not flash a console window.
            command.creation_flags(0x0800_0000);
        }
        command
    } else {
        let mut command = std::process::Command::new(&target.executable);
        if !text.is_empty() {
            // `raw_arg` appends the text verbatim with its own separator, so the launcher's exact
            // quoting (`-epicusername="name"`) survives instead of being re-escaped by Rust.
            push_raw(&mut command, &text);
        }
        command
    };
    for arg in &plan.extra_args {
        command.arg(arg);
    }
    command.current_dir(&target.install_dir);
    let child = command.spawn().map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("cannot start {}: {e}", target.executable.display()),
        ))
    })?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> TokenSet {
        TokenSet {
            account_id: "af83ec282c6f4b18aba5bf7175d6be66".to_string(),
            display_name: "我是普瑞赛斯的狗".to_string(),
            access_token: "eg1~test".to_string(),
            refresh_token: Some("refresh".to_string()),
            token_type: "bearer".to_string(),
            client_id: "34a02cf8f4414e29b15921876da36f9a".to_string(),
            expires_at: util::now_unix() + 3600,
            refresh_expires_at: util::now_unix() + 86_400,
            scope: String::new(),
        }
    }

    fn target() -> LaunchTarget {
        LaunchTarget {
            app_name: "Ginger".to_string(),
            display_name: "Cyberpunk 2077".to_string(),
            install_dir: PathBuf::from("D:\\Games\\Ginger"),
            executable: PathBuf::from("D:\\Games\\Ginger\\redprelauncher.exe"),
            launch_command: String::new(),
            namespace: "77f2b98e2cef40c8a7437518bf420e47".to_string(),
            deployment_id: "c380f33c4c86475e863cedb0126e53a3".to_string(),
        }
    }

    const CODE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn auth_args_match_the_official_launcher_order() {
        let args = build_auth_args(&target(), Some(&account()), Some(CODE), "zh-CN");
        assert_eq!(
            args,
            vec![
                "-AUTH_LOGIN=unused",
                "-AUTH_PASSWORD=0123456789abcdef0123456789abcdef",
                "-AUTH_TYPE=exchangecode",
                "-epicapp=Ginger",
                "-epicenv=Prod",
                "-epicdeploymentid=c380f33c4c86475e863cedb0126e53a3",
                "-EpicPortal",
                "-epicusername=\"我是普瑞赛斯的狗\"",
                "-epicuserid=af83ec282c6f4b18aba5bf7175d6be66",
                "-epiclocale=zh-CN",
                "-epicsandboxid=77f2b98e2cef40c8a7437518bf420e47",
            ]
        );
    }

    #[test]
    fn no_auth_launch_passes_no_epic_arguments() {
        assert!(build_auth_args(&target(), Some(&account()), None, "en-US").is_empty());
        assert!(build_auth_args(&target(), None, Some(CODE), "en-US").is_empty());
    }

    #[test]
    fn optional_arguments_are_omitted_when_unknown() {
        let mut target = target();
        target.deployment_id.clear();
        target.namespace.clear();
        let mut account = account();
        account.account_id.clear();
        account.display_name.clear();
        let args = build_auth_args(&target, Some(&account), Some(CODE), "en-US");
        assert!(!args.iter().any(|arg| arg.contains("deploymentid")));
        assert!(!args.iter().any(|arg| arg.contains("sandboxid")));
        assert!(!args.iter().any(|arg| arg.contains("epicusername")));
        assert!(!args.iter().any(|arg| arg.contains("epicuserid")));
        assert!(args.contains(&"-EpicPortal".to_string()));
    }

    #[test]
    fn display_name_cannot_break_the_command_line() {
        let mut account = account();
        account.display_name = "Evil\" -AUTH_TYPE=password\r\nName\\".to_string();
        let args = build_auth_args(&target(), Some(&account), Some(CODE), "en-US");
        let name = args.iter().find(|arg| arg.starts_with("-epicusername=")).unwrap();
        assert_eq!(name, "-epicusername=\"Evil -AUTH_TYPE=passwordName\"");
    }

    #[test]
    fn locale_is_sanitised() {
        assert_eq!(sanitize_locale("zh-CN"), "zh-CN");
        assert_eq!(sanitize_locale("en US;rm"), "enUSrm");
        assert_eq!(sanitize_locale(""), "en-US");
    }

    #[test]
    fn masked_args_hide_the_exchange_code() {
        let args = build_auth_args(&target(), Some(&account()), Some(CODE), "en-US");
        let masked = masked_args(&args);
        assert!(masked.iter().any(|arg| arg == "-AUTH_PASSWORD=********"));
        assert!(!masked.iter().any(|arg| arg.contains(CODE)));
        assert_eq!(masked.len(), args.len());
    }

    #[test]
    fn plan_debug_hides_the_exchange_code() {
        let plan = LaunchPlan {
            target: target(),
            args: build_auth_args(&target(), Some(&account()), Some(CODE), "en-US"),
            extra_args: Vec::new(),
            authenticated: true,
            dry_run: false,
            exchange_expires_in: 299,
        };
        let text = format!("{plan:?}");
        assert!(!text.contains(CODE), "{text}");
        assert!(text.contains(PLACEHOLDER_CODE), "{text}");
    }

    #[test]
    fn dry_run_never_contacts_epic_and_refuses_to_spawn() {
        let cfg = Config::default();
        let client = Client::plain();
        let tokens = account();
        let opts = LaunchOptions { dry_run: true, ..LaunchOptions::default() };
        let missing = std::env::temp_dir().join(format!("skiff-launch-missing-{}", fastrand::u64(..)));
        // The install does not exist, so planning must fail before any network call; this also
        // proves the dry-run path does not require a reachable Epic API.
        let err = plan(&cfg, &client, Some(&tokens), &missing, "Cyberpunk 2077", &opts).unwrap_err();
        assert!(matches!(err, Error::Integrity(_)), "{err}");

        let plan = LaunchPlan {
            target: target(),
            args: vec!["-AUTH_PASSWORD=********".to_string()],
            extra_args: Vec::new(),
            authenticated: true,
            dry_run: true,
            exchange_expires_in: 0,
        };
        assert!(matches!(spawn(&plan), Err(Error::Usage(_))));
    }

    #[test]
    fn executable_paths_cannot_escape_the_install_dir() {
        let dir = Path::new("D:\\Games\\Ginger");
        let evil = serde_json::json!({ "LaunchExecutable": "..\\..\\Windows\\System32\\cmd.exe" });
        assert!(executable_rel_path(dir, &evil, "").is_err());

        let absolute = serde_json::json!({ "LaunchExecutable": "C:\\Windows\\System32\\cmd.exe" });
        assert!(executable_rel_path(dir, &absolute, "").is_err());

        let good = serde_json::json!({ "LaunchExecutable": "bin\\x64\\Cyberpunk2077.exe" });
        assert_eq!(
            executable_rel_path(dir, &good, "").unwrap(),
            "bin\\x64\\Cyberpunk2077.exe"
        );

        // The manifest fallback is validated the same way.
        assert!(executable_rel_path(dir, &serde_json::json!({}), "..\\evil.exe").is_err());
        assert_eq!(
            executable_rel_path(dir, &serde_json::json!({}), "Game.exe").unwrap(),
            "Game.exe"
        );
    }

    #[test]
    fn launch_command_precedes_the_auth_arguments() {
        let mut target = target();
        target.launch_command = "-epicapp=Override -windowed".to_string();
        let plan = LaunchPlan {
            target,
            args: vec!["-AUTH_LOGIN=unused".to_string()],
            extra_args: Vec::new(),
            authenticated: true,
            dry_run: false,
            exchange_expires_in: 299,
        };
        assert_eq!(plan.argument_text(&plan.args), "-epicapp=Override -windowed -AUTH_LOGIN=unused");
        assert!(plan
            .display_command_line()
            .starts_with("\"D:\\Games\\Ginger\\redprelauncher.exe\" -epicapp=Override"));
    }
}
