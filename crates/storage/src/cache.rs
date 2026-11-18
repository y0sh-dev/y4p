// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/storage/src/cache.rs

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::core::constants::CACHE_FILE_EXTENSION;

/// What `FileCache::store` did with a payload.
#[derive(Debug, PartialEq, Eq)]
pub enum Adoption {
    /// The file now lives at its cache path.
    Placed,
    /// The cache already held this hash, so nothing was written.
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

    /// Writes `data` to `<hash>.cache`, reporting whether it had to.
    ///
    /// Callers dedupe by hash before reaching here, so an existing file at
    /// this path is already the same content and is left alone
    /// ([`Adoption::Present`]); a fresh write reports [`Adoption::Placed`] so
    /// the caller can take it back out if the matching row then fails to
    /// commit. A failed write removes whatever it had created, so an `Err`
    /// never leaves a truncated entry that a later call would mistake for
    /// the real content.
    pub fn store(&self, hash: &str, data: &[u8]) -> io::Result<Adoption> {
        let path = self.path(hash);
        if path.exists() {
            return Ok(Adoption::Present);
        }
        match fs::write(&path, data) {
            Ok(()) => Ok(Adoption::Placed),
            Err(e) => {
                let _ = fs::remove_file(&path);
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

    /// Total size of the cached payload files (`*.cache`) currently on disk.
    ///
    /// Only regular files directly inside the cache directory are counted.
    /// A missing or unreadable directory, and an entry whose metadata cannot
    /// be read, contribute zero rather than failing the caller: the quota is a
    /// best-effort disk bound, never a reason to refuse a clipboard write.
    pub fn total_bytes(&self) -> u64 {
        let Ok(entries) = fs::read_dir(&self.cache_dir) else { return 0 };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == CACHE_FILE_EXTENSION))
            .filter_map(|entry| entry.metadata().ok())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .sum()
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
    fn storing_a_payload_reports_placed_once_and_present_afterwards() {
        let root = dir("store");
        let cache = FileCache::new(root.clone());

        assert_eq!(cache.store("h1", b"payload").unwrap(), Adoption::Placed);
        assert_eq!(fs::read(cache.path("h1")).unwrap(), b"payload");
        assert_eq!(cache.store("h1", b"payload").unwrap(), Adoption::Present, "an existing entry is never rewritten");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_store_that_cannot_write_reports_the_error_and_leaves_no_entry() {
        let root = dir("store-fail");
        let cache = FileCache::new(root.join("absent"));

        assert!(cache.store("h1", b"payload").is_err());
        assert!(!cache.path("h1").exists(), "a failed write must not leave a truncated entry");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn total_bytes_sums_only_cache_files() {
        let root = dir("total");
        let cache = FileCache::new(root.clone());
        assert_eq!(cache.total_bytes(), 0, "an empty cache holds nothing");

        cache.store("h1", b"payload").unwrap();
        cache.store("h2", b"twelve bytes").unwrap();
        // Neither a non-cache extension nor a directory may be counted.
        fs::write(root.join("notes.txt"), b"not a cache entry").unwrap();
        fs::create_dir_all(root.join("sub.cache")).unwrap();

        assert_eq!(cache.total_bytes(), 7 + 12);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn total_bytes_of_a_missing_directory_is_zero() {
        let root = dir("total-missing");
        let cache = FileCache::new(root.join("absent"));
        assert_eq!(cache.total_bytes(), 0, "a cache that was never created costs nothing");
        fs::remove_dir_all(root).unwrap();
    }
}
