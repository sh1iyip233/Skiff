#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

/// Pick a writable directory for WebView2's profile.
///
/// WebView2 defaults to `%LOCALAPPDATA%\<exe>\EBWebView`. On machines where writes under
/// `AppData\Local` are denied for non-elevated processes, Tauri aborts at startup with
/// "Failed to setup app: access denied" - which looks like an instant crash. Pointing WebView2 at a
/// writable folder lets the app start without administrator rights.
fn writable_webview_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("webview-data"));
        }
    }
    candidates.push(std::env::temp_dir().join("epicd-webview"));
    if let Ok(home) = std::env::var("USERPROFILE") {
        candidates.push(PathBuf::from(home).join("epicd-webview"));
    }

    for dir in candidates {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let probe = dir.join(".write-probe");
        let ok = std::fs::write(&probe, b"ok").is_ok();
        let _ = std::fs::remove_file(&probe);
        if ok {
            return Some(dir);
        }
    }
    None
}

fn main() {
    if std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
        if let Some(dir) = writable_webview_dir() {
            // Must be set before Tauri initialises WebView2.
            std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &dir);
        }
    }
    epicd_gui_lib::run()
}