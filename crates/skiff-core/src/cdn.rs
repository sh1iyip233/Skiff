//! CDN URL construction and host rotation.
//!
//! Chunk paths are relative to the CDN base URL that Epic returns in the build asset payload;
//! the full object URL is `{base}/{chunk.path}`.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Directory name for a manifest feature level.
pub fn chunk_dir(manifest_version: u32) -> &'static str {
    match manifest_version {
        22.. => "ChunksV5",
        15..=21 => "ChunksV4",
        6..=14 => "ChunksV3",
        3..=5 => "ChunksV2",
        _ => "Chunks",
    }
}

/// Relative chunk object path exactly as Epic's CDN expects it.
///
/// Pre-v22 layout: `ChunksV4/{group:02}/{rolling_hash:016X}_{guid as 4 x 08X}.chunk`
/// v22+ layout:    `ChunksV5/{secret|plain}/{group:02}/{b64(hash)}_{b64(guid)}.chunk`
pub fn chunk_path(
    manifest_version: u32,
    group_num: u8,
    rolling_hash: u64,
    guid: &[u32; 4],
    secret_guid: Option<&[u32; 4]>,
) -> String {
    let dir = chunk_dir(manifest_version);
    if manifest_version >= 22 {
        let secret_part = match secret_guid {
            Some(sg) if *sg != [0, 0, 0, 0] => base64_url(&flatten(sg)),
            _ => "plain".to_string(),
        };
        format!(
            "{dir}/{secret_part}/{group_num:02}/{}_{}.chunk",
            base64_url(&rolling_hash.to_le_bytes()),
            base64_url(&flatten(guid))
        )
    } else {
        format!(
            "{dir}/{group_num:02}/{rolling_hash:016X}_{:08X}{:08X}{:08X}{:08X}.chunk",
            guid[0], guid[1], guid[2], guid[3]
        )
    }
}

/// Absolute CDN URL for a chunk path.
pub fn chunk_url(base_url: &str, chunk_path: &str) -> String {
    format!("{}/{chunk_path}", base_url.trim_end_matches('/'))
}

/// Derive the CDN base URL from a manifest URL (everything before the final path segment).
pub fn base_from_manifest_uri(uri: &str) -> String {
    match uri.rfind('/') {
        Some(idx) if idx > 8 => uri[..idx].to_string(),
        _ => uri.trim_end_matches('/').to_string(),
    }
}

/// Round-robin CDN host selector.
///
/// Rotating hosts spreads load across the several Akamai/Fastly properties Epic lists per build,
/// which measurably raises aggregate throughput compared to pinning one host.
#[derive(Debug)]
pub struct CdnHosts {
    bases: Vec<String>,
    cursor: AtomicUsize,
}

impl CdnHosts {
    /// Build a selector; duplicate and empty entries are dropped.
    pub fn new(bases: Vec<String>) -> Self {
        let mut unique: Vec<String> = Vec::new();
        for b in bases {
            let trimmed = b.trim_end_matches('/').to_string();
            if !trimmed.is_empty() && !unique.contains(&trimmed) {
                unique.push(trimmed);
            }
        }
        CdnHosts { bases: unique, cursor: AtomicUsize::new(0) }
    }

    /// All distinct base URLs.
    pub fn bases(&self) -> &[String] {
        &self.bases
    }

    /// Number of hosts.
    pub fn len(&self) -> usize {
        self.bases.len()
    }

    /// True when no host is configured.
    pub fn is_empty(&self) -> bool {
        self.bases.is_empty()
    }

    /// Next base URL in rotation.
    pub fn next(&self) -> Option<&str> {
        if self.bases.is_empty() {
            return None;
        }
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.bases.len();
        Some(self.bases[idx].as_str())
    }

    /// Host at a fixed index (used for per-host accounting).
    pub fn at(&self, index: usize) -> Option<&str> {
        self.bases.get(index % self.bases.len().max(1)).map(|s| s.as_str())
    }
}

fn flatten(guid: &[u32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, part) in guid.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&part.to_le_bytes());
    }
    out
}

/// Unpadded url-safe base64 (std only).
pub fn base64_url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_dir_by_version() {
        assert_eq!(chunk_dir(12), "ChunksV3");
        assert_eq!(chunk_dir(15), "ChunksV4");
        assert_eq!(chunk_dir(18), "ChunksV4");
        assert_eq!(chunk_dir(22), "ChunksV5");
        assert_eq!(chunk_dir(24), "ChunksV5");
    }

    /// The expected values below are real URLs observed on Epic's CDN.
    #[test]
    fn chunk_path_matches_observed_urls() {
        // https://.../Builds/UnrealEngineLauncher/CloudDir/ChunksV4/07/18A8FA9DE97AA86A_88AE8A834F3D1500665340A681107E3A.chunk
        let guid = [0x88AE_8A83, 0x4F3D_1500, 0x6653_40A6, 0x8110_7E3A];
        let path = chunk_path(18, 7, 0x18A8_FA9D_E97A_A86A, &guid, None);
        assert_eq!(
            path,
            "ChunksV4/07/18A8FA9DE97AA86A_88AE8A834F3D1500665340A681107E3A.chunk"
        );

        // https://.../ChunksV4/54/7D74B72856986E1D_DF5361F4431D856949929582C9D0CE29.chunk
        let guid2 = [0xDF53_61F4, 0x431D_8569, 0x4992_9582, 0xC9D0_CE29];
        let path2 = chunk_path(18, 54, 0x7D74_B728_5698_6E1D, &guid2, None);
        assert_eq!(
            path2,
            "ChunksV4/54/7D74B72856986E1D_DF5361F4431D856949929582C9D0CE29.chunk"
        );
    }

    #[test]
    fn v22_path_uses_base64_segments() {
        let path = chunk_path(22, 3, 0x0102_0304_0506_0708, &[1, 2, 3, 4], None);
        assert!(path.starts_with("ChunksV5/plain/03/"));
        assert!(path.ends_with(".chunk"));
        assert!(!path.contains('='));
    }

    #[test]
    fn builds_absolute_url() {
        let url = chunk_url("https://cdn.example/Builds/App/CloudDir/", "ChunksV4/07/a_b.chunk");
        assert_eq!(url, "https://cdn.example/Builds/App/CloudDir/ChunksV4/07/a_b.chunk");
    }

    #[test]
    fn derives_base_from_manifest_uri() {
        let base = base_from_manifest_uri("https://cdn.example/Builds/App/CloudDir/manifest.manifest");
        assert_eq!(base, "https://cdn.example/Builds/App/CloudDir");
    }

    #[test]
    fn rotates_hosts_deterministically() {
        let hosts = CdnHosts::new(vec![
            "https://a.example/base/".into(),
            "https://b.example/base".into(),
            "https://a.example/base".into(),
        ]);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts.next(), Some("https://a.example/base"));
        assert_eq!(hosts.next(), Some("https://b.example/base"));
        assert_eq!(hosts.next(), Some("https://a.example/base"));
    }

    #[test]
    fn base64_url_is_unpadded() {
        assert_eq!(base64_url(b""), "");
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foo"), "Zm9v");
    }
}
