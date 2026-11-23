//! Local cover-art cache for the game library.
//!
//! Epic's catalog exposes public key images; the desktop UI downloads the tall box art once per
//! URL and keeps it in `<state>/covers`. Rendering never waits on the network: a missing file is
//! simply a placeholder until a background worker finishes it, and the UI polls [`generation`] to
//! know when a new image is available.
//!
//! Thumbnails are requested from Epic's image CDN with `?h=320&resize=1` (it scales the source by
//! height), so no local image decoding or scaling is needed and the process keeps no image crate
//! dependency of its own.

use skiff_core::http::{Client, Request};
use skiff_core::util;
use slint::Image;
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Requested thumbnail height. Portrait box art comes back at about 240×320.
const COVER_HEIGHT: u32 = 320;
/// Refuse anything larger; cover art is tens of KiB. A misbehaving CDN must not balloon the cache.
const MAX_COVER_BYTES: usize = 2 * 1024 * 1024;
/// Concurrent cover downloads. Small enough to stay polite and to avoid a burst of TLS handshakes.
const WORKERS: usize = 4;
/// Formats Epic's key-image CDN currently serves (and that Slint's decoders understand). The real
/// format is detected from the bytes, not from the URL: some catalog images are PNG even though
/// their path has no extension, and saving those as `.jpg` made the decoder reject them.
const CACHE_EXTENSIONS: [&str; 4] = ["jpg", "png", "webp", "gif"];

/// One process-wide cover store. Clone the [`Arc`], not the struct.
pub struct CoverLoader {
    dir: PathBuf,
    generation: AtomicU64,
    /// URLs already queued or finished in this session, so two library refreshes do not duplicate
    /// work. A failed URL is not retried until the process restarts or a refresh arrives with a
    /// different URL; that avoids a retry storm when the network is down.
    seen: Mutex<HashSet<String>>,
    queue: Mutex<Vec<String>>,
    /// Cache files whose extension already matched their bytes, so the UI thread does not re-read
    /// every cover header on each model rebuild.
    validated: Mutex<HashSet<PathBuf>>,
}

impl CoverLoader {
    /// Create a loader bound to a cache directory (usually `<state>/covers`).
    pub fn new(dir: impl Into<PathBuf>) -> CoverLoader {
        CoverLoader {
            dir: dir.into(),
            generation: AtomicU64::new(0),
            seen: Mutex::new(HashSet::new()),
            queue: Mutex::new(Vec::new()),
            validated: Mutex::new(HashSet::new()),
        }
    }

    /// How many covers have landed since startup. The UI rebuilds rows when this changes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Cached path for a catalog cover URL, when a valid file is already on disk.
    pub fn cached_path(&self, url: &str) -> Option<PathBuf> {
        if url.is_empty() {
            return None;
        }
        let Ok(mut validated) = self.validated.lock() else {
            return None;
        };
        for extension in CACHE_EXTENSIONS {
            let path = self.path_for_ext(url, extension);
            if !path.is_file() {
                continue;
            }
            if validated.contains(&path) {
                return Some(path);
            }
            if file_matches_extension(&path, extension) {
                validated.insert(path.clone());
                return Some(path);
            }
            // Left over from an older build that keyed everything as `.jpg`: drop it so the next
            // request downloads the correct format.
            let _ = std::fs::remove_file(&path);
        }
        None
    }

    /// Decode a cached cover for the UI thread. Returns `None` while the download is still pending.
    pub fn image_for(&self, url: &str) -> Option<Image> {
        let path = self.cached_path(url)?;
        Image::load_from_path(&path).ok()
    }

