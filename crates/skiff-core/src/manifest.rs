//! Epic binary manifest parsing (magic `0x44BEC00C`) with a JSON fallback.
//!
//! Layout, all integers little-endian:
//!
//! ```text
//! header: u32 magic | u32 header_size | u32 size_uncompressed | u32 size_compressed
//!         | [u8;20] sha1(compressed payload -> uncompressed) | u8 stored_as | u32 version
//!         | if version >= 22: [u32;4] secret_guid | [u8;16] gcm_tag
//! body (zlib/brotli compressed when stored_as bit 0):
//!         ManifestMeta | CDL | FML | CustomFields | [EncryptedData]
//! ```
//!
//! Every body section starts with `u32 section_size` and is parsed *columnar*: all values of one
//! field are stored for every element before the next field starts.

use crate::error::{Error, Result};
use std::collections::{BTreeMap, HashMap};

/// Binary manifest magic.
pub const MANIFEST_MAGIC: u32 = 0x44BE_C00C;

/// A single downloadable chunk as described by the manifest chunk data list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    pub guid: [u32; 4],
    pub rolling_hash: u64,
    pub sha1: [u8; 20],
    pub group_num: u8,
    /// Uncompressed size (typically 1 MiB).
    pub window_size: u32,
    /// Compressed size, i.e. the number of bytes fetched from the CDN.
    pub file_size: i64,
    pub secret_guid: Option<[u32; 4]>,
    pub encryption_tag: Option<[u8; 16]>,
}

impl ChunkInfo {
    /// 128-bit numeric GUID used for bookkeeping and hashing.
    pub fn guid_num(&self) -> u128 {
        let g = self.guid;
        ((g[0] as u128) << 96) | ((g[1] as u128) << 64) | ((g[2] as u128) << 32) | (g[3] as u128)
    }

    /// Stable hex formatting: 4 x 8 uppercase digits.
    pub fn guid_hex(&self) -> String {
        format!("{:08X}{:08X}{:08X}{:08X}", self.guid[0], self.guid[1], self.guid[2], self.guid[3])
    }
}

/// One contiguous piece of a file, sourced from a chunk at `chunk_offset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPart {
    pub guid: [u32; 4],
    pub chunk_offset: u32,
    pub size: u32,
    /// Offset inside the target file.
    pub file_offset: u64,
}

/// One file described by the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileManifest {
    pub path: String,
    pub symlink_target: String,
    pub hash: [u8; 20],
    pub flags: u8,
    pub install_tags: Vec<String>,
    pub parts: Vec<ChunkPart>,
    pub size: u64,
    pub md5: Option<[u8; 16]>,
    pub mime_type: String,
    pub sha256: Option<[u8; 32]>,
}

/// Build metadata stored at the head of the manifest body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestMeta {
    pub data_version: u8,
    pub feature_level: u32,
    pub app_name: String,
    pub build_version: String,
    pub launch_exe: String,
    pub launch_command: String,
    pub prereq_ids: Vec<String>,
    pub prereq_name: String,
    pub prereq_path: String,
    pub prereq_args: String,
    pub build_id: String,
    pub uninstall_action_path: String,
    pub uninstall_action_args: String,
}

/// Fully parsed manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Manifest feature level from the header (15..=21 is `ChunksV4`, 22+ is `ChunksV5`).
    pub version: u32,
    pub compressed: bool,
    pub encrypted: bool,
    pub header_sha1: [u8; 20],
    pub meta: ManifestMeta,
    pub chunks: Vec<ChunkInfo>,
    pub files: Vec<FileManifest>,
    pub custom: BTreeMap<String, String>,
    /// Original bytes, preserved so `.egstore/*.manifest` can be written verbatim.
    pub raw: Vec<u8>,
}

impl Manifest {
    /// Parse either binary or JSON manifest bytes.
    pub fn parse(data: &[u8]) -> Result<Manifest> {
        if data.len() >= 4 && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == MANIFEST_MAGIC
        {
            return Manifest::parse_binary(data);
        }
        if data.first() == Some(&b'{') {
            return Manifest::parse_json(data);
        }
        Err(Error::Manifest(format!(
            "unrecognized manifest format (first bytes: {})",
            hex::encode(&data[..data.len().min(8)])
        )))
    }

