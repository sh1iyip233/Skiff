//! Bounded-concurrency chunk download engine.
//!
//! Concurrency is a user-owned contract: exactly `concurrency.downloads` worker threads run, so at
//! saturation the number of in-flight requests equals that number and never exceeds it. The engine
//! never raises or lowers it on its own.
//!
//! Each worker keeps one WinHTTP session (see [`crate::http`]) alive for its lifetime, so the TCP
//! and TLS handshakes are paid once per worker instead of once per 1 MiB chunk.

use crate::cdn::CdnHosts;
use crate::chunk;
use crate::config::Concurrency;
use crate::error::{Error, Result};
use crate::http::{Client, Request};
use crate::manifest::ChunkInfo;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One chunk to fetch.
#[derive(Debug, Clone)]
pub struct ChunkJob {
    /// Index into the manifest chunk list (stable across runs for one manifest).
    pub index: usize,
    pub chunk: ChunkInfo,
    /// Relative CDN path (`ChunksV4/..`).
    pub path: String,
}

/// Destination for decoded chunk plaintext.
pub trait ChunkSink: Send + Sync {
    /// Called exactly once per successfully verified chunk.
    fn write_chunk(&self, index: usize, plaintext: &[u8]) -> Result<()>;
}

/// Snapshot of download progress.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub chunks_total: usize,
    pub chunks_done: usize,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub inflight: usize,
    pub peak_inflight: usize,
    pub elapsed_secs: f64,
    pub speed_bps: f64,
    pub eta_secs: Option<f64>,
    /// Total chunk attempts that had to be retried.
    pub retries: usize,
}

/// Shared counters updated by every worker.
pub struct ProgressTracker {
    chunks_total: usize,
    chunks_done: AtomicUsize,
    bytes_total: u64,
    bytes_done: AtomicU64,
    inflight: AtomicUsize,
    peak_inflight: AtomicUsize,
    retries: AtomicUsize,
    started: Instant,
}

impl ProgressTracker {
    /// Create a tracker for `chunks_total` chunks and `bytes_total` bytes of compressed payload.
    pub fn new(chunks_total: usize, bytes_total: u64) -> Arc<ProgressTracker> {
        Arc::new(ProgressTracker {
            chunks_total,
            chunks_done: AtomicUsize::new(0),
            bytes_total,
            bytes_done: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
            peak_inflight: AtomicUsize::new(0),
            retries: AtomicUsize::new(0),
            started: Instant::now(),
        })
    }

    fn begin_request(&self) {
        let now = self.inflight.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak_inflight.fetch_max(now, Ordering::AcqRel);
    }

    fn end_request(&self) {
        self.inflight.fetch_sub(1, Ordering::AcqRel);
    }

    fn note_retry(&self) {
        self.retries.fetch_add(1, Ordering::AcqRel);
    }

    fn finish_chunk(&self, bytes: u64) {
        self.chunks_done.fetch_add(1, Ordering::AcqRel);
        self.bytes_done.fetch_add(bytes, Ordering::AcqRel);
    }

    /// Current progress snapshot.
    pub fn snapshot(&self) -> Progress {
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let bytes_done = self.bytes_done.load(Ordering::Acquire);
        let chunks_done = self.chunks_done.load(Ordering::Acquire);
        let speed = bytes_done as f64 / elapsed;
        let remaining = self.bytes_total.saturating_sub(bytes_done);
        Progress {
            chunks_total: self.chunks_total,
            chunks_done,
            bytes_total: self.bytes_total,
            bytes_done,
            inflight: self.inflight.load(Ordering::Acquire),
            peak_inflight: self.peak_inflight.load(Ordering::Acquire),
            elapsed_secs: elapsed,
            speed_bps: speed,
            eta_secs: if speed > 1.0 { Some(remaining as f64 / speed) } else { None },
            retries: self.retries.load(Ordering::Acquire),
        }
    }
}

/// Tuning for one download run.
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// User-specified concurrency; authoritative.
    pub concurrency: Concurrency,
    /// Optional bandwidth cap in bytes/second.
    pub limit_bytes_per_sec: Option<u64>,
    /// Attempts per chunk before it is reported as failed.
    pub attempts: u32,
}

impl DownloadOptions {
    /// Defaults derived from a [`Concurrency`] value.
    pub fn new(concurrency: Concurrency) -> DownloadOptions {
        DownloadOptions { concurrency, limit_bytes_per_sec: None, attempts: 5 }
    }
}

/// Aggregate result of a download run.
#[derive(Debug, Clone, Default)]
pub struct DownloadStats {
    pub chunks_done: usize,
    pub bytes_done: u64,
    pub peak_inflight: usize,
    pub elapsed_secs: f64,
    pub failures: Vec<(usize, String)>,
}

