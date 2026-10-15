// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/splice.rs

//! Kernel-level stream-to-file copying: `splice(2)` moves a pipe's pages into
//! a file without the bytes ever entering this process, and `tee(2)` lets a
//! consumer observe the same bytes (for a fingerprint) without consuming them.
//!
//! [`copy_to_file`] is the one entry point. It drains `src` into `dst` under a
//! hard byte limit and reports what it moved:
//!
//! 1. **Kernel path** -- `src` is a pipe (checked with `fstat`, not guessed).
//!    Per round: `tee` duplicates up to one chunk into a private scratch pipe
//!    (only when the tap observes), `splice` moves exactly those bytes from
//!    `src` into `dst`, and the scratch copy is read into a fixed stack buffer
//!    for the tap. No heap buffer holds the payload at any point.
//! 2. **Buffered path** -- anything else, or a kernel that refuses (`EINVAL`,
//!    `ENOSYS`, `EOPNOTSUPP`: a socket source, a file opened `O_APPEND`, a
//!    filesystem without splice support). A fixed stack buffer is filled with
//!    `read`, shown to the tap and written out. Same bounds, same result; only
//!    the user-space copy is paid.
//!
//! The switch from the first path to the second only happens *between* rounds
//! (or before a round has consumed anything): `tee` does not consume `src`, so
//! when the `splice` that follows it is refused, the scratch copy is simply
//! discarded and the buffered path starts from the same stream position. The
//! tap therefore sees every byte exactly once, in order, on every path.
//!
//! The limit is enforced the way `payload::read_bounded` enforces it: one byte
//! beyond `limit` is let through as a probe, so a stream that exactly fits is
//! told apart from one that overflows, and an overflow is an error rather than
//! a silent truncation. On any error `dst` may hold a partial copy; discarding
//! it is the caller's job (see `payload::SpooledFile`, which unlinks on drop).

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::ptr;

use crate::constants::SPLICE_CHUNK_BYTES;
use crate::payload::PayloadError;

/// Observer of a byte stream while it is being copied: the seam through which
/// the daemon fingerprints a payload (SHA3-256) without this crate, or the
/// Wayland layer, depending on a hash implementation.
///
/// `OBSERVES` lets the copier skip `tee(2)` and its scratch pipe entirely when
/// nobody is listening ([`NoTap`]); it is an associated constant, so the choice
/// is made at compile time and costs no branch.
pub trait ByteTap {
    const OBSERVES: bool = true;

    /// Called with each consecutive chunk of the stream, exactly once, in order.
    fn update(&mut self, chunk: &[u8]);
}

/// A tap that observes nothing: the pure `splice(2)` path.
pub struct NoTap;

impl ByteTap for NoTap {
    const OBSERVES: bool = false;

    fn update(&mut self, _chunk: &[u8]) {}
}

/// What a successful copy moved, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Copied {
    /// Total bytes written to the destination (never more than the limit).
    pub len: u64,
    /// Of those, the bytes moved by `splice(2)`. `len == kernel_bytes` is a
    /// fully zero-copy transfer; `0` means the buffered path did everything.
    pub kernel_bytes: u64,
}

/// Copies `src` to `dst` until EOF, at most `limit` bytes, showing every byte
/// to `tap`. See the module documentation for the two paths and their bounds.
///
/// Fails with `PayloadError::Empty` when `src` yielded nothing,
/// `PayloadError::TooLarge` when it carried more than `limit` bytes, and
/// `PayloadError::Io` on a real I/O error.
pub fn copy_to_file<S, T>(src: &mut S, dst: &mut File, limit: u64, tap: &mut T) -> Result<Copied, PayloadError>
where
    S: Read + AsFd,
    T: ByteTap,
{
    let mut copied = Copied::default();
    if is_pipe(src)? && kernel_loop(src.as_fd().as_raw_fd(), dst, limit, tap, &mut copied)? == KernelEnd::Eof {
        return finish(copied);
    }
    buffered_loop(src, dst, limit, tap, &mut copied)?;
    finish(copied)
}

/// The buffered path alone, with no kernel involvement. It is what
/// [`copy_to_file`] falls back to, and the reference the kernel path is
/// compared against in tests and benchmarks.
pub fn copy_to_file_buffered<S, T>(src: &mut S, dst: &mut File, limit: u64, tap: &mut T) -> Result<Copied, PayloadError>
where
    S: Read,
    T: ByteTap,
{
    let mut copied = Copied::default();
    buffered_loop(src, dst, limit, tap, &mut copied)?;
    finish(copied)
}