    /// True when the bytes carry the binary manifest magic.
    pub fn is_binary(data: &[u8]) -> bool {
        data.len() >= 4 && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == MANIFEST_MAGIC
    }

    fn parse_binary(data: &[u8]) -> Result<Manifest> {
        let mut r = Reader::new(data);
        let magic = r.u32()?;
        if magic != MANIFEST_MAGIC {
            return Err(Error::Manifest("manifest magic mismatch".into()));
        }
        let header_size = r.u32()? as usize;
        let size_uncompressed = r.u32()? as usize;
        let _size_compressed = r.u32()? as usize;
        let mut header_sha1 = [0u8; 20];
        header_sha1.copy_from_slice(r.take(20)?);
        let stored_as = r.u8()?;
        let version = r.u32()?;
        let secret_guid = if version >= 22 {
            let mut sg = [0u32; 4];
            for slot in sg.iter_mut() {
                *slot = r.u32()?;
            }
            let mut tag = [0u8; 16];
            tag.copy_from_slice(r.take(16)?);
            Some((sg, tag))
        } else {
            None
        };
        let _ = secret_guid;

        if header_size < r.pos || header_size > data.len() {
            return Err(Error::Manifest(format!(
                "manifest header size {header_size} out of range (read {})",
                r.pos
            )));
        }
        r.seek_to(header_size)?;

        let compressed = stored_as & 0x1 != 0;
        let encrypted = stored_as & 0x2 != 0;
        if encrypted {
            return Err(Error::Manifest(
                "this build uses an encrypted manifest (feature level 22+); \
                 decrypting it requires the manifest secret supplied by Epic's asset API"
                    .into(),
            ));
        }

        let payload = r.rest();
        let body = if compressed {
            let plain = crate::chunk::decompress_unknown(payload)?;
            use sha1::{Digest, Sha1};
            let mut hasher = Sha1::new();
            hasher.update(&plain);
            let actual: [u8; 20] = hasher.finalize().into();
            if actual != header_sha1 {
                return Err(Error::Manifest(format!(
                    "manifest checksum mismatch (expected {}, got {})",
                    hex::encode(header_sha1),
                    hex::encode(actual)
                )));
            }
            if size_uncompressed != 0 && plain.len() != size_uncompressed {
                return Err(Error::Manifest(format!(
                    "manifest size mismatch: header says {size_uncompressed}, decoded {}",
                    plain.len()
                )));
            }
            plain
        } else {
            payload.to_vec()
        };

        let mut body_reader = Reader::new(&body);
        let meta = parse_meta(&mut body_reader)?;
        let chunks = parse_cdl(&mut body_reader, meta.feature_level)?;
        let files = parse_fml(&mut body_reader)?;
        let custom = parse_custom(&mut body_reader)?;

        Ok(Manifest {
            version: if version != 0 { version } else { meta.feature_level },
            compressed,
            encrypted,
            header_sha1,
            meta,
            chunks,
            files,
            custom,
            raw: data.to_vec(),
        })
    }

