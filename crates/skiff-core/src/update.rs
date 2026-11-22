//! Incremental-update planning.
//!
//! An update only needs the chunks whose content actually changed. Chunks are identified by their
//! 128-bit GUID and validated by size, and a file is unchanged when its path, content hash and chunk
//! layout all match the installed build. These are pure helpers so the rules stay testable without a
//! network or a filesystem.

use crate::manifest::{ChunkInfo, FileManifest};
use std::collections::{HashMap, HashSet};

/// Chunk GUIDs that cannot be reused from the installed build (new, or same GUID with a new size).
pub fn changed_chunks(old: &[ChunkInfo], new: &[ChunkInfo]) -> HashSet<[u32; 4]> {
    let mut previous: HashMap<[u32; 4], i64> = HashMap::with_capacity(old.len());
    for chunk in old {
        previous.insert(chunk.guid, chunk.file_size);
    }
    new.iter()
        .filter(|chunk| previous.get(&chunk.guid) != Some(&chunk.file_size))
        .map(|chunk| chunk.guid)
        .collect()
}

/// How many chunks of the new build already exist on disk and can be skipped.
pub fn reusable_chunk_count(old: &[ChunkInfo], new: &[ChunkInfo]) -> usize {
    let mut previous: HashMap<[u32; 4], i64> = HashMap::with_capacity(old.len());
    for chunk in old {
        previous.insert(chunk.guid, chunk.file_size);
    }
    new.iter()
        .filter(|chunk| previous.get(&chunk.guid) == Some(&chunk.file_size))
        .count()
}

/// Files present in the installed build but absent from the new one.
pub fn removed_paths(old: &[String], new: &[String]) -> Vec<String> {
    let keep: HashSet<&str> = new.iter().map(String::as_str).collect();
    let mut out: Vec<String> = old
        .iter()
        .filter(|path| !keep.contains(path.as_str()))
        .cloned()
        .collect();
    out.sort();
    out
}

/// Files whose bytes are identical, so they need neither a rewrite nor a hash check.
///
/// Requires both content hash and chunk layout to match: the same hash with a different layout would
/// still need rewriting because the install sink writes by chunk offset.
pub fn unchanged_files(
    old: &[FileManifest],
    new: &[FileManifest],
    changed: &HashSet<[u32; 4]>,
) -> HashSet<String> {
    let mut previous: HashMap<&str, &FileManifest> = HashMap::with_capacity(old.len());
    for file in old {
        previous.insert(file.path.as_str(), file);
    }
    new.iter()
        .filter(|file| {
            let Some(before) = previous.get(file.path.as_str()) else {
                return false;
            };
            before.hash == file.hash
                && before.size == file.size
                && before.parts == file.parts
                && !file.parts.iter().any(|part| changed.contains(&part.guid))
        })
        .map(|file| file.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(guid: u32, size: i64) -> ChunkInfo {
        ChunkInfo {
            guid: [guid, 0, 0, 0],
            rolling_hash: 0,
            sha1: [0; 20],
            group_num: 0,
            window_size: size.max(0) as u32,
            file_size: size,
            secret_guid: None,
            encryption_tag: None,
        }
    }

    #[test]
    fn detects_new_and_resized_chunks() {
        let old = vec![chunk(1, 100), chunk(2, 100)];
        let new = vec![chunk(1, 100), chunk(2, 200), chunk(3, 100)];
        let changed = changed_chunks(&old, &new);
        assert!(!changed.contains(&[1, 0, 0, 0]), "same guid and size is reusable");
        assert!(changed.contains(&[2, 0, 0, 0]), "resized chunk must be refetched");
        assert!(changed.contains(&[3, 0, 0, 0]), "new chunk must be fetched");
        assert_eq!(reusable_chunk_count(&old, &new), 1);
    }

    #[test]
    fn reports_removed_paths() {
        let old = vec!["a.txt".to_string(), "b.bin".to_string(), "c.dat".to_string()];
        let new = vec!["a.txt".to_string(), "c.dat".to_string()];
        assert_eq!(removed_paths(&old, &new), vec!["b.bin".to_string()]);
    }

    #[test]
    fn identical_files_are_skipped_even_without_change_set() {
        let file = |path: &str, hash: u8| FileManifest {
            path: path.to_string(),
            symlink_target: String::new(),
            hash: [hash; 20],
            flags: 0,
            install_tags: Vec::new(),
            parts: vec![crate::manifest::ChunkPart {
                guid: [1, 0, 0, 0],
                chunk_offset: 0,
                size: 10,
                file_offset: 0,
            }],
            size: 10,
            md5: None,
            sha256: None,
            mime_type: String::new(),
        };
        let old = vec![file("same.dat", 7), file("edit.dat", 1)];
        let new = vec![file("same.dat", 7), file("edit.dat", 2)];
        let changed = changed_chunks(&[chunk(1, 10)], &[chunk(1, 10)]);
        let keep = unchanged_files(&old, &new, &changed);
        assert!(keep.contains("same.dat"));
        assert!(!keep.contains("edit.dat"), "different hash means a rewrite");
    }
}
