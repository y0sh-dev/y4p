// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/pipe.rs

//! Pipe capacity tuning with `fcntl(F_SETPIPE_SZ)`.
//!
//! A Linux pipe holds 64 KiB unless told otherwise. A writer that fills it
//! sleeps until the reader has made room, so a megabyte moves in sixteen
//! sleep/wake pairs; at 1 MiB it moves in one. Wayland clients write into the
//! pipes this daemon hands out, and the daemon writes into the pipes clients
//! hand to it, so the capacity is raised on both.
//!
//! Tuning is strictly best effort. [`grow`] never fails and never panics: a
//! descriptor that is not a pipe, a limit that forbids the size, or a kernel
//! without the call all leave the pipe as it was, and the caller moves its bytes
//! exactly as before, only in more rounds. The capacity is a property of the
//! pipe, not of a descriptor, so either end may be tuned.

use std::os::fd::{AsFd, AsRawFd, RawFd};

use crate::constants::PIPE_TARGET_BYTES;

/// Upper bound on `fcntl` attempts for one pipe: the halving ladder from the
/// target down to one page is 8 rungs, and a signal may repeat one.
const MAX_ATTEMPTS: usize = 16;

/// Raises the capacity of the pipe behind `fd` to [`PIPE_TARGET_BYTES`],
/// stepping down when the system refuses. Returns the capacity in effect
/// afterwards, or `None` when `fd` is not a pipe.
pub fn grow<F: AsFd>(fd: &F) -> Option<usize> {
    grow_to(fd, PIPE_TARGET_BYTES)
}

/// [`grow`] with an explicit target. A pipe already at least that large is left
/// alone: this only ever enlarges (shrinking could fail with `EBUSY` and would
/// serve no purpose here).
///
/// `EPERM` (above `pipe-max-size`, or the user's pipe-page budget is spent) and
/// `ENOMEM` halve the request and retry. Anything else (`EBADF`/`EINVAL`: not a
/// pipe, or an unrepresentable size) ends the attempt, since a smaller request
/// would fail the same way.
pub fn grow_to<F: AsFd>(fd: &F, target: usize) -> Option<usize> {
    grow_raw(fd.as_fd().as_raw_fd(), target)
}

fn grow_raw(fd: RawFd, target: usize) -> Option<usize> {
    let mut current = capacity(fd)?;
    let mut ask = target;
    for _ in 0..MAX_ATTEMPTS {
        if ask <= current {
            break;
        }
        let Ok(size) = libc::c_int::try_from(ask) else {
            ask /= 2;
            continue;
        };
        // SAFETY: `F_SETPIPE_SZ` only takes an integer and acts on the open
        // file description behind `fd`; no pointer is involved. A descriptor
        // that is not open, or not a pipe, makes it fail with an error.
        let set = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, size) };
        if let Ok(now) = usize::try_from(set) {
            return Some(now.max(current));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM | libc::ENOMEM) => ask /= 2,
            Some(libc::EINTR) => {}
            _ => break,
        }
        current = capacity(fd).unwrap_or(current);
    }
    Some(current)
}