/// Token bucket that paces total bytes per second.
struct RateLimiter {
    limit: Option<u64>,
    state: Mutex<(Instant, u64)>,
}

/// Caps concurrent requests per CDN host so a single property cannot be rate limited.
struct PerHostLimiter {
    cap: Option<usize>,
    state: Mutex<Vec<usize>>,
    signal: std::sync::Condvar,
}

impl PerHostLimiter {
    fn new(cap: Option<usize>, hosts: usize) -> PerHostLimiter {
        PerHostLimiter {
            cap: cap.filter(|c| *c > 0),
            state: Mutex::new(vec![0; hosts.max(1)]),
            signal: std::sync::Condvar::new(),
        }
    }

    /// Block until the host has capacity, returning a guard that releases it on drop.
    fn acquire(self: &Arc<Self>, host: usize) -> PerHostGuard {
        if let Some(cap) = self.cap {
            let mut state = self.state.lock().unwrap();
            let index = host % state.len().max(1);
            while state[index] >= cap {
                state = self.signal.wait(state).unwrap();
            }
            state[index] += 1;
            return PerHostGuard { limiter: self.clone(), index };
        }
        let index = host % self.state.lock().unwrap().len().max(1);
        PerHostGuard { limiter: self.clone(), index }
    }
}

struct PerHostGuard {
    limiter: Arc<PerHostLimiter>,
    index: usize,
}

impl Drop for PerHostGuard {
    fn drop(&mut self) {
        if self.limiter.cap.is_some() {
            let mut state = self.limiter.state.lock().unwrap();
            if self.index < state.len() && state[self.index] > 0 {
                state[self.index] -= 1;
            }
            drop(state);
            self.limiter.signal.notify_one();
        }
    }
}

impl RateLimiter {
    fn new(limit: Option<u64>) -> RateLimiter {
        RateLimiter { limit, state: Mutex::new((Instant::now(), 0)) }
    }

    fn acquire(&self, bytes: u64) {
        let Some(limit) = self.limit else { return };
        if limit == 0 {
            return;
        }
        // Sleep outside the lock so workers do not serialize on each other.
        let gap = {
            let mut state = self.state.lock().unwrap();
            state.1 = state.1.saturating_add(bytes);
            let target = Duration::from_secs_f64(state.1 as f64 / limit as f64);
            let elapsed = state.0.elapsed();
            if target > elapsed {
                Some(target - elapsed)
            } else {
                None
            }
        };
        if let Some(gap) = gap {
            std::thread::sleep(gap.min(Duration::from_secs(2)));
        }
    }
}

/// Download every job, writing decoded chunks through `sink`.
///
/// Returns once all workers finish; per-chunk failures are collected in
/// [`DownloadStats::failures`] instead of aborting the whole run.
#[allow(clippy::too_many_arguments)]
pub fn download(
    client: &Client,
    hosts: &CdnHosts,
    jobs: &[ChunkJob],
    opts: &DownloadOptions,
    sink: &dyn ChunkSink,
    tracker: Arc<ProgressTracker>,
    cancel: Arc<AtomicBool>,
    on_progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
) -> Result<DownloadStats> {
    if hosts.is_empty() {
        return Err(Error::net("no CDN base URL available for this build"));
    }
    let workers = opts.concurrency.downloads.max(1);
    let started = Instant::now();
    let cursor = AtomicUsize::new(0);
    let failures: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());
    let limiter = RateLimiter::new(opts.limit_bytes_per_sec);
    let per_host = Arc::new(PerHostLimiter::new(opts.concurrency.per_host, hosts.len()));
    let stop_monitor = Arc::new(AtomicBool::new(false));

    // Progress monitor: samples counters on a fixed cadence so workers never block on printing.
    let monitor = on_progress.map(|callback| {
        let tracker = tracker.clone();
        let stop = stop_monitor.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                callback(tracker.snapshot());
                std::thread::sleep(Duration::from_millis(250));
            }
            callback(tracker.snapshot());
        })
    });

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let cursor = &cursor;
            let failures = &failures;
            let limiter = &limiter;
            let per_host = &per_host;
            let tracker = &tracker;
            let cancel = &cancel;
            scope.spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = cursor.fetch_add(1, Ordering::AcqRel);
                    if index >= jobs.len() {
                        break;
                    }
                    let job = &jobs[index];
                    match fetch_one(
                        client, hosts, job, opts, sink, tracker, limiter, per_host, cancel,
                    ) {
                        Ok(_) => {}
                        Err(e) => failures.lock().unwrap().push((job.index, e.to_string())),
                    }
                }
            });
        }
    });

    stop_monitor.store(true, Ordering::Relaxed);
    if let Some(handle) = monitor {
        let _ = handle.join();
    }

    let snapshot = tracker.snapshot();
    let failures = failures.into_inner().unwrap_or_default();
    Ok(DownloadStats {
        chunks_done: snapshot.chunks_done,
        bytes_done: snapshot.bytes_done,
        peak_inflight: snapshot.peak_inflight,
        elapsed_secs: started.elapsed().as_secs_f64(),
        failures,
    })
}

