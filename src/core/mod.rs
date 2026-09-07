// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/core/mod.rs

pub mod constants;
pub mod utils;

use std::path::{Path, PathBuf};
use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::core::constants::{DB_DIR_NAME, DB_FILE_NAME, SOCKET_FILE_NAME, DEFAULT_MAX_HISTORY, ENV_MAX_HISTORY};

pub static SIG_EXIT: AtomicBool = AtomicBool::new(false);

/// Resolve the effective history retention cap: `Y4P_MAX_HISTORY` when it
/// parses as a positive integer, `DEFAULT_MAX_HISTORY` otherwise (unset,
/// non-numeric, zero, or negative) — never panics, always usable.
pub fn get_max_history() -> usize {
    std::env::var(ENV_MAX_HISTORY)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&n| n > 0)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_HISTORY)
}

/// Securely resolve and initialize the database path following XDG Data Home specs.
/// Returns PathBuf to ensure platform-native path encoding stability.
pub fn get_db_path() -> PathBuf {
    let mut path = if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg_data)
    } else if let Ok(home) = std::env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".local");
        p.push("share");
        p
    } else {
        PathBuf::from(".")
    };

    path.push(DB_DIR_NAME);

    // Security: Explicitly enforce 700 (rwx------) permissions on directory creation.
    // This prevents other users from accessing the clipboard history storage.
    if !path.exists() {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        let _ = builder.create(&path);
    }

    path.push(DB_FILE_NAME);
    path
}

/// Securely resolve the cache directory for binary payloads.
pub fn get_cache_dir() -> PathBuf {
    let mut path = if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg_cache)
    } else if let Ok(home) = std::env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".cache");
        p
    } else {
        PathBuf::from(".")
    };

    path.push(DB_DIR_NAME); // "y4p"

    if !path.exists() {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        let _ = builder.create(&path);
    }
    path
}

/// Resolve the path of the IPC control socket.
///
/// SECURITY FIX: the previous implementation always placed the socket at a
/// predictable path directly under `/tmp` (`/tmp/y4p.<uid>.sock`).
/// `/tmp` is world-writable; a predictable path in a world-writable
/// directory is a classic local symlink/TOCTOU target — another local user
/// can pre-place a symlink at that exact path before the daemon starts, and
/// depending on kernel path-resolution semantics for `bind(2)`, the daemon
/// could end up creating its socket file at an attacker-chosen location
/// instead of `/tmp`.
///
/// `$XDG_RUNTIME_DIR` is a per-user, non-world-writable, mode-0700 directory
/// by specification (already how Wayland's own compositor socket and most
/// other user session daemons — systemd, PipeWire, PulseAudio — place their
/// sockets), so preferring it removes this class of attack entirely. `/tmp`
/// is retained only as a last-resort fallback for environments without a
/// session manager (e.g. certain minimal containers).
pub fn get_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let mut path = PathBuf::from(runtime_dir);
        path.push(DB_DIR_NAME);

        if !path.exists() {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);
            let _ = builder.create(&path);
        }

        path.push(SOCKET_FILE_NAME);
        return path;
    }

    // Fallback: still per-user (uid-suffixed) to avoid cross-user collisions.
    PathBuf::from(format!("/tmp/{}.{}.sock", DB_DIR_NAME, unsafe { libc::getuid() }))
}

pub fn request_exit() {
    SIG_EXIT.store(true, Ordering::SeqCst);
}

pub fn is_exiting() -> bool {
    SIG_EXIT.load(Ordering::SeqCst)
}

/// RAII guard for the IPC socket, ensuring cleanup using robust PathBuf.
pub struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self { path: path.as_ref().to_path_buf() }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