fn finish(copied: Copied) -> Result<Copied, PayloadError> {
    if copied.len == 0 { Err(PayloadError::Empty) } else { Ok(copied) }
}

/// Whether `src` is a FIFO/pipe, the only source `tee(2)` and a pipe-to-file
/// `splice(2)` accept.
fn is_pipe<S: AsFd>(src: &S) -> io::Result<bool> {
    use std::os::unix::fs::FileTypeExt;
    let file = src.as_fd().try_clone_to_owned().map(File::from)?;
    Ok(file.metadata()?.file_type().is_fifo())
}

#[derive(Debug, PartialEq, Eq)]
enum KernelEnd {
    /// `src` reached end-of-stream on the kernel path.
    Eof,
    /// The kernel refused before consuming anything of the current round;
    /// the buffered path continues from the same position.
    Unsupported,
}

/// Errors that mean "this descriptor pair cannot be spliced", as opposed to
/// "the transfer failed".
fn is_unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP))
}

/// `limit + 1`: the probe byte that distinguishes an exact fit from an overflow.
fn allowance(limit: u64) -> u64 {
    limit.saturating_add(1)
}

fn kernel_loop<T: ByteTap>(
    src: RawFd,
    dst: &mut File,
    limit: u64,
    tap: &mut T,
    copied: &mut Copied,
) -> Result<KernelEnd, PayloadError> {
    // The scratch pipe exists only when somebody observes the stream.
    let scratch = if T::OBSERVES {
        match io::pipe() {
            Ok(pair) => Some(pair),
            // No descriptors to spare for the scratch pipe: the buffered path
            // needs none, so this is a fallback, not a failure.
            Err(_) => return Ok(KernelEnd::Unsupported),
        }
    } else {
        None
    };
    let mut staging = [0u8; SPLICE_CHUNK_BYTES];
    let dst_fd = dst.as_raw_fd();
    let allowed = allowance(limit);

    loop {
        let want = usize::try_from((allowed - copied.len).min(SPLICE_CHUNK_BYTES as u64)).unwrap_or(SPLICE_CHUNK_BYTES);

        let round = match &scratch {
            Some((scratch_r, scratch_w)) => {
                // `tee` copies without consuming: `src` still holds every byte
                // it reports, so a refusal below loses nothing.
                let n = match tee_once(src, scratch_w.as_raw_fd(), want) {
                    Ok(0) => return Ok(KernelEnd::Eof),
                    Ok(n) => n,
                    Err(e) if is_unsupported(&e) => return Ok(KernelEnd::Unsupported),
                    Err(e) => return Err(e.into()),
                };
                match splice_exact(src, dst_fd, n) {
                    Ok(()) => {}
                    Err(SpliceFailure::Refused) => return Ok(KernelEnd::Unsupported),
                    Err(SpliceFailure::Io(e)) => return Err(e.into()),
                }
                // Exactly the `n` bytes just moved, as seen through the scratch pipe.
                let mut scratch_r = scratch_r;
                let chunk = staging.get_mut(..n).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
                scratch_r.read_exact(chunk)?;
                tap.update(chunk);
                n
            }
            None => match splice_once(src, dst_fd, want) {
                Ok(0) => return Ok(KernelEnd::Eof),
                Ok(n) => n,
                // A refused `splice` moved nothing, so the buffered path can
                // resume from the same position even after earlier rounds.
                Err(e) if is_unsupported(&e) => return Ok(KernelEnd::Unsupported),
                Err(e) => return Err(e.into()),
            },
        };

        copied.len += round as u64;
        copied.kernel_bytes += round as u64;
        if copied.len > limit {
            return Err(PayloadError::TooLarge { limit });
        }
    }
}

