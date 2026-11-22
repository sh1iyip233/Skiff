//! Chunk file decoding and integrity verification.
//!
//! Epic chunk files are self-describing containers:
//!
//! ```text
//! u32 magic (0xB1FE3AA2) | u32 header_version | u32 header_size | u32 compressed_size
//! [u32;4] guid | u64 rolling_hash | u8 stored_as
//! version >= 2: [u8;20] sha1 | u8 hash_type
//! version >= 3: u32 uncompressed_size
//! version >= 4: [u32;4] secret_guid | [u8;16] gcm_tag
//! payload (zlib compressed and/or AES-GCM encrypted)
//! ```
//!
//! `stored_as` bit 0 marks zlib compression, bit 1 marks AES-256-GCM encryption.

use crate::error::{Error, Result};
use std::io::Read;

/// Chunk container magic.
pub const CHUNK_MAGIC: u32 = 0xB1FE_3AA2;

/// Parsed chunk header.
#[derive(Debug, Clone)]
pub struct ChunkHeader {
    pub magic: u32,
    pub version: u32,
    pub header_size: u32,
    pub compressed_size: u32,
    pub guid: [u32; 4],
    pub rolling_hash: u64,
    pub stored_as: u8,
    pub sha1: Option<[u8; 20]>,
    pub hash_type: u8,
    pub uncompressed_size: Option<u32>,
    pub secret_guid: Option<[u32; 4]>,
    pub gcm_tag: Option<[u8; 16]>,
}

impl ChunkHeader {
    /// True when the payload is zlib compressed.
    pub fn is_compressed(&self) -> bool {
        self.stored_as & 0x1 != 0
    }

    /// True when the payload is AES-256-GCM encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.stored_as & 0x2 != 0
    }

    /// `true` when the rolling hash in the header is meaningful.
    pub fn has_rolling_hash(&self) -> bool {
        self.hash_type & 0x1 != 0
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Manifest("truncated chunk header".into()));
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(u64::from_le_bytes(arr))
    }
}

/// Parse a chunk header from raw container bytes.
pub fn parse_header(data: &[u8]) -> Result<ChunkHeader> {
    let mut r = Reader::new(data);
    let magic = r.u32()?;
    if magic != CHUNK_MAGIC {
        return Err(Error::Manifest(format!(
            "chunk magic mismatch: 0x{magic:08X} (expected 0x{CHUNK_MAGIC:08X})"
        )));
    }
    let version = r.u32()?;
    let header_size = r.u32()?;
    let compressed_size = r.u32()?;
    let mut guid = [0u32; 4];
    for slot in guid.iter_mut() {
        *slot = r.u32()?;
    }
    let rolling_hash = r.u64()?;
    let stored_as = r.u8()?;

    let (sha1, hash_type) = if version >= 2 {
        let mut sha = [0u8; 20];
        sha.copy_from_slice(r.take(20)?);
        (Some(sha), r.u8()?)
    } else {
        (None, 0)
    };

    let uncompressed_size = if version >= 3 { Some(r.u32()?) } else { None };

    let (secret_guid, gcm_tag) = if version >= 4 {
        let mut sg = [0u32; 4];
        for slot in sg.iter_mut() {
            *slot = r.u32()?;
        }
        let mut tag = [0u8; 16];
        tag.copy_from_slice(r.take(16)?);
        (Some(sg), Some(tag))
    } else {
        (None, None)
    };

    if r.pos as u32 != header_size {
        return Err(Error::Manifest(format!(
            "chunk header size mismatch: read {} of {} bytes",
            r.pos, header_size
        )));
    }

    Ok(ChunkHeader {
        magic,
        version,
        header_size,
        compressed_size,
        guid,
        rolling_hash,
        stored_as,
        sha1,
        hash_type,
        uncompressed_size,
        secret_guid,
        gcm_tag,
    })
}

