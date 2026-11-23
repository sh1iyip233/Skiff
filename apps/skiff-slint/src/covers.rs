//! Local cover-art cache and background thumbnail decoder for the game library.
//!
//! Epic's catalog exposes public key images. Although we request `?h=320&resize=1`, some CDN
//! origins ignore resize query parameters and return full 1200x1600 (2–8 MiB) PNG/JPEG assets.
//! Decoding >100 full-resolution images synchronously on the UI thread would freeze startup and
//! consume >1 GiB of RAM.
//!
//! Therefore this module keeps ALL network fetches, disk reads, image decoding, and area-averaged
//! downscaling (`<= 320px`) on a bounded background worker pool. Decoded thumbnails are also
//! persisted as tiny raw `.thumb` files so subsequent launches load in milliseconds. The UI
//! thread only performs O(1) in-memory `SharedPixelBuffer` clones.

use skiff_core::http::{Client, Request};
use skiff_core::util;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Requested thumbnail height and maximum decoded thumbnail edge in pixels.
const COVER_HEIGHT: u32 = 320;
/// Upper bound for a single cover download. Some Epic CDN nodes ignore resize parameters and return
/// raw 1200x1600 / 2560x1440 PNGs that exceed 2 MiB.
const MAX_COVER_BYTES: usize = 12 * 1024 * 1024;
/// Concurrent cover download / decode workers.
const WORKERS: usize = 4;
/// Formats Epic's key-image CDN serves (and that Slint's decoders understand).
const CACHE_EXTENSIONS: [&str; 4] = ["jpg", "png", "webp", "gif"];
/// Legacy RGB8 thumbnail magic (`SKTH` + u16 width + u16 height + RGB8).
const THUMB_MAGIC_RGB: &[u8; 4] = b"SKTH";
/// RGBA8 thumbnail magic (`SKT4` + u16 width + u16 height + RGBA8).
const THUMB_MAGIC_RGBA: &[u8; 4] = b"SKT4";

/// One process-wide cover store. Clone the [`Arc`], not the struct.
pub struct CoverLoader {
    dir: PathBuf,
    generation: AtomicU64,
    active_workers: AtomicUsize,
    /// URLs already queued for network download in this session.
    seen_download: Mutex<HashSet<String>>,
    /// URLs already queued for background decode in this session.
    seen_decode: Mutex<HashSet<String>>,
    /// Local files waiting to be decoded into downscaled thumbnails (drained first).
    decode_queue: Mutex<Vec<String>>,
    /// Remote URLs waiting to be downloaded and then decoded.
    download_queue: Mutex<Vec<String>>,
    /// Cache files whose extension already matched their bytes.
    validated: Mutex<HashSet<PathBuf>>,
    /// In-memory decoded & downscaled RGBA8 thumbnails ready for instant UI-thread lookup.
    decoded: Mutex<HashMap<String, SharedPixelBuffer<Rgba8Pixel>>>,
}

impl CoverLoader {
    /// Create a loader bound to a cache directory (usually `<state>/covers`).
    pub fn new(dir: impl Into<PathBuf>) -> CoverLoader {
        CoverLoader {
            dir: dir.into(),
            generation: AtomicU64::new(0),
            active_workers: AtomicUsize::new(0),
            seen_download: Mutex::new(HashSet::new()),
            seen_decode: Mutex::new(HashSet::new()),
            decode_queue: Mutex::new(Vec::new()),
            download_queue: Mutex::new(Vec::new()),
            validated: Mutex::new(HashSet::new()),
            decoded: Mutex::new(HashMap::new()),
        }
    }

    /// Incremented whenever a new decoded thumbnail becomes ready in memory.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// True once a decoded thumbnail for `url` is available in memory.
    pub fn is_decoded(&self, url: &str) -> bool {
        if url.is_empty() {
            return false;
        }
        self.decoded.lock().map(|map| map.contains_key(url)).unwrap_or(false)
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

    /// Instant, non-blocking UI-thread lookup of an already decoded thumbnail.
    ///
    /// Never performs disk I/O or image decoding on the UI thread.
    pub fn image_for(&self, url: &str) -> Option<Image> {
        if url.is_empty() {
            return None;
        }
        let buffer = self.decoded.lock().ok()?.get(url).cloned()?;
        Some(Image::from_rgba8(buffer))
    }

    /// Queue every URL for background decode (if cached) or download+decode, then ensure workers run.
    ///
    /// Safe and fast to call from either the UI thread or a background refresh worker.
    pub fn request_all(self: &Arc<Self>, urls: impl IntoIterator<Item = String>) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }

