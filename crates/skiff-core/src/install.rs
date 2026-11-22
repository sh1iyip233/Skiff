//! Install orchestration: layout, resume ledger, verification and `.egstore` metadata.
//!
//! Layout written by a successful install:
//!
//! ```text
//! <install_dir>/
//!   .egstore/
//!     <guid>.item       Epic launcher manifest (EGL format)
//!     <guid>.mancpn     manifest comparison stub used by the launcher
//!     <guid>.manifest   the raw manifest bytes served by the CDN
//!     skiff.resume      resume ledger (removed after success)
//!   ...game files...
//! ```

use crate::api::{Api, BuildInfo, GameEntry};
use crate::config::Concurrency;
use crate::download::{self, ChunkJob, ChunkSink, DownloadOptions, Progress, ProgressTracker};
use crate::error::{Error, Result};
use crate::http::{Client, Request};
use crate::manifest::{FileManifest, Manifest};
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Options for one install.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub install_dir: PathBuf,
    pub concurrency: Concurrency,
    pub limit_bytes_per_sec: Option<u64>,
    pub verify: bool,
    pub repair: bool,
    pub register_launcher: bool,
}

/// One resolved app that takes part in an install.
///
/// A normal install has one component; a bundled install has the main game first and one component
/// per add-on. The manifests are already fetched and parsed so the merge and the download pipeline
/// are pure (and testable without an Epic account).
#[derive(Debug, Clone)]
pub struct InstallComponent {
    pub game: GameEntry,
    pub build: BuildInfo,
    pub manifest: Manifest,
    pub raw: Vec<u8>,
}

/// Result of an install.
#[derive(Debug, Clone, Default)]
pub struct InstallOutcome {
    pub app_name: String,
    pub build_version: String,
    pub files: usize,
    pub install_bytes: u64,
    pub chunks_downloaded: usize,
    pub resumed_chunks: usize,
    pub repaired_chunks: usize,
    pub peak_inflight: usize,
    pub elapsed_secs: f64,
    /// Number of apps written into the install directory (1 = main game only).
    pub components: usize,
    /// App names of the add-ons that were installed with the main game.
    pub dlc_apps: Vec<String>,
    /// Add-ons that were requested but skipped (no build, encrypted manifest, network error).
    pub skipped_dlc: Vec<String>,
    /// Same-path files where the earlier component won; the DLC version was not written.
    pub conflicts: usize,
}

/// Verification findings.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyReport {
    pub checked: usize,
    pub ok: usize,
    pub bad: Vec<String>,
    pub missing: Vec<String>,
}