#[allow(clippy::too_many_arguments)]
fn fetch_one(
    client: &Client,
    hosts: &CdnHosts,
    job: &ChunkJob,
    opts: &DownloadOptions,
    sink: &dyn ChunkSink,
    tracker: &ProgressTracker,
    limiter: &RateLimiter,
    per_host: &Arc<PerHostLimiter>,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let attempts = opts.attempts.max(1);
    let mut last_error: Option<Error> = None;

    for attempt in 0..attempts {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::net("download cancelled"));
        }
        let host_index = job.index.wrapping_add(attempt as usize);
        let host = match hosts.at(host_index) {
            Some(host) => host,
            None => return Err(Error::net("no CDN host available")),
        };
        let url = crate::cdn::chunk_url(host, &job.path);

        limiter.acquire(job.chunk.file_size.max(0) as u64);
        let mut body = Vec::with_capacity(job.chunk.file_size.max(0) as usize + 1024);
        let request = Request::get(&url).timeout(120).cancellable(cancel.clone());

        tracker.begin_request();
        // Held for the duration of the request so `--per-host` caps a single CDN property.
        let _host_permit = per_host.acquire(host_index);
        let response = client.request(&request, &mut body);
        tracker.end_request();

        match response {
            Ok(head) if head.is_success() => {
                let payload = decode_and_verify(job, &body)?;
                sink.write_chunk(job.index, &payload)?;
                tracker.finish_chunk(job.chunk.file_size.max(0) as u64);
                return Ok(());
            }
            Ok(head) => {
                let error = Error::Api {
                    status: head.status,
                    code: String::new(),
                    message: format!("chunk HTTP {}", head.status),
                };
                // 403/404 will not improve with time, but another CDN host may still serve it.
                let retryable = crate::net::is_retryable_status(head.status) || head.status == 404;
                if !retryable {
                    return Err(error);
                }
                last_error = Some(error);
            }
            Err(e) => last_error = Some(e),
        }

        if attempt + 1 < attempts {
            // A cancelled request is not a retry: don't count it or log one.
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::net("download cancelled"));
            }
            tracker.note_retry();
            eprintln!(
                "  retry {}/{} for chunk #{} via {}",
                attempt + 1,
                attempts,
                job.index,
                host_of(host)
            );
            // Wait in 100 ms slices so a cancel is honoured instead of sleeping out the backoff.
            if !crate::util::sleep_cancellable(cancel, crate::util::backoff_delay(attempt, 30_000)) {
                return Err(Error::net("download cancelled"));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| Error::net("chunk download failed")))
}

fn decode_and_verify(job: &ChunkJob, container: &[u8]) -> Result<Vec<u8>> {
    let header = chunk::parse_header(container)?;
    let secret: Option<[u8; 32]> = None;
    let plaintext = chunk::decode_payload(&header, container, secret.as_ref())?;
    chunk::verify_chunk(
        &plaintext,
        &job.chunk.sha1,
        Some(job.chunk.rolling_hash),
    )
    .map_err(|e| match e {
        Error::ChunkCorrupt { expected, actual, .. } => Error::ChunkCorrupt {
            chunk: job.chunk.guid_hex(),
            expected,
            actual,
        },
        other => other,
    })?;
    Ok(plaintext)
}

/// Host portion of a base URL, for log lines.
fn host_of(base: &str) -> &str {
    base.split("://").nth(1).and_then(|rest| rest.split('/').next()).unwrap_or(base)
}
/// Build jobs for chunks that still need to be fetched, computing CDN paths.
///
/// `versions` optionally overrides the manifest feature level per chunk (index-aligned). A merged
/// main-game + DLC plan can contain chunks from builds with different feature levels, and the
/// `ChunksV3/V4/V5` directory is part of the object path.
pub fn plan_jobs_with_versions(
    manifest: &crate::manifest::Manifest,
    done: &HashMap<usize, ()>,
    versions: &[u32],
) -> Vec<ChunkJob> {
    let mut jobs = Vec::new();
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        if done.contains_key(&index) {
            continue;
        }
        let version = versions.get(index).copied().unwrap_or(manifest.version);
        let path = crate::cdn::chunk_path(
            version,
            chunk.group_num,
            chunk.rolling_hash,
            &chunk.guid,
            chunk.secret_guid.as_ref(),
        );
        jobs.push(ChunkJob { index, chunk: chunk.clone(), path });
    }
    jobs
}