    fn parse_json(data: &[u8]) -> Result<Manifest> {
        let value: serde_json::Value = serde_json::from_slice(data)
            .map_err(|e| Error::Manifest(format!("invalid JSON manifest: {e}")))?;

        let obj = value
            .as_object()
            .ok_or_else(|| Error::Manifest("JSON manifest is not an object".into()))?;

        let version = obj
            .get("ManifestFileVersion")
            .and_then(|v| v.as_str())
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(18);

        let meta = ManifestMeta {
            feature_level: version,
            app_name: json_str(obj, &["AppNameString", "AppName"]),
            build_version: json_str(obj, &["BuildVersionString", "BuildVersion"]),
            launch_exe: json_str(obj, &["LaunchExeString", "LaunchExecutable"]),
            launch_command: json_str(obj, &["LaunchCommand"]),
            prereq_ids: obj
                .get("PrereqIds")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            ..Default::default()
        };

        let mut chunks = Vec::new();
        if let Some(list) = obj.get("ChunkDataList").and_then(|v| v.get("Chunks")).and_then(|v| v.as_array())
        {
            for entry in list {
                let guid = entry
                    .get("Guid")
                    .and_then(|v| v.as_str())
                    .and_then(parse_guid_hex)
                    .ok_or_else(|| Error::Manifest("chunk without GUID in JSON manifest".into()))?;
                let mut sha1 = [0u8; 20];
                if let Some(s) = entry.get("ShaHash").and_then(|v| v.as_str()) {
                    let bytes = hex::decode(s)
                        .map_err(|_| Error::Manifest("bad ShaHash in JSON manifest".into()))?;
                    if bytes.len() == 20 {
                        sha1.copy_from_slice(&bytes);
                    }
                }
                chunks.push(ChunkInfo {
                    guid,
                    rolling_hash: entry.get("Hash").and_then(|v| v.as_u64()).unwrap_or(0),
                    sha1,
                    group_num: entry.get("GroupNum").and_then(|v| v.as_u64()).unwrap_or(0) as u8,
                    window_size: entry.get("WindowSize").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    file_size: entry.get("FileSize").and_then(|v| v.as_i64()).unwrap_or(0),
                    secret_guid: None,
                    encryption_tag: None,
                });
            }
        }

        let mut files = Vec::new();
        if let Some(list) = obj.get("FileManifestList").and_then(|v| v.as_array()) {
            for entry in list {
                let path = entry
                    .get("Filename")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::Manifest("file without Filename in JSON manifest".into()))?
                    .to_string();
                let mut hash = [0u8; 20];
                if let Some(s) = entry.get("FileHash").and_then(|v| v.as_str()) {
                    if let Ok(bytes) = hex::decode(s) {
                        if bytes.len() == 20 {
                            hash.copy_from_slice(&bytes);
                        }
                    }
                }
                let mut parts = Vec::new();
                let mut offset: u64 = 0;
                if let Some(list) = entry.get("FileChunkParts").and_then(|v| v.as_array()) {
                    for p in list {
                        let guid = p
                            .get("Guid")
                            .and_then(|v| v.as_str())
                            .and_then(parse_guid_hex)
                            .ok_or_else(|| Error::Manifest("chunk part without GUID".into()))?;
                        let size = p.get("Size").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        parts.push(ChunkPart {
                            guid,
                            chunk_offset: p.get("Offset").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                            size,
                            file_offset: offset,
                        });
                        offset += size as u64;
                    }
                }
                files.push(FileManifest {
                    path,
                    symlink_target: String::new(),
                    hash,
                    flags: 0,
                    install_tags: Vec::new(),
                    parts,
                    size: offset,
                    md5: None,
                    mime_type: String::new(),
                    sha256: None,
                });
            }
        }

        let custom = obj
            .get("CustomFields")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Manifest {
            version,
            compressed: false,
            encrypted: false,
            header_sha1: [0u8; 20],
            meta,
            chunks,
            files,
            custom,
            raw: data.to_vec(),
        })
    }

    /// GUID to chunk-index map for resolving file chunk parts.
    pub fn chunk_index(&self) -> HashMap<[u32; 4], usize> {
        let mut map = HashMap::with_capacity(self.chunks.len());
        for (idx, chunk) in self.chunks.iter().enumerate() {
            map.insert(chunk.guid, idx);
        }
        map
    }

    /// Total size of the installed files.
    pub fn install_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Total compressed bytes that have to be fetched from the CDN.
    pub fn download_size(&self) -> u64 {
        self.chunks.iter().map(|c| c.file_size.max(0) as u64).sum()
    }

    /// Serialize back to the binary format (unencrypted, zlib compressed).
    ///
    /// Used by `skiff debug manifest --reserialize` and by round-trip tests.
    pub fn write_binary(&self) -> Result<Vec<u8>> {
        use sha1::{Digest, Sha1};
        if self.version >= 22 {
            return Err(Error::Manifest(
                "serializing feature level 22+ manifests is not supported (encrypted body)".into(),
            ));
        }
        let mut body = Vec::new();
        write_meta(&mut body, &self.meta, self.version);
        write_cdl(&mut body, &self.chunks, self.version);
        write_fml(&mut body, &self.files);
        write_custom(&mut body, &self.custom);

        let mut hasher = Sha1::new();
        hasher.update(&body);
        let sha: [u8; 20] = hasher.finalize().into();

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        encoder
            .write_all(&body)
            .map_err(|e| Error::Manifest(format!("compression failed: {e}")))?;
        let compressed = encoder
            .finish()
            .map_err(|e| Error::Manifest(format!("compression failed: {e}")))?;

        let header_size: u32 = if self.version >= 22 { 73 } else { 41 };
        let mut out = Vec::with_capacity(header_size as usize + compressed.len());
        out.extend_from_slice(&MANIFEST_MAGIC.to_le_bytes());
        out.extend_from_slice(&header_size.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&sha);
        out.push(0x1); // compressed
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&compressed);
        Ok(out)
    }
}

