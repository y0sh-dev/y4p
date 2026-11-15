// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/lib.rs

pub mod constants;
pub mod utils;
pub mod config;
pub mod payload;
pub mod splice;
pub mod pipe;
pub mod memory;
pub mod cdc;

use std::path::{Path, PathBuf};
use std::fs::{self, DirBuilder, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::sync::atomic::{AtomicBool, Ordering};
use crate::constants::{DB_DIR_NAME, DB_FILE_NAME, ENV_MAX_HISTORY, SOCKET_FILE_NAME, SPOOL_DIR_NAME};

pub static SIG_EXIT: AtomicBool = AtomicBool::new(false);

/// Resolve the effective history retention cap.
///
/// Evaluation priority:
/// 1. `Y4P_MAX_HISTORY` environment variable (if set to a positive integer).
/// 2. `y4p.toml`'s `[general] max_history` setting.
/// 3. `DEFAULT_MAX_HISTORY` fallback.
pub fn get_max_history() -> usize {
    std::env::var(ENV_MAX_HISTORY)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&n| n > 0)
        .map(|n| n as usize)
        .unwrap_or_else(|| config::Config::load().general.max_history)
}

/// Securely resolve and initialise the database path following XDG Data Home specs.
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

/// Directory where payloads are spooled while they stream in: a subdirectory of
/// the cache directory (so storage can adopt a finished file into the cache with a `rename(2)`), not
/// created here. See `constants::SPOOL_DIR_NAME`.
pub fn spool_dir_path() -> PathBuf {
    let mut path = get_cache_dir();
    path.push(SPOOL_DIR_NAME);
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
    // SAFETY: `getuid()` takes no arguments and cannot fail.
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

/// RAII exclusive advisory lock (`flock(2)`) on the daemon start-up lock file,
/// `<socket path>.lock`.
///
/// Two daemons must never act on the socket file at once: the exiting one
/// unlinks it from its [`SocketGuard`], and the starting one unlinks a stale
/// file before binding. Without a lock those two windows overlap, and the new
/// daemon can bind a socket the old one is about to remove. Taking the lock
/// before either touches the socket, and releasing it only after both are done
/// (see `daemon::start_daemon`'s declaration order), closes that race.
///
/// The lock is advisory and belongs to the open file description, which is
/// exactly what is wanted here: a second [`SocketLock::try_acquire`] fails with
/// [`std::io::ErrorKind::WouldBlock`] while the first value is alive, even
/// inside the same process. Closing the descriptor releases it, so dropping
/// this value is enough.
#[derive(Debug)]
pub struct SocketLock {
    // Held only for the descriptor's lifetime; never read.
    _file: fs::File,
}

impl SocketLock {
    /// The companion lock path for `socket_path`: the socket's own file name
    /// with `.lock` appended, in the same (per-user, mode-0700) directory.
    pub fn path_for<P: AsRef<Path>>(socket_path: P) -> PathBuf {
        let mut path = socket_path.as_ref().as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// Takes the lock without waiting. An already-locked file reports
    /// [`std::io::ErrorKind::WouldBlock`]; an interrupted call is retried
    /// rather than reported, as `flock(2)` documents.
    ///
    /// The lock file is created mode 0600: it sits beside the socket in the
    /// user's private runtime directory, and its existence is nobody else's
    /// business.
    pub fn try_acquire(lock_path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // The lock file's contents are irrelevant; truncating on every
            // start would only be needless churn (and the lint asks for the
            // choice to be explicit).
            .truncate(false)
            .mode(0o600)
            .open(lock_path)?;
        loop {
            // SAFETY: `file` owns a valid open descriptor for the whole call;
            // `flock(2)` only reads that descriptor and cannot fail unsafely.
            let acquired = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if acquired == 0 {
                return Ok(Self { _file: file });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn lock_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("y4p-socket-lock-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_lock_path_is_the_socket_path_with_a_lock_suffix() {
        assert_eq!(
            SocketLock::path_for("/run/user/1000/y4p/y4p.sock"),
            PathBuf::from("/run/user/1000/y4p/y4p.sock.lock"),
        );
    }

    #[test]
    fn a_socket_lock_is_exclusive_and_released_on_drop() {
        let dir = lock_dir("exclusive");
        let path = SocketLock::path_for(dir.join("y4p.sock"));

        let first = SocketLock::try_acquire(&path).unwrap();
        let error = SocketLock::try_acquire(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock, "a second daemon must not get the lock");

        drop(first);
        let second = SocketLock::try_acquire(&path).unwrap();
        drop(second);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_socket_lock_file_is_private_to_its_user() {
        let dir = lock_dir("mode");
        let path = SocketLock::path_for(dir.join("y4p.sock"));

        drop(SocketLock::try_acquire(&path).unwrap());

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the lock file must not be readable by others");

        fs::remove_dir_all(dir).unwrap();
    }
}