fn buffered_loop<S: Read + ?Sized, T: ByteTap>(
    src: &mut S,
    dst: &mut File,
    limit: u64,
    tap: &mut T,
    copied: &mut Copied,
) -> Result<(), PayloadError> {
    let mut buf = [0u8; SPLICE_CHUNK_BYTES];
    let allowed = allowance(limit);
    loop {
        let want = usize::try_from((allowed - copied.len).min(SPLICE_CHUNK_BYTES as u64)).unwrap_or(SPLICE_CHUNK_BYTES);
        let n = match src.read(buf.get_mut(..want).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        let chunk = buf.get(..n).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        dst.write_all(chunk)?;
        if T::OBSERVES {
            tap.update(chunk);
        }
        copied.len += n as u64;
        if copied.len > limit {
            return Err(PayloadError::TooLarge { limit });
        }
    }
}

enum SpliceFailure {
    /// The kernel refused the pair before moving a single byte.
    Refused,
    Io(io::Error),
}

/// Moves exactly `n` bytes (already known to be in `src`) into `dst`.
///
/// A refusal is only reported as such when nothing has been moved yet; a
/// refusal after partial progress cannot be recovered from and is an I/O error.
fn splice_exact(src: RawFd, dst: RawFd, n: usize) -> Result<(), SpliceFailure> {
    let mut moved = 0usize;
    while moved < n {
        match splice_once(src, dst, n - moved) {
            Ok(0) => return Err(SpliceFailure::Io(io::Error::from(io::ErrorKind::UnexpectedEof))),
            Ok(m) => moved += m,
            Err(e) if is_unsupported(&e) && moved == 0 => return Err(SpliceFailure::Refused),
            Err(e) => return Err(SpliceFailure::Io(e)),
        }
    }
    Ok(())
}

fn splice_once(src: RawFd, dst: RawFd, len: usize) -> io::Result<usize> {
    loop {
        // SAFETY: `src` and `dst` are open descriptors that the caller keeps
        // alive for the whole call; both offset pointers are null, which asks
        // the kernel to use (and advance) each descriptor's own position, so
        // no user memory is read or written.
        let n = unsafe { libc::splice(src, ptr::null_mut(), dst, ptr::null_mut(), len, libc::SPLICE_F_MOVE) };
        match usize::try_from(n) {
            Ok(n) => return Ok(n),
            Err(_) => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

fn tee_once(src: RawFd, dst: RawFd, len: usize) -> io::Result<usize> {
    loop {
        // SAFETY: `src` and `dst` are open pipe descriptors that the caller
        // keeps alive for the whole call; `tee(2)` only exchanges pipe buffer
        // references in the kernel and touches no user memory.
        let n = unsafe { libc::tee(src, dst, len, 0) };
        match usize::try_from(n) {
            Ok(n) => return Ok(n),
            Err(_) => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Seek;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Order-sensitive checksum: it changes if any byte is skipped, repeated
    /// or reordered, which is exactly what a tap must never see.
    #[derive(Default)]
    struct Checksum {
        value: u64,
        seen: u64,
        chunks: u64,
    }

    impl ByteTap for Checksum {
        fn update(&mut self, chunk: &[u8]) {
            for &b in chunk {
                self.value = self.value.wrapping_mul(0x100_0000_01b3).wrapping_add(u64::from(b) + 1);
            }
            self.seen += chunk.len() as u64;
            self.chunks += 1;
        }
    }

    fn checksum_of(data: &[u8]) -> u64 {
        let mut c = Checksum::default();
        c.update(data);
        c.value
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + i / 251) % 251) as u8).collect()
    }

    fn scratch_path(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!("y4p-splice-{}-{}-{}", tag, std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        p
    }

    /// A destination file plus its path (removed by the caller).
    fn dest(tag: &str) -> (File, PathBuf) {
        let path = scratch_path(tag);
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        (file, path)
    }

    /// A pipe whose writer thread feeds `data` and then closes, like a
    /// compositor client finishing a transfer.
    fn fed_pipe(data: Vec<u8>) -> (File, std::thread::JoinHandle<()>) {
        let (reader, mut writer) = io::pipe().unwrap();
        let handle = std::thread::spawn(move || {
            let _ = writer.write_all(&data);
        });
        (File::from(std::os::fd::OwnedFd::from(reader)), handle)
    }

    fn read_back(path: &PathBuf) -> Vec<u8> {
        let data = std::fs::read(path).unwrap();
        std::fs::remove_file(path).unwrap();
        data
    }

    #[test]
    fn a_pipe_is_moved_by_splice_and_the_tap_sees_every_byte_once_in_order() {
        let data = payload(3 * 1024 * 1024 + 17);
        let (mut src, writer) = fed_pipe(data.clone());
        let (mut dst, path) = dest("tee");
        let mut tap = Checksum::default();

        let copied = copy_to_file(&mut src, &mut dst, 64 << 20, &mut tap).unwrap();
        writer.join().unwrap();

        assert_eq!(copied.len, data.len() as u64);
        assert_eq!(copied.kernel_bytes, copied.len, "every byte must have moved by splice(2)");
        assert_eq!((tap.seen, tap.value), (data.len() as u64, checksum_of(&data)));
        assert_eq!(read_back(&path), data);
    }

    #[test]
    fn without_a_tap_the_copy_is_pure_splice() {
        let data = payload(1 << 20);
        let (mut src, writer) = fed_pipe(data.clone());
        let (mut dst, path) = dest("notap");

        let copied = copy_to_file(&mut src, &mut dst, 64 << 20, &mut NoTap).unwrap();
        writer.join().unwrap();

        assert_eq!((copied.len, copied.kernel_bytes), (data.len() as u64, data.len() as u64));
        assert_eq!(read_back(&path), data);
    }

    #[test]
    fn a_regular_file_source_takes_the_buffered_path_with_the_same_result() {
        let data = payload(300_000);
        let src_path = scratch_path("src");
        std::fs::write(&src_path, &data).unwrap();
        let mut src = File::open(&src_path).unwrap();
        let (mut dst, path) = dest("buffered");
        let mut tap = Checksum::default();

        let copied = copy_to_file(&mut src, &mut dst, 64 << 20, &mut tap).unwrap();

        assert_eq!((copied.len, copied.kernel_bytes), (data.len() as u64, 0));
        assert_eq!((tap.seen, tap.value), (data.len() as u64, checksum_of(&data)));
        assert_eq!(read_back(&path), data);
        std::fs::remove_file(src_path).unwrap();
    }

    #[test]
    fn a_refused_splice_after_tee_falls_back_without_losing_or_repeating_a_byte() {
        // `O_APPEND` destinations are refused by splice(2) (EINVAL) but accept
        // write(2): tee succeeds, the splice that follows it does not, and the
        // buffered path must resume from the very same stream position.
        let data = payload(700_000);
        let (mut src, writer) = fed_pipe(data.clone());
        let path = scratch_path("append");
        let mut dst = std::fs::OpenOptions::new().append(true).create_new(true).open(&path).unwrap();
        let mut tap = Checksum::default();

        let copied = copy_to_file(&mut src, &mut dst, 64 << 20, &mut tap).unwrap();
        writer.join().unwrap();

        assert_eq!(copied.len, data.len() as u64);
        assert_eq!(copied.kernel_bytes, 0, "the refusal must have moved the transfer to the buffered path");
        assert_eq!((tap.seen, tap.value), (data.len() as u64, checksum_of(&data)));
        assert_eq!(read_back(&path), data);
    }

    #[test]
    fn a_refused_splice_without_a_tap_also_falls_back() {
        let data = payload(200_000);
        let (mut src, writer) = fed_pipe(data.clone());
        let path = scratch_path("append-notap");
        let mut dst = std::fs::OpenOptions::new().append(true).create_new(true).open(&path).unwrap();

        let copied = copy_to_file(&mut src, &mut dst, 64 << 20, &mut NoTap).unwrap();
        writer.join().unwrap();

        assert_eq!((copied.len, copied.kernel_bytes), (data.len() as u64, 0));
        assert_eq!(read_back(&path), data);
    }

    #[test]
    fn a_stream_exactly_at_the_limit_fits_and_one_byte_more_does_not() {
        for tap_on in [false, true] {
            for (len, fits) in [(100_000usize, true), (100_001, false)] {
                let data = payload(len);
                let (mut src, writer) = fed_pipe(data);
                let (mut dst, path) = dest("limit");
                let result = if tap_on {
                    copy_to_file(&mut src, &mut dst, 100_000, &mut Checksum::default())
                } else {
                    copy_to_file(&mut src, &mut dst, 100_000, &mut NoTap)
                };
                drop(src);
                writer.join().unwrap();
                match (fits, result) {
                    (true, Ok(c)) => assert_eq!(c.len, 100_000),
                    (false, Err(PayloadError::TooLarge { limit })) => assert_eq!(limit, 100_000),
                    (_, other) => panic!("len {len}, tap {tap_on}: unexpected {other:?}"),
                }
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn an_overflowing_stream_is_cut_off_at_the_probe_byte_not_drained() {
        // 8 MiB offered against a 64 KiB limit: the copy must stop at limit+1
        // bytes written, not read the whole stream first.
        let (mut src, writer) = fed_pipe(payload(8 << 20));
        let (mut dst, path) = dest("probe");
        let result = copy_to_file(&mut src, &mut dst, 65_536, &mut NoTap);
        assert!(matches!(result, Err(PayloadError::TooLarge { limit: 65_536 })));
        drop(src);
        let _ = writer.join();
        assert!(std::fs::metadata(&path).unwrap().len() <= 65_537);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn an_empty_stream_is_reported_as_empty_on_every_path() {
        let (reader, writer) = io::pipe().unwrap();
        drop(writer);
        let mut src = File::from(std::os::fd::OwnedFd::from(reader));
        let (mut dst, path) = dest("empty");
        assert!(matches!(copy_to_file(&mut src, &mut dst, 1024, &mut Checksum::default()), Err(PayloadError::Empty)));
        assert!(matches!(copy_to_file_buffered(&mut io::empty(), &mut dst, 1024, &mut NoTap), Err(PayloadError::Empty)));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_zero_limit_admits_nothing() {
        let (mut src, writer) = fed_pipe(vec![1]);
        let (mut dst, path) = dest("zero");
        assert!(matches!(copy_to_file(&mut src, &mut dst, 0, &mut NoTap), Err(PayloadError::TooLarge { limit: 0 })));
        drop(src);
        let _ = writer.join();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_slow_trickling_writer_is_copied_completely() {
        let (reader, mut writer) = io::pipe().unwrap();
        let handle = std::thread::spawn(move || {
            for i in 0..50u8 {
                let _ = writer.write_all(&[i; 997]);
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        });
        let mut src = File::from(std::os::fd::OwnedFd::from(reader));
        let (mut dst, path) = dest("trickle");
        let mut tap = Checksum::default();
        let copied = copy_to_file(&mut src, &mut dst, 1 << 20, &mut tap).unwrap();
        handle.join().unwrap();
        let expected: Vec<u8> = (0..50u8).flat_map(|i| [i; 997]).collect();
        assert_eq!(copied.len, expected.len() as u64);
        assert_eq!(tap.value, checksum_of(&expected));
        assert_eq!(read_back(&path), expected);
    }

    #[test]
    fn the_copy_continues_at_the_destinations_current_position() {
        // The daemon writes the already-read head first, then splices the rest.
        let (mut src, writer) = fed_pipe(b"-tail".to_vec());
        let path = scratch_path("offset");
        let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).unwrap();
        dst.write_all(b"head").unwrap();
        copy_to_file(&mut src, &mut dst, 1024, &mut NoTap).unwrap();
        writer.join().unwrap();
        assert_eq!(read_back(&path), b"head-tail");
    }

    #[test]
    fn the_buffered_reference_matches_the_kernel_path_byte_for_byte() {
        let data = payload(1_234_567);
        let src_path = scratch_path("ref-src");
        std::fs::write(&src_path, &data).unwrap();

        let (mut kernel_src, writer) = fed_pipe(data);
        let (mut kernel_dst, kernel_path) = dest("ref-kernel");
        let mut kernel_tap = Checksum::default();
        copy_to_file(&mut kernel_src, &mut kernel_dst, 64 << 20, &mut kernel_tap).unwrap();
        writer.join().unwrap();

        let mut buffered_src = File::open(&src_path).unwrap();
        buffered_src.rewind().unwrap();
        let (mut buffered_dst, buffered_path) = dest("ref-buffered");
        let mut buffered_tap = Checksum::default();
        copy_to_file_buffered(&mut buffered_src, &mut buffered_dst, 64 << 20, &mut buffered_tap).unwrap();

        assert_eq!(kernel_tap.value, buffered_tap.value);
        assert_eq!(read_back(&kernel_path), read_back(&buffered_path));
        std::fs::remove_file(src_path).unwrap();
    }

    #[test]
    fn a_failing_source_surfaces_as_an_io_error() {
        struct Broken(File);
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }
        impl AsFd for Broken {
            fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
                self.0.as_fd()
            }
        }
        // A regular file descriptor, so the buffered path (and its error) is taken.
        let file = File::open("/dev/null").unwrap();
        let (mut dst, path) = dest("broken");
        let result = copy_to_file(&mut Broken(file), &mut dst, 1024, &mut NoTap);
        assert!(matches!(result, Err(PayloadError::Io(_))));
        std::fs::remove_file(path).unwrap();
    }
}