fn json_str(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(s) = v.as_str() {
                return s.to_string();
            }
            if let Some(n) = v.as_u64() {
                return n.to_string();
            }
        }
    }
    String::new()
}

fn parse_guid_hex(text: &str) -> Option<[u32; 4]> {
    let cleaned: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() != 32 {
        return None;
    }
    let bytes = hex::decode(cleaned).ok()?;
    let mut guid = [0u32; 4];
    for (i, slot) in guid.iter_mut().enumerate() {
        *slot = u32::from_le_bytes([bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2], bytes[i * 4 + 3]]);
    }
    Some(guid)
}

// ---------------------------------------------------------------------------------------------
// section parsers
// ---------------------------------------------------------------------------------------------

fn parse_meta(r: &mut Reader<'_>) -> Result<ManifestMeta> {
    let start = r.pos;
    let meta_size = r.u32()? as usize;
    let data_version = r.u8()?;
    let feature_level = r.u32()?;
    let _is_file_data = r.u8()?;
    let _app_id = r.u32()?;

    let mut meta = ManifestMeta {
        data_version,
        feature_level,
        app_name: r.fstring()?,
        build_version: r.fstring()?,
        launch_exe: r.fstring()?,
        launch_command: r.fstring()?,
        ..Default::default()
    };

    let prereq_count = r.u32()? as usize;
    for _ in 0..prereq_count {
        meta.prereq_ids.push(r.fstring()?);
    }
    meta.prereq_name = r.fstring()?;
    meta.prereq_path = r.fstring()?;
    meta.prereq_args = r.fstring()?;
    if data_version >= 1 {
        meta.build_id = r.fstring()?;
    }
    if data_version >= 2 {
        meta.uninstall_action_path = r.fstring()?;
        meta.uninstall_action_args = r.fstring()?;
    }

    r.align_to_section(start, meta_size, "manifest metadata")?;
    Ok(meta)
}

fn parse_cdl(r: &mut Reader<'_>, manifest_version: u32) -> Result<Vec<ChunkInfo>> {
    let start = r.pos;
    let size = r.u32()? as usize;
    let _version = r.u8()?;
    let count = r.u32()? as usize;
    // Each chunk needs at least 4+8+20+1+4+8 bytes, so an absurd count means a corrupt stream.
    if count > 0 && count > (size.saturating_sub(9)) / 45 + 1 {
        return Err(Error::Manifest(format!(
            "chunk list count {count} is impossible for a {size} byte section"
        )));
    }

    let mut chunks = vec![
        ChunkInfo {
            guid: [0; 4],
            rolling_hash: 0,
            sha1: [0; 20],
            group_num: 0,
            window_size: 0,
            file_size: 0,
            secret_guid: None,
            encryption_tag: None,
        };
        count
    ];

    for chunk in chunks.iter_mut() {
        let mut guid = [0u32; 4];
        for slot in guid.iter_mut() {
            *slot = r.u32()?;
        }
        chunk.guid = guid;
    }
    for chunk in chunks.iter_mut() {
        chunk.rolling_hash = r.u64()?;
    }
    for chunk in chunks.iter_mut() {
        let mut sha = [0u8; 20];
        sha.copy_from_slice(r.take(20)?);
        chunk.sha1 = sha;
    }
    for chunk in chunks.iter_mut() {
        chunk.group_num = r.u8()?;
    }
    for chunk in chunks.iter_mut() {
        chunk.window_size = r.u32()?;
    }
    for chunk in chunks.iter_mut() {
        let mut b = [0u8; 8];
        b.copy_from_slice(r.take(8)?);
        chunk.file_size = i64::from_le_bytes(b);
    }
    if manifest_version >= 22 {
        for chunk in chunks.iter_mut() {
            let mut sg = [0u32; 4];
            for slot in sg.iter_mut() {
                *slot = r.u32()?;
            }
            chunk.secret_guid = Some(sg);
        }
        for _ in chunks.iter() {
            let _window_size_compressed = r.u32()?;
        }
        for chunk in chunks.iter_mut() {
            let mut tag = [0u8; 16];
            tag.copy_from_slice(r.take(16)?);
            chunk.encryption_tag = Some(tag);
        }
    }

    r.align_to_section(start, size, "chunk data list")?;
    Ok(chunks)
}

