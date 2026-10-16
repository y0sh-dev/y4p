// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/spool.rs

//! The daemon's [`PayloadSpooler`]: drains a compositor pipe into a private file
//! with `splice(2)` while fingerprinting the stream with SHA3-256 as it passes.
//!
//! This is the one place the three concerns meet that the Wayland sensor layer
//! must not know about -- where the cache lives, how files are named, which
//! digest keys the database -- which is why the Wayland crate only defines the
//! [`PayloadSpooler`] trait and the daemon implements it. The copy mechanics
//! (kernel path, tee, fallbacks, bounds) are `core::splice`; this module adds
//! the file lifecycle around them:
//!
//! - files are created `0600` with `O_EXCL` (`create_new`) in a `0700`
//!   directory inside the cache directory, under a name no other file can have
//!   (`<pid>-<sequence>.part`);
//! - every failure path unlinks the file it created, so only a completed
//!   payload ever outlives the call, and then only as a [`SpooledFile`] that
//!   unlinks itself unless the worker adopts it;
//! - debris of a crashed run (a `.part` file whose process is gone) is swept
//!   when the spooler opens.
//!
//! The same file lifecycle serves the second half of the pipeline:
//! [`transcode_spooled`] turns one spool file into another through the image
//! converter, hashing the output as it is written.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha3::{Digest, Sha3_256};

use crate::core::constants::{DIGEST_LEN, SPOOL_FILE_EXTENSION};
use crate::core::payload::{PayloadError, SpooledFile};
use crate::core::splice::{self, ByteTap};
use crate::image::pipeline::PipelineError;
use crate::image::transcode::{transcode_file, Transcode};
use crate::wayland::spool::{PayloadSpooler, SpoolOutcome};

/// SHA3-256 as a [`ByteTap`]: the fingerprint is finished the moment the last
/// byte has been moved, with no second pass over the data.
struct Sha3Tap(Sha3_256);

impl ByteTap for Sha3Tap {
    fn update(&mut self, chunk: &[u8]) {
        Digest::update(&mut self.0, chunk);
    }
}

impl Sha3Tap {
    fn new() -> Self {
        Self(Sha3_256::new())
    }

    fn finish(self) -> Option<[u8; DIGEST_LEN]> {
        <[u8; DIGEST_LEN]>::try_from(self.0.finalize().as_slice()).ok()
    }
}

/// Unlinks the file at its path on drop unless disarmed: what keeps a failed or
/// rejected spool from leaving a file behind.
struct Unlink(Option<PathBuf>);

impl Unlink {
    fn disarm(mut self) -> Option<PathBuf> {
        self.0.take()
    }
}

impl Drop for Unlink {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Spools payloads into one directory. Shared by all ingestion threads.
pub struct FileSpooler {
    dir: PathBuf,
    next: AtomicU64,
}

impl FileSpooler {
    /// Prepares `dir` (created `0700` if missing), sweeps debris left by dead
    /// processes, and returns a spooler writing there.
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        sweep_stale(&dir);
        Ok(Self { dir, next: AtomicU64::new(0) })
    }

    fn fresh_path(&self) -> PathBuf {
        let seq = self.next.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!("{}-{}.{}", std::process::id(), seq, SPOOL_FILE_EXTENSION))
    }

    fn create(&self) -> io::Result<(File, PathBuf)> {
        let path = self.fresh_path();
        let open = || OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path);
        let file = match open() {
            // A cache wipe (`y4p wipe`) removes the whole cache directory, spool
            // directory included. Re-create it rather than leave every later
            // large payload on the in-memory road until the next restart.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
                open()?
            }
            other => other?,
        };
        Ok((file, path))
    }
}

