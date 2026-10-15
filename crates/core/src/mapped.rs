// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/mapped.rs

//! A read-only memory mapping of a whole file, viewed as `&[u8]`.
//!
//! The persistence layer (`crates/storage`, frozen) takes a payload as a byte
//! slice. For a payload that was spooled to disk, mapping that file hands the
//! storage layer the real bytes straight from the page cache: no `Vec`, no
//! copy, and no page is even faulted in unless the slice is actually read
//! (an image row only needs its length; its bytes are adopted by `rename(2)`).

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::slice;

/// A private, read-only mapping of an entire non-empty file, unmapped on drop.
///
/// Not `Send`/`Sync` (it holds a raw pointer): a mapping stays on the thread
/// that made it, which is all the worker needs.
pub struct MappedFile {
    ptr: NonNull<u8>,
    len: usize,
}

impl MappedFile {
    /// Maps the whole file at `path`.
    ///
    /// Fails with `InvalidInput` for an empty file (`mmap(2)` rejects a zero
    /// length, and an empty payload is never worth keeping) and with the OS
    /// error when the file cannot be opened or mapped (for example on a
    /// filesystem without `mmap` support), so a caller can fall back to
    /// reading it.
    ///
    /// # Safety
    ///
    /// The file must not be truncated or modified for as long as the mapping
    /// (or any slice borrowed from it) exists: a shrunken file raises `SIGBUS`
    /// on access and a rewritten one changes bytes under a shared `&[u8]`.
    /// Replacing or unlinking the *path* is harmless; the mapping keeps the
    /// original inode alive. Callers use this only on files private to the
    /// process (a spool file in a mode-0700 directory) that nobody writes after
    /// the copy that created them.
    pub unsafe fn map(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        if len == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // SAFETY: a null hint lets the kernel choose the address; `len` is the
        // file's current non-zero length; `file` is an open descriptor for the
        // duration of the call (the mapping outlives the descriptor, which is
        // allowed); and the arguments request a private read-only mapping.
        let raw = unsafe { libc::mmap(ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        match NonNull::new(raw.cast::<u8>()) {
            Some(ptr) => Ok(Self { ptr, len }),
            None => Err(io::Error::from(io::ErrorKind::InvalidData)),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` is the start of a live `len`-byte read-only mapping
        // created in `map` and unmapped only in `drop`, and `map`'s contract
        // keeps the file unmodified for the mapping's lifetime. The returned
        // borrow cannot outlive `self`.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` describe exactly the mapping `map` created, and
        // `drop` runs once, so it is unmapped once and never used afterwards.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast::<libc::c_void>(), self.len);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("y4p-mapped-{}-{}", tag, std::process::id()));
        p
    }

    #[test]
    fn a_mapping_shows_the_file_contents() {
        let path = temp("contents");
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        // SAFETY: the test owns the file and does not touch it while mapped.
        let map = unsafe { MappedFile::map(&path) }.unwrap();
        assert_eq!((map.len(), &map[..]), (data.len(), data.as_slice()));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_mapping_survives_the_path_being_unlinked_or_renamed() {
        let path = temp("unlink");
        let moved = temp("unlink-moved");
        std::fs::write(&path, b"still here").unwrap();
        // SAFETY: the test owns the file and only renames/unlinks the path.
        let map = unsafe { MappedFile::map(&path) }.unwrap();
        std::fs::rename(&path, &moved).unwrap();
        assert_eq!(&map[..], b"still here");
        std::fs::remove_file(&moved).unwrap();
        assert_eq!(&map[..], b"still here");
    }

    #[test]
    fn an_empty_or_missing_file_is_an_error_not_a_panic() {
        let path = temp("empty");
        std::fs::write(&path, b"").unwrap();
        // SAFETY: the test owns the file.
        let empty = unsafe { MappedFile::map(&path) };
        assert_eq!(empty.err().map(|e| e.kind()), Some(io::ErrorKind::InvalidInput));
        std::fs::remove_file(&path).unwrap();
        // SAFETY: nothing exists at the path, so there is nothing to modify.
        assert!(unsafe { MappedFile::map(&path) }.is_err());
    }
}
