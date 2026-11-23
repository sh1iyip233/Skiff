//! End-to-end install test against a local mock CDN.
//!
//! Verifies the parts unit tests cannot reach: chunk URL construction, the concurrent downloader,
//! file scatter by offset, resume from a ledger, repair of corrupt files and `.egstore` output.

use skiff_core::api::{BuildInfo, GameEntry};
use skiff_core::cdn::{chunk_path, CdnHosts};
use skiff_core::chunk;
use skiff_core::config::Concurrency;
use skiff_core::download::{
    self, ChunkJob, ChunkSink, DownloadOptions, Phase, Progress, ProgressTracker,
};
use skiff_core::http::Client;
use skiff_core::install::{self, InstallComponent, InstallOptions};
use skiff_core::launch;
use skiff_core::manifest::{ChunkInfo, ChunkPart, FileManifest, Manifest, ManifestMeta};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

const CHUNK_COUNT: usize = 6;
const CHUNK_SIZE: usize = 64 * 1024;

fn payload(index: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(CHUNK_SIZE);
    for i in 0..CHUNK_SIZE {
        data.push(((i + index * 31) % 251) as u8);
    }
    data
}

fn sha1(data: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Build a chunk container (header version 3, zlib compressed) around `data`.
fn container(data: &[u8], guid: [u32; 4]) -> Vec<u8> {
    let encoder = flate2_write(data);
    let compressed_len = encoder.len();
    let mut out = Vec::new();
    out.extend_from_slice(&chunk::CHUNK_MAGIC.to_le_bytes());
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&66u32.to_le_bytes());
    out.extend_from_slice(&(compressed_len as u32).to_le_bytes());
    for part in guid {
        out.extend_from_slice(&part.to_le_bytes());
    }
    out.extend_from_slice(&chunk::rolling_hash(data).to_le_bytes());
    out.push(0x1); // compressed
    out.extend_from_slice(&sha1(data));
    out.push(0x3); // rolling + sha
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&encoder);
    out
}