impl VerifyReport {
    /// True when every checked file matched.
    pub fn is_clean(&self) -> bool {
        self.bad.is_empty() && self.missing.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ResumeLedger {
    schema: u32,
    app_name: String,
    build_version: String,
    manifest_sha1: String,
    done_chunks: Vec<usize>,
    bytes_done: u64,
    updated_at: u64,
}

impl ResumeLedger {
    fn path(install_dir: &Path) -> PathBuf {
        install_dir.join(".egstore").join("skiff.resume")
    }

    /// Pre-rename bookkeeping file; read once so an install paused by the old binary can resume.
    fn legacy_path(install_dir: &Path) -> PathBuf {
        install_dir.join(".egstore").join("epicd.resume")
    }

    fn load(install_dir: &Path, app_name: &str, manifest_sha1: &str) -> ResumeLedger {
        let path = ResumeLedger::path(install_dir);
        let text = std::fs::read_to_string(&path)
            .or_else(|_| std::fs::read_to_string(ResumeLedger::legacy_path(install_dir)));
        let Ok(text) = text else {
            return ResumeLedger {
                schema: 1,
                app_name: app_name.to_string(),
                manifest_sha1: manifest_sha1.to_string(),
                ..Default::default()
            };
        };
        match serde_json::from_str::<ResumeLedger>(&text) {
            // A different build invalidates the ledger; the caller falls back to a fresh start.
            Ok(ledger) if ledger.manifest_sha1 == manifest_sha1 && ledger.app_name == app_name => {
                ledger
            }
            _ => ResumeLedger {
                schema: 1,
                app_name: app_name.to_string(),
                manifest_sha1: manifest_sha1.to_string(),
                ..Default::default()
            },
        }
    }

    fn save(&self, install_dir: &Path) -> Result<()> {
        if let Some(parent) = ResumeLedger::path(install_dir).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut copy = self.clone();
        copy.updated_at = util::now_unix();
        let json = serde_json::to_vec(&copy)
            .map_err(|e| Error::integrity(format!("cannot serialize resume ledger: {e}")))?;
        util::atomic_write(&ResumeLedger::path(install_dir), &json)
    }

    fn remove(install_dir: &Path) {
        let _ = std::fs::remove_file(ResumeLedger::path(install_dir));
        let _ = std::fs::remove_file(ResumeLedger::legacy_path(install_dir));
    }
}

struct FileSlot {
    handle: Mutex<File>,
}

/// Sink that scatters decoded chunk bytes into their target file offsets.
struct InstallSink {
    slots: Vec<FileSlot>,
    /// chunk index -> [(file index, part index)]
    mapping: HashMap<usize, Vec<(usize, usize)>>,
    files: Vec<FileManifest>,
    ledger: Mutex<ResumeLedger>,
    install_dir: PathBuf,
    pending: AtomicUsize,
}

impl ChunkSink for InstallSink {
    fn write_chunk(&self, index: usize, plaintext: &[u8]) -> Result<()> {
        if let Some(parts) = self.mapping.get(&index) {
            for &(file_idx, part_idx) in parts {
                let part = &self.files[file_idx].parts[part_idx];
                let start = part.chunk_offset as usize;
                let end = start
                    .checked_add(part.size as usize)
                    .ok_or_else(|| Error::integrity("chunk part size overflow"))?;
                if end > plaintext.len() {
                    return Err(Error::integrity(format!(
                        "chunk {index} part exceeds decoded length ({} > {})",
                        end,
                        plaintext.len()
                    )));
                }
                let slot = &self.slots[file_idx];
                let handle = slot.handle.lock().unwrap();
                write_at(&handle, &plaintext[start..end], part.file_offset)?;
            }
        }

        {
            let mut ledger = self.ledger.lock().unwrap();
            ledger.done_chunks.push(index);
            ledger.bytes_done = ledger
                .bytes_done
                .saturating_add(self.bytes_for_chunk(index));
        }
        let flushed = self.pending.fetch_add(1, Ordering::AcqRel) + 1;
        if flushed.is_multiple_of(64) {
            let ledger = self.ledger.lock().unwrap();
            ledger.save(&self.install_dir)?;
        }
        Ok(())
    }
}

impl InstallSink {
    fn bytes_for_chunk(&self, index: usize) -> u64 {
        self.mapping
            .get(&index)
            .map(|parts| {
                parts
                    .iter()
                    .map(|&(f, p)| self.files[f].parts[p].size as u64)
                    .sum()
            })
            .unwrap_or(0)
    }
}

/// Random-access write.
fn write_at(file: &File, data: &[u8], offset: u64) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut written = 0usize;
        while written < data.len() {
            let n = file
                .seek_write(&data[written..], offset + written as u64)
                .map_err(|e| Error::integrity(format!("write failed: {e}")))?;
            if n == 0 {
                return Err(Error::integrity("write returned 0 bytes"));
            }
            written += n;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        let mut written = 0usize;
        while written < data.len() {
            let n = file
                .write_at(&data[written..], offset + written as u64)
                .map_err(|e| Error::integrity(format!("write failed: {e}")))?;
            if n == 0 {
                return Err(Error::integrity("write returned 0 bytes"));
            }
            written += n;
        }
        Ok(())
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (file, data, offset);
        Err(Error::integrity("random access writes are unsupported on this platform"))
    }
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Keep only CDN hosts that actually serve the first chunk of this build.
///
/// One short request per host; a host that fails is dropped so the download does not waste a retry
/// budget on it. If every host fails the probe, all hosts are kept - the download itself then
/// produces the real error.
///
/// The probes run in parallel and the wait is cancel-aware: a dead property can take ~10 s of DNS
/// and connect time to fail, and neither the other probes nor a cancel should queue behind it. The
/// probe threads are detached, so abandoning them on cancel costs only their own timeout.
fn probe_cdn_hosts(
    client: &Client,
    bases: &[String],
    manifest: &Manifest,
    cancel: &Arc<AtomicBool>,
) -> Vec<String> {
    if bases.len() <= 1 {
        return bases.to_vec();
    }
    let Some(first) = manifest.chunks.first() else {
        return bases.to_vec();
    };
    let path = crate::cdn::chunk_path(
        manifest.version,
        first.group_num,
        first.rolling_hash,
        &first.guid,
        first.secret_guid.as_ref(),
    );

    let (tx, rx) = mpsc::channel();
    for (index, base) in bases.iter().enumerate() {
        let tx = tx.clone();
        let base = base.clone();
        let path = path.clone();
        let cancel = cancel.clone();
        let client = client.clone();
        std::thread::spawn(move || {
            if cancelled(&cancel) {
                return;
            }
            let url = crate::cdn::chunk_url(&base, &path);
            let request = Request::get(&url).timeout(8).cancellable(cancel.clone());
            let mut sink = std::io::sink();
            let healthy = match client.request(&request, &mut sink) {
                Ok(head) if head.is_success() => true,
                Ok(head) => {
                    eprintln!(
                        "warning: CDN host {} answered HTTP {}; dropping it for this install",
                        host_of(&base),
                        head.status
                    );
                    false
                }
                Err(e) => {
                    eprintln!(
                        "warning: CDN host {} is unreachable ({e}); dropping it for this install",
                        host_of(&base)
                    );
                    false
                }
            };
            let _ = tx.send((index, healthy));
        });
    }
    drop(tx);

    let mut healthy_flags = vec![false; bases.len()];
    let mut answered = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    while answered < bases.len() && !cancelled(cancel) && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((index, healthy)) => {
                healthy_flags[index] = healthy;
                answered += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if cancelled(cancel) {
        // The caller is about to abort; the probe result is irrelevant.
        return bases.to_vec();
    }

    let healthy: Vec<String> = bases
        .iter()
        .zip(healthy_flags.iter())
        .filter(|(_, ok)| **ok)
        .map(|(base, _)| base.clone())
        .collect();
    if healthy.is_empty() {
        eprintln!("warning: no CDN host passed the probe; retrying with all of them");
        bases.to_vec()
    } else {
        eprintln!(
            "using {} of {} CDN host(s): {}",
            healthy.len(),
            bases.len(),
            healthy.iter().map(|b| host_of(b)).collect::<Vec<_>>().join(", ")
        );
        healthy
    }
}

/// Host portion of a base URL, for log lines.
fn host_of(base: &str) -> &str {
    base.split("://").nth(1).and_then(|rest| rest.split('/').next()).unwrap_or(base)
}
/// Fail early, and with the offending path, when the target directory cannot be written.
///
/// `os error 5` on a bare path is the least actionable message a downloader can emit, so the path
/// and the escape hatch are always part of the text.
fn ensure_writable_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir.join(".egstore")).map_err(|e| {
        Error::integrity(format!(
            "cannot create install directory {}: {e} (choose another location with --dir)",
            dir.display()
        ))
    })?;
    let probe = dir.join(".egstore").join(".write-probe");
    std::fs::write(&probe, b"ok").map_err(|e| {
        Error::integrity(format!(
            "cannot write into {}: {e} (choose another location with --dir)",
            dir.display()
        ))
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// One-line result of deleting an unfinished install.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscardReport {
    /// Files that were deleted.
    pub removed: usize,
    /// True when a completed install was present and its files were kept.
    pub kept_existing_install: bool,
}

fn files_list_path(install_dir: &Path) -> PathBuf {
    install_dir.join(".egstore").join("skiff.files")
}

/// Pre-rename file list; read for compatibility with an install paused by the old binary.
fn legacy_files_list_path(install_dir: &Path) -> PathBuf {
    install_dir.join(".egstore").join("epicd.files")
}

/// Record the files this run may create.
///
/// Written once before the first file is touched and removed after a successful install, this list is
/// what lets a cancel delete exactly this game's partial files — never an unrelated file the user
/// keeps in the same folder.
fn save_files_list(install_dir: &Path, files: &[(String, FileManifest)]) -> Result<()> {
    let rels: Vec<&str> = files.iter().map(|(rel, _)| rel.as_str()).collect();
    let json = serde_json::to_vec(&rels)
        .map_err(|e| Error::integrity(format!("cannot serialize the file list: {e}")))?;
    util::atomic_write(&files_list_path(install_dir), &json)
}

fn load_files_list(install_dir: &Path) -> Vec<String> {
    std::fs::read(files_list_path(install_dir))
        .or_else(|_| std::fs::read(legacy_files_list_path(install_dir)))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .unwrap_or_default()
}

fn remove_files_list(install_dir: &Path) {
    let _ = std::fs::remove_file(files_list_path(install_dir));
    let _ = std::fs::remove_file(legacy_files_list_path(install_dir));
}

/// True when `install_dir` holds an unfinished download (paused or interrupted).
pub fn has_partial_install(install_dir: &Path) -> bool {
    ResumeLedger::path(install_dir).exists()
        || ResumeLedger::legacy_path(install_dir).exists()
        || files_list_path(install_dir).exists()
        || legacy_files_list_path(install_dir).exists()
}

/// Delete the files an unfinished install created.
///
/// Safe by construction: only the paths recorded by [`save_files_list`] are removed, directories are
/// pruned only when they become empty, and a completed install is never touched (that would destroy a
/// working game just because an update was cancelled).
pub fn discard_partial_install(install_dir: &Path) -> DiscardReport {
    let had_completed_install = load_installed_manifest(install_dir).is_ok();
    discard_partial(install_dir, had_completed_install)
}

fn discard_partial(install_dir: &Path, kept_existing_install: bool) -> DiscardReport {
    if kept_existing_install {
        // Updating a working install: drop the bookkeeping, keep the game.
        ResumeLedger::remove(install_dir);
        remove_files_list(install_dir);
        return DiscardReport { removed: 0, kept_existing_install: true };
    }

    let listed = load_files_list(install_dir);
    let mut removed = 0usize;
    let mut parents: Vec<PathBuf> = Vec::new();
    for rel in listed {
        let Ok(safe) = util::sanitize_rel_path(&rel) else { continue };
        let path = util::install_path(install_dir, &safe);
        if !util::is_within(install_dir, &path) {
            continue;
        }
        if let Some(parent) = path.parent() {
            parents.push(parent.to_path_buf());
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("warning: could not remove {}: {e}", path.display()),
        }
    }
    // `.egstore` holds only our own bookkeeping while the install is unfinished.
    let _ = std::fs::remove_dir_all(install_dir.join(".egstore"));
    // Deepest directories first, so parents become empty before we try to remove them.
    parents.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    parents.dedup();
    for dir in parents {
        if util::is_within(install_dir, &dir) {
            let _ = std::fs::remove_dir(&dir);
        }
    }
    let _ = std::fs::remove_dir(install_dir);
    DiscardReport { removed, kept_existing_install: false }
}
/// What a front-end wants to happen when the running install stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopKind {    /// Stop and keep the partial files: a later install resumes from the ledger.
    Pause,
    /// Stop and delete the partial files of this run.
    Cancel,
}

/// Cancellation slot shared with a front-end.
///
/// `install_manifest` used to create its own flag, so a GUI "cancel" button could never reach the
/// download workers. A front-end now hands its flag in here first.
static EXTERNAL_CANCEL: std::sync::Mutex<Option<Arc<AtomicBool>>> = std::sync::Mutex::new(None);

/// How the current stop request wants the partial data handled.
static STOP_KIND: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Register the stop flag a front-end will toggle.
///
/// Each new run starts in [`StopKind::Pause`] mode so an unqualified stop never deletes data.
pub fn set_cancel_flag(flag: Arc<AtomicBool>) {
    STOP_KIND.store(0, Ordering::Relaxed);
    if let Ok(mut slot) = EXTERNAL_CANCEL.lock() {
        *slot = Some(flag);
    }
}

/// Ask the running install to stop.
///
/// The kind decides what happens to the partial download: [`StopKind::Pause`] keeps it for a resume,
/// [`StopKind::Cancel`] deletes the files this run created.
pub fn request_stop(kind: StopKind) {
    STOP_KIND.store(
        if kind == StopKind::Cancel { 1 } else { 0 },
        Ordering::Relaxed,
    );
    if let Ok(slot) = EXTERNAL_CANCEL.lock() {
        if let Some(flag) = slot.as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// The kind of the latest stop request.
fn requested_stop() -> StopKind {
    if STOP_KIND.load(Ordering::Relaxed) == 1 {
        StopKind::Cancel
    } else {
        StopKind::Pause
    }
}

fn shared_cancel() -> Arc<AtomicBool> {
    EXTERNAL_CANCEL
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default()
}

/// True when the front-end asked for cancellation.
fn cancelled(cancel: &Arc<AtomicBool>) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// Error returned for a user-requested stop. The cleanup pass rewrites it with what was deleted.
fn cancelled_error() -> Error {
    Error::Cancelled { removed: 0, kept_existing_install: false }
}

/// Fetch the manifest and install (or resume) one game.
pub fn install(
    client: &Client,
    api: &Api<'_>,
    game: &GameEntry,
    opts: &InstallOptions,
    on_progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
) -> Result<InstallOutcome> {
    let cancel = shared_cancel();
    // The build lookup, the manifest download and the CDN probe are the long blocking requests
    // before the first byte is downloaded; make them honour the same flag as the workers so a
    // cancel does not have to wait for them.
    let api = api.clone().with_cancel(cancel.clone());

    if cancelled(&cancel) {
        return Err(cancelled_error());
    }
    let component = resolve_component(&api, game)?;
    install_components(client, &[component], opts, on_progress)
}

/// Install a main game and its add-ons into one directory.
///
/// Add-ons are resolved individually: a DLC whose build is missing, encrypted or otherwise
/// unavailable is reported and skipped so it can never block the main game. The returned outcome
/// lists both the installed add-ons and the skipped ones.
pub fn install_bundle(
    client: &Client,
    api: &Api<'_>,
    base: &GameEntry,
    dlcs: &[GameEntry],
    opts: &InstallOptions,
    on_progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
) -> Result<InstallOutcome> {
    let cancel = shared_cancel();
    let api = api.clone().with_cancel(cancel.clone());

    if cancelled(&cancel) {
        return Err(cancelled_error());
    }
    let mut components = vec![resolve_component(&api, base)?];
    let mut skipped_dlc: Vec<String> = Vec::new();
    for dlc in dlcs {
        if dlc.app_name.eq_ignore_ascii_case(&base.app_name) {
            continue;
        }
        if components
            .iter()
            .any(|component| component.game.app_name.eq_ignore_ascii_case(&dlc.app_name))
        {
            continue;
        }
        if cancelled(&cancel) {
            return Err(cancelled_error());
        }
        match resolve_component(&api, dlc) {
            Ok(component) => components.push(component),
            Err(e) => {
                eprintln!(
                    "warning: skipping DLC {} ({}): {e}",
                    if dlc.title.is_empty() { &dlc.app_name } else { &dlc.title },
                    dlc.app_name
                );
                skipped_dlc.push(dlc.app_name.clone());
            }
        }
    }

    let mut outcome = install_components(client, &components, opts, on_progress)?;
    outcome.skipped_dlc = skipped_dlc;
    Ok(outcome)
}

fn resolve_component(api: &Api<'_>, game: &GameEntry) -> Result<InstallComponent> {
    let build = api.build(game)?;
    let raw = api.manifest(&build.manifest_uri)?;
    let manifest = Manifest::parse(&raw)?;
    Ok(InstallComponent { game: game.clone(), build, manifest, raw })
}

/// Install from an already fetched manifest.
///
/// Split out from [`install`] so the whole layout/resume/verify/egstore pipeline can be tested
/// against a local mock CDN without any Epic account.
pub fn install_manifest(
    client: &Client,
    build: &BuildInfo,
    game: &GameEntry,
    manifest: &Manifest,
    raw: &[u8],
    opts: &InstallOptions,
    on_progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
) -> Result<InstallOutcome> {
    let component = InstallComponent {
        game: game.clone(),
        build: build.clone(),
        manifest: manifest.clone(),
        raw: raw.to_vec(),
    };
    install_components(client, &[component], opts, on_progress)
}

/// Install one or more resolved components into `opts.install_dir`.
///
/// The first component is the main game; its app name and install folder own the result. Add-ons
/// are merged into the same directory (that is how Epic ships them) and get their own `.egstore`
/// metadata entries.
pub fn install_components(
    client: &Client,
    components: &[InstallComponent],
    opts: &InstallOptions,
    on_progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
) -> Result<InstallOutcome> {
    if components.is_empty() {
        return Err(Error::usage("an install needs at least one component"));
    }
    // Remembered before this run touches anything: it decides whether a cancel may delete files.
    let had_completed_install = load_installed_manifest(&opts.install_dir).is_ok();
    let result = install_components_inner(client, components, opts, on_progress);

    // A cancel (as opposed to a pause) removes what this run created before the error is returned.
    if let Err(Error::Cancelled { .. }) = &result {
        if requested_stop() == StopKind::Cancel {
            let report = discard_partial(&opts.install_dir, had_completed_install);
            return Err(Error::Cancelled {
                removed: report.removed,
                kept_existing_install: report.kept_existing_install,
            });
        }
    }
    result
}

/// A main-game + add-on plan merged into one download/verify surface.
struct MergedPlan {
    manifest: Manifest,
    /// Feature level per merged chunk (index-aligned with `manifest.chunks`).
    chunk_versions: Vec<u32>,
    bases: Vec<String>,
    conflicts: usize,
}

/// Merge the component manifests.
///
/// Rules:
/// - files are keyed by sanitised relative path; identical copies are deduplicated and a different
///   copy is skipped in favour of the earlier component (main game first) with a warning;
/// - chunks are keyed by GUID; a GUID that disagrees on size or SHA-1 is an integrity error;
/// - each chunk remembers the feature level of the manifest it came from, because the CDN path
///   contains `ChunksV3/V4/V5`.
fn merge_components(components: &[InstallComponent]) -> Result<MergedPlan> {
    let base = components
        .first()
        .ok_or_else(|| Error::usage("an install needs at least one component"))?;
    if let Some(encrypted) = components.iter().find(|component| component.manifest.encrypted) {
        return Err(Error::Manifest(format!(
            "{} ships an encrypted manifest; install it through the official launcher",
            encrypted.game.app_name
        )));
    }

    let mut files: Vec<(String, FileManifest)> = Vec::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut conflicts = 0usize;

    for component in components {
        for file in &component.manifest.files {
            let safe = util::sanitize_rel_path(&file.path)?;
            if let Some(&index) = by_path.get(&safe) {
                let existing = &files[index].1;
                if existing.hash == file.hash
                    && existing.size == file.size
                    && existing.parts == file.parts
                {
                    continue;
                }
                conflicts += 1;
                if conflicts <= 5 {
                    eprintln!(
                        "warning: {} appears in both {} and {} with different content; keeping the earlier one",
                        safe, existing.path, file.path
                    );
                }
                continue;
            }
            by_path.insert(safe.clone(), files.len());
            files.push((safe, file.clone()));
        }
    }

    let mut chunks: Vec<crate::manifest::ChunkInfo> = Vec::new();
    let mut chunk_versions = Vec::new();
    let mut by_guid: HashMap<[u32; 4], usize> = HashMap::new();
    for component in components {
        let version = component.manifest.version;
        for chunk in &component.manifest.chunks {
            if let Some(&index) = by_guid.get(&chunk.guid) {
                let existing = &chunks[index];
                if existing.file_size != chunk.file_size || existing.sha1 != chunk.sha1 {
                    return Err(Error::integrity(format!(
                        "chunk {} differs between {} and {}; refusing to merge",
                        hex::encode_upper(crate::chunk::flatten_guid(&chunk.guid)),
                        components[0].game.app_name,
                        component.game.app_name
                    )));
                }
                continue;
            }
            by_guid.insert(chunk.guid, chunks.len());
            chunks.push(chunk.clone());
            chunk_versions.push(version);
        }
    }

    let mut bases: Vec<String> = Vec::new();
    for component in components {
        for base_url in &component.build.base_urls {
            if !base_url.trim().is_empty() && !bases.contains(base_url) {
                bases.push(base_url.clone());
            }
        }
    }

    let mut meta = base.manifest.meta.clone();
    // The merged manifest is not a real build; the caller writes the original bytes per component.
    meta.app_name = base.game.app_name.clone();
    let manifest = Manifest {
        version: base.manifest.version,
        compressed: base.manifest.compressed,
        encrypted: false,
        header_sha1: base.manifest.header_sha1,
        meta,
        chunks,
        files: files.into_iter().map(|(_, file)| file).collect(),
        custom: Default::default(),
        raw: Vec::new(),
    };

    Ok(MergedPlan { manifest, chunk_versions, bases, conflicts })
}

/// Stable identity of a bundle, used to invalidate the resume ledger when the DLC set changes.
fn bundle_signature(components: &[InstallComponent]) -> Vec<u8> {
    let mut out = Vec::new();
    for component in components {
        out.extend_from_slice(component.game.app_name.as_bytes());
        out.push(0);
        out.extend_from_slice(component.build.build_version.as_bytes());
        out.push(0);
        out.extend_from_slice(&component.raw);
    }
    out
}

fn install_components_inner(
    client: &Client,
    components: &[InstallComponent],
    opts: &InstallOptions,
    on_progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
) -> Result<InstallOutcome> {
    opts.concurrency.validate()?;
    if opts.concurrency.auto {
        return Err(Error::usage(
            "automatic concurrency tuning is not available; pass an explicit --concurrency value",
        ));
    }
    let started = std::time::Instant::now();
    let base = &components[0];
    let game = &base.game;
    let build = &base.build;
    let plan = merge_components(components)?;
    let manifest = &plan.manifest;
    let raw = bundle_signature(components);

    if manifest.encrypted {
        return Err(Error::Manifest(
            "this build ships an encrypted manifest; install it through the official launcher"
                .into(),
        ));
    }

    // Validate every path before touching the disk.
    let mut files: Vec<(String, FileManifest)> = Vec::with_capacity(manifest.files.len());
    for file in &manifest.files {
        let safe = util::sanitize_rel_path(&file.path)?;
        files.push((safe, file.clone()));
    }

    let manifest_sha1 = sha1_hex(&raw);
    let install_dir = &opts.install_dir;
    ensure_writable_dir(install_dir)?;
    // Record what this run may create, so a later cancel can remove exactly that.
    save_files_list(install_dir, &files)?;

    let ledger = ResumeLedger::load(install_dir, &game.app_name, &manifest_sha1);
    let resumed_chunks = ledger.done_chunks.len();
    let mut done: HashMap<usize, ()> = HashMap::new();
    for idx in &ledger.done_chunks {
        done.insert(*idx, ());
    }

    // Incremental update: a chunk whose GUID and size match an installed component is already
    // correct on disk. The previous install may itself be a bundle, so every component contributes.
    let previous_components = load_installed_components(install_dir).unwrap_or_default();
    let previous_chunks: Vec<crate::manifest::ChunkInfo> = previous_components
        .iter()
        .flat_map(|component| component.manifest.chunks.iter().cloned())
        .collect();
    let previous_files: Vec<FileManifest> = previous_components
        .iter()
        .flat_map(|component| component.manifest.files.iter().cloned())
        .collect();

    let mut unchanged_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !previous_chunks.is_empty() {
        let changed = crate::update::changed_chunks(&previous_chunks, &manifest.chunks);
        let reusable = crate::update::reusable_chunk_count(&previous_chunks, &manifest.chunks);
        if reusable > 0 {
            for (index, chunk) in manifest.chunks.iter().enumerate() {
                if !changed.contains(&chunk.guid) {
                    done.insert(index, ());
                }
            }
            eprintln!(
                "incremental: reusing {reusable} of {} chunk(s) already on disk",
                manifest.chunks.len()
            );
        }
        let unchanged =
            crate::update::unchanged_files(&previous_files, &manifest.files, &changed);
        if !unchanged.is_empty() {
            eprintln!(
                "incremental: skipping verification of {} unchanged file(s)",
                unchanged.len()
            );
        }
        unchanged_paths = unchanged;
    }

    // Open (and preallocate) every target file. Large games ship tens of thousands of files, so
    // check for cancellation periodically instead of only after the whole loop.
    let cancel = shared_cancel();
    let mut slots = Vec::with_capacity(files.len());
    for (index, (rel, meta)) in files.iter().enumerate() {
        if index % 64 == 0 && cancelled(&cancel) {
            return Err(cancelled_error());
        }
        let path = util::install_path(install_dir, rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let handle = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let expected = meta.size;
        let current = handle.metadata().map(|m| m.len()).unwrap_or(0);
        if current != expected {
            handle.set_len(expected)?;
        }
        slots.push(FileSlot { handle: Mutex::new(handle) });
    }

    // chunk index -> target offsets
    let chunk_index = manifest.chunk_index();
    let mut mapping: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for (file_idx, (_, meta)) in files.iter().enumerate() {
        for (part_idx, part) in meta.parts.iter().enumerate() {
            let Some(&chunk_idx) = chunk_index.get(&part.guid) else {
                return Err(Error::integrity(format!(
                    "file {} references chunk {} which is not in the chunk list",
                    meta.path,
                    hex::encode_upper(crate::chunk::flatten_guid(&part.guid))
                )));
            };
            mapping.entry(chunk_idx).or_default().push((file_idx, part_idx));
        }
    }

    // Drop CDN hosts that cannot serve this build. Epic lists several properties per build and on
    // some networks one of them is blocked; round-robining into a dead host burns a full retry
    // budget per chunk and stalls the tail of the download.
    let mut bases = probe_cdn_hosts(client, &plan.bases, manifest, &cancel);
    if cancelled(&cancel) {
        return Err(cancelled_error());
    }

    if let Some(pref) = crate::config::Config::load()
        .ok()
        .and_then(|cfg| cfg.preferred_cdn)
        .filter(|p| !p.trim().is_empty())
    {
        let (matching, rest): (Vec<String>, Vec<String>) =
            bases.iter().cloned().partition(|base| base.contains(&pref));
        if matching.is_empty() {
            eprintln!("warning: no CDN host matches preferred_cdn '{pref}'");
        } else {
            let mut ordered = matching;
            ordered.extend(rest);
            bases = ordered;
            eprintln!("preferred_cdn '{pref}': {bases:?}");
        }
    }

    let mut jobs = download::plan_jobs_with_versions(manifest, &done, &plan.chunk_versions);

    // A resume ledger only records which chunks were written; it cannot prove the files are intact.
    // Files are pre-allocated to their final size, so an interrupted run leaves full-size holes that
    // look "present" to the ledger but fail verification. The completion marker is `.egstore/*.manifest`,
    // written only after a successful install, so its absence means the ledger is ahead of reality.
    let egstore_complete = std::fs::read_dir(install_dir.join(".egstore"))
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .ends_with(".manifest")
            })
        })
        .unwrap_or(false);
    if jobs.is_empty() && !egstore_complete {
        eprintln!(
            "warning: resume ledger claims every chunk is present, but this install never completed; re-downloading all chunks"
        );
        jobs = download::plan_jobs_with_versions(
            manifest,
            &std::collections::HashMap::new(),
            &plan.chunk_versions,
        );
    }
    let total_bytes: u64 = jobs.iter().map(|j: &ChunkJob| j.chunk.file_size.max(0) as u64).sum();
    let tracker = ProgressTracker::new(jobs.len(), total_bytes);

    let mut sink = InstallSink {
        slots,
        mapping,
        files: files.iter().map(|(_, m)| m.clone()).collect(),
        ledger: Mutex::new(ledger.clone()),
        install_dir: install_dir.clone(),
        pending: AtomicUsize::new(0),
    };

    let mut stats = download::download(
        client,
        &crate::cdn::CdnHosts::new(bases.clone()),
        &jobs,
        &DownloadOptions {
            concurrency: opts.concurrency.clone(),
            limit_bytes_per_sec: opts.limit_bytes_per_sec,
            attempts: 5,
        },
        &sink,
        tracker,
        cancel.clone(),
        on_progress,
    )?;

    // A cancelled run must not fall through to verification: the files are known-incomplete, and the
    // verify + repair pass would re-download against a tripped cancel flag and then report an
    // integrity error that reads like corruption. Persist the ledger so the next run resumes.
    if cancelled(&cancel) {
        let mut ledger = sink.ledger.lock().unwrap();
        ledger.done_chunks.sort_unstable();
        ledger.done_chunks.dedup();
        let _ = ledger.save(install_dir);
        return Err(cancelled_error());
    }

    if !stats.failures.is_empty() {
        let mut ledger = sink.ledger.lock().unwrap();
        ledger.done_chunks.sort_unstable();
        ledger.done_chunks.dedup();
        let _ = ledger.save(install_dir);
        let sample: Vec<String> = stats
            .failures
            .iter()
            .take(3)
            .map(|(idx, msg)| format!("#{idx}: {msg}"))
            .collect();
        return Err(Error::net(format!(
            "{} chunk(s) failed and the download can be resumed; first failures: {}",
            stats.failures.len(),
            sample.join("; ")
        )));
    }

    // Verification: cheap enough to always run on the chunks we just wrote.
    let mut report = if unchanged_paths.is_empty() {
        verify_files(install_dir, &files, opts.concurrency.verify_threads())?
    } else {
        let subset: Vec<_> = files
            .iter()
            .filter(|(rel, _)| !unchanged_paths.contains(rel))
            .cloned()
            .collect();
        verify_files(install_dir, &subset, opts.concurrency.verify_threads())?
    };
    if cancelled(&cancel) {
        return Err(cancelled_error());
    }
    let mut repaired_chunks = 0usize;

    if opts.repair && !report.is_clean() {
        let bad: HashSet<&String> = report.bad.iter().chain(report.missing.iter()).collect();
        let mut redo: HashSet<usize> = HashSet::new();
        for (rel, meta) in &files {
            if bad.contains(rel) {
                for part in &meta.parts {
                    if let Some(&idx) = chunk_index.get(&part.guid) {
                        redo.insert(idx);
                    }
                }
            }
        }
        if !redo.is_empty() {
            let mut done_map = HashMap::new();
            for (index, _) in manifest.chunks.iter().enumerate() {
                if !redo.contains(&index) {
                    done_map.insert(index, ());
                }
            }
            let redo_jobs =
                download::plan_jobs_with_versions(manifest, &done_map, &plan.chunk_versions);
            repaired_chunks = redo_jobs.len();
            let total: u64 = redo_jobs
                .iter()
                .map(|j| j.chunk.file_size.max(0) as u64)
                .sum();
            let tracker = ProgressTracker::new(redo_jobs.len(), total);
            let redo_sink = InstallSink {
                slots: std::mem::take(&mut sink.slots),
                mapping: std::mem::take(&mut sink.mapping),
                files: std::mem::take(&mut sink.files),
                ledger: Mutex::new(ResumeLedger::default()),
                install_dir: install_dir.clone(),
                pending: AtomicUsize::new(0),
            };
            let repair_stats = download::download(
                client,
                &crate::cdn::CdnHosts::new(bases.clone()),
                &redo_jobs,
                &DownloadOptions {
                    concurrency: opts.concurrency.clone(),
                    limit_bytes_per_sec: opts.limit_bytes_per_sec,
                    attempts: 5,
                },
                &redo_sink,
                tracker,
                cancel.clone(),
                None,
            )?;
            stats.chunks_done += repair_stats.chunks_done;
            report = verify_files(install_dir, &files, opts.concurrency.verify_threads())?;
        }
    }

    if cancelled(&cancel) {
        return Err(cancelled_error());
    }

    if !report.is_clean() {
        return Err(Error::integrity(format!(
            "verification failed: {} file(s) mismatched, {} missing (re-run with --repair)",
            report.bad.len(),
            report.missing.len()
        )));
    }

    // Remove files a previous build shipped and this component no longer contains. Deletion is
    // per component: files of an add-on that is not part of this run are never touched.
    remove_obsolete_files(install_dir, &previous_components, components, &files);

    let guids = write_egstore_components(install_dir, components)?;
    if opts.register_launcher {
        register_with_launcher_components(components, &guids)?;
    }
    ResumeLedger::remove(install_dir);
    remove_files_list(install_dir);

    let mut outcome = InstallOutcome {
        app_name: game.app_name.clone(),
        build_version: build.build_version.clone(),
        files: files.len(),
        install_bytes: files.iter().map(|(_, file)| file.size).sum(),
        chunks_downloaded: stats.chunks_done,
        resumed_chunks,
        repaired_chunks,
        peak_inflight: stats.peak_inflight,
        elapsed_secs: started.elapsed().as_secs_f64(),
        components: components.len(),
        conflicts: plan.conflicts,
        ..Default::default()
    };
    if components.len() > 1 {
        outcome.dlc_apps = components[1..]
            .iter()
            .map(|component| component.game.app_name.clone())
            .collect();
    }
    Ok(outcome)
}

/// Delete files that belonged to a component in the previous install but are gone from its new
/// manifest. Components that are not part of this run are left alone.
fn remove_obsolete_files(
    install_dir: &Path,
    previous: &[InstalledComponent],
    components: &[InstallComponent],
    merged_files: &[(String, FileManifest)],
) {
    let merged: HashSet<&str> = merged_files.iter().map(|(rel, _)| rel.as_str()).collect();
    for component in components {
        let Some(old) = previous
            .iter()
            .find(|old| old.app_name.eq_ignore_ascii_case(&component.game.app_name))
        else {
            continue;
        };
        let old_paths: Vec<String> = old.manifest.files.iter().map(|f| f.path.clone()).collect();
        let new_paths: Vec<String> =
            component.manifest.files.iter().map(|f| f.path.clone()).collect();
        let mut removed = 0usize;
        for rel in crate::update::removed_paths(&old_paths, &new_paths) {
            if merged.contains(rel.as_str()) {
                continue;
            }
            let Ok(safe) = util::sanitize_rel_path(&rel) else { continue };
            let target = util::install_path(install_dir, &safe);
            if !util::is_within(install_dir, &target) {
                eprintln!("warning: refusing to delete {rel}: outside the install directory");
                continue;
            }
            match std::fs::remove_file(&target) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("warning: could not remove {rel}: {e}"),
            }
        }
        if removed > 0 {
            eprintln!(
                "incremental: removed {removed} obsolete file(s) from {}",
                component.game.app_name
            );
        }
    }
}