fn parse_fml(r: &mut Reader<'_>) -> Result<Vec<FileManifest>> {
    let start = r.pos;
    let size = r.u32()? as usize;
    let version = r.u8()?;
    let count = r.u32()? as usize;
    if count > 0 && count * 60 > size.saturating_mul(64) {
        return Err(Error::Manifest(format!("implausible file count {count}")));
    }

    let mut files: Vec<FileManifest> = (0..count)
        .map(|_| FileManifest {
            path: String::new(),
            symlink_target: String::new(),
            hash: [0; 20],
            flags: 0,
            install_tags: Vec::new(),
            parts: Vec::new(),
            size: 0,
            md5: None,
            mime_type: String::new(),
            sha256: None,
        })
        .collect();

    for file in files.iter_mut() {
        file.path = r.fstring()?;
    }
    for file in files.iter_mut() {
        file.symlink_target = r.fstring()?;
    }
    for file in files.iter_mut() {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(r.take(20)?);
        file.hash = hash;
    }
    for file in files.iter_mut() {
        file.flags = r.u8()?;
    }
    for file in files.iter_mut() {
        let tag_count = r.u32()? as usize;
        for _ in 0..tag_count {
            file.install_tags.push(r.fstring()?);
        }
    }
    for file in files.iter_mut() {
        let part_count = r.u32()? as usize;
        let mut file_offset: u64 = 0;
        for _ in 0..part_count {
            let part_start = r.pos;
            let part_size = r.u32()? as usize;
            let mut guid = [0u32; 4];
            for slot in guid.iter_mut() {
                *slot = r.u32()?;
            }
            let chunk_offset = r.u32()?;
            let size = r.u32()?;
            file.parts.push(ChunkPart { guid, chunk_offset, size, file_offset });
            file_offset += size as u64;
            let consumed = r.pos - part_start;
            if consumed > part_size {
                return Err(Error::Manifest(format!(
                    "chunk part claims {part_size} bytes but {consumed} were needed"
                )));
            }
            r.skip(part_size - consumed)?;
        }
        file.size = file_offset;
    }

    if version >= 1 {
        for file in files.iter_mut() {
            let has_md5 = r.u32()?;
            if has_md5 != 0 {
                let mut md5 = [0u8; 16];
                md5.copy_from_slice(r.take(16)?);
                file.md5 = Some(md5);
            }
        }
        for file in files.iter_mut() {
            file.mime_type = r.fstring()?;
        }
    }
    if version >= 2 {
        for file in files.iter_mut() {
            let mut sha = [0u8; 32];
            sha.copy_from_slice(r.take(32)?);
            file.sha256 = Some(sha);
        }
    }

    r.align_to_section(start, size, "file manifest list")?;
    Ok(files)
}

fn parse_custom(r: &mut Reader<'_>) -> Result<BTreeMap<String, String>> {
    if r.remaining() < 9 {
        return Ok(BTreeMap::new());
    }
    let start = r.pos;
    let size = r.u32()? as usize;
    let _version = r.u8()?;
    let count = r.u32()? as usize;
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        keys.push(r.fstring()?);
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(r.fstring()?);
    }
    r.align_to_section(start, size, "custom fields")?;
    Ok(keys.into_iter().zip(values).collect())
}

// ---------------------------------------------------------------------------------------------
// section writers (used by write_binary and tests)
// ---------------------------------------------------------------------------------------------

