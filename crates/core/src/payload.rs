// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/payload.rs

//! One vocabulary for "where a clipboard payload's bytes live", shared by
//! ingress (`y4p-wayland` -> `y4p-daemon`) and egress (`y4p-daemon` ->
//! `y4p-wayland`).
//!
//! - [`Payload`]: the places bytes can be -- a heap buffer, a streaming
//!   descriptor ([`StreamDescriptor`]), a file on disk, or a [`SpooledFile`]
//!   (a payload already drained into a file this value owns and deletes).
//! - [`PayloadReader`]: an `enum` implementing [`Read`] over the file- and
//!   memory-backed variants, so a consumer reads any payload without a
//!   `Box<dyn Read>` allocation and can still ask for the underlying
//!   descriptor (for `sendfile(2)`/`splice(2)`).
//! - [`route_payload`]: the heuristic that separates the in-memory fast path
//!   from the descriptor path.
//! - [`read_bounded`]: the single "read at most N bytes, never truncate"
//!   primitive that every materialisation goes through.
//!
//! Everything here is `std` only and panic-free; the only descriptor handling
//! is std's own RAII (`File`/`OwnedFd`), so there is no `unsafe`.

use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use crate::constants::{DIGEST_LEN, INLINE_PAYLOAD_THRESHOLD_BYTES, PAYLOAD_PREALLOC_CAP_BYTES};
use crate::utils::is_text_like_mime;

// --- StreamDescriptor ---

/// An owned, readable descriptor (pipe, socket, memfd, ...) with an optional
/// advisory length.
///
/// The descriptor is closed exactly once, when the value is dropped, on every
/// path (including an early return or an unwinding thread). Internally it is a
/// `File`: std's own owner of a descriptor, which gives `Read` and `dup(2)`
/// without any `unsafe` of ours. `into_owned_fd` hands the descriptor back.
///
/// `len_hint` is *advisory*: it pre-sizes buffers and feeds [`route_payload`],
/// but nothing trusts it for correctness -- a reader still stops at its own
/// limit and at EOF, so a peer that under- or over-states it cannot cause an
/// over-read or a truncation.
#[derive(Debug)]
pub struct StreamDescriptor {
    file: File,
    len_hint: Option<u64>,
}

impl StreamDescriptor {
    pub fn new(fd: OwnedFd, len_hint: Option<u64>) -> Self {
        Self { file: File::from(fd), len_hint }
    }

    pub fn len_hint(&self) -> Option<u64> {
        self.len_hint
    }

    /// `dup(2)`s the descriptor. The two share one open file description, so
    /// for a pipe or socket the clones draw from the *same* stream: a
    /// consumer-once source, not a replayable one.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self { file: self.file.try_clone()?, len_hint: self.len_hint })
    }

    pub fn into_owned_fd(self) -> OwnedFd {
        OwnedFd::from(self.file)
    }

    pub fn into_file(self) -> File {
        self.file
    }
}

impl AsFd for StreamDescriptor {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl AsRawFd for StreamDescriptor {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Read for StreamDescriptor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

// --- Payload ---

/// Where a payload's bytes live.
#[derive(Debug)]
pub enum Payload {
    /// Fully materialised on the heap. Moving it between threads moves the
    /// `Vec`'s pointer, never its bytes.
    Memory(Vec<u8>),
    /// Still in flight on a descriptor; nothing has been read yet.
    Stream(StreamDescriptor),
    /// On disk (the content cache). Opened lazily, by whoever reads it. The
    /// file is borrowed: dropping the payload never touches it.
    File(PathBuf),
    /// A payload already drained from its pipe into a spool file (see
    /// [`SpooledFile`]), complete and fingerprinted. Unlike `File`, the file is
    /// *owned*: it is deleted when the payload is dropped, so a job that is
    /// shed from a full queue or abandoned by a failing worker leaves nothing
    /// behind. Boxed to keep `Payload` (and so every queue slot) the size of
    /// the other variants.
    Spooled(Box<SpooledFile>),
}

impl Payload {
    pub fn memory(data: Vec<u8>) -> Self {
        Self::Memory(data)
    }