/// Verify every file against the SHA-1 recorded in the manifest.
///
/// Verification of a large install touches every byte on disk, so it observes the front-end's
/// cancellation flag: a cancel during verification returns the partial report instead of hashing a
/// 100 GiB game first.
pub fn verify_files(
    install_dir: &Path,
    files: &[(String, FileManifest)],
    threads: usize,
) -> Result<VerifyReport> {
    let threads = threads.max(1);
    let counter = AtomicUsize::new(0);
    let report: Mutex<VerifyReport> = Mutex::new(VerifyReport::default());
    let cancel = shared_cancel();

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let counter = &counter;
            let report = &report;
            let cancel = &cancel;
            scope.spawn(move || loop {
                if cancelled(cancel) {
                    break;
                }
                let index = counter.fetch_add(1, Ordering::AcqRel);
                if index >= files.len() {
                    break;
                }
                let (rel, meta) = &files[index];
                let path = util::install_path(install_dir, rel);
                match verify_one(&path, meta, cancel) {
                    Ok(true) => {
                        let mut guard = report.lock().unwrap();
                        guard.checked += 1;
                        guard.ok += 1;
                    }
                    Ok(false) => {
                        let mut guard = report.lock().unwrap();
                        guard.checked += 1;
                        guard.bad.push(rel.clone());
                    }
                    Err(_) => {
                        let mut guard = report.lock().unwrap();
                        guard.checked += 1;
                        guard.missing.push(rel.clone());
                    }
                }
            });
        }
    });

    Ok(report.into_inner().unwrap_or_default())
}