fn write_fstring(out: &mut Vec<u8>, text: &str) {
    if text.is_empty() {
        out.extend_from_slice(&0i32.to_le_bytes());
        return;
    }
    if text.is_ascii() {
        out.extend_from_slice(&((text.len() as i32) + 1).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    } else {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        out.extend_from_slice(&(-((utf16.len() as i32) + 1)).to_le_bytes());
        for unit in utf16 {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
    }
}

fn write_meta(out: &mut Vec<u8>, meta: &ManifestMeta, feature_level: u32) {
    let start = out.len();
    out.extend_from_slice(&0u32.to_le_bytes()); // size placeholder
    out.push(0); // data_version 0 keeps build_id/uninstall fields out
    out.extend_from_slice(&feature_level.to_le_bytes());
    out.push(0); // is_file_data
    out.extend_from_slice(&0u32.to_le_bytes()); // app_id
    write_fstring(out, &meta.app_name);
    write_fstring(out, &meta.build_version);
    write_fstring(out, &meta.launch_exe);
    write_fstring(out, &meta.launch_command);
    out.extend_from_slice(&(meta.prereq_ids.len() as u32).to_le_bytes());
    for id in &meta.prereq_ids {
        write_fstring(out, id);
    }
    write_fstring(out, &meta.prereq_name);
    write_fstring(out, &meta.prereq_path);
    write_fstring(out, &meta.prereq_args);
    let end = out.len();
    out[start..start + 4].copy_from_slice(&((end - start) as u32).to_le_bytes());
}

fn write_cdl(out: &mut Vec<u8>, chunks: &[ChunkInfo], manifest_version: u32) {
    let start = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0); // cdl version
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for chunk in chunks {
        for part in chunk.guid {
            out.extend_from_slice(&part.to_le_bytes());
        }
    }
    for chunk in chunks {
        out.extend_from_slice(&chunk.rolling_hash.to_le_bytes());
    }
    for chunk in chunks {
        out.extend_from_slice(&chunk.sha1);
    }
    for chunk in chunks {
        out.push(chunk.group_num);
    }
    for chunk in chunks {
        out.extend_from_slice(&chunk.window_size.to_le_bytes());
    }
    for chunk in chunks {
        out.extend_from_slice(&chunk.file_size.to_le_bytes());
    }
    if manifest_version >= 22 {
        for chunk in chunks {
            let sg = chunk.secret_guid.unwrap_or([0; 4]);
            for part in sg {
                out.extend_from_slice(&part.to_le_bytes());
            }
        }
        for _ in chunks {
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        for chunk in chunks {
            out.extend_from_slice(&chunk.encryption_tag.unwrap_or([0; 16]));
        }
    }
    let end = out.len();
    out[start..start + 4].copy_from_slice(&((end - start) as u32).to_le_bytes());
}

fn write_fml(out: &mut Vec<u8>, files: &[FileManifest]) {
    let start = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0); // fml version 0: no md5/mime/sha256 sections
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for file in files {
        write_fstring(out, &file.path);
    }
    for file in files {
        write_fstring(out, &file.symlink_target);
    }
    for file in files {
        out.extend_from_slice(&file.hash);
    }
    for file in files {
        out.push(file.flags);
    }
    for file in files {
        out.extend_from_slice(&(file.install_tags.len() as u32).to_le_bytes());
        for tag in &file.install_tags {
            write_fstring(out, tag);
        }
    }
    for file in files {
        out.extend_from_slice(&(file.parts.len() as u32).to_le_bytes());
        for part in &file.parts {
            out.extend_from_slice(&28u32.to_le_bytes());
            for slot in part.guid {
                out.extend_from_slice(&slot.to_le_bytes());
            }
            out.extend_from_slice(&part.chunk_offset.to_le_bytes());
            out.extend_from_slice(&part.size.to_le_bytes());
        }
    }
    let end = out.len();
    out[start..start + 4].copy_from_slice(&((end - start) as u32).to_le_bytes());
}

fn write_custom(out: &mut Vec<u8>, custom: &BTreeMap<String, String>) {
    let start = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&(custom.len() as u32).to_le_bytes());
    for key in custom.keys() {
        write_fstring(out, key);
    }
    for value in custom.values() {
        write_fstring(out, value);
    }
    let end = out.len();
    out[start..start + 4].copy_from_slice(&((end - start) as u32).to_le_bytes());
}

