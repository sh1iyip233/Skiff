//! Pause vs cancel semantics: pausing keeps the partial download, cancelling deletes it.
//!
//! These tests drive the process-wide stop flag, so they live in their own test binary (one process)
//! and run their scenarios sequentially.

use epic_core::api::{BuildInfo, GameEntry};
use epic_core::config::Concurrency;
use epic_core::http::Client;
use epic_core::install::{self, InstallOptions, StopKind};
use epic_core::manifest::{ChunkInfo, ChunkPart, FileManifest, Manifest, ManifestMeta};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn fixtures() -> (Manifest, Vec<u8>) {
    let chunk_a = ChunkInfo {
        guid: [1, 0, 0, 0],
        rolling_hash: 0xA,
        sha1: [1; 20],
        group_num: 1,
        window_size: 1024,
        file_size: 1024,
        secret_guid: None,
        encryption_tag: None,
    };
    let chunk_b = ChunkInfo {
        guid: [2, 0, 0, 0],
        rolling_hash: 0xB,
        sha1: [2; 20],
        group_num: 2,
        window_size: 1024,
        file_size: 1024,
        secret_guid: None,
        encryption_tag: None,
    };
    let file = |path: &str, chunk: &ChunkInfo| FileManifest {
        path: path.to_string(),
        symlink_target: String::new(),
        hash: [0; 20],
        flags: 0,
        install_tags: Vec::new(),
        parts: vec![ChunkPart {
            guid: chunk.guid,
            chunk_offset: 0,
            size: 1024,
            file_offset: 0,
        }],
        size: 1024,
        md5: None,
        mime_type: String::new(),
        sha256: None,
    };
    let files = vec![file("a.bin", &chunk_a), file("sub/b.bin", &chunk_b)];
    let manifest = Manifest {
        version: 18,
        compressed: true,
        encrypted: false,
        header_sha1: [0; 20],
        meta: ManifestMeta::default(),
        chunks: vec![chunk_a, chunk_b],
        files,
        custom: BTreeMap::new(),
        raw: Vec::new(),
    };
    let raw = manifest.write_binary().expect("write manifest");
    (manifest, raw)
}

fn game() -> GameEntry {
    GameEntry {
        app_name: "CancelGame".to_string(),
        namespace: "cancel-test".to_string(),
        catalog_item_id: "cid".to_string(),
        title: "Cancel Test".to_string(),
        developer: "Tests".to_string(),
        folder_name: "CancelTest".to_string(),
        entitlement_type: "EXECUTABLE".to_string(),
        ..Default::default()
    }
}

fn build_info(base: &str) -> BuildInfo {
    BuildInfo {
        app_name: "CancelGame".to_string(),
        namespace: "cancel-test".to_string(),
        catalog_item_id: "cid".to_string(),
        label_name: "Live-Windows".to_string(),
        build_version: "v1".to_string(),
        manifest_uri: format!("{base}/manifest.bin"),
        manifest_hash: "unused".to_string(),
        base_urls: vec![base.to_string()],
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-tmp")
        .join(format!("cancel-{tag}-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Headers arrive, the body never does: the download blocks until a stop request arrives.
fn stalling_cdn() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalling cdn");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line == "\r\n" || line == "\n" {
                break;
            }
            line.clear();
        }
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n");
        let _ = stream.flush();
        // Hold the connection open until the client gives up, then drain it.
        let mut sink = [0u8; 4096];
        let _ = std::io::Read::read(&mut stream, &mut sink);
    });
    (format!("http://{addr}"), handle)
}

fn options(dir: &Path) -> InstallOptions {
    InstallOptions {
        install_dir: dir.to_path_buf(),
        concurrency: Concurrency { downloads: 1, writers: 1, ..Concurrency::default() },
        limit_bytes_per_sec: None,
        verify: true,
        repair: true,
        register_launcher: false,
    }
}

/// Run one interrupted install and return the result.
///
/// The stalling server thread is detached: it may still be blocked reading the socket, and joining it
/// would outlive the test for no benefit.
fn interrupted_install(
    kind: StopKind,
    dir: &Path,
) -> epic_core::Result<install::InstallOutcome> {
    let (base, _server) = stalling_cdn();
    let (manifest, raw) = fixtures();
    let client = Client::plain();
    let build = build_info(&base);
    let game = game();
    let opts = options(dir);

    // A fresh stop flag per scenario; `request_stop` records the kind.
    install::set_cancel_flag(Arc::new(AtomicBool::new(false)));
    let on_progress: Arc<dyn Fn(epic_core::download::Progress) + Send + Sync> =
        Arc::new(move |_| install::request_stop(kind));

    install::install_manifest(
        &client,
        &build,
        &game,
        &manifest,
        &raw,
        &opts,
        Some(on_progress),
    )
}

#[test]
fn pause_keeps_files_and_cancel_deletes_them() {
    // ---- pause: partial files and the resume ledger stay ----
    let pause_dir = temp_dir("pause");
    let paused = interrupted_install(StopKind::Pause, &pause_dir);
    match &paused {
        Err(epic_core::Error::Cancelled { removed, kept_existing_install }) => {
            assert_eq!(*removed, 0, "a pause must not delete anything");
            assert!(!kept_existing_install);
        }
        other => panic!("expected a pause, got {other:?}"),
    }
    assert!(pause_dir.join("a.bin").exists(), "pause keeps preallocated files");
    assert!(pause_dir.join("sub").join("b.bin").exists());
    assert!(install::has_partial_install(&pause_dir), "pause keeps the resume state");
    std::fs::remove_dir_all(&pause_dir).ok();

    // ---- cancel: the same run deletes every file it created ----
    let cancel_dir = temp_dir("cancel");
    let cancelled = interrupted_install(StopKind::Cancel, &cancel_dir);
    match &cancelled {
        Err(epic_core::Error::Cancelled { removed, kept_existing_install }) => {
            assert_eq!(*removed, 2, "both partial files should be deleted");
            assert!(!kept_existing_install);
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
    assert!(!cancel_dir.join("a.bin").exists(), "cancel deletes the partial files");
    assert!(!cancel_dir.join("sub").join("b.bin").exists());
    assert!(!cancel_dir.join(".egstore").exists(), "cancel deletes the bookkeeping");
    assert!(!cancel_dir.exists(), "an otherwise empty install dir is removed");
}

#[test]
fn cancel_never_deletes_a_completed_install() {
    let dir = temp_dir("completed");
    let (manifest, raw) = fixtures();
    // A completed install: `.egstore` carries the manifest backup, the game files are present.
    std::fs::create_dir_all(dir.join(".egstore")).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join(".egstore").join("ABCD.manifest"), &raw).unwrap();
    std::fs::write(dir.join("a.bin"), b"complete").unwrap();
    std::fs::write(dir.join("sub").join("b.bin"), b"complete").unwrap();
    assert_eq!(manifest.files.len(), 2);

    let report = install::discard_partial_install(&dir);
    assert!(report.kept_existing_install);
    assert_eq!(report.removed, 0);
    assert!(dir.join("a.bin").exists(), "a working install is never deleted");
    assert!(dir.join("sub").join("b.bin").exists());
    assert!(dir.join(".egstore").join("ABCD.manifest").exists());
    std::fs::remove_dir_all(&dir).ok();
}