fn verify_one(path: &Path, meta: &FileManifest, cancel: &Arc<AtomicBool>) -> Result<bool> {
    use sha1::{Digest, Sha1};
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        // A single file can be tens of GiB, so check inside the hash loop, not only between files.
        if cancelled(cancel) {
            return Err(cancelled_error());
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    if total != meta.size {
        return Ok(false);
    }
    let actual: [u8; 20] = hasher.finalize().into();
    Ok(actual == meta.hash)
}

fn generate_guid() -> String {
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let value = fastrand::u64(..);
        chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
    }
    hex::encode_upper(bytes)
}

/// Metadata written for one component; the GUID also names its three `.egstore` files.
struct EgstoreEntry {
    guid: String,
    #[allow(dead_code)]
    app_name: String,
    item: serde_json::Value,
}

/// Build the EGL `.item` JSON for one component.
///
/// `main_game` is the component's own entry for the main game and the main game's entry for an
/// add-on. The add-on entry points at the main game via `MainGameAppName` and shares its install
/// folder, which is how the official launcher models DLC that lives inside the base install.
fn build_item_json(
    install_dir: &Path,
    build: &BuildInfo,
    game: &GameEntry,
    manifest: &Manifest,
    guid: &str,
    main_game: &GameEntry,
    is_dlc: bool,
    host_guid: &str,
) -> serde_json::Value {
    let install_str = install_dir.to_string_lossy().replace('/', "\\");
    let main_folder = if main_game.folder_name.is_empty() {
        main_game.app_name.clone()
    } else {
        main_game.folder_name.clone()
    };
    let categories = if game.categories.is_empty() {
        if is_dlc {
            serde_json::json!(["addons"])
        } else {
            serde_json::json!(["public", "games", "applications"])
        }
    } else {
        serde_json::json!(game.categories)
    };
    serde_json::json!({
        "AppCategories": categories,
        "AppName": game.app_name,
        "AppVersionString": build.build_version,
        "BaseURLs": build.base_urls,
        "BuildLabel": if build.label_name.is_empty() { "Live".to_string() } else { build.label_name.clone() },
        "CatalogItemId": game.catalog_item_id,
        "CatalogNamespace": game.namespace,
        "ChunkDbs": [],
        "CompatibleApps": [],
        "DisplayName": game.title,
        "FormatVersion": 0,
        "HostInstallationGuid": if is_dlc { host_guid } else { guid },
        "InstallComponents": [],
        "InstallLocation": install_str,
        "InstallSessionId": guid,
        "InstallSize": manifest.install_size(),
        "InstallTags": [],
        "InstallationGuid": guid,
        "LaunchCommand": if is_dlc { String::new() } else { manifest.meta.launch_command.clone() },
        "LaunchExecutable": if is_dlc { String::new() } else { manifest.meta.launch_exe.clone() },
        "MainGameAppName": main_game.app_name,
        "MainWindowProcessName": "",
        "MandatoryAppFolderName": main_folder,
        "ManifestLocation": format!("{install_str}\\.egstore"),
        "OwnershipToken": "false",
        "PrereqIds": if is_dlc { Vec::<String>::new() } else { manifest.meta.prereq_ids.clone() },
        "ProcessNames": [],
        "StagingLocation": format!("{install_str}\\.egstore\\bps"),
        "TechnicalType": "",
        "VaultThumbnailUrl": "",
        "VaultTitleText": "",
        "bCanRunOffline": true,
        "bIsApplication": !is_dlc,
        "bIsExecutable": !is_dlc,
        "bIsIncompleteInstall": false,
        "bIsManaged": false,
        "bNeedsValidation": false,
        "bRequiresAuth": true,
    })
}