    /// Length in bytes when it is known without any I/O: the buffer's length,
    /// or the descriptor's advisory hint. A `File` is `None` here -- stating
    /// it is I/O; [`Payload::into_reader`] reports the real size.
    pub fn len_hint(&self) -> Option<u64> {
        match self {
            Self::Memory(data) => Some(data.len() as u64),
            Self::Stream(stream) => stream.len_hint(),
            Self::File(_) => None,
            Self::Spooled(spooled) => Some(spooled.len()),
        }
    }

    /// The bytes, when (and only when) they are already in memory.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Memory(data) => Some(data),
            Self::Stream(_) | Self::File(_) | Self::Spooled(_) => None,
        }
    }

    /// An independent handle to the same bytes, for a source that is served
    /// more than once: the buffer is copied, the path is cloned, and a stream
    /// is `dup(2)`ed (see [`StreamDescriptor::try_clone`] for what that shares).
    /// A spooled payload owns its file's lifetime, so it cannot be duplicated
    /// (`ErrorKind::Unsupported`); read it, or hand its path on explicitly.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            Self::Memory(data) => Self::Memory(data.clone()),
            Self::Stream(stream) => Self::Stream(stream.try_clone()?),
            Self::File(path) => Self::File(path.clone()),
            Self::Spooled(_) => return Err(io::Error::from(io::ErrorKind::Unsupported)),
        })
    }

    /// Opens the payload for reading. Only a `File` can fail here (it is
    /// opened and stated); the other two are already in hand.
    pub fn into_reader(self) -> io::Result<PayloadReader> {
        Ok(match self {
            Self::Memory(data) => PayloadReader::Memory(Cursor::new(data)),
            Self::Stream(stream) => PayloadReader::Stream(stream),
            Self::File(path) => {
                let file = File::open(&path)?;
                let len = file.metadata().ok().map(|m| m.len());
                PayloadReader::File { file, len }
            }
            // The open descriptor outlives the unlink that dropping `spooled`
            // performs, so the reader stays valid and nothing is left on disk.
            Self::Spooled(spooled) => {
                let file = File::open(spooled.path())?;
                PayloadReader::File { file, len: Some(spooled.len()) }
            }
        })
    }

    /// Materialises the payload as at most `limit` bytes.
    ///
    /// `Memory` is validated and returned as is -- no copy, no allocation.
    /// A `File` larger than `limit` is refused from its size alone, before any
    /// byte is read. Otherwise this is [`read_bounded`] over the reader.
    pub fn read_bounded(self, limit: u64) -> Result<Vec<u8>, PayloadError> {
        match self {
            Self::Memory(data) => {
                if data.is_empty() {
                    Err(PayloadError::Empty)
                } else if data.len() as u64 > limit {
                    Err(PayloadError::TooLarge { limit })
                } else {
                    Ok(data)
                }
            }
            Self::Stream(stream) => {
                let hint = stream.len_hint();
                read_bounded(stream, limit, hint)
            }
            Self::File(path) => {
                let file = File::open(&path)?;
                let len = file.metadata().ok().map(|m| m.len());
                if len.is_some_and(|len| len > limit) {
                    return Err(PayloadError::TooLarge { limit });
                }
                read_bounded(file, limit, len)
            }
            Self::Spooled(spooled) => {
                if spooled.len() > limit {
                    return Err(PayloadError::TooLarge { limit });
                }
                let file = File::open(spooled.path())?;
                read_bounded(file, limit, Some(spooled.len()))
            }
        }
    }
}

// --- SpooledFile ---

/// A complete payload on disk in the spool directory, with its fingerprint,
/// owned by this value: dropping it unlinks the file.
///
/// It is produced by the ingestion thread after it has drained a compositor
/// pipe with `splice(2)` (see `core::splice`) and consumed by the daemon's
/// worker, which either adopts the file into the content cache with a
/// `rename(2)` ([`SpooledFile::into_path`] disarms the unlink) or lets it drop.
/// Neither the queue nor the worker therefore ever holds the payload's bytes on
/// the heap, and a payload that is shed, rejected or dropped on error cannot
/// leak a file.
#[derive(Debug)]
pub struct SpooledFile {
    path: PathBuf,
    len: u64,
    digest: [u8; DIGEST_LEN],
    owned: bool,
}