    /// Queue every missing URL and start (or top up) the worker pool.
    ///
    /// Safe to call from either the UI thread or the library-refresh worker. Calls are cheap when
    /// everything is already cached.
    pub fn request_all(self: &Arc<Self>, urls: impl IntoIterator<Item = String>) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }

        let mut queued = 0usize;
        {
            let Ok(mut seen) = self.seen.lock() else { return };
            let Ok(mut queue) = self.queue.lock() else { return };
            for url in urls {
                if url.is_empty() || !url.starts_with("https://") {
                    continue;
                }
                if self.cached_path(&url).is_some() {
                    continue;
                }
                if !seen.insert(url.clone()) {
                    continue;
                }
                queue.push(url);
                queued += 1;
            }
        }
        if queued == 0 {
            return;
        }

        for _ in 0..WORKERS {
            let loader = Arc::clone(self);
            let _ = std::thread::Builder::new()
                .name("skiff-cover".to_string())
                .spawn(move || loader.work());
        }
    }

    /// Consume queued URLs until the queue is empty.
    fn work(&self) {
        loop {
            let next = {
                let Ok(mut queue) = self.queue.lock() else { return };
                queue.pop()
            };
            let Some(url) = next else { return };
            if self.fetch(&url) {
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn fetch(&self, url: &str) -> bool {
        let client = Client::new();
        let request = Request::get(thumbnail_url(url)).timeout(20);
        let mut sink = LimitedBuffer::new(MAX_COVER_BYTES);
        let Ok(head) = client.request(&request, &mut sink) else {
            return false;
        };
        if !head.is_success() || sink.buf.is_empty() {
            return false;
        }
        let image = head
            .header("content-type")
            .map(|value| value.to_ascii_lowercase().starts_with("image/"))
            .unwrap_or(true);
        if !image {
            return false;
        }
        let extension = detect_extension(&sink.buf);
        let path = self.path_for_ext(url, extension);
        if util::atomic_write(&path, &sink.buf).is_err() {
            return false;
        }
        if let Ok(mut validated) = self.validated.lock() {
            validated.insert(path);
        }
        true
    }

    fn path_for_ext(&self, url: &str, extension: &str) -> PathBuf {
        self.dir.join(format!("{}.{extension}", cache_stem(&thumbnail_url(url))))
    }
}

/// Append the CDN resize parameters without clobbering an existing query string.
fn thumbnail_url(url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}h={COVER_HEIGHT}&resize=1")
}

/// Stable, filesystem-safe cache stem derived from the thumbnail URL.
fn cache_stem(url: &str) -> String {
    format!("{:016x}", fnv1a(url.as_bytes()))
}

/// Detect the image container from its magic bytes; fall back to JPEG for unknown payloads so the
/// file at least keeps a decodable extension.
fn detect_extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "jpg"
    }
}

/// True when the first bytes of `path` match `extension`.
fn file_matches_extension(path: &std::path::Path, extension: &str) -> bool {
    let mut head = [0u8; 12];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(read) = file.read(&mut head) else {
        return false;
    };
    detect_extension(&head[..read]) == extension
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Write sink that refuses to grow past `limit`, so one bad response cannot exhaust memory.
struct LimitedBuffer {
    buf: Vec<u8>,
    limit: usize,
}

impl LimitedBuffer {
    fn new(limit: usize) -> LimitedBuffer {
        LimitedBuffer { buf: Vec::new(), limit }
    }
}

impl Write for LimitedBuffer {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.limit {
            return Err(io::Error::new(io::ErrorKind::Other, "cover image exceeds size limit"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resize_parameters_are_appended_once() {
        assert_eq!(
            thumbnail_url("https://cdn.example/a"),
            "https://cdn.example/a?h=320&resize=1"
        );
        assert_eq!(
            thumbnail_url("https://cdn.example/a?x=1"),
            "https://cdn.example/a?x=1&h=320&resize=1"
        );
    }

    #[test]
    fn cache_names_are_stable_and_distinct() {
        let a = cache_stem("https://cdn.example/a");
        let b = cache_stem("https://cdn.example/b");
        assert_eq!(a, cache_stem("https://cdn.example/a"));
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn detects_the_image_container_from_bytes() {
        assert_eq!(detect_extension(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]), "png");
        assert_eq!(detect_extension(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpg");
        assert_eq!(detect_extension(b"GIF89a"), "gif");
        assert_eq!(detect_extension(b"RIFF0000WEBP"), "webp");
        assert_eq!(detect_extension(b"not-an-image"), "jpg");
    }

    #[test]
    fn oversized_images_are_rejected() {
        let mut sink = LimitedBuffer::new(4);
        assert!(sink.write(b"abc").is_ok());
        assert!(sink.write(b"de").is_err());
        assert_eq!(sink.buf, b"abc");
    }

    #[test]
    fn missing_and_empty_urls_have_no_cache_path() {
        let loader = CoverLoader::new(std::env::temp_dir().join("skiff-cover-test-none"));
        assert!(loader.cached_path("").is_none());
        assert!(loader.cached_path("https://cdn.example/never-downloaded").is_none());
    }

    #[test]
    fn path_is_inside_the_cache_directory() {
        let dir = std::env::temp_dir().join("skiff-cover-test-dir");
        let loader = CoverLoader::new(&dir);
        let path = loader.path_for_ext("https://cdn.example/x", "jpg");
        assert_eq!(path.parent(), Some(Path::new(&dir)));
    }
}
