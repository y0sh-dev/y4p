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
}