impl PayloadSpooler for FileSpooler {
    fn spool(&self, head: &[u8], mut src: File, limit: u64) -> SpoolOutcome {
        let Ok((mut file, path)) = self.create() else { return SpoolOutcome::Unavailable(src) };
        let guard = Unlink(Some(path));

        let head_len = head.len() as u64;
        if head_len > limit {
            return SpoolOutcome::Rejected;
        }
        let mut tap = Sha3Tap::new();
        tap.update(head);
        if file.write_all(head).is_err() {
            return SpoolOutcome::Rejected;
        }

        // The remainder, after the head, within what is left of the limit. An
        // empty remainder is fine: the payload was exactly the head.
        let rest = match splice::copy_to_file(&mut src, &mut file, limit - head_len, &mut tap) {
            Ok(copied) => copied.len,
            Err(PayloadError::Empty) => 0,
            Err(_) => return SpoolOutcome::Rejected,
        };

        let (Some(digest), Some(path)) = (tap.finish(), guard.disarm()) else { return SpoolOutcome::Rejected };
        SpoolOutcome::Spooled(SpooledFile::new(path, head_len + rest, digest))
    }
}

/// Converts the spooled payload `source` into a new spool file, descriptor to
/// descriptor, and returns that file with the digest of what was written.
///
/// `source` is handed to the converter as its stdin and the converter's stdout
/// is spliced into the new file while a [`Sha3Tap`] fingerprints it, so neither
/// the original nor the converted bytes ever enter this process's heap (see
/// `y4p_image::transcode::transcode_file` for the mechanics and the fallbacks).
/// `limit` bounds the converted file, as `MAX_PAYLOAD_BYTES` bounds ingestion.
///
/// The result is what an ingested payload of that content would have been: a
/// private `0600` file in the spool directory that deletes itself on drop and
/// whose digest keys the database. `source` is left exactly as it was, so a
/// failed conversion (any `Err`; the half-written file is unlinked here) leaves
/// the caller free to store the original. Deleting `source` after a successful
/// one is the caller's decision.
///
/// The new file is named after its source (`<pid>-<n>-t.part`): unique because
/// the source name is, and still parsed to the owning pid by [`sweep_stale`], so
/// a crash between the two files leaves nothing a later start cannot sweep.
/// Failures to open either file are `Source`/`Sink` errors, which the image
/// router treats as the host's problem rather than the converter's.
pub fn transcode_spooled(source: &SpooledFile, job: Transcode, limit: u64) -> Result<SpooledFile, PipelineError> {
    let input = File::open(source.path()).map_err(PipelineError::Source)?;
    let (mut output, path) = create_derived(source.path()).map_err(PipelineError::Sink)?;
    let guard = Unlink(Some(path));

    let mut tap = Sha3Tap::new();
    let transfer = transcode_file(job, input, &mut output, limit, &mut tap)?;

    let (Some(digest), Some(path)) = (tap.finish(), guard.disarm()) else {
        return Err(PipelineError::Sink(io::Error::other("the converted payload's digest was unavailable")));
    };
    Ok(SpooledFile::new(path, transfer.bytes_out, digest))
}

/// Creates, `0600` and exclusively, the file derived from the spool file at
/// `source`: same directory, `<stem>-t.part`. Opened for reading as well as
/// writing: the converter checks the first bytes it wrote.
fn create_derived(source: &Path) -> io::Result<(File, PathBuf)> {
    let (Some(dir), Some(stem)) = (source.parent(), source.file_stem().and_then(|s| s.to_str())) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a spool file path"));
    };
    let path = dir.join(format!("{}-t.{}", stem, SPOOL_FILE_EXTENSION));
    let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
    Ok((file, path))
}

/// Removes `.part` files in `dir` that no live process owns: names that do not
/// parse, and names whose process id is gone. A live sibling's spool file (a
/// second daemon during a restart) is never touched, nor is one under our own
/// pid, which may belong to a spooler already running in this process.
fn sweep_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(SPOOL_FILE_EXTENSION) {
            continue;
        }
        let owner = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok());
        let orphaned = match owner {
            Some(pid) => pid != std::process::id() as i32 && !process_exists(pid),
            None => true,
        };
        if orphaned {
            let _ = fs::remove_file(path);
        }
    }
}

