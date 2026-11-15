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

use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::{Duration, Instant};

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

/// Marks `fd` non-blocking (`O_NONBLOCK`).
pub fn set_nonblocking<F: AsFd>(fd: &F) -> io::Result<()> {
    let raw = fd.as_fd().as_raw_fd();
    // SAFETY: `F_GETFL` takes no pointer arguments and only reads the file
    // status flags of `raw`.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0 {
        return Ok(());
    }
    // SAFETY: `F_SETFL` sets the file status flags of `raw` to `flags | O_NONBLOCK`.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Blocks until `fd` is readable (or at EOF/error) or `deadline` expires.
///
/// Returns `Err(io::ErrorKind::TimedOut)` if `deadline` elapses before `fd`
/// becomes ready.
pub fn wait_readable<F: AsFd>(fd: &F, deadline: Instant) -> io::Result<()> {
    wait_readable_raw(fd.as_fd().as_raw_fd(), deadline)
}

pub(crate) fn wait_readable_raw(raw: RawFd, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe read deadline exceeded"));
        }
        // Round sub-millisecond remainders up to 1 ms so `poll` does not busy-spin.
        let ms = remaining.as_millis();
        let timeout_ms = if ms == 0 && remaining > Duration::ZERO {
            1
        } else {
            libc::c_int::try_from(ms).unwrap_or(libc::c_int::MAX)
        };
        let mut fds = [libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 }];
        // SAFETY: `fds` is a valid, stack-allocated 1-element `pollfd` array
        // for `poll(2)` to inspect and populate `revents` in.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
        if rc > 0 {
            return Ok(());
        }
        if rc == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe read deadline exceeded"));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Non-blocking reader wrapper that enforces a wall-clock `deadline` on every
/// `read` via `poll(2)`.
pub struct DeadlineReader<F: AsFd + Read> {
    inner: F,
    deadline: Instant,
}

impl<F: AsFd + Read> DeadlineReader<F> {
    /// Marks `inner` non-blocking and wraps it with `deadline`.
    pub fn new(inner: F, deadline: Instant) -> io::Result<Self> {
        set_nonblocking(&inner)?;
        Ok(Self { inner, deadline })
    }

    /// Returns the underlying descriptor. Its `O_NONBLOCK` flag remains set.
    pub fn into_inner(self) -> F {
        self.inner
    }
}

impl<F: AsFd + Read> AsFd for DeadlineReader<F> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
    }
}

impl<F: AsFd + Read> Read for DeadlineReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    wait_readable(&self.inner, self.deadline)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
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

    #[test]
    fn deadline_reader_times_out_on_stalled_pipe() {
        let (reader, _writer) = std::io::pipe().unwrap();
        let deadline = Instant::now() + Duration::from_millis(30);
        let mut dr = DeadlineReader::new(reader, deadline).unwrap();
        let mut buf = [0u8; 64];
        let err = dr.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn deadline_reader_succeeds_when_data_arrives_within_deadline() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut dr = DeadlineReader::new(reader, deadline).unwrap();

        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            writer.write_all(b"delayed-ok").unwrap();
        });

        let mut got = Vec::new();
        dr.read_to_end(&mut got).unwrap();
        handle.join().unwrap();
        assert_eq!(got, b"delayed-ok");
    }

    #[test]
    fn deadline_reader_returns_eof_when_writer_is_closed() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let mut dr = DeadlineReader::new(reader, Instant::now() + Duration::from_millis(200)).unwrap();
        let mut got = Vec::new();
        assert_eq!(dr.read_to_end(&mut got).unwrap(), 0);
    }
}