impl SpooledFile {
    /// Takes ownership of the file at `path`, which must already hold exactly
    /// `len` bytes whose SHA3-256 is `digest`.
    pub fn new(path: PathBuf, len: u64, digest: [u8; DIGEST_LEN]) -> Self {
        Self { path, len, digest, owned: true }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn digest(&self) -> &[u8; DIGEST_LEN] {
        &self.digest
    }

    /// The fingerprint as the lowercase hexadecimal string the database keys on.
    pub fn hex_digest(&self) -> String {
        to_hex(&self.digest)
    }

    /// Gives up ownership: the file is no longer deleted on drop, and the
    /// caller is responsible for it (typically by renaming it into the cache).
    pub fn into_path(mut self) -> PathBuf {
        self.owned = false;
        std::mem::take(&mut self.path)
    }
}

impl Drop for SpooledFile {
    fn drop(&mut self) {
        if self.owned {
            // Best effort: the file may already be gone (a cache wipe, a sweep).
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Lowercase hexadecimal encoding of `bytes`, in a single allocation.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

// --- PayloadReader ---

/// [`Read`] over any [`Payload`] by static dispatch: one `match` per call, no
/// vtable and no allocation. (`Memory` owns its `Vec` inside a `Cursor`, so
/// building a reader from a buffer moves the buffer and copies nothing.)
#[derive(Debug)]
pub enum PayloadReader {
    Memory(Cursor<Vec<u8>>),
    Stream(StreamDescriptor),
    /// `len` is the size when the file was opened (`None` if it could not be
    /// stated).
    File { file: File, len: Option<u64> },
}

impl PayloadReader {
    /// The underlying descriptor, or `None` for an in-memory source (which has
    /// none). Never panics and never opens anything.
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        match self {
            Self::Memory(_) => None,
            Self::Stream(stream) => Some(stream.as_fd()),
            Self::File { file, .. } => Some(file.as_fd()),
        }
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        self.fd().map(|fd| fd.as_raw_fd())
    }

    /// Total length when known: the buffer's, the descriptor's hint, or the
    /// file's size at open.
    pub fn len_hint(&self) -> Option<u64> {
        match self {
            Self::Memory(cursor) => Some(cursor.get_ref().len() as u64),
            Self::Stream(stream) => stream.len_hint(),
            Self::File { len, .. } => *len,
        }
    }
}

impl Read for PayloadReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Memory(cursor) => cursor.read(buf),
            Self::Stream(stream) => stream.read(buf),
            Self::File { file, .. } => file.read(buf),
        }
    }
}

// --- Bounded reads ---

/// Why a payload could not be materialised.
#[derive(Debug)]
pub enum PayloadError {
    /// Nothing was read: an empty clipboard is not a payload.
    Empty,
    /// More than `limit` bytes were available. The bytes read so far are
    /// discarded rather than kept as a silently truncated payload.
    TooLarge { limit: u64 },
    Io(io::Error),
}

impl fmt::Display for PayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("payload is empty"),
            Self::TooLarge { limit } => write!(f, "payload exceeds the {limit}-byte limit"),
            Self::Io(e) => write!(f, "payload read failed: {e}"),
        }
    }
}

impl std::error::Error for PayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Empty | Self::TooLarge { .. } => None,
        }
    }
}