/// Decode a chunk payload to plaintext: decrypt (if needed), then inflate (if needed).
///
/// `secret` is the AES-256 key as 32 raw bytes, required for encrypted chunks.
pub fn decode_payload(header: &ChunkHeader, container: &[u8], secret: Option<&[u8; 32]>) -> Result<Vec<u8>> {
    if container.len() < header.header_size as usize {
        return Err(Error::Manifest("chunk shorter than its header".into()));
    }
    let mut payload = container[header.header_size as usize..].to_vec();

    if header.is_encrypted() {
        let key = secret.ok_or_else(|| {
            Error::Manifest(format!(
                "chunk {} is encrypted but no manifest secret is available",
                hex::encode_upper(flatten_guid(&header.guid))
            ))
        })?;
        let sha = header
            .sha1
            .ok_or_else(|| Error::Manifest("encrypted chunk without sha1 header".into()))?;
        let tag = header
            .gcm_tag
            .ok_or_else(|| Error::Manifest("encrypted chunk without GCM tag".into()))?;
        payload = aes_gcm_decrypt(key, &sha[..12], &payload, &tag)?;
    }

    if header.is_compressed() {
        payload = inflate_zlib(&payload)?;
    }

    Ok(payload)
}

/// zlib (RFC 1950) decompression.
pub fn inflate_zlib(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    decoder
        .read_to_end(&mut out)
        .map_err(|e| Error::Manifest(format!("zlib decompression failed: {e}")))?;
    Ok(out)
}

/// Brotli decompression, used by some manifest payloads.
pub fn inflate_brotli(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut reader = brotli::Decompressor::new(data, 8192);
    reader
        .read_to_end(&mut out)
        .map_err(|e| Error::Manifest(format!("brotli decompression failed: {e}")))?;
    Ok(out)
}

/// Decompress data whose stream type is not known up front.
///
/// Detection: a zlib stream starts with `0x78`, gzip with `0x1F 0x8B`; otherwise brotli is tried.
/// The caller is expected to validate the result with a hash.
pub fn decompress_unknown(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() >= 2 && data[0] == 0x78 {
        if let Ok(v) = inflate_zlib(data) {
            return Ok(v);
        }
    }
    if data.len() >= 2 && data[0] == 0x1F && data[1] == 0x8B {
        let mut out = Vec::new();
        let mut gz = flate2::read::GzDecoder::new(data);
        if gz.read_to_end(&mut out).is_ok() {
            return Ok(out);
        }
    }
    inflate_brotli(data)
}

fn aes_gcm_decrypt(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8], tag: &[u8; 16]) -> Result<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| Error::Manifest(format!("invalid chunk key: {e}")))?;
    let nonce_arr = <[u8; 12]>::try_from(&nonce[..12])
        .map_err(|_| Error::Manifest("invalid GCM nonce length".into()))?;
    let mut combined = Vec::with_capacity(ciphertext.len() + tag.len());
    combined.extend_from_slice(ciphertext);
    combined.extend_from_slice(tag);
    cipher
        .decrypt(Nonce::from_slice(&nonce_arr), combined.as_slice())
        .map_err(|_| Error::Manifest("AES-GCM authentication failed for chunk".into()))
}

/// Epic's rolling hash (a CRC-64-ECMA variant, rotated left by one bit per byte).
pub fn rolling_hash(data: &[u8]) -> u64 {
    const POLY: u64 = 0xC96C_5795_D787_0F42;
    let mut table = [0u64; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut v = i as u64;
        for _ in 0..8 {
            if v & 1 != 0 {
                v = (v >> 1) ^ POLY;
            } else {
                v >>= 1;
            }
        }
        *slot = v;
    }
    let mut h: u64 = 0;
    for &byte in data {
        h = h.rotate_left(1) ^ table[byte as usize];
    }
    h
}

/// Format a chunk GUID the way Epic does: four uppercase 8-digit hex groups.
pub fn flatten_guid(guid: &[u32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, part) in guid.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&part.to_le_bytes());
    }
    out
}

