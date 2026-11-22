//! Small shared helpers: hex, sizes, safe paths, atomic writes, rate parsing.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Lowercase hex encoding.
pub fn hex_lower(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Uppercase hex encoding (Epic uses uppercase inside chunk file names).
pub fn hex_upper(bytes: &[u8]) -> String {
    hex::encode_upper(bytes)
}

/// Human readable byte count.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = n as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Windows device names that cannot be used as file names.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate and normalize a manifest-supplied relative path.
///
/// Rejects absolute paths, drive letters, UNC prefixes, `..` traversal, reserved device names
/// and characters Windows cannot represent. Returns the path with `\` separators.
pub fn sanitize_rel_path(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(Error::integrity("manifest contains an empty file path"));
    }
    let unified = raw.replace('/', "\\");
    if unified.starts_with('\\') {
        return Err(Error::integrity(format!("absolute path in manifest: {raw}")));
    }
    if unified.len() >= 2 && unified.as_bytes()[1] == b':' {
        return Err(Error::integrity(format!("drive-letter path in manifest: {raw}")));
    }
    if unified.chars().any(|c| (c as u32) < 0x20) {
        return Err(Error::integrity(format!("control character in manifest path: {raw}")));
    }
    let mut out: Vec<&str> = Vec::new();
    for segment in unified.split('\\') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(Error::integrity(format!("path traversal in manifest: {raw}")));
        }
        if segment.ends_with(' ') || segment.ends_with('.') {
            return Err(Error::integrity(format!("invalid trailing character in path: {raw}")));
        }
        let stem = segment.split('.').next().unwrap_or(segment).to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return Err(Error::integrity(format!("reserved device name in path: {raw}")));
        }
        out.push(segment);
    }
    if out.is_empty() {
        return Err(Error::integrity(format!("degenerate path in manifest: {raw}")));
    }
    Ok(out.join("\\"))
}

/// Join a validated relative path onto the install root, using a long-path prefix on Windows so
/// deep manifests do not hit `MAX_PATH`.
pub fn install_path(root: &Path, rel: &str) -> PathBuf {
    #[cfg(windows)]
    {
        if !root.is_absolute() {
            // A `\\?\` prefix is only valid in front of an absolute path.
            return root.join(rel);
        }
        let mut s = root.to_string_lossy().replace('/', "\\");
        if let Some(stripped) = s.strip_prefix("\\\\?\\") {
            s = stripped.to_string();
        }
        if !s.ends_with('\\') {
            s.push('\\');
        }
        s.push_str(rel);
        PathBuf::from(format!("\\\\?\\{s}"))
    }
    #[cfg(not(windows))]
    {
        root.join(rel.replace('\\', "/"))
    }
}

/// True when `path` (as produced by [`install_path`]) stays inside `root`.
///
/// `install_path` prefixes absolute Windows paths with `\\?\` for long-path support, so a plain
/// `path.starts_with(root)` comparison is always false there. Comparing against the same
/// normalised root keeps the containment guarantee meaningful on every platform.
pub fn is_within(root: &Path, path: &Path) -> bool {
    path.starts_with(install_path(root, ""))
}

/// Write a file atomically (temp file in the same directory, then rename).
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    tmp.set_file_name(format!(".{file_name}.tmp"));
    let ctx = |e: std::io::Error| {
        Error::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
    };
    std::fs::write(&tmp, data).map_err(ctx)?;
    // Windows rename fails if the destination exists, so remove it first.
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).map_err(ctx)?;
    Ok(())
}

/// Parse a bandwidth limit such as `50M`, `1.5G`, `800k` or `0` (= unlimited).
pub fn parse_rate(text: &str) -> Result<Option<u64>> {
    let t = text.trim();
    if t.is_empty() || t == "0" {
        return Ok(None);
    }
    let (num, suffix) = match t.find(|c: char| c.is_ascii_alphabetic()) {
        Some(idx) => (&t[..idx], &t[idx..]),
        None => (t, ""),
    };
    let value: f64 = num
        .parse()
        .map_err(|_| Error::usage(format!("cannot parse rate limit '{text}'")))?;
    let multiplier: f64 = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        other => return Err(Error::usage(format!("unknown rate suffix '{other}'"))),
    };
    let bytes = value * multiplier;
    if bytes <= 0.0 {
        return Ok(None);
    }
    Ok(Some(bytes as u64))
}