/// Write `<guid>.{manifest,mancpn,item}` for one component.
fn write_component_metadata(
    install_dir: &Path,
    component: &InstallComponent,
    guid: &str,
    main_game: &GameEntry,
    is_dlc: bool,
    host_guid: &str,
) -> Result<serde_json::Value> {
    let egstore = install_dir.join(".egstore");
    std::fs::create_dir_all(&egstore)?;

    util::atomic_write(&egstore.join(format!("{guid}.manifest")), &component.raw)?;

    let mancpn = serde_json::json!({
        "FormatVersion": 0,
        "AppName": component.game.app_name,
        "CatalogItemId": component.game.catalog_item_id,
        "CatalogNamespace": component.game.namespace,
    });
    let mancpn_bytes = serde_json::to_vec_pretty(&mancpn)
        .map_err(|e| Error::integrity(format!("cannot serialize .mancpn: {e}")))?;
    util::atomic_write(&egstore.join(format!("{guid}.mancpn")), &mancpn_bytes)?;

    let item = build_item_json(
        install_dir,
        &component.build,
        &component.game,
        &component.manifest,
        guid,
        main_game,
        is_dlc,
        host_guid,
    );
    let item_bytes = serde_json::to_vec_pretty(&item)
        .map_err(|e| Error::integrity(format!("cannot serialize .item: {e}")))?;
    util::atomic_write(&egstore.join(format!("{guid}.item")), &item_bytes)?;

    Ok(item)
}