impl From<io::Error> for PayloadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Reads at most `limit` bytes from `reader`.
///
/// Takes one byte more than `limit` so a stream that genuinely exceeds it can
/// be told apart from one that exactly fits: `Read::take` stops silently at
/// its cap, so without the probe byte a truncated transfer and a complete one
/// would both look like `Ok`, and the truncated bytes would be persisted as if
/// they were the whole payload.
///
/// `capacity_hint` pre-sizes the buffer (clamped to `limit` and to
/// `PAYLOAD_PREALLOC_CAP_BYTES`, and skipped, not fatal, if the allocator
/// refuses): a hint equal to the real length makes the read a single
/// allocation. A wrong hint costs nothing but a different growth pattern.
pub fn read_bounded<R: Read>(reader: R, limit: u64, capacity_hint: Option<u64>) -> Result<Vec<u8>, PayloadError> {
    let mut payload = Vec::new();
    if let Some(hint) = capacity_hint {
        let cap = usize::try_from(hint.min(limit)).unwrap_or(usize::MAX).min(PAYLOAD_PREALLOC_CAP_BYTES);
        let _ = payload.try_reserve_exact(cap);
    }

    reader.take(limit.saturating_add(1)).read_to_end(&mut payload)?;

    if payload.is_empty() {
        Err(PayloadError::Empty)
    } else if payload.len() as u64 > limit {
        Err(PayloadError::TooLarge { limit })
    } else {
        Ok(payload)
    }
}

// --- Routing ---

/// Which road a payload takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadRoute {
    /// Fast path: read it into a heap buffer.
    Inline,
    /// Slow-and-large path: keep it on a descriptor and let the kernel move it.
    Stream,
}