/// Remove anything that looks like a credential before it reaches a log line.
pub fn redact(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut redact_next = false;
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        if lower == "bearer" || lower == "basic" || lower.ends_with("authorization:") {
            out.push(token);
            redact_next = true;
            continue;
        }
        if redact_next {
            out.push("<redacted>");
            redact_next = false;
            continue;
        }
        if lower.contains("refresh_token=")
            || lower.contains("device_code=")
            || lower.contains("access_token=")
            || lower.contains("authorization_code=")
        {
            out.push("<redacted>");
        } else {
            out.push(token);
        }
    }
    out.join(" ")
}

/// Exponential backoff with jitter, capped.
pub fn backoff_delay(attempt: u32, cap_ms: u64) -> std::time::Duration {
    let base = 200u64.saturating_mul(1u64 << attempt.min(6));
    let capped = base.min(cap_ms);
    let jitter = fastrand::u64(0..=capped / 2 + 1);
    std::time::Duration::from_millis(capped + jitter)
}

/// Longest uninterrupted nap while waiting for a cancellable operation.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Sleep up to `total`, waking every 100 ms to observe `cancel`.
///
/// A plain `thread::sleep` between download retries could keep a cancelled task alive for the whole
/// backoff window (tens of seconds). Returns `false` when cancellation was requested.
pub fn sleep_cancellable(cancel: &Arc<AtomicBool>, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(CANCEL_POLL));
    }
}

/// Unix timestamp in seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(sanitize_rel_path("../evil.exe").is_err());
        assert!(sanitize_rel_path("a/../../evil.exe").is_err());
        assert!(sanitize_rel_path("C:\\Windows\\evil.exe").is_err());
        assert!(sanitize_rel_path("\\\\server\\share\\evil.exe").is_err());
        assert!(sanitize_rel_path("").is_err());
    }

    #[test]
    fn rejects_reserved_and_trailing_dots() {
        assert!(sanitize_rel_path("CON").is_err());
        assert!(sanitize_rel_path("dir/NUL.txt").is_err());
        assert!(sanitize_rel_path("dir/name.").is_err());
        assert!(sanitize_rel_path("dir/name ").is_err());
    }

    #[test]
    fn normalizes_valid_paths() {
        assert_eq!(sanitize_rel_path("Binaries/Win64/Game.exe").unwrap(), "Binaries\\Win64\\Game.exe");
        assert_eq!(sanitize_rel_path("a//b/./c.txt").unwrap(), "a\\b\\c.txt");
    }

    #[test]
    fn parses_rate_limits() {
        assert_eq!(parse_rate("50M").unwrap(), Some(50 * 1024 * 1024));
        assert_eq!(parse_rate("1.5G").unwrap(), Some((1.5 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_rate("0").unwrap(), None);
        assert!(parse_rate("fast").is_err());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 3), "3.00 MiB");
    }

    #[test]
    fn containment_matches_install_paths() {
        let root = std::env::temp_dir().join("epicd-within-test");
        let inside = install_path(&root, "sub/file.bin");
        assert!(is_within(&root, &inside));
        assert!(!is_within(&root, Path::new("D:\\somewhere-else\\file.bin")));
    }

    #[test]
    fn redacts_credentials() {
        let s = redact("Authorization: bearer ABCDEF refresh_token=xyz");
        assert!(!s.contains("ABCDEF"));
        assert!(!s.contains("xyz"));
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let a = backoff_delay(0, 30_000).as_millis() as u64;
        let b = backoff_delay(5, 30_000).as_millis() as u64;
        assert!(b >= a);
        assert!(backoff_delay(30, 30_000).as_millis() as u64 <= 45_000);
    }

    #[test]
    fn cancellable_sleep_stops_as_soon_as_the_flag_rises() {
        let flag = Arc::new(AtomicBool::new(false));
        let waiter = flag.clone();
        let raiser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            waiter.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        // A 30 s nap must end with the flag, not with the deadline.
        assert!(!sleep_cancellable(&flag, Duration::from_secs(30)));
        let elapsed = started.elapsed();
        raiser.join().unwrap();
        assert!(elapsed < Duration::from_secs(2), "cancel took {elapsed:?}");
    }

    #[test]
    fn cancellable_sleep_finishes_when_not_cancelled() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(sleep_cancellable(&flag, Duration::from_millis(50)));
    }
}