/// Write metadata for every component and remove metadata left by earlier runs.
///
/// A reinstall generates new GUIDs; without the cleanup `verify`/`inspect` and the official
/// launcher would keep seeing the previous build's `.item`/`.mancpn`/`.manifest`.
fn write_egstore_components(
    install_dir: &Path,
    components: &[InstallComponent],
) -> Result<Vec<EgstoreEntry>> {
    let base = components
        .first()
        .ok_or_else(|| Error::usage("an install needs at least one component"))?;
    let base_guid = generate_guid();
    let mut entries: Vec<EgstoreEntry> = Vec::with_capacity(components.len());
    for (index, component) in components.iter().enumerate() {
        let guid = if index == 0 { base_guid.clone() } else { generate_guid() };
        let is_dlc = index > 0;
        let item = write_component_metadata(
            install_dir,
            component,
            &guid,
            &base.game,
            is_dlc,
            &base_guid,
        )?;
        entries.push(EgstoreEntry {
            guid,
            app_name: component.game.app_name.clone(),
            item,
        });
    }
    cleanup_egstore(install_dir, &entries)?;
    Ok(entries)
}

/// Drop `.item/.mancpn/.manifest` files that do not belong to the metadata just written.
fn cleanup_egstore(install_dir: &Path, entries: &[EgstoreEntry]) -> Result<()> {
    let egstore = install_dir.join(".egstore");
    let mut keep: HashSet<String> = HashSet::new();
    for entry in entries {
        keep.insert(format!("{}.item", entry.guid));
        keep.insert(format!("{}.mancpn", entry.guid));
        keep.insert(format!("{}.manifest", entry.guid));
    }
    let Ok(read) = std::fs::read_dir(&egstore) else { return Ok(()) };
    for entry in read.flatten() {
        let path = entry.path();
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !matches!(extension.as_str(), "item" | "mancpn" | "manifest") {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if !keep.contains(&name) {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Single-component metadata writer kept for the existing unit test and simple call sites.
#[allow(dead_code)]
fn write_egstore(
    install_dir: &Path,
    build: &BuildInfo,
    game: &GameEntry,
    manifest: &Manifest,
    raw: &[u8],
    guid: &str,
) -> Result<()> {
    let component = InstallComponent {
        game: game.clone(),
        build: build.clone(),
        manifest: manifest.clone(),
        raw: raw.to_vec(),
    };
    write_component_metadata(install_dir, &component, guid, game, false, guid)?;
    Ok(())
}

fn launcher_manifests_dir() -> PathBuf {
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    Path::new(&program_data)
        .join("Epic")
        .join("EpicGamesLauncher")
        .join("Data")
        .join("Manifests")
}

/// Copy every component's `.item` where the official launcher looks for installed apps.
fn register_with_launcher_components(
    components: &[InstallComponent],
    entries: &[EgstoreEntry],
) -> Result<()> {
    let manifests = launcher_manifests_dir();
    if !manifests.exists() {
        std::fs::create_dir_all(&manifests).map_err(|e| {
            Error::integrity(format!(
                "cannot create {} ({e}); run skiff as administrator or drop --register-launcher",
                manifests.display()
            ))
        })?;
    }

    // Remove stale registrations for these apps before writing the new ones, so a reinstall does not
    // leave two `.item` files describing the same game.
    let app_set: HashSet<String> =
        components.iter().map(|component| component.game.app_name.to_lowercase()).collect();
    if let Ok(read) = std::fs::read_dir(&manifests) {
        for entry in read.flatten() {
            let path = entry.path();
            if !path.extension().map(|ext| ext == "item").unwrap_or(false) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            let app = value
                .get("AppName")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_lowercase();
            if app_set.contains(&app) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    for entry in entries {
        let bytes = serde_json::to_vec_pretty(&entry.item)
            .map_err(|e| Error::integrity(format!("cannot serialize launcher manifest: {e}")))?;
        util::atomic_write(&manifests.join(format!("{}.item", entry.guid)), &bytes)?;
    }
    Ok(())
}

/// One installed component read back from `.egstore`.
#[derive(Debug, Clone)]
pub struct InstalledComponent {
    pub guid: String,
    pub app_name: String,
    pub main_app_name: String,
    pub item: serde_json::Value,
    pub manifest: Manifest,
    pub raw: Vec<u8>,
}

impl InstalledComponent {
    /// True when this entry is the main game rather than an add-on.
    pub fn is_main_game(&self) -> bool {
        self.app_name.eq_ignore_ascii_case(&self.main_app_name)
    }
}

/// Read every component of an install back from `.egstore`.
///
/// The base game is returned first. Add-ons point at it through `MainGameAppName`.
pub fn load_installed_components(install_dir: &Path) -> Result<Vec<InstalledComponent>> {
    let egstore = install_dir.join(".egstore");
    let entries = std::fs::read_dir(&egstore).map_err(|e| {
        Error::integrity(format!("no .egstore in {} ({e})", install_dir.display()))
    })?;

    let mut components: Vec<InstalledComponent> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().map(|ext| ext == "item").unwrap_or(false) {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let item: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        let app_name = item
            .get("AppName")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let main_app_name = item
            .get("MainGameAppName")
            .and_then(|v| v.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| app_name.clone());
        let guid = item
            .get("InstallationGuid")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("HostInstallationGuid").and_then(|v| v.as_str()))
            .or_else(|| path.file_stem().and_then(|stem| stem.to_str()))
            .unwrap_or_default()
            .to_string();
        let manifest_path = egstore.join(format!("{guid}.manifest"));
        let raw = match std::fs::read(&manifest_path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let manifest = match Manifest::parse(&raw) {
            Ok(manifest) => manifest,
            Err(e) => {
                eprintln!("warning: ignoring unreadable {}: {e}", manifest_path.display());
                continue;
            }
        };
        if app_name.is_empty() {
            continue;
        }
        components.push(InstalledComponent {
            guid,
            app_name,
            main_app_name,
            item,
            manifest,
            raw,
        });
    }

    // Older or hand-made installs may have a manifest but no `.item`; keep those readable.
    if components.is_empty() {
        if let Ok(read) = std::fs::read_dir(&egstore) {
            for entry in read.flatten() {
                let path = entry.path();
                if !path.extension().map(|ext| ext == "manifest").unwrap_or(false) {
                    continue;
                }
                let raw = match std::fs::read(&path) {
                    Ok(raw) => raw,
                    Err(_) => continue,
                };
                let Ok(manifest) = Manifest::parse(&raw) else { continue };
                let app_name = if manifest.meta.app_name.is_empty() {
                    path.file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                        .unwrap_or_default()
                } else {
                    manifest.meta.app_name.clone()
                };
                components.push(InstalledComponent {
                    guid: path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    app_name: app_name.clone(),
                    main_app_name: app_name,
                    item: serde_json::Value::Null,
                    manifest,
                    raw,
                });
            }
        }
    }

    if components.is_empty() {
        return Err(Error::integrity(format!(
            "no install metadata found in {}",
            egstore.display()
        )));
    }

    components.sort_by(|a, b| {
        b.is_main_game()
            .cmp(&a.is_main_game())
            .then_with(|| a.app_name.to_lowercase().cmp(&b.app_name.to_lowercase()))
    });
    Ok(components)
}

/// Load the main game's manifest of an existing install (used by `verify`/`update`).
pub fn load_installed_manifest(install_dir: &Path) -> Result<Manifest> {
    let mut components = load_installed_components(install_dir)?;
    if components.is_empty() {
        return Err(Error::integrity(format!(
            "no manifest backup found in {}",
            install_dir.display()
        )));
    }
    Ok(components.remove(0).manifest)
}

/// Read back the file list of one installed manifest for verification.
pub fn installed_files(manifest: &Manifest) -> Vec<(String, FileManifest)> {
    let mut out = Vec::new();
    for file in &manifest.files {
        match util::sanitize_rel_path(&file.path) {
            Ok(safe) => out.push((safe, file.clone())),
            Err(_) => continue,
        }
    }
    out
}

/// Union of every installed component's files (deduplicated by path).
pub fn load_installed_files(install_dir: &Path) -> Result<Vec<(String, FileManifest)>> {
    let components = load_installed_components(install_dir)?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for component in &components {
        for (path, file) in installed_files(&component.manifest) {
            if seen.insert(path.clone()) {
                out.push((path, file));
            }
        }
    }
    Ok(out)
}

/// Summary of an installed game read from `.egstore`.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledInfo {
    pub app_name: String,
    pub version: String,
    pub install_location: String,
    pub install_size: u64,
    pub files: usize,
    /// Number of apps in this install directory (main game + add-ons).
    pub components: usize,
}

/// Inspect an installed directory.
pub fn inspect_install(install_dir: &Path) -> Result<InstalledInfo> {
    let components = load_installed_components(install_dir)?;
    let main = components.first();
    let item = main.map(|component| component.item.clone()).unwrap_or(serde_json::Value::Null);
    let manifest = main.map(|component| &component.manifest);
    Ok(InstalledInfo {
        app_name: item
            .get("AppName")
            .and_then(|v| v.as_str())
            .or_else(|| manifest.map(|manifest| manifest.meta.app_name.as_str()))
            .unwrap_or_default()
            .to_string(),
        version: item
            .get("AppVersionString")
            .and_then(|v| v.as_str())
            .or_else(|| manifest.map(|manifest| manifest.meta.build_version.as_str()))
            .unwrap_or_default()
            .to_string(),
        install_location: install_dir.to_string_lossy().to_string(),
        install_size: components.iter().map(|component| component.manifest.install_size()).sum(),
        files: components.iter().map(|component| component.manifest.files.len()).sum(),
        components: components.len(),
    })
}

/// Uninstall: remove `.egstore` and (optionally) game files, then drop the launcher manifests.
///
/// Every component of a bundled install is removed: the main game and each add-on.
pub fn uninstall(install_dir: &Path, keep_files: bool, app_name: &str) -> Result<()> {
    let components = load_installed_components(install_dir).unwrap_or_default();
    if !keep_files {
        for component in &components {
            for file in &component.manifest.files {
                if let Ok(rel) = util::sanitize_rel_path(&file.path) {
                    let path = util::install_path(install_dir, &rel);
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    let egstore = install_dir.join(".egstore");
    if let Ok(entries) = std::fs::read_dir(&egstore) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_item = path.extension().map(|e| e == "item").unwrap_or(false);
            if keep_files && !is_item {
                continue;
            }
            let _ = std::fs::remove_file(&path);
        }
    }
    // A full uninstall leaves no metadata behind; `keep_files` keeps the manifests and the directory.
    if !keep_files {
        let _ = std::fs::remove_dir(&egstore);
    }

    let base_app = components
        .first()
        .map(|component| component.main_app_name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| app_name.to_string());
    if base_app.is_empty() {
        // Without an app name there is nothing we can safely identify in ProgramData.
        return Ok(());
    }
    let app_set: HashSet<String> = components
        .iter()
        .map(|component| component.app_name.to_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    let install_text = install_dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let launcher_manifests = launcher_manifests_dir();
    if let Ok(entries) = std::fs::read_dir(&launcher_manifests) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().map(|e| e == "item").unwrap_or(false) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    let item_app = value
                        .get("AppName")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_lowercase();
                    let item_main = value
                        .get("MainGameAppName")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_lowercase();
                    let item_location = value
                        .get("InstallLocation")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .trim_end_matches('\\')
                        .to_lowercase();
                    let same_install = !install_text.is_empty() && item_location == install_text;
                    if app_set.contains(&item_app)
                        || (same_install && item_main == base_app.to_lowercase())
                    {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Helper used by CLI: map a raw game entry plus config into an install directory.
pub fn default_install_dir(root: &Path, game: &GameEntry) -> PathBuf {
    let folder = if game.folder_name.is_empty() {
        game.app_name.clone()
    } else {
        game.folder_name.clone()
    };
    root.join(folder)
}

/// Find an existing install of `app_name` under `root`.
///
/// Catalog enrichment can fill in a `FolderName` that was previously empty, so the default install
/// directory may change between versions for the same game. Before creating a second copy, reuse the
/// directory that already carries the game's `.egstore` metadata.
pub fn find_existing_install(root: &Path, app_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join(".egstore").is_dir() {
            continue;
        }
        let Ok(components) = load_installed_components(&dir) else { continue };
        if components.iter().any(|component| {
            component.app_name.eq_ignore_ascii_case(app_name)
                || component.main_app_name.eq_ignore_ascii_case(app_name)
        }) {
            return Some(dir);
        }
    }
    None
}

/// Cache for merged game lists (kept simple: one file in the config dir).
///
/// The cache is versioned because the DLC relation (`is_dlc` / `parent_app_name`) is written during
/// the network merge; a cache from an older build has no relation fields, so showing it would put
/// every add-on back in the list as a separate game. Readers treat an old cache as empty and fetch
/// once instead of silently displaying stale data.
pub const GAME_CACHE_SCHEMA: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct GameCache {
    schema: u32,
    games: Vec<GameEntry>,
}

pub fn cache_games(games: &[GameEntry]) -> Result<()> {
    let path = crate::config::Config::dir().join("games.json");
    let cache = GameCache { schema: GAME_CACHE_SCHEMA, games: games.to_vec() };
    let json = serde_json::to_vec(&cache)
        .map_err(|e| Error::integrity(format!("cannot serialize game cache: {e}")))?;
    util::atomic_write(&path, &json)
}

/// Load the cached game list.
///
/// Returns an error (which every caller treats as "no cache") when the file predates the DLC
/// relation fields, so a refresh is forced exactly once after upgrading.
pub fn load_cached_games() -> Result<Vec<GameEntry>> {
    let path = crate::config::Config::dir().join("games.json");
    let raw = std::fs::read(&path)?;
    let cache: GameCache = serde_json::from_slice(&raw)
        .map_err(|_| Error::integrity("game cache predates DLC grouping; refresh required"))?;
    if cache.schema < GAME_CACHE_SCHEMA {
        return Err(Error::integrity(format!(
            "game cache schema {} is older than {GAME_CACHE_SCHEMA}; refresh required",
            cache.schema
        )));
    }
    Ok(cache.games)
}

/// Map from relative path to manifest metadata, used by the CLI for reporting.
pub fn file_map(manifest: &Manifest) -> BTreeMap<String, u64> {
    manifest
        .files
        .iter()
        .map(|f| (f.path.clone(), f.size))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ChunkPart, ChunkInfo};

    fn temp_dir(tag: &str) -> PathBuf {
        // `std::env::temp_dir()` is not reliably writable in every environment; keep test
        // artefacts inside the workspace instead.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-tmp");
        let dir = base.join(format!("skiff-test-{tag}-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_files() -> Vec<(String, FileManifest)> {
        vec![(
            "a.txt".to_string(),
            FileManifest {
                path: "a.txt".to_string(),
                symlink_target: String::new(),
                hash: [0u8; 20],
                flags: 0,
                install_tags: Vec::new(),
                parts: vec![ChunkPart {
                    guid: [1, 2, 3, 4],
                    chunk_offset: 0,
                    size: 5,
                    file_offset: 0,
                }],
                size: 5,
                md5: None,
                mime_type: String::new(),
                sha256: None,
            },
        )]
    }

    #[test]
    fn write_at_scatters_bytes() {
        let dir = temp_dir("writeat");
        let path = dir.join("f.bin");
        let file = OpenOptions::new().write(true).create(true).truncate(false).open(&path).unwrap();
        file.set_len(12).unwrap();
        write_at(&file, b"hello", 2).unwrap();
        write_at(&file, b"world", 7).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 12);
        assert_eq!(&data[2..7], b"hello");
        assert_eq!(&data[7..12], b"world");
        assert_eq!(&data[..2], &[0u8, 0u8]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verification_detects_missing_and_bad_files() {
        let dir = temp_dir("verify");
        let files = sample_files();
        let report = verify_files(&dir, &files, 2).unwrap();
        assert_eq!(report.missing.len(), 1);
        assert!(report.bad.is_empty());

        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let mut fixed = files.clone();
        {
            use sha1::{Digest, Sha1};
            let mut hasher = Sha1::new();
            hasher.update(b"hello");
            fixed[0].1.hash = hasher.finalize().into();
        }
        let report = verify_files(&dir, &fixed, 2).unwrap();
        assert!(report.is_clean());

        std::fs::write(dir.join("a.txt"), b"hellx").unwrap();
        let report = verify_files(&dir, &fixed, 2).unwrap();
        assert_eq!(report.bad.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_ledger_requires_matching_manifest() {
        let dir = temp_dir("ledger");
        let ledger = ResumeLedger {
            schema: 1,
            app_name: "Game".into(),
            build_version: "++Game+1-Windows".into(),
            manifest_sha1: "aaa".into(),
            done_chunks: vec![1, 2, 3],
            bytes_done: 42,
            updated_at: 0,
        };
        ledger.save(&dir).unwrap();
        let loaded = ResumeLedger::load(&dir, "Game", "aaa");
        assert_eq!(loaded.done_chunks, vec![1, 2, 3]);
        let other = ResumeLedger::load(&dir, "Game", "bbb");
        assert!(other.done_chunks.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn egstore_writes_item_mancpn_and_manifest() {
        let dir = temp_dir("egstore");
        let build = BuildInfo {
            app_name: "MyGame".into(),
            namespace: "ns".into(),
            catalog_item_id: "cid".into(),
            label_name: "Live-Windows".into(),
            build_version: "++MyGame+1-Windows".into(),
            manifest_uri: "https://cdn/x.manifest".into(),
            manifest_hash: "h".into(),
            base_urls: vec!["https://cdn".into()],
        };
        let game = GameEntry {
            app_name: "MyGame".into(),
            namespace: "ns".into(),
            catalog_item_id: "cid".into(),
            title: "My Game".into(),
            developer: "Dev".into(),
            folder_name: "MyGame".into(),
            entitlement_type: "EXECUTABLE".into(),
            ..Default::default()
        };
        let mut manifest = Manifest {
            version: 18,
            compressed: true,
            encrypted: false,
            header_sha1: [0; 20],
            meta: crate::manifest::ManifestMeta {
                app_name: "MyGame".into(),
                launch_exe: "Game.exe".into(),
                launch_command: String::new(),
                ..Default::default()
            },
            chunks: Vec::<ChunkInfo>::new(),
            files: Vec::new(),
            custom: BTreeMap::new(),
            raw: Vec::new(),
        };
        manifest.files.push(sample_files()[0].1.clone());

        write_egstore(&dir, &build, &game, &manifest, b"raw-manifest", "ABCDEF0123456789").unwrap();
        let egstore = dir.join(".egstore");
        assert!(egstore.join("ABCDEF0123456789.item").exists());
        assert!(egstore.join("ABCDEF0123456789.mancpn").exists());
        assert!(egstore.join("ABCDEF0123456789.manifest").exists());

        let item: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(egstore.join("ABCDEF0123456789.item")).unwrap())
                .unwrap();
        assert_eq!(item["AppName"], "MyGame");
        assert_eq!(item["LaunchExecutable"], "Game.exe");
        assert_eq!(item["bIsIncompleteInstall"], false);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_guid_is_32_hex_chars() {
        let guid = generate_guid();
        assert_eq!(guid.len(), 32);
        assert!(guid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    fn file(path: &str, hash: u8, size: u64) -> FileManifest {
        FileManifest {
            path: path.to_string(),
            symlink_target: String::new(),
            hash: [hash; 20],
            flags: 0,
            install_tags: Vec::new(),
            parts: Vec::new(),
            size,
            md5: None,
            mime_type: String::new(),
            sha256: None,
        }
    }

    fn chunk(guid: u32, size: i64, sha: u8) -> ChunkInfo {
        ChunkInfo {
            guid: [guid, 0, 0, 0],
            rolling_hash: guid as u64,
            sha1: [sha; 20],
            group_num: 1,
            window_size: 0,
            file_size: size,
            secret_guid: None,
            encryption_tag: None,
        }
    }

    fn component(
        app: &str,
        version: u32,
        files: Vec<FileManifest>,
        chunks: Vec<ChunkInfo>,
    ) -> InstallComponent {
        InstallComponent {
            game: GameEntry {
                app_name: app.to_string(),
                namespace: "ns".to_string(),
                catalog_item_id: format!("cid-{app}"),
                title: app.to_string(),
                is_dlc: app != "Base",
                parent_app_name: if app == "Base" { String::new() } else { "Base".to_string() },
                ..Default::default()
            },
            build: BuildInfo {
                app_name: app.to_string(),
                namespace: "ns".to_string(),
                catalog_item_id: format!("cid-{app}"),
                label_name: "Live".to_string(),
                build_version: format!("++{app}+1"),
                manifest_uri: "https://cdn/x.manifest".to_string(),
                manifest_hash: "h".to_string(),
                base_urls: vec![format!("https://cdn/{app}")],
            },
            manifest: Manifest {
                version,
                compressed: true,
                encrypted: false,
                header_sha1: [0; 20],
                meta: crate::manifest::ManifestMeta {
                    app_name: app.to_string(),
                    ..Default::default()
                },
                chunks,
                files,
                custom: BTreeMap::new(),
                raw: Vec::new(),
            },
            raw: app.as_bytes().to_vec(),
        }
    }

    #[test]
    fn merge_dedupes_identical_files_and_keeps_the_first_on_conflict() {
        let base = component(
            "Base",
            21,
            vec![file("a.txt", 1, 5), file("same.txt", 7, 5)],
            vec![chunk(1, 100, 1)],
        );
        let dlc = component(
            "Dlc",
            21,
            vec![file("a.txt", 2, 5), file("same.txt", 7, 5), file("dlc/b.txt", 3, 5)],
            vec![chunk(1, 100, 1), chunk(2, 100, 2)],
        );
        let plan = merge_components(&[base, dlc]).unwrap();
        assert_eq!(plan.conflicts, 1);
        assert_eq!(plan.manifest.files.len(), 3);
        let a = plan.manifest.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(a.hash, [1u8; 20], "the main game's file wins");
        assert_eq!(plan.manifest.chunks.len(), 2, "shared chunk is deduplicated");
        assert_eq!(plan.chunk_versions, vec![21, 21]);
        assert!(plan.bases.contains(&"https://cdn/Base".to_string()));
        assert!(plan.bases.contains(&"https://cdn/Dlc".to_string()));
    }

    #[test]
    fn merge_keeps_per_chunk_manifest_versions() {
        let base = component("Base", 21, Vec::new(), vec![chunk(1, 100, 1)]);
        let dlc = component("Dlc", 17, Vec::new(), vec![chunk(2, 100, 2)]);
        let plan = merge_components(&[base, dlc]).unwrap();
        assert_eq!(plan.chunk_versions, vec![21, 17]);
    }

    #[test]
    fn merge_rejects_a_chunk_guid_with_different_content() {
        let base = component("Base", 21, Vec::new(), vec![chunk(1, 100, 1)]);
        let dlc = component("Dlc", 21, Vec::new(), vec![chunk(1, 200, 2)]);
        assert!(merge_components(&[base, dlc]).is_err());
    }

    #[test]
    fn finds_an_existing_install_by_app_name() {
        let root = temp_dir("existing-root");
        let dir = root.join("GameDir");
        let mut base = component("Base", 21, Vec::new(), Vec::new());
        // `find_existing_install` reads the manifest back, so the fixture must be a real one.
        base.raw = br#"{"ManifestFileVersion":"18","AppNameString":"Base"}"#.to_vec();
        write_egstore_components(&dir, &[base]).unwrap();

        assert_eq!(find_existing_install(&root, "Base"), Some(dir.clone()));
        assert!(find_existing_install(&root, "Other").is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