        let mut enqueued = false;
        for url in urls {
            if url.is_empty() || !url.starts_with("https://") {
                continue;
            }
            if self.is_decoded(&url) {
                continue;
            }
            // Fast path: if a pre-scaled `.thumb` or cached source file exists, decode in background.
            if self.thumb_path(&url).is_file() || self.cached_path(&url).is_some() {
                let is_new = self
                    .seen_decode
                    .lock()
                    .map(|mut set| set.insert(url.clone()))
                    .unwrap_or(false);
                if is_new {
                    if let Ok(mut queue) = self.decode_queue.lock() {
                        queue.push(url);
                        enqueued = true;
                    }
                }
                continue;
            }
            let is_new = self
                .seen_download
                .lock()
                .map(|mut set| set.insert(url.clone()))
                .unwrap_or(false);
            if is_new {
                if let Ok(mut queue) = self.download_queue.lock() {
                    queue.push(url);
                    enqueued = true;
                }
            }
        }
        if !enqueued {
            return;
        }

        self.ensure_workers();
    }

    fn ensure_workers(self: &Arc<Self>) {
        loop {
            let current = self.active_workers.load(Ordering::Acquire);
            if current >= WORKERS {
                break;
            }
            if self
                .active_workers
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let loader = Arc::clone(self);
                if std::thread::Builder::new()
                    .name("skiff-cover".to_string())
                    .spawn(move || loader.work())
                    .is_err()
                {
                    self.active_workers.fetch_sub(1, Ordering::Release);
                    break;
                }
            }
        }
    }

    /// Consume local decode tasks first (fast), then remote download+decode tasks.
    fn work(&self) {
        struct WorkerGuard<'a>(&'a AtomicUsize);
        impl Drop for WorkerGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        let _guard = WorkerGuard(&self.active_workers);

        loop {
            let local_url = self.decode_queue.lock().ok().and_then(|mut q| q.pop());
            if let Some(url) = local_url {
                if self.decode_into_memory(&url) {
                    self.generation.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }

            let remote_url = self.download_queue.lock().ok().and_then(|mut q| q.pop());
            if let Some(url) = remote_url {
                if self.fetch(&url) && self.decode_into_memory(&url) {
                    self.generation.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }

            break;
        }
    }

    fn decode_into_memory(&self, url: &str) -> bool {
        if self.is_decoded(url) {
            return false;
        }
        let thumb_path = self.thumb_path(url);
        if let Some(buf) = read_thumb(&thumb_path) {
            if let Ok(mut map) = self.decoded.lock() {
                map.insert(url.to_string(), buf);
            }
            return true;
        }

        let Some(src_path) = self.cached_path(url) else {
            return false;
        };
        let Ok(image) = Image::load_from_path(&src_path) else {
            return false;
        };
        // `to_rgba8` handles both RGB and RGBA sources (including transparent PNG/WebP), whereas
        // `to_rgb8` returns `None` for any image with an alpha channel.
        let Some(rgba) = image.to_rgba8() else {
            return false;
        };
        let scaled = downscale_rgba8(&rgba, COVER_HEIGHT);
        let _ = write_thumb(&thumb_path, &scaled);
        if let Ok(mut map) = self.decoded.lock() {
            map.insert(url.to_string(), scaled);
        }
        true
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

    fn thumb_path(&self, url: &str) -> PathBuf {
        self.dir.join(format!("{}.thumb", cache_stem(&thumbnail_url(url))))
    }
}

/// Area-averaged RGBA8 downscaler keeping aspect ratio so neither dimension exceeds `max_edge`.
fn downscale_rgba8(
    src: &SharedPixelBuffer<Rgba8Pixel>,
    max_edge: u32,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let sw = src.width();
    let sh = src.height();
    if sw == 0 || sh == 0 || (sw <= max_edge && sh <= max_edge) {
        return src.clone();
    }
    let scale = (max_edge as f32 / sw as f32).min(max_edge as f32 / sh as f32);
    let dw = ((sw as f32 * scale).round() as u32).max(1);
    let dh = ((sh as f32 * scale).round() as u32).max(1);

    let src_pixels = src.as_slice();
    let mut dst = SharedPixelBuffer::<Rgba8Pixel>::new(dw, dh);
    let dst_pixels = dst.make_mut_slice();

    for dy in 0..dh {
        let y0 = (dy * sh) / dh;
        let y1 = (((dy + 1) * sh) / dh).max(y0 + 1).min(sh);
        for dx in 0..dw {
            let x0 = (dx * sw) / dw;
            let x1 = (((dx + 1) * sw) / dw).max(x0 + 1).min(sw);
            let mut r: u64 = 0;
            let mut g: u64 = 0;
            let mut b: u64 = 0;
            let mut a: u64 = 0;
            let mut count: u64 = 0;
            for sy in y0..y1 {
                let row_offset = (sy * sw) as usize;
                for sx in x0..x1 {
                    let px = src_pixels[row_offset + sx as usize];
                    r += u64::from(px.r);
                    g += u64::from(px.g);
                    b += u64::from(px.b);
                    a += u64::from(px.a);
                    count += 1;
                }
            }
            dst_pixels[(dy * dw + dx) as usize] = Rgba8Pixel {
                r: (r / count) as u8,
                g: (g / count) as u8,
                b: (b / count) as u8,
                a: (a / count) as u8,
            };
        }
    }
    dst
}

fn write_thumb(path: &Path, buf: &SharedPixelBuffer<Rgba8Pixel>) -> io::Result<()> {
    let w = u16::try_from(buf.width()).map_err(|_| io::Error::other("width overflow"))?;
    let h = u16::try_from(buf.height()).map_err(|_| io::Error::other("height overflow"))?;
    let raw = buf.as_bytes();
    let mut out = Vec::with_capacity(8 + raw.len());
    out.extend_from_slice(THUMB_MAGIC_RGBA);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(raw);
    util::atomic_write(path, &out).map_err(|e| io::Error::other(e.to_string()))
}

fn read_thumb(path: &Path) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 8 {
        return None;
    }
    let w = u16::from_le_bytes([bytes[4], bytes[5]]) as u32;
    let h = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return None;
    }
    let pixels = (w as usize).checked_mul(h as usize)?;
    if &bytes[0..4] == THUMB_MAGIC_RGBA {
        let expected = pixels.checked_mul(4)?;
        if bytes.len() != 8 + expected {
            return None;
        }
        return Some(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&bytes[8..], w, h));
    }
    if &bytes[0..4] == THUMB_MAGIC_RGB {
        let expected = pixels.checked_mul(3)?;
        if bytes.len() != 8 + expected {
            return None;
        }
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
        for (dst, chunk) in buf.make_mut_slice().iter_mut().zip(bytes[8..].chunks_exact(3)) {
            *dst = Rgba8Pixel { r: chunk[0], g: chunk[1], b: chunk[2], a: 255 };
        }
        return Some(buf);
    }
    None
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

    #[test]
    fn downscale_and_thumb_roundtrip_handles_rgba_images() {
        let mut big = SharedPixelBuffer::<Rgba8Pixel>::new(1200, 1600);
        for px in big.make_mut_slice() {
            *px = Rgba8Pixel { r: 100, g: 150, b: 200, a: 180 };
        }
        let small = downscale_rgba8(&big, 320);
        assert_eq!(small.width(), 240);
        assert_eq!(small.height(), 320);
        assert_eq!(small.as_slice()[0], Rgba8Pixel { r: 100, g: 150, b: 200, a: 180 });

        let path = std::env::temp_dir().join("skiff-cover-thumb-rgba-test.thumb");
        let _ = std::fs::remove_file(&path);
        write_thumb(&path, &small).unwrap();
        let loaded = read_thumb(&path).unwrap();
        assert_eq!(loaded.width(), 240);
        assert_eq!(loaded.height(), 320);
        assert_eq!(loaded.as_slice()[0], Rgba8Pixel { r: 100, g: 150, b: 200, a: 180 });
        let _ = std::fs::remove_file(&path);
    }
}