// ---------------------------------------------------------------------------------------------
// byte reader
// ---------------------------------------------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Manifest(format!(
                "manifest truncated: needed {n} bytes at offset {} but only {} remain",
                self.pos,
                self.remaining()
            )));
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Manifest(format!(
                "manifest truncated while skipping {n} bytes at offset {}",
                self.pos
            )));
        }
        self.pos += n;
        Ok(())
    }

    fn seek_to(&mut self, target: usize) -> Result<()> {
        if target > self.buf.len() {
            return Err(Error::Manifest(format!(
                "manifest seek to {target} beyond end ({})",
                self.buf.len()
            )));
        }
        self.pos = target;
        Ok(())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }

    /// Epic `FString`: positive length = ASCII + NUL, negative = UTF-16 + 2-byte NUL, 0 = empty.
    fn fstring(&mut self) -> Result<String> {
        let len = self.u32()? as i32;
        if len == 0 {
            return Ok(String::new());
        }
        if len > 0 {
            // Length includes the NUL terminator.
            let total = len as usize;
            let bytes = self.take(total)?;
            let text = &bytes[..total.saturating_sub(1)];
            return Ok(String::from_utf8_lossy(text).to_string());
        }
        let char_count = (-len) as usize;
        let byte_len = char_count.saturating_mul(2).saturating_sub(2);
        let bytes = self.take(byte_len)?;
        let mut units = Vec::with_capacity(byte_len / 2);
        for pair in bytes.chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        self.skip(2)?;
        Ok(String::from_utf16_lossy(&units))
    }

    /// Consume up to `start + size` for a columnar section.
    fn align_to_section(&mut self, start: usize, size: usize, what: &str) -> Result<()> {
        let section_end = start
            .checked_add(size)
            .ok_or_else(|| Error::Manifest(format!("{what} section size overflows")))?;
        if self.pos > section_end {
            return Err(Error::Manifest(format!(
                "{what} section over-read: consumed {} of {size} bytes",
                self.pos - start
            )));
        }
        let skip = section_end - self.pos;
        if skip > 0 {
            self.skip(skip)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> Manifest {
        let chunks = vec![
            ChunkInfo {
                guid: [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444],
                rolling_hash: 0xDEAD_BEEF_CAFE_1234,
                sha1: [0xAB; 20],
                group_num: 7,
                window_size: 1024 * 1024,
                file_size: 900_000,
                secret_guid: None,
                encryption_tag: None,
            },
            ChunkInfo {
                guid: [0x5555_5555, 0x6666_6666, 0x7777_7777, 0x8888_8888],
                rolling_hash: 0x1234_5678_9ABC_DEF0,
                sha1: [0xCD; 20],
                group_num: 54,
                window_size: 1024 * 1024,
                file_size: 100_000,
                secret_guid: None,
                encryption_tag: None,
            },
        ];
        let files = vec![
            FileManifest {
                path: "Binaries/Win64/Game.exe".to_string(),
                symlink_target: String::new(),
                hash: [0x01; 20],
                flags: 1,
                install_tags: vec![],
                parts: vec![
                    ChunkPart {
                        guid: chunks[0].guid,
                        chunk_offset: 0,
                        size: 600_000,
                        file_offset: 0,
                    },
                    ChunkPart {
                        guid: chunks[1].guid,
                        chunk_offset: 1000,
                        size: 50_000,
                        file_offset: 600_000,
                    },
                ],
                size: 650_000,
                md5: None,
                mime_type: String::new(),
                sha256: None,
            },
            FileManifest {
                path: "readme.txt".to_string(),
                symlink_target: String::new(),
                hash: [0x02; 20],
                flags: 0,
                install_tags: vec![],
                parts: vec![ChunkPart {
                    guid: chunks[1].guid,
                    chunk_offset: 0,
                    size: 4096,
                    file_offset: 0,
                }],
                size: 4096,
                md5: None,
                mime_type: String::new(),
                sha256: None,
            },
        ];

        let mut custom = BTreeMap::new();
        custom.insert("CloudDir".to_string(), "CloudDir".to_string());
        custom.insert("FolderName".to_string(), "MyGame".to_string());

        Manifest {
            version: 18,
            compressed: true,
            encrypted: false,
            header_sha1: [0; 20],
            meta: ManifestMeta {
                data_version: 0,
                feature_level: 18,
                app_name: "MyGame".to_string(),
                build_version: "++MyGame+Release-1.0-Windows".to_string(),
                launch_exe: "Binaries/Win64/Game.exe".to_string(),
                launch_command: "-epicapp=MyGame".to_string(),
                prereq_ids: vec!["VCRedist".to_string()],
                prereq_name: "VCRedist".to_string(),
                prereq_path: "prereq".to_string(),
                prereq_args: "/q".to_string(),
                ..Default::default()
            },
            chunks,
            files,
            custom,
            raw: Vec::new(),
        }
    }

    #[test]
    fn binary_roundtrip_preserves_everything() {
        let original = sample_manifest();
        let bytes = original.write_binary().unwrap();
        assert!(Manifest::is_binary(&bytes));
        let parsed = Manifest::parse(&bytes).unwrap();

        assert_eq!(parsed.version, original.version);
        assert_eq!(parsed.meta.app_name, "MyGame");
        assert_eq!(parsed.meta.launch_exe, "Binaries/Win64/Game.exe");
        assert_eq!(parsed.chunks.len(), 2);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.chunks, original.chunks);
        assert_eq!(parsed.files, original.files);
        assert_eq!(parsed.custom, original.custom);
        assert_eq!(parsed.install_size(), 650_000 + 4096);
        assert_eq!(parsed.download_size(), 1_000_000);
    }

    #[test]
    fn chunk_index_resolves_parts() {
        let parsed = Manifest::parse(&sample_manifest().write_binary().unwrap()).unwrap();
        let index = parsed.chunk_index();
        assert_eq!(index.len(), 2);
        for file in &parsed.files {
            for part in &file.parts {
                assert!(index.contains_key(&part.guid));
            }
        }
    }

    #[test]
    fn corrupt_payload_is_rejected() {
        let mut bytes = sample_manifest().write_binary().unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(Manifest::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_format() {
        assert!(Manifest::parse(b"not a manifest").is_err());
        assert!(Manifest::parse(&[0x1C, 0x00, 0x00, 0x00, 0x01]).is_err());
    }

    #[test]
    fn parses_json_manifest() {
        let json = br#"{
            "ManifestFileVersion": "16.0.0",
            "AppNameString": "JsonGame",
            "BuildVersionString": "++JsonGame+1-Windows",
            "LaunchExeString": "Game.exe",
            "LaunchCommand": "",
            "PrereqIds": [],
            "ChunkDataList": { "Chunks": [
                { "Guid": "11111111222222223333333344444444", "Hash": 1234,
                  "ShaHash": "abababababababababababababababababababab",
                  "GroupNum": 3, "WindowSize": 1048576, "FileSize": 5000 }
            ]},
            "FileManifestList": [
                { "Filename": "Game.exe",
                  "FileHash": "0101010101010101010101010101010101010101",
                  "FileChunkParts": [
                    { "Guid": "11111111222222223333333344444444", "Offset": 0, "Size": 4096 }
                  ]}
            ],
            "CustomFields": { "CloudDir": "CloudDir" }
        }"#;
        let parsed = Manifest::parse(json).unwrap();
        assert_eq!(parsed.meta.app_name, "JsonGame");
        assert_eq!(parsed.version, 16);
        assert_eq!(parsed.chunks.len(), 1);
        assert_eq!(parsed.chunks[0].guid, [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444]);
        assert_eq!(parsed.files[0].parts[0].size, 4096);
        assert_eq!(parsed.custom.get("CloudDir").map(String::as_str), Some("CloudDir"));
    }

    #[test]
    fn utf16_strings_survive_roundtrip() {
        let mut manifest = sample_manifest();
        manifest.meta.app_name = "游戏名称".to_string();
        let parsed = Manifest::parse(&manifest.write_binary().unwrap()).unwrap();
        assert_eq!(parsed.meta.app_name, "游戏名称");
    }
}