/// Decides between the in-memory and the descriptor path.
///
/// 1. **A length hint decides.** It is the only first-hand evidence: at or
///    below `INLINE_PAYLOAD_THRESHOLD_BYTES` is `Inline`, above is `Stream`,
///    whatever the MIME says (a megabyte of text is still a megabyte).
/// 2. **No hint: the MIME is the prior.** Text-like types (plain text, markup,
///    JSON, URI lists...) are nearly always a few lines and go `Inline`;
///    anything else (images, archives, unknown binaries) may be a screenshot
///    and goes `Stream`.
///
/// Cheap by construction: a comparison when a hint exists, otherwise one
/// allocation-free MIME scan. It is a *heuristic* -- a wrong guess only picks
/// the slower of two correct paths, never a wrong result.
pub fn route_payload(mime: &str, len_hint: Option<u64>) -> PayloadRoute {
    match len_hint {
        Some(len) if len <= INLINE_PAYLOAD_THRESHOLD_BYTES as u64 => PayloadRoute::Inline,
        Some(_) => PayloadRoute::Stream,
        None if is_text_like_mime(mime) => PayloadRoute::Inline,
        None => PayloadRoute::Stream,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;

    const LIMIT: u64 = 1024;

    /// A pipe pre-loaded with `bytes` and with its writer closed, so the read
    /// end yields exactly them and then EOF.
    fn loaded_stream(bytes: &[u8], hint: Option<u64>) -> StreamDescriptor {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(bytes).unwrap();
        drop(writer);
        StreamDescriptor::new(OwnedFd::from(reader), hint)
    }

    fn temp_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("y4p-payload-test-{}-{tag}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    // --- read_bounded ---

    #[test]
    fn read_bounded_within_and_exactly_at_the_limit_returns_everything() {
        assert_eq!(read_bounded(&b"hello"[..], 10, None).unwrap(), b"hello");
        assert_eq!(read_bounded(&b"abcde"[..], 5, None).unwrap(), b"abcde");
    }

    #[test]
    fn read_bounded_one_byte_over_is_discarded_not_truncated() {
        assert!(matches!(read_bounded(&b"abcdef"[..], 5, None), Err(PayloadError::TooLarge { limit: 5 })));
    }

    #[test]
    fn read_bounded_stops_reading_at_the_cap_on_an_endless_stream() {
        let result = read_bounded(std::io::repeat(0x41), 64, None);
        assert!(matches!(result, Err(PayloadError::TooLarge { limit: 64 })));
    }

    #[test]
    fn read_bounded_empty_input_is_empty_not_an_empty_vec() {
        assert!(matches!(read_bounded(&b""[..], 10, None), Err(PayloadError::Empty)));
    }

    #[test]
    fn read_bounded_surfaces_a_read_error() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
        }
        assert!(matches!(read_bounded(Failing, 10, None), Err(PayloadError::Io(_))));
    }

    #[test]
    fn an_exact_hint_makes_the_read_a_single_allocation_and_a_lying_one_is_harmless() {
        let data = vec![7u8; 300];
        let exact = read_bounded(data.as_slice(), LIMIT, Some(300)).unwrap();
        assert_eq!(exact.len(), 300);
        assert_eq!(exact.capacity(), 300, "an exact hint must not over-allocate");

        // Far too small, and far too large (clamped, never trusted).
        assert_eq!(read_bounded(data.as_slice(), LIMIT, Some(1)).unwrap(), data);
        let huge = read_bounded(data.as_slice(), LIMIT, Some(u64::MAX)).unwrap();
        assert_eq!(huge, data);
        assert!(huge.capacity() as u64 <= LIMIT, "a hint above the limit must be clamped to it");
    }

    #[test]
    fn a_huge_limit_cannot_overflow_the_probe_byte() {
        assert_eq!(read_bounded(&b"x"[..], u64::MAX, None).unwrap(), b"x");
    }

    // --- StreamDescriptor ---

    #[test]
    fn a_stream_descriptor_reads_what_was_written_and_reports_its_hint() {
        let mut stream = loaded_stream(b"streamed", Some(8));
        assert_eq!(stream.len_hint(), Some(8));
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        assert_eq!(out, "streamed");
    }

    #[test]
    fn dropping_a_stream_descriptor_closes_it() {
        let (reader, writer) = std::io::pipe().unwrap();
        let stream = StreamDescriptor::new(OwnedFd::from(reader), None);
        drop(stream);
        // With the only reader gone, a write to the pipe must report EPIPE.
        let mut writer = writer;
        let err = writer.write_all(b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn a_cloned_stream_descriptor_keeps_the_pipe_open_until_both_are_dropped() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let first = StreamDescriptor::new(OwnedFd::from(reader), Some(1));
        let second = first.try_clone().unwrap();
        assert_eq!(second.len_hint(), Some(1));
        drop(first);
        writer.write_all(b"x").unwrap();
        drop(second);
        assert_eq!(writer.write_all(b"y").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn into_owned_fd_hands_the_same_descriptor_back() {
        let stream = loaded_stream(b"abc", None);
        let raw = stream.as_raw_fd();
        let fd = stream.into_owned_fd();
        assert_eq!(fd.as_raw_fd(), raw);
    }

    // --- Payload / PayloadReader ---

    #[test]
    fn payload_len_hints_need_no_io() {
        assert_eq!(Payload::memory(vec![0; 9]).len_hint(), Some(9));
        assert_eq!(Payload::Stream(loaded_stream(b"", Some(42))).len_hint(), Some(42));
        assert_eq!(Payload::Stream(loaded_stream(b"", None)).len_hint(), None);
        assert_eq!(Payload::File(PathBuf::from("/definitely/not/there")).len_hint(), None);
    }

    #[test]
    fn as_bytes_is_some_only_for_memory() {
        assert_eq!(Payload::memory(b"hi".to_vec()).as_bytes(), Some(&b"hi"[..]));
        assert!(Payload::Stream(loaded_stream(b"hi", None)).as_bytes().is_none());
        assert!(Payload::File(PathBuf::from("x")).as_bytes().is_none());
    }

    #[test]
    fn a_memory_reader_has_no_descriptor_and_never_panics_asking() {
        let reader = Payload::memory(b"abc".to_vec()).into_reader().unwrap();
        assert!(reader.fd().is_none());
        assert!(reader.raw_fd().is_none());
        assert_eq!(reader.len_hint(), Some(3));
    }

    #[test]
    fn stream_and_file_readers_expose_a_valid_descriptor() {
        let stream = Payload::Stream(loaded_stream(b"abc", Some(3))).into_reader().unwrap();
        assert!(stream.raw_fd().is_some_and(|fd| fd >= 0));
        assert!(stream.fd().is_some());

        let path = temp_file("fd", b"file bytes");
        let file = Payload::File(path.clone()).into_reader().unwrap();
        assert!(file.raw_fd().is_some_and(|fd| fd >= 0));
        assert_eq!(file.len_hint(), Some(10));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn every_reader_variant_yields_the_same_bytes() {
        let path = temp_file("same", b"same bytes");
        let payloads = [
            Payload::memory(b"same bytes".to_vec()),
            Payload::Stream(loaded_stream(b"same bytes", None)),
            Payload::File(path.clone()),
        ];
        for payload in payloads {
            let mut out = Vec::new();
            payload.into_reader().unwrap().read_to_end(&mut out).unwrap();
            assert_eq!(out, b"same bytes");
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn opening_a_missing_file_is_an_error_not_a_panic() {
        assert!(Payload::File(PathBuf::from("/definitely/not/there")).into_reader().is_err());
        assert!(matches!(
            Payload::File(PathBuf::from("/definitely/not/there")).read_bounded(LIMIT),
            Err(PayloadError::Io(_))
        ));
    }

    #[test]
    fn try_clone_of_a_memory_payload_is_independent() {
        let original = Payload::memory(b"abc".to_vec());
        let copy = original.try_clone().unwrap();
        assert_eq!(copy.as_bytes(), original.as_bytes());
        drop(original);
        assert_eq!(copy.as_bytes(), Some(&b"abc"[..]));
    }

    #[test]
    fn memory_read_bounded_returns_the_same_allocation() {
        let data = b"payload".to_vec();
        let ptr = data.as_ptr();
        let out = Payload::memory(data).read_bounded(LIMIT).unwrap();
        assert_eq!(out.as_ptr(), ptr, "an in-memory payload must be handed over, not copied");
    }

    #[test]
    fn memory_read_bounded_applies_the_same_rules_as_a_stream() {
        assert!(matches!(Payload::memory(Vec::new()).read_bounded(LIMIT), Err(PayloadError::Empty)));
        assert!(matches!(
            Payload::memory(vec![0; 11]).read_bounded(10),
            Err(PayloadError::TooLarge { limit: 10 })
        ));
        assert_eq!(Payload::memory(vec![1; 10]).read_bounded(10).unwrap().len(), 10);
    }

    #[test]
    fn stream_read_bounded_enforces_the_limit_even_when_the_hint_lies() {
        let lying = Payload::Stream(loaded_stream(&[9; 50], Some(3)));
        assert!(matches!(lying.read_bounded(10), Err(PayloadError::TooLarge { limit: 10 })));
        let honest = Payload::Stream(loaded_stream(&[9; 5], Some(1_000_000)));
        assert_eq!(honest.read_bounded(10).unwrap(), vec![9; 5]);
    }

    #[test]
    fn file_read_bounded_refuses_an_oversized_file_and_reads_a_fitting_one() {
        let path = temp_file("bounded", &[5; 64]);
        assert!(matches!(
            Payload::File(path.clone()).read_bounded(63),
            Err(PayloadError::TooLarge { limit: 63 })
        ));
        assert_eq!(Payload::File(path.clone()).read_bounded(64).unwrap(), vec![5; 64]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn payload_errors_display_something_useful() {
        assert!(PayloadError::Empty.to_string().contains("empty"));
        assert!(PayloadError::TooLarge { limit: 7 }.to_string().contains('7'));
        let io = PayloadError::from(io::Error::other("disk"));
        assert!(io.to_string().contains("disk"));
        assert!(std::error::Error::source(&io).is_some());
    }

    // --- route_payload ---

    #[test]
    fn a_length_hint_decides_at_the_threshold() {
        let t = INLINE_PAYLOAD_THRESHOLD_BYTES as u64;
        assert_eq!(route_payload("image/png", Some(0)), PayloadRoute::Inline);
        assert_eq!(route_payload("image/png", Some(t)), PayloadRoute::Inline, "the threshold itself is inline");
        assert_eq!(route_payload("image/png", Some(t + 1)), PayloadRoute::Stream);
    }

    #[test]
    fn a_length_hint_overrides_the_mime_in_both_directions() {
        let t = INLINE_PAYLOAD_THRESHOLD_BYTES as u64;
        assert_eq!(route_payload("text/plain", Some(t + 1)), PayloadRoute::Stream, "big text still streams");
        assert_eq!(route_payload("image/png", Some(16)), PayloadRoute::Inline, "a tiny image stays in memory");
    }

    #[test]
    fn without_a_hint_text_like_mimes_go_inline_and_the_rest_stream() {
        for mime in ["text/plain", "TEXT/HTML; charset=utf-8", "application/json", "text/uri-list", "UTF8_STRING"] {
            assert_eq!(route_payload(mime, None), PayloadRoute::Inline, "{mime}");
        }
        for mime in ["image/png", "image/svg+xml", "application/octet-stream", "video/mp4", ""] {
            assert_eq!(route_payload(mime, None), PayloadRoute::Stream, "{mime}");
        }
    }

    #[test]
    fn the_threshold_is_one_page() {
        assert_eq!(INLINE_PAYLOAD_THRESHOLD_BYTES, 4096);
    }

    // --- SpooledFile / Payload::Spooled ---

    fn spooled(tag: &str, bytes: &[u8]) -> SpooledFile {
        SpooledFile::new(temp_file(tag, bytes), bytes.len() as u64, [0xab; DIGEST_LEN])
    }

    #[test]
    fn dropping_a_spooled_file_unlinks_it() {
        let spool = spooled("drop", b"spooled");
        let path = spool.path().to_path_buf();
        assert!(path.exists());
        drop(spool);
        assert!(!path.exists(), "a dropped (shed or abandoned) spool file must not linger");
    }

    #[test]
    fn into_path_hands_the_file_over_and_disarms_the_unlink() {
        let spool = spooled("adopt", b"adopt me");
        let path = spool.into_path();
        assert!(path.exists(), "an adopted file must survive the guard");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_spooled_payload_reports_its_length_and_has_no_bytes_in_memory() {
        let payload = Payload::Spooled(Box::new(spooled("len", b"12345")));
        assert_eq!(payload.len_hint(), Some(5));
        assert!(payload.as_bytes().is_none());
        assert_eq!(payload.try_clone().unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn a_spooled_payload_is_materialised_within_the_bound_and_its_file_is_gone_after() {
        let spool = spooled("read", b"from the spool");
        let path = spool.path().to_path_buf();
        let data = Payload::Spooled(Box::new(spool)).read_bounded(LIMIT).unwrap();
        assert_eq!(data, b"from the spool");
        assert!(!path.exists());
    }

    #[test]
    fn a_spooled_payload_over_the_limit_is_refused_from_its_recorded_length() {
        let payload = Payload::Spooled(Box::new(spooled("big", &[7u8; 64])));
        assert!(matches!(payload.read_bounded(63), Err(PayloadError::TooLarge { limit: 63 })));
    }

    #[test]
    fn a_reader_over_a_spooled_payload_outlives_the_unlink() {
        let spool = spooled("reader", b"still readable");
        let path = spool.path().to_path_buf();
        let mut reader = Payload::Spooled(Box::new(spool)).into_reader().unwrap();
        assert!(!path.exists(), "the payload was consumed, so its file is unlinked");
        assert_eq!(reader.len_hint(), Some(14));
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "still readable");
    }

    #[test]
    fn a_spooled_payload_keeps_the_size_of_the_other_variants() {
        assert_eq!(std::mem::size_of::<Payload>(), 32, "the boxed variant must not widen every queue slot");
    }

    #[test]
    fn the_hex_digest_is_lowercase_and_full_width() {
        let mut digest = [0u8; DIGEST_LEN];
        digest[0] = 0x0f;
        digest[1] = 0xa0;
        digest[31] = 0xff;
        let spool = SpooledFile::new(PathBuf::from("/nonexistent/y4p"), 1, digest);
        let hex = spool.hex_digest();
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("0fa0") && hex.ends_with("ff"));
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x00, 0x7f, 0x80]), "007f80");
    }
}
