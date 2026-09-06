// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/storage/cache.rs

use std::fs;
use std::path::PathBuf;

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
        p.push(format!("{}.cache", hash));
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
