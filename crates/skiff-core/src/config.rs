//! On-disk configuration.
//!
//! Precedence for every value: explicit CLI flag > `config.toml` > built-in default.
//! Concurrency values are *contractual*: the download engine never overrides them silently.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Public client credentials used by the official launcher (`launcherAppClient2`).
pub const DEFAULT_CLIENT_ID: &str = "34a02cf8f4414e29b15921876da36f9a";
/// Secret belonging to [`DEFAULT_CLIENT_ID`]; overridable because Epic can rotate or block it.
pub const DEFAULT_CLIENT_SECRET: &str = "daafbccc737745039dffe53d94fc76cf";

/// Concurrency knobs, all user controlled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Concurrency {
    /// Chunk downloads in flight at once (`-j`).
    pub downloads: usize,
    /// Blocking writer threads.
    pub writers: usize,
    /// Files verified in parallel (0 = derive from CPU count).
    pub verify: usize,
    /// Optional per-CDN-host cap; `None` means no extra cap.
    pub per_host: Option<usize>,
    /// Opt-in auto tuning; incompatible with an explicit `-j`.
    pub auto: bool,
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency { downloads: 16, writers: 4, verify: 0, per_host: None, auto: false }
    }
}

impl Concurrency {
    /// Hard validation limits; violations are usage errors, never silently clamped.
    pub fn validate(&self) -> Result<()> {
        if self.downloads < 1 || self.downloads > 128 {
            return Err(Error::usage(format!(
                "--concurrency must be between 1 and 128 (got {})",
                self.downloads
            )));
        }
        if self.writers < 1 || self.writers > 16 {
            return Err(Error::usage(format!(
                "--write-threads must be between 1 and 16 (got {})",
                self.writers
            )));
        }
        if self.verify > 32 {
            return Err(Error::usage(format!(
                "--verify-threads must be between 1 and 32 (got {})",
                self.verify
            )));
        }
        if let Some(p) = self.per_host {
            if !(1..=128).contains(&p) {
                return Err(Error::usage(format!(
                    "--per-host must be between 1 and 128 (got {p})"
                )));
            }
        }
        Ok(())
    }

    /// Effective verify parallelism.
    pub fn verify_threads(&self) -> usize {
        if self.verify > 0 {
            self.verify
        } else {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)
        }
    }
}

/// Persistent configuration file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    /// Root directory new installs are placed under.
    pub install_root: String,
    pub platform: String,
    pub label: String,
    pub locale: String,
    pub country: String,
    pub concurrency: Concurrency,
    /// Optional bandwidth cap such as `50M`.
    pub limit_rate: Option<String>,
    /// Preferred CDN host (substring match against candidate base URLs).
    pub preferred_cdn: Option<String>,
    /// Overrides the CDN base URL; used by tests and mirrors.
    pub cdn_base_override: Option<String>,
    /// Write launcher registration data (ProgramData + registry).
    pub register_launcher: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            client_id: DEFAULT_CLIENT_ID.to_string(),
            client_secret: DEFAULT_CLIENT_SECRET.to_string(),
            install_root: default_install_root(),
            platform: "Windows".to_string(),
            label: "Live".to_string(),
            locale: "en-US".to_string(),
            country: "US".to_string(),
            concurrency: Concurrency::default(),
            limit_rate: None,
            preferred_cdn: None,
            cdn_base_override: None,
            register_launcher: false,
        }
    }
}

/// True when `dir` can be created (if needed) and written to.
fn dir_writable(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Writable alternative used when the configured install root is unusable.
///
/// Returns `None` when nothing is writable: silently switching to a relative path would scatter
/// installs next to whatever the working directory happens to be, which is worse than an error.
fn default_install_root_fallback() -> Option<String> {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let mut candidates = Vec::new();
    if !home.is_empty() {
        candidates.push(format!("{home}\\Games"));
        candidates.push(format!("{home}\\Documents\\Games"));
    }
    candidates.into_iter().find(|cand| dir_writable(std::path::Path::new(cand)))
}
fn default_install_root() -> String {
    // D:\Games when that drive exists, otherwise the user profile.
    if std::path::Path::new("D:\\").exists() {
        "D:\\Games".to_string()
    } else {
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
        format!("{home}\\Games")
    }
}

fn env_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SKIFF_HOME") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(p).join("Skiff");
    }
    PathBuf::from(".Skiff")
}

/// Pre-rename state location (`epicd`), used only to migrate an existing login/cache.
fn legacy_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EPICD_HOME") {
        return Some(PathBuf::from(p));
    }
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|p| PathBuf::from(p).join("epicd"))
}

