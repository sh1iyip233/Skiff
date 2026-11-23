//! Free-space queries for an install target volume.
//!
//! The front ends ask "does this drive still have room for the game?" before a download starts, and
//! the install engine uses the same answer as a last-resort guard. Only the Windows backend is
//! implemented (the product itself is Windows-only). Elsewhere the query reports "unsupported" and
//! every caller treats the size as unknown instead of turning it into a download gate.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Free and total bytes of the volume that backs one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeSpace {
    /// Bytes the current user may still write (quota-aware).
    pub free_bytes: u64,
    /// Capacity of the volume.
    pub total_bytes: u64,
}

/// Query the volume that contains `path`.
///
/// `path` does not have to exist yet: the deepest existing ancestor is used, so a not-yet-created
/// install directory still resolves to its drive (or network share). Relative paths are resolved
/// against the current directory. Any error means "unknown", never "no space": callers must not
/// block a download on a failed query.
pub fn volume_space(path: &Path) -> Result<VolumeSpace> {
    let target = existing_ancestor(path)?;
    platform::volume_space(&target)
}

/// Deepest existing ancestor of `path` (or `path` itself when it exists).
fn existing_ancestor(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(Error::integrity("cannot query disk space for an empty path"));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| {
                Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("cannot resolve the working directory: {e}"),
                ))
            })?
            .join(path)
    };
    let mut current = absolute.as_path();
    loop {
        if current.exists() {
            return Ok(current.to_path_buf());
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            // Reached a drive root such as `D:\` that does not exist (or `/` on Unix).
            _ => {
                return Err(Error::integrity(format!(
                    "no existing directory above {} to read free space from",
                    path.display()
                )))
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{Result, VolumeSpace};
    use crate::error::Error;
    use std::path::Path;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_bytes_available: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    pub fn volume_space(dir: &Path) -> Result<VolumeSpace> {
        // Win32 accepts a plain directory path; the long-path prefix used elsewhere is stripped
        // because the oldest supported systems reject it here.
        let text = dir.as_os_str().to_string_lossy().replace('/', "\\");
        let plain = text.strip_prefix(r"\\?\").unwrap_or(&text);
        let wide: Vec<u16> = plain.encode_utf16().chain(std::iter::once(0)).collect();

        let mut free = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free)
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("GetDiskFreeSpaceExW failed for {} (Windows error {code})", dir.display()),
            )));
        }
        Ok(VolumeSpace { free_bytes: free, total_bytes: total })
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Result, VolumeSpace};
    use crate::error::Error;
    use std::path::Path;

    pub fn volume_space(_dir: &Path) -> Result<VolumeSpace> {
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "volume space is only implemented on Windows",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        // The system temp directory is not reliably writable in every environment; keep test
        // artefacts inside the workspace instead.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-tmp");
        let dir = base.join(format!("skiff-disk-{tag}-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolves_the_deepest_existing_ancestor() {
        let dir = temp_dir("ancestor");
        let deep = dir.join("not-created-yet").join("game");
        assert_eq!(existing_ancestor(&deep).unwrap(), dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_path_is_rejected() {
        assert!(volume_space(Path::new("")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn reports_the_volume_of_the_workspace() {
        let dir = temp_dir("volume");
        let space = volume_space(&dir).unwrap();
        assert!(space.total_bytes > 0, "a real volume has a capacity");
        assert!(space.free_bytes <= space.total_bytes, "free space is part of the volume");
        // The estimate must also work before the install directory exists.
        let deep = dir.join("not-created-yet").join("nested");
        assert_eq!(volume_space(&deep).unwrap(), space);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn a_missing_drive_reports_an_error() {
        // `Q:` is not expected on build machines; skip instead of failing on a machine that has it.
        if Path::new("Q:\\").exists() {
            return;
        }
        assert!(volume_space(Path::new("Q:\\Skiff-does-not-exist")).is_err());
    }
}