fn flate2_write(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// (manifest, raw manifest bytes, path -> chunk container, chunk payloads)
type Fixtures = (Manifest, Vec<u8>, HashMap<String, Vec<u8>>, Vec<Vec<u8>>);

/// Two files: `a.bin` uses chunks 0..3, `b.bin` uses chunks 3..6 (chunk 3 is shared).
fn fixtures() -> Fixtures {
    let mut chunks = Vec::new();
    let mut files_map: HashMap<String, Vec<u8>> = HashMap::new();
    let mut payloads = Vec::new();
    let mut containers = HashMap::new();

    for index in 0..CHUNK_COUNT {
        let data = payload(index);
        let guid = [index as u32 + 1, 0x1000, 0x2000, 0x3000];
        let bytes = container(&data, guid);
        let path = chunk_path(18, index as u8, chunk::rolling_hash(&data), &guid, None);
        containers.insert(format!("/{path}"), bytes.clone());
        chunks.push(ChunkInfo {
            guid,
            rolling_hash: chunk::rolling_hash(&data),
            sha1: sha1(&data),
            group_num: index as u8,
            window_size: data.len() as u32,
            file_size: bytes.len() as i64,
            secret_guid: None,
            encryption_tag: None,
        });
        payloads.push(data);
    }

    let mut file_a = Vec::new();
    for data in payloads.iter().take(3) {
        file_a.extend_from_slice(data);
    }
    let mut file_b = Vec::new();
    for data in payloads.iter().skip(3).take(3) {
        file_b.extend_from_slice(data);
    }
    files_map.insert("a.bin".to_string(), file_a.clone());
    files_map.insert("b.bin".to_string(), file_b.clone());

    let files = vec![
        FileManifest {
            path: "a.bin".to_string(),
            symlink_target: String::new(),
            hash: sha1(&file_a),
            flags: 0,
            install_tags: Vec::new(),
            parts: (0..3)
                .map(|i| ChunkPart {
                    guid: chunks[i].guid,
                    chunk_offset: 0,
                    size: CHUNK_SIZE as u32,
                    file_offset: (i * CHUNK_SIZE) as u64,
                })
                .collect(),
            size: file_a.len() as u64,
            md5: None,
            mime_type: String::new(),
            sha256: None,
        },
        FileManifest {
            path: "sub/b.bin".to_string(),
            symlink_target: String::new(),
            hash: sha1(&file_b),
            flags: 1,
            install_tags: Vec::new(),
            parts: (3..6)
                .map(|i| ChunkPart {
                    guid: chunks[i].guid,
                    chunk_offset: 0,
                    size: CHUNK_SIZE as u32,
                    file_offset: ((i - 3) * CHUNK_SIZE) as u64,
                })
                .collect(),
            size: file_b.len() as u64,
            md5: None,
            mime_type: String::new(),
            sha256: None,
        },
    ];

    let manifest = Manifest {
        version: 18,
        compressed: true,
        encrypted: false,
        header_sha1: [0; 20],
        meta: ManifestMeta {
            data_version: 0,
            feature_level: 18,
            app_name: "E2EGame".to_string(),
            build_version: "++E2EGame+Release-1.0-Windows".to_string(),
            launch_exe: "a.bin".to_string(),
            launch_command: "-epicapp=E2EGame".to_string(),
            ..Default::default()
        },
        chunks,
        files,
        custom: BTreeMap::new(),
        raw: Vec::new(),
    };
    let raw = manifest.write_binary().unwrap();
    (manifest, raw, containers, payloads)
}

fn game() -> GameEntry {
    GameEntry {
        app_name: "E2EGame".to_string(),
        namespace: "e2e".to_string(),
        catalog_item_id: "cid".to_string(),
        title: "E2E Game".to_string(),
        developer: "Tests".to_string(),
        folder_name: "E2EGame".to_string(),
        entitlement_type: "EXECUTABLE".to_string(),
        ..Default::default()
    }
}

fn build_info(base: &str) -> BuildInfo {
    BuildInfo {
        app_name: "E2EGame".to_string(),
        namespace: "e2e".to_string(),
        catalog_item_id: "cid".to_string(),
        label_name: "Live-Windows".to_string(),
        build_version: "++E2EGame+Release-1.0-Windows".to_string(),
        manifest_uri: format!("{base}/manifest.bin"),
        manifest_hash: "unused".to_string(),
        base_urls: vec![base.to_string()],
    }
}

fn options(dir: &Path, downloads: usize) -> InstallOptions {
    InstallOptions {
        install_dir: dir.to_path_buf(),
        concurrency: Concurrency { downloads, writers: 2, ..Concurrency::default() },
        limit_bytes_per_sec: None,
        verify: true,
        repair: true,
        register_launcher: false,
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-tmp");
    let dir = base.join(format!("e2e-{tag}-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------------------------
// mock CDN
// ---------------------------------------------------------------------------------------------

struct MockCdn {
    base: String,
    stop: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockCdn {
    /// Serve `files` (path -> bytes). Requests are answered after a short delay so concurrent
    /// workers genuinely overlap, which is what makes the in-flight measurement meaningful.
    fn start(files: HashMap<String, Vec<u8>>) -> MockCdn {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock cdn");
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let files = Arc::new(files);

        let thread_stop = stop.clone();
        let thread_requests = requests.clone();
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let files = files.clone();
                        let requests = thread_requests.clone();
                        std::thread::spawn(move || {
                            let _ = serve(stream, &files, &requests);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        MockCdn {
            base: format!("http://{addr}"),
            stop,
            requests,
            handle: Some(handle),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

impl Drop for MockCdn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    files: &HashMap<String, Vec<u8>>,
    requests: &AtomicUsize,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    requests.fetch_add(1, Ordering::Relaxed);

    // Keep the request in flight long enough for all workers to overlap.
    std::thread::sleep(Duration::from_millis(15));

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    match files.get(&path) {
        Some(body) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(body)?;
        }
        None => {
            let head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(head.as_bytes())?;
        }
    }
    stream.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

#[test]
fn installs_files_and_writes_egstore() {
    let (_, raw, containers, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();
    let cdn = MockCdn::start(containers);
    let dir = temp_dir("install");
    let opts = options(&dir, CHUNK_COUNT);

    let outcome = install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        None,
    )
    .expect("install should succeed");

    assert_eq!(outcome.files, 2);
    assert_eq!(outcome.chunks_downloaded, CHUNK_COUNT);
    assert_eq!(
        outcome.peak_inflight, CHUNK_COUNT,
        "with {CHUNK_COUNT} workers every chunk should be in flight at once"
    );

    let (expected_manifest, _, _, _) = fixtures();
    let a = std::fs::read(dir.join("a.bin")).unwrap();
    let b = std::fs::read(dir.join("sub").join("b.bin")).unwrap();
    assert_eq!(a.len() as u64, expected_manifest.files[0].size);
    assert_eq!(b.len() as u64, expected_manifest.files[1].size);
    assert_eq!(sha1(&a).to_vec(), expected_manifest.files[0].hash.to_vec());
    assert_eq!(sha1(&b).to_vec(), expected_manifest.files[1].hash.to_vec());

    let egstore = dir.join(".egstore");
    let entries: Vec<String> = std::fs::read_dir(&egstore)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(entries.iter().any(|n| n.ends_with(".item")), "entries: {entries:?}");
    assert!(entries.iter().any(|n| n.ends_with(".mancpn")));
    assert!(entries.iter().any(|n| n.ends_with(".manifest")));
    assert!(
        !entries.iter().any(|n| n.contains("resume")),
        "the resume ledger must be removed after a clean install"
    );

    // The report is clean when re-verified independently.
    let files = install::installed_files(&parsed);
    let report = install::verify_files(&dir, &files, 2).unwrap();
    assert!(report.is_clean(), "report: {report:?}");

    std::fs::remove_dir_all(&dir).ok();
}

/// The `.item` written by an install is enough to build a launch command: same executable, app name
/// and namespace the official launcher would pass to the game.
#[test]
fn launch_target_reads_the_written_item() {
    let (_, raw, containers, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();
    let cdn = MockCdn::start(containers);
    let dir = temp_dir("launch");
    let opts = options(&dir, CHUNK_COUNT);
    install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        None,
    )
    .expect("install should succeed");

    let target = launch::resolve_target(&dir, "E2E Game", None).expect("launch target");
    assert_eq!(target.app_name, "E2EGame");
    assert_eq!(target.display_name, "E2E Game");
    assert_eq!(target.executable, dir.join("a.bin"));
    assert_eq!(target.launch_command, "-epicapp=E2EGame");
    assert_eq!(target.namespace, "e2e");
    assert!(target.deployment_id.is_empty());

    let tokens = skiff_core::auth::TokenSet {
        account_id: "acc".into(),
        display_name: "Tester".into(),
        access_token: "token".into(),
        refresh_token: None,
        token_type: "bearer".into(),
        client_id: "client".into(),
        expires_at: 0,
        refresh_expires_at: 0,
        scope: String::new(),
    };
    let args = launch::build_auth_args(&target, Some(&tokens), Some("0123456789abcdef0123456789abcdef"), "en-US");
    assert_eq!(args.first().map(String::as_str), Some("-AUTH_LOGIN=unused"));
    assert!(args.iter().any(|arg| arg == "-epicapp=E2EGame"));
    assert!(args.iter().any(|arg| arg == "-epicsandboxid=e2e"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn install_reports_download_then_verify_phases() {
    let (_, raw, containers, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();
    let cdn = MockCdn::start(containers);
    let dir = temp_dir("phases");
    let opts = options(&dir, 2);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let seen = seen.clone();
        Arc::new(move |progress: Progress| seen.lock().unwrap().push(progress))
            as Arc<dyn Fn(Progress) + Send + Sync>
    };
    install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        Some(callback),
    )
    .expect("install should succeed");

    let events = seen.lock().unwrap();
    assert!(
        events.iter().any(|event| event.phase == Phase::Download),
        "the download stage should report progress"
    );
    let last_verify = events
        .iter()
        .rev()
        .find(|event| event.phase == Phase::Verify)
        .expect("the verification stage should report progress");
    assert_eq!(last_verify.verify.files_done, 2);
    assert_eq!(last_verify.verify.files_total, 2);
    assert_eq!(last_verify.verify.bytes_done, last_verify.verify.bytes_total);
    assert_eq!(last_verify.verify.threads, opts.concurrency.verify_threads());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn concurrency_is_never_exceeded_and_one_is_serial() {
    let (_, raw, containers, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();

    let cdn = MockCdn::start(containers);
    let dir = temp_dir("serial");
    let opts = options(&dir, 1);
    let outcome = install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        None,
    )
    .unwrap();
    assert_eq!(outcome.peak_inflight, 1, "concurrency 1 must serialize requests");
    assert_eq!(outcome.chunks_downloaded, CHUNK_COUNT);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn resumes_without_redownloading_finished_chunks() {
    let (_, raw, _, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();

    // Pretend a previous run finished every chunk but left no game files behind.
    let dir = temp_dir("resume");
    std::fs::create_dir_all(dir.join(".egstore")).unwrap();
    let manifest_sha1 = hex::encode(sha1(&raw));
    let ledger = serde_json::json!({
        "schema": 1,
        "app_name": "E2EGame",
        "build_version": "++E2EGame+Release-1.0-Windows",
        "manifest_sha1": manifest_sha1,
        "done_chunks": (0..CHUNK_COUNT).collect::<Vec<usize>>(),
        "bytes_done": 0,
        "updated_at": 0,
    });
    std::fs::write(
        dir.join(".egstore").join("skiff.resume"),
        serde_json::to_vec(&ledger).unwrap(),
    )
    .unwrap();

    // The CDN must not be asked for any chunk, so serve an empty map.
    let cdn = MockCdn::start(HashMap::new());
    let opts = InstallOptions { repair: false, ..options(&dir, 4) };
    // A ledger that claims completion, with no `.egstore/*.manifest` and no files on disk, is the
    // pathological case: files are pre-allocated to full size, so trusting the ledger would skip the
    // download and then fail verification. The install must therefore distrust it and re-fetch.
    let err = install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        None,
    )
    .expect_err("an empty CDN cannot satisfy a re-download");
    assert!(
        !err.to_string().contains("verification"),
        "the run must fail while downloading, not while verifying: {err}"
    );
    assert!(
        cdn.request_count() > 0,
        "a ledger claiming completion without an installed manifest must not be trusted"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn repair_redownloads_only_damaged_chunks() {
    let (_, raw, containers, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();

    // First: a clean install.
    let cdn = MockCdn::start(containers.clone());
    let dir = temp_dir("repair");
    let opts = options(&dir, 4);
    install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        None,
    )
    .unwrap();

    // Corrupt one byte of the second file and mark every chunk as complete so the engine has to
    // reach the verification/repair path instead of simply downloading everything again.
    let target = dir.join("sub").join("b.bin");
    let mut data = std::fs::read(&target).unwrap();
    data[0] ^= 0xFF;
    std::fs::write(&target, &data).unwrap();

    let manifest_sha1 = hex::encode(sha1(&raw));
    let ledger = serde_json::json!({
        "schema": 1,
        "app_name": "E2EGame",
        "build_version": "++E2EGame+Release-1.0-Windows",
        "manifest_sha1": manifest_sha1,
        "done_chunks": (0..CHUNK_COUNT).collect::<Vec<usize>>(),
        "bytes_done": 0,
        "updated_at": 0,
    });
    std::fs::write(
        dir.join(".egstore").join("skiff.resume"),
        serde_json::to_vec(&ledger).unwrap(),
    )
    .unwrap();

    let cdn = MockCdn::start(containers);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let callback = {
        let seen = seen.clone();
        Arc::new(move |progress: Progress| seen.lock().unwrap().push(progress))
            as Arc<dyn Fn(Progress) + Send + Sync>
    };
    let outcome = install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        Some(callback),
    )
    .unwrap();

    assert_eq!(
        outcome.repaired_chunks, 3,
        "only the three chunks backing the damaged file should be refetched"
    );
    let phases: Vec<Phase> = seen.lock().unwrap().iter().map(|event| event.phase).collect();
    assert!(phases.contains(&Phase::Verify), "verification should report progress");
    assert!(phases.contains(&Phase::Repair), "the repair download should report progress");
    let repaired = std::fs::read(&target).unwrap();
    assert_eq!(sha1(&repaired).to_vec(), parsed.files[1].hash.to_vec());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn truncated_chunk_is_rejected() {
    let (_, raw, containers, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();

    // Serve a corrupted container for the first chunk only.
    let mut broken = containers.clone();
    if let Some((path, body)) = broken.iter_mut().next() {
        let index = body.len() / 2;
        body[index] ^= 0xFF;
        let _ = path;
    }

    let cdn = MockCdn::start(broken);
    let dir = temp_dir("corrupt");
    let opts = InstallOptions { repair: false, ..options(&dir, 2) };
    let err = install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &raw,
        &opts,
        None,
    )
    .expect_err("a corrupt chunk must fail the install");
    let text = err.to_string();
    assert!(
        text.contains("chunk") || text.contains("integrity") || text.contains("verification"),
        "unexpected error: {text}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cdn_hosts_rotation_is_used_by_install() {
    // Two mock CDNs; base_urls carries both and the engine rotates across them.
    let (_, raw, containers, _) = fixtures();
    let parsed = Manifest::parse(&raw).unwrap();
    let cdn_a = MockCdn::start(containers.clone());
    let cdn_b = MockCdn::start(containers);

    let dir = temp_dir("rotate");
    let mut build = build_info(&cdn_a.base);
    build.base_urls = vec![cdn_a.base.clone(), cdn_b.base.clone()];
    let opts = options(&dir, 2);
    let outcome = install::install_manifest(
        &Client::plain(),
        &build,
        &game(),
        &parsed,
        &raw,
        &opts,
        None,
    )
    .unwrap();
    assert_eq!(outcome.chunks_downloaded, CHUNK_COUNT);
    assert!(
        cdn_a.request_count() > 0 && cdn_b.request_count() > 0,
        "both hosts should serve chunks: {} / {}",
        cdn_a.request_count(),
        cdn_b.request_count()
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn hosts_helper_is_round_robin() {
    let hosts = CdnHosts::new(vec!["http://a".into(), "http://b".into()]);
    assert_eq!(hosts.at(0), Some("http://a"));
    assert_eq!(hosts.at(1), Some("http://b"));
    assert_eq!(hosts.at(2), Some("http://a"));
}

struct NullSink;

impl ChunkSink for NullSink {
    fn write_chunk(&self, _index: usize, _plaintext: &[u8]) -> skiff_core::Result<()> {
        Ok(())
    }
}

/// A CDN that sends a valid response head and then goes silent.
///
/// This is exactly the state a cancel must break out of: the worker is blocked in a body read and a
/// naive implementation only notices the flag after the read times out (120 s).
fn stalling_server() -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling server");
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        // Drain the request line and headers, then answer with a body that never arrives.
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line == "\r\n" || line == "\n" {
                break;
            }
            line.clear();
        }
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 65536\r\nContent-Type: application/octet-stream\r\n\r\n",
        );
        let _ = stream.flush();
        while !thread_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    (format!("http://{addr}"), stop, handle)
}

#[test]
fn cancel_interrupts_a_stalled_chunk_download() {
    let (base, stop, server) = stalling_server();
    let client = Client::plain();
    let hosts = CdnHosts::new(vec![base]);
    let job = ChunkJob {
        index: 0,
        chunk: ChunkInfo {
            guid: [1, 2, 3, 4],
            rolling_hash: 0xAA,
            sha1: [0; 20],
            group_num: 1,
            window_size: 1024,
            file_size: 65_536,
            secret_guid: None,
            encryption_tag: None,
        },
        path: "ChunksV4/01/00000000000000000000000000000000/".to_string(),
    };
    let tracker = ProgressTracker::new(1, 65_536);
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();

    let worker = {
        let cancel = cancel.clone();
        let client = client.clone();
        std::thread::spawn(move || {
            let stats = download::download(
                &client,
                &hosts,
                std::slice::from_ref(&job),
                &DownloadOptions::new(Concurrency::default()),
                &NullSink,
                tracker,
                cancel,
                None,
            );
            let _ = tx.send(stats.is_ok());
        })
    };

    // Give the worker time to reach the blocked body read, then cancel.
    std::thread::sleep(Duration::from_millis(400));
    let started = Instant::now();
    cancel.store(true, Ordering::Relaxed);
    let finished = rx.recv_timeout(Duration::from_secs(5));
    let elapsed = started.elapsed();

    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    let _ = server.join();

    assert!(finished.is_ok(), "the download did not observe the cancel");
    eprintln!("cancel latency on a stalled body: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "cancel took {elapsed:?}");
}

// Silence "unused" for helpers used only in some test builds.
#[allow(dead_code)]
fn _read_all(mut stream: TcpStream) {
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
}

fn dlc_game() -> GameEntry {
    GameEntry {
        app_name: "E2EDlc".to_string(),
        namespace: "e2e".to_string(),
        catalog_item_id: "dlc-cid".to_string(),
        title: "E2E Add-on".to_string(),
        developer: "Tests".to_string(),
        folder_name: String::new(),
        entitlement_type: "EXECUTABLE".to_string(),
        is_dlc: true,
        parent_app_name: "E2EGame".to_string(),
        ..Default::default()
    }
}

#[test]
fn bundle_installs_base_and_dlc_into_one_directory() {
    let (base_manifest, base_raw, mut containers, payloads) = fixtures();
    let parsed = Manifest::parse(&base_raw).unwrap();

    // DLC content: one shared chunk (deduped with the base build) plus one new chunk.
    let shared_chunk = base_manifest.chunks[0].clone();
    let extra_data = payload(6);
    let extra_guid = [7u32, 0x1000, 0x2000, 0x3000];
    let extra_bytes = container(&extra_data, extra_guid);
    let extra_path = chunk_path(18, 6, chunk::rolling_hash(&extra_data), &extra_guid, None);
    containers.insert(format!("/{extra_path}"), extra_bytes.clone());
    let extra_chunk = ChunkInfo {
        guid: extra_guid,
        rolling_hash: chunk::rolling_hash(&extra_data),
        sha1: sha1(&extra_data),
        group_num: 6,
        window_size: extra_data.len() as u32,
        file_size: extra_bytes.len() as i64,
        secret_guid: None,
        encryption_tag: None,
    };

    let mut dlc_content = payloads[0].clone();
    dlc_content.extend_from_slice(&extra_data);
    let dlc_file = FileManifest {
        path: "dlc/c.bin".to_string(),
        symlink_target: String::new(),
        hash: sha1(&dlc_content),
        flags: 0,
        install_tags: Vec::new(),
        parts: vec![
            ChunkPart {
                guid: shared_chunk.guid,
                chunk_offset: 0,
                size: CHUNK_SIZE as u32,
                file_offset: 0,
            },
            ChunkPart {
                guid: extra_guid,
                chunk_offset: 0,
                size: CHUNK_SIZE as u32,
                file_offset: CHUNK_SIZE as u64,
            },
        ],
        size: dlc_content.len() as u64,
        md5: None,
        mime_type: String::new(),
        sha256: None,
    };
    let mut dlc_manifest = Manifest {
        version: 18,
        compressed: true,
        encrypted: false,
        header_sha1: [0; 20],
        meta: ManifestMeta {
            feature_level: 18,
            app_name: "E2EDlc".to_string(),
            build_version: "++E2EDlc+1-Windows".to_string(),
            ..Default::default()
        },
        chunks: vec![shared_chunk, extra_chunk],
        files: vec![dlc_file],
        custom: BTreeMap::new(),
        raw: Vec::new(),
    };
    let dlc_raw = dlc_manifest.write_binary().unwrap();
    // Keep the parsed manifest around: write_binary may normalise fields the installer does not use.
    dlc_manifest.raw = dlc_raw.clone();

    let cdn = MockCdn::start(containers);
    let dir = temp_dir("bundle");
    let opts = options(&dir, CHUNK_COUNT);

    let base_component = InstallComponent {
        game: game(),
        build: build_info(&cdn.base),
        manifest: parsed,
        raw: base_raw,
    };
    let dlc_component = InstallComponent {
        game: dlc_game(),
        build: BuildInfo {
            app_name: "E2EDlc".to_string(),
            namespace: "e2e".to_string(),
            catalog_item_id: "dlc-cid".to_string(),
            label_name: "Live-Windows".to_string(),
            build_version: "++E2EDlc+1-Windows".to_string(),
            manifest_uri: format!("{}/dlc.manifest", cdn.base),
            manifest_hash: "unused".to_string(),
            base_urls: vec![cdn.base.clone()],
        },
        manifest: dlc_manifest,
        raw: dlc_raw,
    };

    let outcome = install::install_components(
        &Client::plain(),
        &[base_component, dlc_component],
        &opts,
        None,
    )
    .expect("bundle install should succeed");

    assert_eq!(outcome.components, 2);
    assert_eq!(outcome.dlc_apps, vec!["E2EDlc".to_string()]);
    assert_eq!(outcome.files, 3, "two base files plus one DLC file");
    assert_eq!(outcome.conflicts, 0);
    assert_eq!(outcome.chunks_downloaded, CHUNK_COUNT + 1, "the shared chunk is downloaded once");
    assert_eq!(
        std::fs::read(dir.join("dlc/c.bin")).unwrap(),
        payloads[0]
            .iter()
            .chain(payload(6).iter())
            .copied()
            .collect::<Vec<u8>>()
    );

    let egstore = dir.join(".egstore");
    let item_files: Vec<PathBuf> = std::fs::read_dir(&egstore)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "item").unwrap_or(false))
        .collect();
    assert_eq!(item_files.len(), 2, "one .item per component");
    let dlc_item = item_files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .find(|item| item["AppName"] == "E2EDlc")
        .expect("DLC .item should exist");
    assert_eq!(dlc_item["MainGameAppName"], "E2EGame");
    assert_eq!(dlc_item["InstallLocation"], dir.to_string_lossy().to_string());
    assert_eq!(dlc_item["bIsApplication"], false);
    assert_eq!(dlc_item["bIsExecutable"], false);

    let installed = install::load_installed_components(&dir).expect("components readable");
    assert_eq!(installed.len(), 2);
    assert!(installed[0].is_main_game(), "the main game must come first");
    assert_eq!(installed[0].app_name, "E2EGame");
    assert_eq!(install::load_installed_files(&dir).unwrap().len(), 3);

    let report = install::uninstall(&dir, false, "E2EGame").expect("uninstall should succeed");
    assert_eq!(report.components, 2);
    assert_eq!(report.files_removed, 3);
    assert_eq!(report.failed_count, 0);
    assert!(report.metadata_removed);
    assert!(report.install_dir_removed, "the emptied install directory should be removed");
    assert!(!dir.join("dlc/c.bin").exists(), "DLC files must be removed");
    assert!(!egstore.exists(), "metadata must be removed");
}

// ---------------------------------------------------------------------------------------------
// adding an add-on to a finished install
// ---------------------------------------------------------------------------------------------

/// An add-on build: one chunk shared with the main game plus one chunk only it ships.
///
/// Returned as (manifest, raw manifest, extra CDN objects, expected file bytes) so the caller can
/// register the objects with the mock CDN before it starts (the CDN base URL comes from its port).
fn addon_fixtures(
    base_manifest: &Manifest,
    payloads: &[Vec<u8>],
) -> (Manifest, Vec<u8>, Vec<(String, Vec<u8>)>, Vec<u8>) {
    let shared_chunk = base_manifest.chunks[0].clone();
    let extra_data = payload(6);
    let extra_guid = [7u32, 0x1000, 0x2000, 0x3000];
    let extra_bytes = container(&extra_data, extra_guid);
    let extra_path = chunk_path(18, 6, chunk::rolling_hash(&extra_data), &extra_guid, None);
    let extra_chunk = ChunkInfo {
        guid: extra_guid,
        rolling_hash: chunk::rolling_hash(&extra_data),
        sha1: sha1(&extra_data),
        group_num: 6,
        window_size: extra_data.len() as u32,
        file_size: extra_bytes.len() as i64,
        secret_guid: None,
        encryption_tag: None,
    };

    let mut content = payloads[0].clone();
    content.extend_from_slice(&extra_data);
    let file = FileManifest {
        path: "dlc/c.bin".to_string(),
        symlink_target: String::new(),
        hash: sha1(&content),
        flags: 0,
        install_tags: Vec::new(),
        parts: vec![
            ChunkPart {
                guid: shared_chunk.guid,
                chunk_offset: 0,
                size: CHUNK_SIZE as u32,
                file_offset: 0,
            },
            ChunkPart {
                guid: extra_guid,
                chunk_offset: 0,
                size: CHUNK_SIZE as u32,
                file_offset: CHUNK_SIZE as u64,
            },
        ],
        size: content.len() as u64,
        md5: None,
        mime_type: String::new(),
        sha256: None,
    };

    let mut manifest = Manifest {
        version: 18,
        compressed: true,
        encrypted: false,
        header_sha1: [0; 20],
        meta: ManifestMeta {
            feature_level: 18,
            app_name: "E2EDlc".to_string(),
            build_version: "++E2EDlc+1-Windows".to_string(),
            ..Default::default()
        },
        chunks: vec![shared_chunk, extra_chunk],
        files: vec![file],
        custom: BTreeMap::new(),
        raw: Vec::new(),
    };
    let raw = manifest.write_binary().unwrap();
    manifest.raw = raw.clone();
    (manifest, raw, vec![(format!("/{extra_path}"), extra_bytes)], content)
}

fn addon_build(cdn_base: &str) -> BuildInfo {
    BuildInfo {
        app_name: "E2EDlc".to_string(),
        namespace: "e2e".to_string(),
        catalog_item_id: "dlc-cid".to_string(),
        label_name: "Live-Windows".to_string(),
        build_version: "++E2EDlc+1-Windows".to_string(),
        manifest_uri: format!("{cdn_base}/dlc.manifest"),
        manifest_hash: "unused".to_string(),
        base_urls: vec![cdn_base.to_string()],
    }
}

/// The `.item` the launcher would read for `app`.
fn item_for(dir: &Path, app: &str) -> serde_json::Value {
    std::fs::read_dir(dir.join(".egstore"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().map(|ext| ext == "item").unwrap_or(false))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .find(|item| item["AppName"] == app)
        .unwrap_or_else(|| panic!("no .item for {app}"))
}

#[test]
fn adds_an_add_on_without_touching_the_finished_install() {
    let (base_manifest, base_raw, mut containers, payloads) = fixtures();
    let parsed = Manifest::parse(&base_raw).unwrap();
    let (addon_manifest, addon_raw, addon_objects, addon_content) =
        addon_fixtures(&base_manifest, &payloads);
    for (path, bytes) in addon_objects {
        containers.insert(path, bytes);
    }
    let cdn = MockCdn::start(containers);
    let dir = temp_dir("addon-add");
    let opts = options(&dir, 4);

    // 1. A finished main-game install, exactly like a normal `skiff install`.
    let base_outcome = install::install_manifest(
        &Client::plain(),
        &build_info(&cdn.base),
        &game(),
        &parsed,
        &base_raw,
        &opts,
        None,
    )
    .expect("main game install should succeed");
    assert_eq!(base_outcome.components, 1);

    let main_guid_before = install::load_installed_components(&dir).unwrap()[0].guid.clone();
    let main_item_before = item_for(&dir, "E2EGame");
    let file_a_before = std::fs::read(dir.join("a.bin")).unwrap();
    let file_b_before = std::fs::read(dir.join("sub/b.bin")).unwrap();

    // 2. Add the add-on to that directory.
    let addon = InstallComponent {
        game: dlc_game(),
        build: addon_build(&cdn.base),
        manifest: addon_manifest,
        raw: addon_raw,
    };
    let outcome =
        install::add_components_to_install(&Client::plain(), &game(), &[addon], &opts, None)
            .expect("add-on install should succeed");

    assert_eq!(outcome.components, 2);
    assert_eq!(outcome.dlc_apps, vec!["E2EDlc".to_string()]);
    assert_eq!(outcome.files, 3, "two main-game files plus one add-on file");
    assert_eq!(outcome.conflicts, 0);
    // The add-on shares one chunk with the main game. That chunk is fetched once and scattered to
    // both files (the new file has no bytes there yet); the add-on's own chunk comes with it.
    assert_eq!(
        outcome.chunks_downloaded, 2,
        "only the add-on's chunks are fetched, never the main game's own chunk set"
    );

    // The main game is byte-identical and its launcher entry was not rewritten.
    assert_eq!(std::fs::read(dir.join("a.bin")).unwrap(), file_a_before);
    assert_eq!(std::fs::read(dir.join("sub/b.bin")).unwrap(), file_b_before);
    assert_eq!(item_for(&dir, "E2EGame"), main_item_before);
    assert_eq!(
        install::load_installed_components(&dir).unwrap()[0].guid,
        main_guid_before
    );

    // The add-on's files landed, and its metadata points at the existing main-game identity.
    assert_eq!(std::fs::read(dir.join("dlc/c.bin")).unwrap(), addon_content);
    let components = install::load_installed_components(&dir).unwrap();
    assert_eq!(components.len(), 2);
    let main = components.iter().find(|component| component.is_main_game()).unwrap();
    let added = components.iter().find(|component| component.app_name == "E2EDlc").unwrap();
    assert_eq!(added.main_app_name, "E2EGame");
    assert_eq!(added.guid, added.item["InstallationGuid"].as_str().unwrap());
    assert_eq!(
        added.item["HostInstallationGuid"].as_str().unwrap(),
        main.guid,
        "the add-on must reuse the main game's InstallationGuid"
    );

    // Adding the same add-on again is a no-op, not a second download.
    let addon_again = InstallComponent {
        game: dlc_game(),
        build: addon_build(&cdn.base),
        manifest: Manifest::parse(&components[0].raw).unwrap(),
        raw: components[0].raw.clone(),
    };
    assert!(
        install::add_components_to_install(&Client::plain(), &game(), &[addon_again], &opts, None)
            .is_err(),
        "re-adding an installed add-on must not run an install"
    );

    std::fs::remove_dir_all(&dir).ok();
}

