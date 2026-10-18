// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/storage/src/cache.rs

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::core::constants::CACHE_FILE_EXTENSION;

/// What `FileCache::adopt` did with a file.
#[derive(Debug, PartialEq, Eq)]
pub enum Adoption {
    /// The file now lives at its cache path (renamed in, or copied if the two
    /// paths are on different filesystems) and its old name is gone.
    Placed,
    /// The cache already held this hash, so nothing was written. The caller's
    /// file is untouched and still the caller's to delete.
    Present,
}

/// Encapsulates the on-disk binary cache (`~/.cache/y4p/<hash>.cache`) used
/// for large payloads that aren't stored inline in SQLite.
pub struct FileCache {
    cache_dir: PathBuf,
}

impl FileCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    pub fn path(&self, hash: &str) -> PathBuf {
        let mut p = self.cache_dir.clone();
        p.push(format!("{}.{}", hash, CACHE_FILE_EXTENSION));
        p
    }

    /// Callers dedupe by hash before reaching here, so an existing file at
    /// this path is already the same content — skip the rewrite.
    pub fn store(&self, hash: &str, data: &[u8]) -> std::io::Result<()> {
        let path = self.path(hash);
        if !path.exists() {
            fs::write(path, data)?;
        }
        Ok(())
    }

    /// Takes over a finished file as the cache entry for `hash`, without
    /// copying its bytes: `rename(2)` moves the inode, so the cost is
    /// independent of the file's size. Callers dedupe by hash first, so an
    /// existing cache file is the same content and is left alone
    /// ([`Adoption::Present`]); it is never overwritten.
    ///
    /// Under the single-daemon writer invariant, concurrent writes are excluded.
    /// Furthermore, because cache keys are content-addressed (SHA3-256), any
    /// potential race on the same hash is idempotent.
    ///
    /// `source` should be on the cache's filesystem (the spool directory is
    /// inside the cache directory for exactly this reason). If `rename` is
    /// refused anyway (`EXDEV`, a cross-mount boundary or permission quirk),
    /// the bytes are copied via `std::fs::copy` (which attempts kernel-space
    /// `copy_file_range` on Linux where supported, falling back gracefully)
    /// and `source` is then unlinked. A partial copy is removed on failure, so
    /// an `Err` never leaves a truncated entry behind. On `Placed` the source
    /// name is therefore always gone.
    pub fn adopt(&self, hash: &str, source: &Path) -> io::Result<Adoption> {
        let target = self.path(hash);
        if target.exists() {
            return Ok(Adoption::Present);
        }
        if fs::rename(source, &target).is_ok() {
            return Ok(Adoption::Placed);
        }
        match fs::copy(source, &target) {
            Ok(_) => {
                let _ = fs::remove_file(source);
                Ok(Adoption::Placed)
            }
            Err(e) => {
                let _ = fs::remove_file(&target);
                Err(e)
            }
        }
    }

    pub fn read(&self, hash: &str) -> Option<Vec<u8>> {
        fs::read(self.path(hash)).ok()
    }

    pub fn remove(&self, hash: &str) {
        let _ = fs::remove_file(self.path(hash));
    }

    pub fn remove_many(&self, hashes: &[String]) {
        for h in hashes {
            self.remove(h);
        }
    }

    /// Wipes and recreates the cache directory. Must never be pointed at the
    /// same path as the SQLite data directory (see `ClipboardDb::wipe`) —
    /// this removes the directory wholesale.
    pub fn clear(&self) -> std::io::Result<()> {
        fs::remove_dir_all(&self.cache_dir)?;
        fs::create_dir_all(&self.cache_dir)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("y4p-cache-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn cache_files_are_named_hash_dot_cache() {
        let cache = FileCache::new(PathBuf::from("/c"));
        assert_eq!(cache.path("abc"), PathBuf::from("/c/abc.cache"));
    }

    #[test]
    fn adopting_a_file_moves_its_inode_into_place() {
        let root = dir("rename");
        let cache = FileCache::new(root.clone());
        let source = root.join("1-1.part");
        fs::write(&source, b"payload").unwrap();
        let ino = fs::metadata(&source).unwrap().ino();

        assert_eq!(cache.adopt("h1", &source).unwrap(), Adoption::Placed);

        assert!(!source.exists(), "the old name is gone");
        assert_eq!(fs::read(cache.path("h1")).unwrap(), b"payload");
        assert_eq!(fs::metadata(cache.path("h1")).unwrap().ino(), ino, "renamed, not copied");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adopting_never_replaces_an_existing_entry() {
        let root = dir("present");
        let cache = FileCache::new(root.clone());
        fs::write(cache.path("h1"), b"existing").unwrap();
        let source = root.join("1-1.part");
        fs::write(&source, b"incoming").unwrap();

        assert_eq!(cache.adopt("h1", &source).unwrap(), Adoption::Present);

        assert_eq!(fs::read(cache.path("h1")).unwrap(), b"existing");
        assert!(source.exists(), "an unadopted file stays with its owner");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adopting_a_missing_file_is_an_error_and_creates_no_entry() {
        let root = dir("missing");
        let cache = FileCache::new(root.clone());
        assert!(cache.adopt("h1", &root.join("nope.part")).is_err());
        assert!(!cache.path("h1").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adopting_across_filesystems_copies_and_still_consumes_the_source() {
        // `/dev/shm` and the temp directory are separate tmpfs mounts on most
        // Linux systems, which makes `rename(2)` fail with `EXDEV`. Where they
        // are the same filesystem this exercises the rename path instead; the
        // contract asserted (entry placed, source gone) holds on both.
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            return;
        }
        let root = dir("xdev");
        let source = shm.join(format!("y4p-cache-xdev-{}.part", std::process::id()));
        fs::write(&source, b"across").unwrap();
        let cache = FileCache::new(root.clone());

        assert_eq!(cache.adopt("h1", &source).unwrap(), Adoption::Placed);

        assert_eq!(fs::read(cache.path("h1")).unwrap(), b"across");
        assert!(!source.exists(), "the source name is gone either way");
        fs::remove_dir_all(root).unwrap();
    }
}