/// Build jobs for one manifest, using its own feature level for every chunk.
pub fn plan_jobs(
    manifest: &crate::manifest::Manifest,
    done: &HashMap<usize, ()>,
) -> Vec<ChunkJob> {
    plan_jobs_with_versions(manifest, done, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct CountingSink {
        count: AtomicUsize,
        bytes: AtomicUsize,
    }

    impl ChunkSink for CountingSink {
        fn write_chunk(&self, _index: usize, plaintext: &[u8]) -> Result<()> {
            self.count.fetch_add(1, Ordering::AcqRel);
            self.bytes.fetch_add(plaintext.len(), Ordering::AcqRel);
            Ok(())
        }
    }

    #[test]
    fn tracker_reports_peak_and_eta() {
        let tracker = ProgressTracker::new(4, 400);
        tracker.begin_request();
        tracker.begin_request();
        tracker.end_request();
        tracker.finish_chunk(100);
        let snap = tracker.snapshot();
        assert_eq!(snap.chunks_total, 4);
        assert_eq!(snap.chunks_done, 1);
        assert_eq!(snap.bytes_done, 100);
        assert_eq!(snap.peak_inflight, 2);
        assert!(snap.speed_bps > 0.0);
    }

    #[test]
    fn empty_host_list_is_an_error() {
        let client = Client::plain();
        let hosts = CdnHosts::new(vec![]);
        let tracker = ProgressTracker::new(0, 0);
        let err = download(
            &client,
            &hosts,
            &[],
            &DownloadOptions::new(Concurrency::default()),
            &CountingSink { count: AtomicUsize::new(0), bytes: AtomicUsize::new(0) },
            tracker,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("CDN"));
    }

    #[test]
    fn plans_only_missing_chunks() {
        use crate::manifest::{ChunkInfo, FileManifest, Manifest, ManifestMeta};
        use std::collections::BTreeMap;

        let manifest = Manifest {
            version: 18,
            compressed: true,
            encrypted: false,
            header_sha1: [0; 20],
            meta: ManifestMeta::default(),
            chunks: vec![
                ChunkInfo {
                    guid: [1, 2, 3, 4],
                    rolling_hash: 0xAA,
                    sha1: [1; 20],
                    group_num: 7,
                    window_size: 1024,
                    file_size: 100,
                    secret_guid: None,
                    encryption_tag: None,
                },
                ChunkInfo {
                    guid: [5, 6, 7, 8],
                    rolling_hash: 0xBB,
                    sha1: [2; 20],
                    group_num: 8,
                    window_size: 1024,
                    file_size: 200,
                    secret_guid: None,
                    encryption_tag: None,
                },
            ],
            files: Vec::<FileManifest>::new(),
            custom: BTreeMap::new(),
            raw: Vec::new(),
        };
        let mut done = HashMap::new();
        done.insert(0usize, ());
        let jobs = plan_jobs(&manifest, &done);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].index, 1);
        assert!(jobs[0].path.starts_with("ChunksV4/08/"));
    }

    #[test]
    fn plans_per_chunk_manifest_versions() {
        use crate::manifest::{ChunkInfo, FileManifest, Manifest, ManifestMeta};
        use std::collections::BTreeMap;

        let chunk = |guid: u32, group: u8| ChunkInfo {
            guid: [guid, 0, 0, 0],
            rolling_hash: guid as u64,
            sha1: [guid as u8; 20],
            group_num: group,
            window_size: 1024,
            file_size: 100,
            secret_guid: None,
            encryption_tag: None,
        };
        let manifest = Manifest {
            version: 21,
            compressed: true,
            encrypted: false,
            header_sha1: [0; 20],
            meta: ManifestMeta::default(),
            chunks: vec![chunk(1, 7), chunk(2, 8)],
            files: Vec::<FileManifest>::new(),
            custom: BTreeMap::new(),
            raw: Vec::new(),
        };
        let jobs = plan_jobs_with_versions(&manifest, &HashMap::new(), &[18, 22]);
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].path.starts_with("ChunksV4/07/"), "{}", jobs[0].path);
        assert!(jobs[1].path.starts_with("ChunksV5/plain/08/"), "{}", jobs[1].path);
    }

    #[test]
    fn rate_limiter_without_limit_is_instant() {
        let limiter = RateLimiter::new(None);
        let started = Instant::now();
        limiter.acquire(10 * 1024 * 1024);
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