/// Whether process `pid` exists (signal 0 delivers nothing, it only checks).
/// `EPERM` means it exists but belongs to someone else.
fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill(pid, 0)` sends no signal; it only reports whether `pid`
    // could be signalled, and takes no pointers.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::payload::to_hex;
    use std::io::Read;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("y4p-spool-test-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 7 + i / 13) % 251) as u8).collect()
    }

    fn reference_digest(bytes: &[u8]) -> String {
        to_hex(Sha3_256::digest(bytes).as_slice())
    }

    /// A pipe fed from a thread, as a compositor client would.
    fn fed_pipe(bytes: Vec<u8>) -> (File, std::thread::JoinHandle<()>) {
        let (reader, mut writer) = io::pipe().unwrap();
        let handle = std::thread::spawn(move || {
            let _ = writer.write_all(&bytes);
        });
        (File::from(std::os::fd::OwnedFd::from(reader)), handle)
    }

    fn files_in(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    /// Splits `bytes` the way the ingestion thread does: a head, then the rest on a pipe.
    fn spool_split(spooler: &FileSpooler, bytes: &[u8], head_len: usize, limit: u64) -> SpoolOutcome {
        let (head, rest) = bytes.split_at(head_len.min(bytes.len()));
        let (src, feeder) = fed_pipe(rest.to_vec());
        let outcome = spooler.spool(head, src, limit);
        let _ = feeder.join();
        outcome
    }

    #[test]
    fn a_pipe_is_spooled_with_the_digest_of_the_whole_stream() {
        let dir = temp_dir("digest");
        let spooler = FileSpooler::open(dir.clone()).unwrap();
        let bytes = data(2 * 1024 * 1024 + 5);

        let SpoolOutcome::Spooled(spooled) = spool_split(&spooler, &bytes, 4096, 64 << 20) else { panic!("not spooled") };

        assert_eq!(spooled.len(), bytes.len() as u64);
        assert_eq!(spooled.hex_digest(), reference_digest(&bytes), "the streaming digest must equal a one-shot SHA3-256");
        assert_eq!(fs::read(spooled.path()).unwrap(), bytes, "head and spliced remainder must be contiguous");
        drop(spooled);
        assert_eq!(files_in(&dir), 0, "dropping the spooled payload removes its file");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_payload_that_is_exactly_the_head_is_a_valid_payload() {
        let dir = temp_dir("exact");
        let spooler = FileSpooler::open(dir.clone()).unwrap();
        let bytes = data(4096);
        let SpoolOutcome::Spooled(spooled) = spool_split(&spooler, &bytes, 4096, 1 << 20) else { panic!("not spooled") };
        assert_eq!((spooled.len(), spooled.hex_digest()), (4096, reference_digest(&bytes)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stream_over_the_limit_is_rejected_and_leaves_no_file() {
        let dir = temp_dir("over");
        let spooler = FileSpooler::open(dir.clone()).unwrap();
        let bytes = data(300_000);
        assert!(matches!(spool_split(&spooler, &bytes, 4096, 200_000), SpoolOutcome::Rejected));
        assert_eq!(files_in(&dir), 0, "a rejected payload must not leave its partial file");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_limit_counts_the_head_too() {
        let dir = temp_dir("headcount");
        let spooler = FileSpooler::open(dir.clone()).unwrap();
        let bytes = data(10_000);
        assert!(matches!(spool_split(&spooler, &bytes, 4096, 10_000), SpoolOutcome::Spooled(_)), "exactly at the limit fits");
        assert!(matches!(spool_split(&spooler, &bytes, 4096, 9_999), SpoolOutcome::Rejected), "one byte over does not");
        assert!(matches!(spool_split(&spooler, &bytes, 4096, 100), SpoolOutcome::Rejected), "a head over the limit is rejected up front");
        assert_eq!(files_in(&dir), 0, "the spooled payload was dropped by the assertion and every rejected one cleaned up after itself");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_spool_directory_removed_by_a_cache_wipe_is_recreated() {
        let dir = temp_dir("wiped");
        let spooler = FileSpooler::open(dir.clone()).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        let bytes = data(10_000);
        let SpoolOutcome::Spooled(spooled) = spool_split(&spooler, &bytes, 4096, 1 << 20) else { panic!("not spooled") };
        assert_eq!(fs::read(spooled.path()).unwrap(), bytes);
        drop(spooled);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unusable_spool_directory_gives_the_pipe_back() {
        let dir = temp_dir("unusable");
        let spooler = FileSpooler::open(dir.clone()).unwrap();
        // Replace the directory by a regular file: no file can be created in it
        // and it cannot be re-created either.
        fs::remove_dir_all(&dir).unwrap();
        fs::write(&dir, b"in the way").unwrap();
        let (src, feeder) = fed_pipe(data(10_000));
        match spooler.spool(&data(4096), src, 1 << 20) {
            SpoolOutcome::Unavailable(mut pipe) => {
                let mut rest = Vec::new();
                pipe.read_to_end(&mut rest).unwrap();
                assert_eq!(rest.len(), 10_000, "the pipe must be untouched, ready for an in-memory read");
            }
            other => panic!("expected the pipe back, got {other:?}"),
        }
        feeder.join().unwrap();
        fs::remove_file(dir).unwrap();
    }

    #[test]
    fn spool_files_are_private_and_uniquely_named() {
        let dir = temp_dir("private");
        let spooler = FileSpooler::open(dir.clone()).unwrap();
        let bytes = data(8192);
        let (SpoolOutcome::Spooled(a), SpoolOutcome::Spooled(b)) =
            (spool_split(&spooler, &bytes, 4096, 1 << 20), spool_split(&spooler, &bytes, 4096, 1 << 20))
        else {
            panic!("not spooled")
        };
        assert_ne!(a.path(), b.path());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(a.path()).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        drop((a, b));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opening_sweeps_debris_of_dead_processes_but_not_of_live_ones_or_other_files() {
        let dir = temp_dir("sweep");
        fs::create_dir_all(&dir).unwrap();
        let mine = dir.join(format!("{}-99.part", std::process::id()));
        // Process 1 always exists and is never us: a live sibling's file.
        let sibling = dir.join("1-5.part");
        let dead = dir.join("2147483646-1.part");
        let junk = dir.join("not-a-pid.part");
        let cached = dir.join("0123abcd.cache");
        for p in [&mine, &sibling, &dead, &junk, &cached] {
            fs::write(p, b"x").unwrap();
        }

        let _spooler = FileSpooler::open(dir.clone()).unwrap();

        assert!(!dead.exists(), "a dead process's spool file is debris");
        assert!(!junk.exists(), "an unparseable .part file is debris");
        assert!(mine.exists(), "a file under our own pid may belong to a spooler already running here");
        assert!(sibling.exists(), "a live process's file must be left alone");
        assert!(cached.exists(), "only .part files are ever swept");
        fs::remove_dir_all(dir).unwrap();
    }

    // --- transcode_spooled: spool file -> spool file through the converter ---

    use crate::image::transcode::{InputFormat, OutputFormat};

    const PNG_TO_PNG: Transcode = Transcode { input: InputFormat::Png, output: OutputFormat::Png };

    fn magick_available() -> bool {
        if crate::image::magick::is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    fn generated_png() -> Vec<u8> {
        let mut out = Vec::new();
        crate::image::magick::run(
            &["-size", "128x128", "plasma:fractal", "-depth", "8", "-set", "comment", "SECRETCOMMENT", "png:-"],
            io::empty(),
            &mut out,
        )
        .unwrap();
        out
    }

    /// Writes `bytes` as a finished spool file named like the spooler would.
    fn spooled_file(dir: &Path, name: &str, bytes: &[u8]) -> SpooledFile {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        let digest = <[u8; DIGEST_LEN]>::try_from(Sha3_256::digest(bytes).as_slice()).unwrap();
        SpooledFile::new(path, bytes.len() as u64, digest)
    }

    #[test]
    fn a_spooled_png_is_converted_into_a_second_spool_file_with_its_own_digest() {
        if !magick_available() { return; }
        let dir = temp_dir("convert");
        let png = generated_png();
        let source = spooled_file(&dir, &format!("{}-1.part", std::process::id()), &png);

        let converted = transcode_spooled(&source, PNG_TO_PNG, 64 << 20).unwrap();

        let stored = fs::read(converted.path()).unwrap();
        assert_ne!(converted.path(), source.path());
        assert_eq!(converted.path().parent(), source.path().parent(), "the new file lives beside its source");
        assert!(converted.path().to_string_lossy().ends_with("-1-t.part"), "{:?}", converted.path());
        assert_eq!(converted.len(), stored.len() as u64);
        assert_eq!(converted.hex_digest(), reference_digest(&stored), "the digest must describe the converted bytes, not the original");
        assert_ne!(converted.hex_digest(), source.hex_digest());
        assert!(!stored.windows(13).any(|w| w == b"SECRETCOMMENT"), "metadata must be stripped");
        assert_eq!(fs::read(source.path()).unwrap(), png, "the source is left exactly as it was");

        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(converted.path()).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(files_in(&dir), 2);
        drop((converted, source));
        assert_eq!(files_in(&dir), 0, "both files delete themselves on drop");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_converted_file_is_byte_identical_to_what_the_in_memory_converter_makes() {
        // The digest keys deduplication: the two roads must not disagree.
        if !magick_available() { return; }
        let dir = temp_dir("agree");
        let png = generated_png();
        let source = spooled_file(&dir, &format!("{}-2.part", std::process::id()), &png);
        let mut expected = Vec::new();
        crate::image::transcode::transcode(PNG_TO_PNG, io::Cursor::new(png), &mut expected).unwrap();

        let converted = transcode_spooled(&source, PNG_TO_PNG, 64 << 20).unwrap();

        assert!(fs::read(converted.path()).unwrap() == expected);
        assert_eq!(converted.hex_digest(), reference_digest(&expected));
        drop((converted, source));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_conversion_leaves_the_source_alone_and_no_second_file() {
        if !magick_available() { return; }
        let dir = temp_dir("fail");
        let mut corrupt = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        corrupt.extend(data(50_000));
        let source = spooled_file(&dir, &format!("{}-3.part", std::process::id()), &corrupt);

        let result = transcode_spooled(&source, PNG_TO_PNG, 64 << 20);

        assert!(matches!(result, Err(PipelineError::Failed { .. })), "{result:?}");
        assert_eq!(files_in(&dir), 1, "only the source remains: the half-written file is unlinked");
        assert_eq!(fs::read(source.path()).unwrap(), corrupt);
        drop(source);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_converted_file_over_the_limit_is_refused_and_unlinked() {
        if !magick_available() { return; }
        let dir = temp_dir("limit");
        let source = spooled_file(&dir, &format!("{}-4.part", std::process::id()), &generated_png());

        let result = transcode_spooled(&source, PNG_TO_PNG, 100);

        assert!(matches!(result, Err(PipelineError::Sink(_))), "{result:?}");
        assert_eq!(files_in(&dir), 1);
        drop(source);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_name_collision_is_an_error_that_never_touches_the_existing_file() {
        let dir = temp_dir("collide");
        let source = spooled_file(&dir, &format!("{}-5.part", std::process::id()), b"source");
        let squatter = dir.join(format!("{}-5-t.part", std::process::id()));
        fs::write(&squatter, b"someone else's").unwrap();

        let result = transcode_spooled(&source, PNG_TO_PNG, 1 << 20);

        match result {
            Err(PipelineError::Sink(e)) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
            other => panic!("expected a sink error, got {other:?}"),
        }
        assert_eq!(fs::read(&squatter).unwrap(), b"someone else's", "O_EXCL: an existing file is never opened, let alone removed");
        drop(source);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_source_is_a_source_error_and_creates_nothing() {
        let dir = temp_dir("nosource");
        let source = spooled_file(&dir, &format!("{}-6.part", std::process::id()), b"source");
        fs::remove_file(source.path()).unwrap();
        let result = transcode_spooled(&source, PNG_TO_PNG, 1 << 20);
        assert!(matches!(result, Err(PipelineError::Source(_))), "{result:?}");
        assert_eq!(files_in(&dir), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_sweep_treats_converted_files_like_any_other_spool_file() {
        // `<pid>-<n>-t.part` must still parse to its owner: a crash between the
        // two files has to be recoverable.
        let dir = temp_dir("sweep-derived");
        fs::create_dir_all(&dir).unwrap();
        let dead = dir.join("2147483646-1-t.part");
        let sibling = dir.join("1-5-t.part");
        for p in [&dead, &sibling] {
            fs::write(p, b"x").unwrap();
        }
        let _spooler = FileSpooler::open(dir.clone()).unwrap();
        assert!(!dead.exists());
        assert!(sibling.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