/// Current capacity of the pipe behind `fd` (`F_GETPIPE_SZ`); `None` when it is
/// not a pipe.
fn capacity(fd: RawFd) -> Option<usize> {
    // SAFETY: `F_GETPIPE_SZ` takes no argument and only reads a property of the
    // open file description behind `fd`; it fails on anything else.
    let size = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    usize::try_from(size).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::constants::PIPE_DEFAULT_BYTES;
    use std::io::{Read, Write};

    /// The unprivileged ceiling of this machine, when it can be read.
    fn pipe_max_size() -> Option<usize> {
        std::fs::read_to_string("/proc/sys/fs/pipe-max-size").ok()?.trim().parse().ok()
    }

    fn is_root() -> bool {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn a_new_pipe_is_grown_to_the_target_or_to_what_the_system_allows() {
        let (reader, writer) = std::io::pipe().unwrap();
        assert_eq!(capacity(reader.as_raw_fd()), Some(PIPE_DEFAULT_BYTES), "the premise: a stock pipe");

        let grown = grow(&writer).unwrap();

        let reachable = pipe_max_size().map_or(PIPE_TARGET_BYTES, |max| max.min(PIPE_TARGET_BYTES));
        assert!(grown >= PIPE_DEFAULT_BYTES, "never smaller than before");
        assert!(grown <= PIPE_TARGET_BYTES.max(reachable));
        assert!(grown * 2 > reachable || is_root(), "{grown} is far below the {reachable} the system allows");
        assert_eq!(capacity(reader.as_raw_fd()), Some(grown), "the capacity belongs to the pipe: both ends see it");
    }

    #[test]
    fn a_target_beyond_the_system_limit_steps_down_instead_of_failing() {
        let (_reader, writer) = std::io::pipe().unwrap();
        let target = 256 * 1024 * 1024;

        let grown = grow_to(&writer, target).unwrap();

        assert!(grown >= PIPE_DEFAULT_BYTES && grown <= target);
        if let (Some(max), false) = (pipe_max_size(), is_root()) {
            assert!(grown <= max, "an unprivileged process cannot exceed {max}");
            assert!(grown * 2 > max, "halving must land within a factor of two of the limit, got {grown}");
        }
    }

    #[test]
    fn a_target_not_above_the_current_capacity_changes_nothing() {
        let (_reader, writer) = std::io::pipe().unwrap();
        assert_eq!(grow_to(&writer, 4096), Some(PIPE_DEFAULT_BYTES));
        assert_eq!(grow_to(&writer, PIPE_DEFAULT_BYTES), Some(PIPE_DEFAULT_BYTES));
    }

    #[test]
    fn a_pipe_that_is_already_large_is_not_asked_again() {
        let (_reader, writer) = std::io::pipe().unwrap();
        let first = grow(&writer).unwrap();
        assert_eq!(grow(&writer), Some(first));
    }

    #[test]
    fn what_is_not_a_pipe_is_left_alone_without_error() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(grow(&file), None);

        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(grow(&socket), None);
    }

    #[test]
    fn an_invalid_descriptor_is_none_not_a_panic() {
        assert_eq!(grow_raw(-1, PIPE_TARGET_BYTES), None);
        let closed = {
            let (reader, _writer) = std::io::pipe().unwrap();
            reader.as_raw_fd()
        };
        // The descriptor number is now closed (another thread could in principle
        // reuse it, which would only make this a pipe or some other descriptor:
        // either way the call returns).
        let _ = grow_raw(closed, PIPE_TARGET_BYTES);
    }

    #[test]
    fn an_unrepresentable_size_is_stepped_down_not_passed_to_the_kernel() {
        let (_reader, writer) = std::io::pipe().unwrap();
        let grown = grow_to(&writer, usize::MAX).unwrap();
        assert!(grown >= PIPE_DEFAULT_BYTES);
    }

    #[test]
    fn bytes_cross_a_grown_pipe_intact_and_in_order() {
        let (mut reader, mut writer) = std::io::pipe().unwrap();
        grow(&writer).unwrap();
        let data: Vec<u8> = (0..3 * PIPE_TARGET_BYTES + 17).map(|i| (i % 253) as u8).collect();
        let expected = data.clone();
        let handle = std::thread::spawn(move || writer.write_all(&data).unwrap());

        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        handle.join().unwrap();

        assert_eq!(got, expected);
    }

    #[test]
    fn a_grown_pipe_takes_a_write_larger_than_the_default_without_a_reader() {
        let (_reader, mut writer) = std::io::pipe().unwrap();
        let grown = grow(&writer).unwrap();
        if grown <= PIPE_DEFAULT_BYTES {
            return; // The system refused: nothing to demonstrate, and nothing failed.
        }
        // Would block forever on a 64 KiB pipe with no reader.
        writer.write_all(&vec![7u8; PIPE_DEFAULT_BYTES + 4096]).unwrap();
    }
}