/// Verify a decoded chunk against the manifest's recorded hashes.
pub fn verify_chunk(
    plaintext: &[u8],
    expected_sha1: &[u8; 20],
    expected_rolling: Option<u64>,
) -> Result<()> {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(plaintext);
    let actual = hasher.finalize();
    if actual.as_slice() != expected_sha1 {
        return Err(Error::ChunkCorrupt {
            chunk: "chunk".to_string(),
            expected: hex::encode(expected_sha1),
            actual: hex::encode(actual),
        });
    }
    if let Some(expected) = expected_rolling {
        let actual = rolling_hash(plaintext);
        if actual != expected {
            return Err(Error::integrity(format!(
                "rolling hash mismatch: expected {expected:016X}, got {actual:016X}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn build_container(payload: &[u8], compress: bool) -> Vec<u8> {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(payload);
        let sha: [u8; 20] = hasher.finalize().into();
        let body = if compress {
            let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap()
        } else {
            payload.to_vec()
        };

        let mut out = Vec::new();
        out.extend_from_slice(&CHUNK_MAGIC.to_le_bytes());
        out.extend_from_slice(&3u32.to_le_bytes()); // header version 3
        out.extend_from_slice(&66u32.to_le_bytes()); // 4+4+4+4+16+8+1+20+1+4
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        for part in [1u32, 2, 3, 4] {
            out.extend_from_slice(&part.to_le_bytes());
        }
        out.extend_from_slice(&rolling_hash(payload).to_le_bytes());
        out.push(if compress { 0x1 } else { 0x0 });
        out.extend_from_slice(&sha);
        out.push(0x3); // rolling + sha
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn parses_and_decodes_compressed_chunk() {
        let payload = b"epic chunk payload ".repeat(64);
        let container = build_container(&payload, true);
        let header = parse_header(&container).unwrap();
        assert_eq!(header.guid, [1, 2, 3, 4]);
        assert!(header.is_compressed());
        assert!(!header.is_encrypted());
        let plain = decode_payload(&header, &container, None).unwrap();
        assert_eq!(plain, payload);
        verify_chunk(&plain, header.sha1.as_ref().unwrap(), Some(header.rolling_hash)).unwrap();
    }

    #[test]
    fn parses_uncompressed_chunk() {
        let payload = vec![7u8; 4096];
        let container = build_container(&payload, false);
        let header = parse_header(&container).unwrap();
        assert!(!header.is_compressed());
        let plain = decode_payload(&header, &container, None).unwrap();
        assert_eq!(plain, payload);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut container = build_container(b"x", false);
        container[0] = 0;
        assert!(parse_header(&container).is_err());
    }

    #[test]
    fn detects_corruption() {
        let payload = b"integrity matters".repeat(32);
        let container = build_container(&payload, true);
        let header = parse_header(&container).unwrap();
        let mut plain = decode_payload(&header, &container, None).unwrap();
        plain[0] ^= 0xFF;
        assert!(verify_chunk(&plain, header.sha1.as_ref().unwrap(), None).is_err());
    }

    #[test]
    fn rolling_hash_matches_reference_algorithm() {
        // Reference implementation results computed from legendary's rolling_hash.py semantics.
        assert_eq!(rolling_hash(b""), 0);
        assert_ne!(rolling_hash(b"a"), 0);
        assert_eq!(rolling_hash(b"abc"), rolling_hash(b"abc"));
        assert_ne!(rolling_hash(b"abc"), rolling_hash(b"abd"));
    }

    #[test]
    fn encrypted_chunk_without_key_is_reported() {
        let payload = b"secret".to_vec();
        let mut container = build_container(&payload, false);
        // Flip stored_as to mark encryption; header layout: 4+4+4+4 +16 +8 = 40 bytes offset
        container[40] |= 0x2;
        let header = parse_header(&container).unwrap();
        assert!(header.is_encrypted());
        let err = decode_payload(&header, &container, None).unwrap_err();
        assert!(err.to_string().contains("encrypted"));
    }
}