/// Move the old `epicd` state directory into the new `Skiff` location.
///
/// Renaming is atomic and preserves DPAPI-protected tokens; if the old directory is locked (the
/// previous binary is still running) the small state files are copied instead.
fn migrate_legacy_state(target: &Path) {
    // An explicit SKIFF_HOME is the user's choice; never move unrelated state into it.
    if std::env::var_os("SKIFF_HOME").is_some() || target.join("auth.json").exists() {
        return;
    }
    let Some(legacy) = legacy_dir().filter(|dir| dir.is_dir()) else { return };
    if legacy == target {
        return;
    }
    if std::fs::rename(&legacy, target).is_ok() {
        eprintln!(
            "note: migrated state directory {} -> {}",
            legacy.display(),
            target.display()
        );
        return;
    }
    if std::fs::create_dir_all(target).is_err() {
        return;
    }
    for name in ["auth.json", "config.toml", "games.json", "titles.json", "parents.json"] {
        let from = legacy.join(name);
        let to = target.join(name);
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

/// Candidate state directories, most preferred first.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let first = env_dir();
    out.push(first);
    if let Ok(appdata) = std::env::var("APPDATA") {
        out.push(PathBuf::from(appdata).join("Skiff"));
    }
    out.push(std::env::temp_dir().join("Skiff"));
    out.push(PathBuf::from(".Skiff"));
    // Last resort: if the new locations are unwritable, an existing old state directory still lets
    // the user read their login. Migration handles the normal case before this is reached.
    if std::env::var_os("SKIFF_HOME").is_none() {
        if let Some(legacy) = legacy_dir().filter(|dir| dir.is_dir()) {
            out.push(legacy);
        }
    }
    out
}

/// True when `dir` can be created and written to.
fn is_writable(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

static RESOLVED_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

impl Config {
    /// Directory holding config, tokens, logs and state.
    ///
    /// Resolved once per process: the first candidate that accepts a probe write wins. A locked
    /// down `%LOCALAPPDATA%` (mandatory integrity label, AV policy, roaming profile) must not make
    /// the tool unusable, so we fall back instead of failing.
    pub fn dir() -> PathBuf {
        RESOLVED_DIR
            .get_or_init(|| {
                let first = env_dir();
                migrate_legacy_state(&first);
                let candidates = candidates();
                // Prefer a directory that already holds state. Writability can differ between
                // processes (sandboxed shells, elevated launches), and picking the first writable
                // directory each time would split state across locations - which silently loses the
                // title cache and the login token.
                if let Some(existing) = candidates
                    .iter()
                    .find(|dir| dir.join("auth.json").exists() && is_writable(dir))
                {
                    return existing.clone();
                }
                for cand in &candidates {
                    if is_writable(cand) {
                        if *cand != candidates[0] {
                            eprintln!(
                                "warning: {} is not writable, using {} for state",
                                candidates[0].display(),
                                cand.display()
                            );
                        }
                        return cand.clone();
                    }
                }
                candidates.last().cloned().unwrap_or_else(env_dir)
            })
            .clone()
    }

    /// Configuration file path.
    pub fn path() -> PathBuf {
        Config::dir().join("config.toml")
    }

    /// Load configuration, writing defaults on first run.
    ///
    /// A read-only or missing state directory must not make read-only commands fail, so a failure
    /// to persist the defaults is reported as a warning and the in-memory defaults are used.
    pub fn load() -> Result<Config> {
        let path = Config::path();
        if !path.exists() {
            let cfg = Config::default();
            if let Err(e) = cfg.save() {
                eprintln!(
                    "warning: cannot write {} ({e}); continuing with defaults",
                    path.display()
                );
            }
            return Ok(cfg);
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!(
                    "warning: cannot read {} ({e}); continuing with defaults",
                    path.display()
                );
                return Ok(Config::default());
            }
        };
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|e| Error::usage(format!("invalid config {}: {e}", path.display())))?;
        cfg.concurrency.validate()?;

        // A configured install root can become unwritable (drive removed, ACL change, BitLocker).
        // Falling back keeps `install` usable instead of failing with a bare "access denied".
        if !dir_writable(std::path::Path::new(&cfg.install_root)) {
            match default_install_root_fallback() {
                Some(fallback) => {
                    eprintln!(
                        "warning: install root {} is not writable, using {}",
                        cfg.install_root, fallback
                    );
                    cfg.install_root = fallback;
                }
                None => eprintln!(
                    "warning: install root {} is not writable; set one with `skiff config set install-root <path>`",
                    cfg.install_root
                ),
            }
        }
        Ok(cfg)
    }

    /// Persist configuration atomically.
    pub fn save(&self) -> Result<()> {
        let dir = Config::dir();
        std::fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::usage(format!("cannot serialize config: {e}")))?;
        crate::util::atomic_write(&Config::path(), text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_rejects_out_of_range() {
        assert!(Concurrency { downloads: 0, ..Concurrency::default() }.validate().is_err());
        assert!(Concurrency { downloads: 129, ..Concurrency::default() }.validate().is_err());
        assert!(Concurrency { writers: 0, ..Concurrency::default() }.validate().is_err());
        assert!(Concurrency { downloads: 32, writers: 4, ..Concurrency::default() }
            .validate()
            .is_ok());
    }

    #[test]
    fn concurrency_defaults_are_sane() {
        let c = Concurrency::default();
        assert_eq!(c.downloads, 16);
        assert_eq!(c.writers, 4);
        assert!(!c.auto);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.concurrency.downloads, cfg.concurrency.downloads);
        assert_eq!(back.platform, cfg.platform);
    }
}
