// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/pipeline.rs

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use crate::core::constants::{PIPELINE_CHUNK_BYTES, PIPELINE_STDERR_CAP_BYTES};
use crate::core::payload::PayloadError;
use crate::core::splice::{self, ByteTap};

/// Why a filter run failed. Every variant is recoverable: nothing in this
/// module panics, so a bad image or a dying child can never take a resident
/// daemon (`panic = "abort"`) down with it.
#[derive(Debug)]
pub enum PipelineError {
    /// The child could not be started (binary missing, not executable, ...).
    Spawn(io::Error),
    /// Reading the caller's input stream failed.
    Source(io::Error),
    /// Writing to the caller's output stream failed.
    Sink(io::Error),
    /// I/O with the child's pipes, or waiting on it, failed.
    Child(io::Error),
    /// The child ran but exited unsuccessfully (non-zero code, or killed by
    /// a signal when `code` is `None`). `stderr` is a capped excerpt.
    Failed { code: Option<i32>, stderr: String },
    /// The child outlived its wall-clock budget and was killed (together
    /// with any processes it had spawned). Carries the budget that was exceeded.
    Timeout(Duration),
    /// The isolated environment the child must run in (a private scratch
    /// directory) could not be prepared, so nothing was started. Like
    /// `Spawn` this says nothing about the data or the tool's health.
    Isolation(io::Error),
}

impl PipelineError {
    /// True when the tool itself could not be spawned (absent, wrong architecture,
    /// invalid binary format, permission denied, or OS spawn failure) — the caller's
    /// cue to fall back to raw persistence rather than treating it as input rejection.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Spawn(_))
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to start external process: {e}"),
            Self::Source(e) => write!(f, "failed to read pipeline input: {e}"),
            Self::Sink(e) => write!(f, "failed to write pipeline output: {e}"),
            Self::Child(e) => write!(f, "pipe I/O with external process failed: {e}"),
            Self::Failed { code: Some(code), stderr } => write!(f, "external process exited with status {code}: {stderr}"),
            Self::Failed { code: None, stderr } => write!(f, "external process was terminated by a signal: {stderr}"),
            Self::Timeout(limit) => write!(f, "external process exceeded its {}s time limit and was killed", limit.as_secs_f64()),
            Self::Isolation(e) => write!(f, "failed to prepare an isolated environment for the external process: {e}"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// The complete environment a child runs with. Applying it first discards
/// every inherited variable, then sets exactly the listed ones, so nothing in
/// the daemon's own environment (`MAGICK_CONFIGURE_PATH`, `LD_PRELOAD`,
/// `HOME`, ...) can steer how the child loads configuration or code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildEnv {
    vars: Vec<(OsString, OsString)>,
}

impl ChildEnv {
    /// An empty environment: the child sees no variables at all.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) one variable.
    pub fn set(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let (key, value) = (key.into(), value.into());
        self.vars.retain(|(existing, _)| *existing != key);
        self.vars.push((key, value));
        self
    }

    pub fn vars(&self) -> &[(OsString, OsString)] {
        &self.vars
    }

    /// Replaces `command`'s environment with this one.
    pub fn apply(&self, command: &mut Command) {
        command.env_clear();
        command.envs(self.vars.iter().map(|(key, value)| (key, value)));
    }
}

/// Byte counts of a completed run. `bytes_in` counts what the child
/// accepted on stdin, which is less than the input's length if the child
/// stopped reading early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    pub bytes_in: u64,
    pub bytes_out: u64,
}

enum FeedFailure {
    Source(io::Error),
    Child(io::Error),
}

enum RelayFailure {
    Sink(io::Error),
    Child(io::Error),
}

/// Copies `source` into the child's stdin, then drops it so the child sees EOF.
///
/// A `BrokenPipe` on the write is not an error here: it means the child
/// stopped reading (it finished, or rejected the input and exited), and the
/// exit status — inspected by the caller — is the authoritative verdict.
fn feed<R: Read, W: Write>(mut source: R, mut stdin: W) -> Result<u64, FeedFailure> {
    let mut chunk = vec![0u8; PIPELINE_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let n = match source.read(&mut chunk) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(FeedFailure::Source(e)),
        };
        match stdin.write_all(&chunk[..n]) {
            Ok(()) => total += n as u64,
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(total),
            Err(e) => return Err(FeedFailure::Child(e)),
        }
    }
}

/// Copies the child's stdout into the caller's sink, then flushes it.
fn relay<R: Read, W: Write>(mut from: R, to: &mut W) -> Result<u64, RelayFailure> {
    let mut chunk = vec![0u8; PIPELINE_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(RelayFailure::Child(e)),
        };
        to.write_all(&chunk[..n]).map_err(RelayFailure::Sink)?;
        total += n as u64;
    }
    to.flush().map_err(RelayFailure::Sink)?;
    Ok(total)
}

/// Reads `from` to EOF but keeps only the first `cap` bytes. Draining to EOF
/// is the point: a child that fills the stderr pipe while nobody reads it
/// blocks forever, which would stall the whole pipeline.
fn drain_capped<R: Read>(mut from: R, cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    kept
}

// `try_splice` and `splice_all` live in `core::splice` beside the rest of the
// kernel-copy machinery; re-exported so existing callers keep their path.
pub use crate::core::splice::{splice_all, try_splice};

/// SIGKILLs the child's whole process group. The child is started as the
/// leader of its own group (`process_group(0)`, so its pgid is its pid),
/// which makes this reach any helper processes it spawned too. That matters
/// beyond tidiness: a surviving grandchild would keep the stdout/stderr
/// pipes open, so the reader threads would never see EOF and the pipeline
/// would still hang after the "kill".
///
/// Callers must only use this while the child has not been reaped (see
/// `Watchdog`): until then the pid is reserved by the kernel and cannot name
/// an unrelated process.
fn kill_process_group(pid: u32) {
    let Ok(pgid) = i32::try_from(pid) else { return };
    // SAFETY: `kill(2)` takes plain integers and has no memory-safety
    // preconditions; a negative pid addresses the process group `pgid`.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Blocks until the child has exited, *without* reaping it. The zombie keeps
/// its pid reserved, so a watchdog that is still about to signal that pid
/// cannot hit a recycled one; the actual reap (`Child::wait`) happens only
/// after the watchdog has been told to stand down.
fn wait_exit_without_reaping(pid: u32) {
    // SAFETY: an all-zero `siginfo_t` is a valid value for `waitid` to
    // overwrite; it is only ever written to, never read.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `info` is a valid, exclusive out-parameter for the call,
        // and `P_PID` with a pid we spawned and have not yet reaped is valid.
        let rc = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 || io::Error::last_os_error().kind() != ErrorKind::Interrupted {
            return;
        }
    }
}

/// Best-effort termination plus reaping, so an abandoned child never lingers
/// as a zombie or keeps a pipe end open.
fn reap(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchState {
    Running,
    /// The child exited on its own (or was killed for another reason) and
    /// the watchdog must not signal anything any more.
    Finished,
    /// The deadline passed and the watchdog killed the child's group.
    TimedOut,
}

/// Wall-clock deadline for one child. The state lives under a mutex so that
/// "deadline passed, kill" and "child finished, stand down" are mutually
/// exclusive: the kill only ever happens while the state is `Running`, and
/// the main thread flips it to `Finished` *before* reaping. Combined with
/// `wait_exit_without_reaping`, the pid is guaranteed to still belong to our
/// (possibly zombie) child whenever a kill is sent.
struct Watchdog {
    state: Mutex<WatchState>,
    wake: Condvar,
}

impl Watchdog {
    fn new() -> Self {
        Self { state: Mutex::new(WatchState::Running), wake: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, WatchState> {
        // A poisoned lock only means another thread panicked; the plain-enum
        // state inside is still valid, so carry on rather than propagate.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Body of the watchdog thread: sleeps until the deadline or until told
    /// to stand down, and kills the process group only in the former case.
    fn supervise(&self, pid: u32, timeout: Duration) {
        let (mut state, _) = self
            .wake
            .wait_timeout_while(self.lock(), timeout, |s| *s == WatchState::Running)
            .unwrap_or_else(PoisonError::into_inner);
        if *state == WatchState::Running {
            kill_process_group(pid);
            *state = WatchState::TimedOut;
        }
    }

    /// Stands the watchdog down. Returns whether it had already fired.
    fn finish(&self) -> bool {
        let mut state = self.lock();
        if *state == WatchState::Running {
            *state = WatchState::Finished;
        }
        let timed_out = *state == WatchState::TimedOut;
        drop(state);
        self.wake.notify_all();
        timed_out
    }
}

fn join_worker<T>(handle: thread::ScopedJoinHandle<'_, T>) -> Result<T, PipelineError> {
    handle
        .join()
        .map_err(|_| PipelineError::Child(io::Error::other("pipeline worker thread panicked")))
}

/// Runs `program args...` as a filter: `input` is streamed into its stdin
/// and its stdout is streamed into `output`, neither side ever buffered
/// whole in memory (at most one `PIPELINE_CHUNK_BYTES` chunk per direction).
///
/// # Deadlock freedom
///
/// A pipe holds roughly 64KiB. If the parent wrote all of `input` before
/// reading any output, a filter that emits output while it is still
/// consuming input would fill its stdout pipe and block; the parent would
/// in turn block on the full stdin pipe, and neither could ever proceed.
/// To rule that out structurally, three activities always run concurrently:
///
/// 1. a scoped worker feeds `input` into the child's stdin,
/// 2. a scoped worker drains the child's stderr (capped excerpt kept),
/// 3. the calling thread relays the child's stdout into `output`.
///
/// Each pipe therefore always has a live peer, whatever order the child
/// reads and writes in. The chunk buffers are the price of the `Read`/`Write`
/// boundary; when both ends are descriptors, [`run_filter_file_in_env`] runs the
/// same child with no buffer and no feeder thread.
///
/// # Failure semantics
///
/// On `Err`, `output` may already hold partial bytes and must be discarded.
/// When several things go wrong at once, the most causal one is reported:
/// sink and source failures (the caller's own streams), then child pipe
/// failures, then the child's exit status. If the sink fails the child is
/// killed, because nobody will read its output any more.
pub fn run_filter<R, W>(program: &str, args: &[&str], input: R, output: &mut W) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    run_filter_to_writer(program, args, None, input, output, None)
}

/// [`run_filter`] with a wall-clock budget. If the child (or anything it
/// spawned) is still running `timeout` after the start, its whole process
/// group is SIGKILLed: every pipe closes, all three workers unblock, the
/// child is reaped, and the run ends with [`PipelineError::Timeout`] —
/// taking precedence over the secondary symptoms of the kill (a signal exit
/// status, broken pipes). A run that finishes in time is unaffected, and its
/// watchdog thread is released immediately rather than sleeping out the
/// remaining budget. As with every error, `output` may be partial.
pub fn run_filter_timeout<R, W>(
    program: &str,
    args: &[&str],
    input: R,
    output: &mut W,
    timeout: Duration,
) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    run_filter_to_writer(program, args, None, input, output, Some(timeout))
}

/// [`run_filter`] / [`run_filter_timeout`] in a controlled environment: the
/// child gets exactly `env` and nothing inherited (a `timeout` of `None`
/// means no deadline). Everything else, including the failure semantics,
/// is identical.
pub fn run_filter_in_env<R, W>(
    program: &str,
    args: &[&str],
    env: &ChildEnv,
    input: R,
    output: &mut W,
    timeout: Option<Duration>,
) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    run_filter_to_writer(program, args, Some(env), input, output, timeout)
}

/// Where a filter's stdin comes from.
enum Stdin<R> {
    /// A caller stream, copied into the child's stdin pipe by a feeder thread.
    Stream(R),
    /// A descriptor the child inherits *as* its stdin: no feeder thread, no
    /// copy, no buffer. The parent's handle is consumed and released as soon
    /// as the child is spawned, so the child is the only one left holding it.
    Descriptor(File),
}

/// [`run_filter_in_env`]'s engine for a caller-supplied sink: the child's
/// stdout is relayed through a chunk buffer into `output`.
fn run_filter_to_writer<R, W>(
    program: &str,
    args: &[&str],
    env: Option<&ChildEnv>,
    input: R,
    output: &mut W,
    timeout: Option<Duration>,
) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    run_filter_inner(program, args, env, Stdin::Stream(input), |stdout| relay(stdout, output), timeout)
        .map(|(bytes_in, bytes_out)| Transfer { bytes_in, bytes_out })
}

/// Where a descriptor-to-descriptor run puts the child's output: a file, under
/// a byte limit, with every byte shown to a [`ByteTap`] (the fingerprint)
/// while it passes.
pub struct FileSink<'a, T: ByteTap> {
    pub file: &'a mut File,
    pub limit: u64,
    pub tap: &'a mut T,
}

/// Runs `program args...` descriptor to descriptor: `input` becomes the child's
/// stdin as it is, and the child's stdout is moved into `sink.file` by
/// `splice(2)` (with `tee(2)` feeding the tap), so no payload byte is buffered
/// on this process's heap in either direction. Compare [`run_filter_in_env`],
/// which relays both directions through chunk buffers and a feeder thread.
///
/// - **Input.** `input` is read by the child from its current offset (a
///   freshly opened file starts at 0). Because the child holds the same open
///   file description, the offset it leaves behind is not meaningful to the
///   caller; reopen the file to read it again. `Transfer::bytes_in` is the size
///   of the file when the run started: a child that stopped early read less.
/// - **Output.** [`splice::copy_to_file`] does the copy, and falls back to a
///   stack-buffer copy when the kernel cannot splice between the two
///   descriptors (`O_APPEND` destination, exotic filesystem), with identical
///   limits and tap semantics. An empty output is not an error here, for the
///   same reason an empty relay is not: the child's exit status explains it
///   (a rejected image exits non-zero), and a "successful" run with no output
///   is for the caller to judge.
/// - **Failures.** Everything else is as for [`run_filter_in_env`]: the
///   deadline kills the whole process group, the sandboxed environment is
///   whatever `env` says, and on `Err` the file may hold partial bytes and must
///   be discarded. An output over `sink.limit` kills the child and is
///   reported as [`PipelineError::Sink`], as is a failed write to the file; an
///   I/O error reading the child's pipe cannot be told apart from those, so it
///   is reported the same way.
pub fn run_filter_file_in_env<T: ByteTap>(
    program: &str,
    args: &[&str],
    env: &ChildEnv,
    input: File,
    sink: FileSink<'_, T>,
    timeout: Option<Duration>,
) -> Result<Transfer, PipelineError> {
    let FileSink { file, limit, tap } = sink;
    run_filter_inner::<io::Empty, _, _>(
        program,
        args,
        Some(env),
        Stdin::Descriptor(input),
        |stdout| match splice::copy_to_file(stdout, file, limit, tap) {
            Ok(copied) => Ok(copied.len),
            Err(PayloadError::Empty) => Ok(0),
            Err(PayloadError::TooLarge { limit }) => {
                Err(RelayFailure::Sink(io::Error::other(format!("output exceeds the {limit}-byte limit"))))
            }
            Err(PayloadError::Io(e)) => Err(RelayFailure::Sink(e)),
        },
        timeout,
    )
    .map(|(bytes_in, bytes_out)| Transfer { bytes_in, bytes_out })
}

/// The one implementation behind every `run_filter*`: spawns the child, feeds
/// its stdin (by thread, or by handing a descriptor over), lets `drain` consume
/// its stdout on the calling thread, and returns `(bytes_in, drained)`.
fn run_filter_inner<R, T, D>(
    program: &str,
    args: &[&str],
    env: Option<&ChildEnv>,
    input: Stdin<R>,
    drain: D,
    timeout: Option<Duration>,
) -> Result<(u64, T), PipelineError>
where
    R: Read + Send,
    D: FnOnce(&mut ChildStdout) -> Result<T, RelayFailure>,
{
    let (stdin, stream, descriptor_len) = match input {
        Stdin::Stream(source) => (Stdio::piped(), Some(source), 0),
        Stdin::Descriptor(file) => {
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            (Stdio::from(file), None, len)
        }
    };
    let mut command = Command::new(program);
    command.args(args);
    if let Some(env) = env {
        env.apply(&mut command);
    }
    let spawned = command
        // Own process group, so a kill can reach the whole process tree.
        .process_group(0)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    // `Command` keeps the descriptor it was given until it is dropped; release
    // it now rather than at the end of the run, so the child alone holds it.
    drop(command);
    let mut child = spawned.map_err(PipelineError::Spawn)?;

    let child_stdin = child.stdin.take();
    let (Some(mut stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        reap(&mut child);
        return Err(PipelineError::Child(io::Error::other("child pipe handle missing")));
    };
    // A stream needs the child's stdin pipe; a descriptor must not have made one.
    if stream.is_some() != child_stdin.is_some() {
        reap(&mut child);
        return Err(PipelineError::Child(io::Error::other("child pipe handle missing")));
    }
    // Best effort, before any byte moves: a 1 MiB pipe lets the child (or the
    // feeder) run ahead of this thread instead of sleeping every 64 KiB.
    let _ = crate::core::pipe::grow(&stdout);
    if let Some(stdin) = &child_stdin {
        let _ = crate::core::pipe::grow(stdin);
    }

    // Declared before the scope so the watchdog thread may borrow them.
    let pid = child.id();
    let watchdog = Watchdog::new();

    thread::scope(|scope| {
        // `Builder::spawn_scoped` instead of `Scope::spawn`: the latter
        // panics when the OS refuses a thread, which this module must not do.
        let feeder = match stream.zip(child_stdin) {
            None => None,
            Some((source, stdin)) => {
                match thread::Builder::new().name("y4p-pipe-stdin".into()).spawn_scoped(scope, move || feed(source, stdin)) {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        reap(&mut child);
                        return Err(PipelineError::Child(e));
                    }
                }
            }
        };
        let drainer = match thread::Builder::new()
            .name("y4p-pipe-stderr".into())
            .spawn_scoped(scope, move || drain_capped(stderr, PIPELINE_STDERR_CAP_BYTES))
        {
            Ok(handle) => handle,
            Err(e) => {
                // Killing the child breaks the feeder's pipe, so the scope's
                // implicit join cannot hang on it.
                reap(&mut child);
                return Err(PipelineError::Child(e));
            }
        };

        let watchdog = &watchdog;
        let watcher = match timeout {
            None => None,
            Some(limit) => {
                let spawned = thread::Builder::new()
                    .name("y4p-pipe-watchdog".into())
                    .spawn_scoped(scope, move || watchdog.supervise(pid, limit));
                match spawned {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        reap(&mut child);
                        return Err(PipelineError::Child(e));
                    }
                }
            }
        };

        let relayed = drain(&mut stdout);
        if relayed.is_err() {
            kill_process_group(pid);
        }
        let timed_out = if watcher.is_some() {
            // Wait for the exit without reaping, stand the watchdog down,
            // and only then reap: see `Watchdog` for why this order is what
            // keeps a late kill from ever hitting a recycled pid.
            wait_exit_without_reaping(pid);
            watchdog.finish()
        } else {
            false
        };
        let status = child.wait();
        let fed = feeder.map(join_worker).transpose();
        let captured = join_worker(drainer);
        if let Some(handle) = watcher {
            let _ = join_worker(handle);
        }

        if timed_out {
            return Err(PipelineError::Timeout(timeout.unwrap_or_default()));
        }

        let drained = match relayed {
            Ok(drained) => drained,
            Err(RelayFailure::Sink(e)) => return Err(PipelineError::Sink(e)),
            Err(RelayFailure::Child(e)) => return Err(PipelineError::Child(e)),
        };
        let bytes_in = match fed? {
            None => descriptor_len,
            Some(Ok(n)) => n,
            Some(Err(FeedFailure::Source(e))) => return Err(PipelineError::Source(e)),
            Some(Err(FeedFailure::Child(e))) => return Err(PipelineError::Child(e)),
        };
        let status = status.map_err(PipelineError::Child)?;
        if !status.success() {
            let stderr = String::from_utf8_lossy(&captured?).trim().to_owned();
            return Err(PipelineError::Failed { code: status.code(), stderr });
        }
        Ok((bytes_in, drained))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Deterministic, non-repeating-looking payload so any reordering,
    /// truncation or duplication shows up as an inequality.
    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(i >> 8)) as u8).collect()
    }

    /// Runs `f` on a helper thread and fails the test, rather than hanging
    /// the whole suite, if it has not finished in time — a regression back
    /// to a write-all-then-read design would deadlock, not fail an assert.
    fn within_deadline<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("pipeline deadlocked: no completion within 30s")
    }

    fn run_sh(script: &'static str, input: Vec<u8>) -> (Result<Transfer, PipelineError>, Vec<u8>) {
        within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("sh", &["-c", script], Cursor::new(input), &mut out);
            (result, out)
        })
    }

    #[test]
    fn small_block_echo_of_256kib_does_not_deadlock() {
        // 256KiB is 4x the 64KiB pipe capacity in each direction. `dd
        // bs=1024` echoes in 1KiB pieces, so it can buffer almost nothing
        // itself: a parent that wrote all input before reading any output
        // would block on a full stdin pipe while dd blocks on a full stdout
        // pipe, forever. (`cat` is deliberately not used here: its own
        // 128KiB buffer hides this deadlock at 256KiB.)
        let payload = patterned(256 * 1024);
        let expected = payload.clone();
        let (result, out) = within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("dd", &["bs=1024"], Cursor::new(payload), &mut out);
            (result, out)
        });
        let transfer = result.unwrap();
        assert_eq!(out, expected);
        assert_eq!(transfer, Transfer { bytes_in: 256 * 1024, bytes_out: 256 * 1024 });
    }

    #[test]
    fn multi_megabyte_payload_round_trips_intact() {
        // Well past what pipes plus `cat`'s own buffer can absorb (~256KiB),
        // so a sequential write-then-read parent deadlocks here too.
        let payload = patterned(8 * 1024 * 1024 + 17);
        let expected = payload.clone();
        let (result, out) = within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("cat", &[], Cursor::new(payload), &mut out);
            (result, out)
        });
        result.unwrap();
        assert!(out == expected, "output differs from input");
    }

    #[test]
    fn input_that_is_not_a_multiple_of_the_chunk_size_is_not_truncated() {
        let payload = patterned(PIPELINE_CHUNK_BYTES * 3 + 1);
        let expected = payload.clone();
        let (result, out) = within_deadline(move || {
            let mut out = Vec::new();
            let result = run_filter("cat", &[], Cursor::new(payload), &mut out);
            (result, out)
        });
        result.unwrap();
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_input_completes_with_empty_output() {
        let (result, out) = run_sh("cat", Vec::new());
        assert_eq!(result.unwrap(), Transfer { bytes_in: 0, bytes_out: 0 });
        assert!(out.is_empty());
    }

    #[test]
    fn child_flooding_stdout_and_stderr_before_reading_stdin_does_not_deadlock() {
        // The child emits 300KB on each output pipe *before* touching stdin,
        // while the parent is simultaneously pushing 300KB in: all three
        // pipes overflow at once. Only three-way concurrency survives this.
        let (result, out) = run_sh(
            "head -c 300000 /dev/zero; head -c 300000 /dev/zero >&2; cat >/dev/null",
            patterned(300_000),
        );
        let transfer = result.unwrap();
        assert_eq!(out.len(), 300_000);
        assert_eq!(transfer.bytes_in, 300_000);
        assert_eq!(transfer.bytes_out, 300_000);
    }

    #[test]
    fn early_exit_with_unread_large_input_reports_failure_not_a_hang() {
        // The child never reads stdin and exits at once; the parent's
        // 4MiB write hits a broken pipe, which must not be mistaken for the
        // verdict — the exit status is.
        let (result, _) = run_sh("echo rejected >&2; exit 3", patterned(4 * 1024 * 1024));
        match result {
            Err(PipelineError::Failed { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert_eq!(stderr, "rejected");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn child_killed_by_a_signal_is_reported_with_no_exit_code() {
        let (result, _) = run_sh("kill -9 $$", Vec::new());
        assert!(matches!(result, Err(PipelineError::Failed { code: None, .. })));
    }

    #[test]
    fn retained_stderr_is_capped_but_fully_drained() {
        // 1MiB of stderr: far past both the pipe buffer and the retained cap.
        let (result, _) = run_sh("head -c 1048576 /dev/zero | tr '\\0' 'e' >&2; exit 1", Vec::new());
        match result {
            Err(PipelineError::Failed { stderr, .. }) => assert_eq!(stderr.len(), PIPELINE_STDERR_CAP_BYTES),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn missing_program_is_classified_as_unavailable() {
        let mut out = Vec::new();
        let err = run_filter("y4p-definitely-not-a-real-binary", &[], Cursor::new(Vec::new()), &mut out)
            .unwrap_err();
        assert!(matches!(err, PipelineError::Spawn(_)));
        assert!(err.is_unavailable());
    }

    #[test]
    fn rejected_input_is_not_classified_as_unavailable() {
        let (result, _) = run_sh("exit 1", Vec::new());
        assert!(!result.unwrap_err().is_unavailable());
    }

    /// Sink that accepts `limit` bytes and then fails, like a closed socket.
    struct FailingSink {
        limit: usize,
        written: usize,
    }

    impl Write for FailingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.written >= self.limit {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "sink closed"));
            }
            self.written += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failing_sink_stops_the_child_and_reports_a_sink_error() {
        // `yes` would write forever; the run can only end because the sink
        // failure makes the pipeline kill the child.
        let result = within_deadline(|| {
            let mut sink = FailingSink { limit: 100_000, written: 0 };
            run_filter("yes", &[], Cursor::new(Vec::new()), &mut sink)
        });
        assert!(matches!(result, Err(PipelineError::Sink(_))));
    }

    /// Source that yields `good` bytes and then fails, like a dropped file.
    struct FailingSource {
        good: usize,
    }

    impl Read for FailingSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.good == 0 {
                return Err(io::Error::other("source went away"));
            }
            let n = buf.len().min(self.good);
            buf[..n].fill(b'x');
            self.good -= n;
            Ok(n)
        }
    }

    #[test]
    fn failing_source_is_reported_as_a_source_error() {
        let result = within_deadline(|| {
            let mut out = Vec::new();
            run_filter("cat", &[], FailingSource { good: 200_000 }, &mut out)
        });
        assert!(matches!(result, Err(PipelineError::Source(_))));
    }

    #[test]
    fn feed_treats_broken_pipe_as_a_clean_stop() {
        struct ClosedPipe;
        impl Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let fed = feed(Cursor::new(patterned(1000)), ClosedPipe);
        assert!(matches!(fed, Ok(0)));
    }

    #[test]
    fn drain_capped_keeps_only_the_prefix() {
        let kept = drain_capped(Cursor::new(patterned(10_000)), 100);
        assert_eq!(kept, patterned(100));
    }

    #[test]
    fn display_distinguishes_exit_codes_from_signals() {
        let coded = PipelineError::Failed { code: Some(2), stderr: "bad".into() }.to_string();
        let signalled = PipelineError::Failed { code: None, stderr: "bad".into() }.to_string();
        assert!(coded.contains("status 2"));
        assert!(signalled.contains("signal"));
    }

    #[test]
    fn is_unavailable_returns_true_for_all_spawn_error_kinds() {
        assert!(PipelineError::Spawn(io::Error::new(io::ErrorKind::NotFound, "not found")).is_unavailable());
        assert!(PipelineError::Spawn(io::Error::new(io::ErrorKind::PermissionDenied, "denied")).is_unavailable());
        assert!(PipelineError::Spawn(io::Error::new(io::ErrorKind::InvalidData, "exec format error")).is_unavailable());
        assert!(PipelineError::Spawn(io::Error::other("dynamic linker fault")).is_unavailable());

        // Non-spawn failures must NOT be reported as unavailable
        assert!(!PipelineError::Source(io::Error::other("read error")).is_unavailable());
        assert!(!PipelineError::Sink(io::Error::other("write error")).is_unavailable());
        assert!(!PipelineError::Child(io::Error::other("pipe broken")).is_unavailable());
        assert!(!PipelineError::Failed { code: Some(1), stderr: "syntax error".into() }.is_unavailable());
    }

    // --- wall-clock timeout ---

    fn run_sh_timeout(
        script: &'static str,
        input: Vec<u8>,
        timeout: Duration,
    ) -> (Result<Transfer, PipelineError>, Vec<u8>, Duration) {
        within_deadline(move || {
            let started = std::time::Instant::now();
            let mut out = Vec::new();
            let result = run_filter_timeout("sh", &["-c", script], Cursor::new(input), &mut out, timeout);
            (result, out, started.elapsed())
        })
    }

    /// True once `pid` is no longer actively executing (either completely reaped,
    /// or in state 'Z' waiting for PID 1 / init to reap it).
    fn process_is_gone(pid: u32) -> bool {
        let Ok(p) = i32::try_from(pid) else { return true };
        // SAFETY: signal 0 performs error checking without delivering a signal.
        let exists = unsafe { libc::kill(p, 0) == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) };
        if !exists {
            return true;
        }

        // Process entry still present in kernel table: check if it has transitioned to
        // Zombie ('Z') state, meaning SIGKILL successfully terminated its execution.
        match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Err(_) => true,
            Ok(status) => status.lines().any(|l| l.starts_with("State:") && l.contains('Z')),
        }
    }

    fn wait_until_gone(pid: u32) -> bool {
        // Allow up to 5 seconds (100 * 50ms) to accommodate heavy CI runner load.
        for _ in 0..100 {
            if process_is_gone(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline_with_a_timeout_error() {
        let limit = Duration::from_millis(300);
        let (result, _, elapsed) = run_sh_timeout("sleep 30; echo unreachable", Vec::new(), limit);
        match result {
            Err(PipelineError::Timeout(reported)) => assert_eq!(reported, limit),
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(elapsed >= limit, "must not fire early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(10), "must not wait out the child: {elapsed:?}");
    }

    #[test]
    fn a_hung_child_that_never_reads_a_large_input_is_still_unblocked() {
        // 4MiB can never fit in the pipe: the stdin feeder is blocked in
        // write(2) until the kill breaks the pipe.
        let (result, _, elapsed) = run_sh_timeout("sleep 30", patterned(4 * 1024 * 1024), Duration::from_millis(300));
        assert!(matches!(result, Err(PipelineError::Timeout(_))), "got {result:?}");
        assert!(elapsed < Duration::from_secs(10));
    }

    #[test]
    fn a_child_that_closes_stdout_and_then_hangs_is_still_killed() {
        // The relay sees EOF at once; only the wait for exit can hang.
        let (result, _, elapsed) = run_sh_timeout("exec >&-; sleep 30", Vec::new(), Duration::from_millis(300));
        assert!(matches!(result, Err(PipelineError::Timeout(_))), "got {result:?}");
        assert!(elapsed < Duration::from_secs(10));
    }

    #[test]
    fn the_killed_child_is_reaped_rather_than_left_as_a_zombie() {
        let (result, out, _) = run_sh_timeout("echo $$; sleep 30; echo x", Vec::new(), Duration::from_millis(400));
        assert!(matches!(result, Err(PipelineError::Timeout(_))));
        let pid: u32 = String::from_utf8_lossy(&out).trim().parse().unwrap();
        // A zombie would still have a /proc entry (state Z); a reaped
        // child has none at all.
        assert!(
            std::fs::metadata(format!("/proc/{pid}")).is_err(),
            "pid {pid} was killed but not reaped"
        );
    }

    #[test]
    fn helper_processes_spawned_by_the_child_are_killed_too() {
        // The grandchild inherits the stdout pipe. If only the direct child
        // were killed, the pipe would stay open and the run would hang.
        let (result, out, elapsed) = run_sh_timeout("sleep 60 & echo $!; wait", Vec::new(), Duration::from_millis(500));
        assert!(matches!(result, Err(PipelineError::Timeout(_))), "got {result:?}");
        assert!(elapsed < Duration::from_secs(10));
        let grandchild: u32 = String::from_utf8_lossy(&out).trim().parse().unwrap();
        assert!(wait_until_gone(grandchild), "grandchild {grandchild} survived the group kill");
    }

    #[test]
    fn a_child_that_finishes_in_time_is_unaffected_and_the_watchdog_is_released_promptly() {
        // A 60s budget the run must not sit out.
        let payload = patterned(256 * 1024);
        let expected = payload.clone();
        let (result, elapsed, out) = within_deadline(move || {
            let started = std::time::Instant::now();
            let mut out = Vec::new();
            let result = run_filter_timeout("cat", &[], Cursor::new(payload), &mut out, Duration::from_secs(60));
            (result, started.elapsed(), out)
        });
        result.unwrap();
        assert_eq!(out, expected);
        assert!(elapsed < Duration::from_secs(10), "the watchdog thread kept the run waiting: {elapsed:?}");
    }

    #[test]
    fn ordinary_failures_are_not_mistaken_for_timeouts() {
        let (result, _, _) = run_sh_timeout("echo nope >&2; exit 3", Vec::new(), Duration::from_secs(30));
        match result {
            Err(PipelineError::Failed { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert_eq!(stderr, "nope");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_budget_times_out_immediately_instead_of_hanging() {
        let (result, _, elapsed) = run_sh_timeout("sleep 30", Vec::new(), Duration::ZERO);
        assert!(matches!(result, Err(PipelineError::Timeout(_))), "got {result:?}");
        assert!(elapsed < Duration::from_secs(10));
    }

    #[test]
    fn a_failing_sink_is_still_reported_as_a_sink_error_under_a_timeout() {
        let result = within_deadline(|| {
            let mut sink = FailingSink { limit: 100_000, written: 0 };
            run_filter_timeout("yes", &[], Cursor::new(Vec::new()), &mut sink, Duration::from_secs(60))
        });
        assert!(matches!(result, Err(PipelineError::Sink(_))), "got {result:?}");
    }

    #[test]
    fn many_quick_runs_under_a_deadline_never_race_into_a_false_timeout() {
        // Hammers the finish-versus-fire handshake: every run completes long
        // before its deadline, so any Timeout would be a race bug.
        within_deadline(|| {
            for round in 0..150 {
                let mut out = Vec::new();
                let result = run_filter_timeout("cat", &[], Cursor::new(patterned(1000 + round)), &mut out, Duration::from_secs(20));
                assert!(result.is_ok(), "round {round}: {result:?}");
                assert_eq!(out.len(), 1000 + round);
            }
        });
    }

    #[test]
    fn timeout_display_names_the_limit() {
        let text = PipelineError::Timeout(Duration::from_secs(15)).to_string();
        assert!(text.contains("15") && text.contains("time limit"), "{text}");
        assert!(!PipelineError::Timeout(Duration::from_secs(15)).is_unavailable());
    }

    // --- splice(2) primitives ---

    /// A uniquely named file under `target/`, removed on drop, so the tests
    /// can use real regular-file descriptors without a temp-dir crate.
    struct ScratchFile(std::path::PathBuf);

    impl ScratchFile {
        fn new(tag: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("splice-test-tmp");
            std::fs::create_dir_all(&dir).unwrap();
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(dir.join(format!("{}-{id}-{tag}", std::process::id())))
        }

        /// Opens (creating/truncating) the file for reading and writing.
        fn open_rw(&self) -> std::fs::File {
            std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&self.0).unwrap()
        }

        fn contents(&self) -> Vec<u8> {
            std::fs::read(&self.0).unwrap()
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // The bare primitives live in `core::splice`, which tests them directly;
    // what follows checks that the re-exported pair still serves this module's
    // callers, including with a real child process on the other end.

    #[test]
    fn splice_drains_many_chunks_in_one_call() {
        within_deadline(|| {
            let payload = patterned(1 << 20);
            assert!(payload.len() > 4 * PIPELINE_CHUNK_BYTES, "the test must span several chunks");
            let (a_read, a_write) = io::pipe().unwrap();
            let (b_read, b_write) = io::pipe().unwrap();

            thread::scope(|scope| {
                let expected = &payload;
                scope.spawn(move || {
                    let mut a_write = a_write;
                    a_write.write_all(expected).unwrap();
                });
                let reader = scope.spawn(move || {
                    let mut b_read = b_read;
                    let mut received = Vec::new();
                    b_read.read_to_end(&mut received).unwrap();
                    received
                });

                let moved = splice_all(&a_read, &b_write).unwrap();
                drop(b_write);
                assert_eq!(moved, Some(payload.len() as u64));
                assert_eq!(reader.join().unwrap(), payload);
            });
        });
    }

    #[test]
    fn splice_between_a_pipe_and_a_file_works_in_both_directions() {
        let payload = patterned(30_000);
        let scratch = ScratchFile::new("pipe-file");

        // pipe -> file
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(&payload).unwrap();
        drop(writer);
        let file = scratch.open_rw();
        assert_eq!(splice_all(&reader, &file).unwrap(), Some(payload.len() as u64));
        assert_eq!(scratch.contents(), payload);

        // file -> pipe
        let source = std::fs::File::open(&scratch.0).unwrap();
        let (mut reader, writer) = io::pipe().unwrap();
        assert_eq!(splice_all(&source, &writer).unwrap(), Some(payload.len() as u64));
        drop(writer);
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, payload);
    }

    #[test]
    fn splice_drains_a_child_process_stdout_into_a_file() {
        within_deadline(|| {
            const LEN: u64 = 200_000;
            let scratch = ScratchFile::new("child-stdout");
            let file = scratch.open_rw();
            let mut child = Command::new("head")
                .args(["-c", &LEN.to_string(), "/dev/zero"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();

            assert_eq!(splice_all(&stdout, &file).unwrap(), Some(LEN));
            assert!(child.wait().unwrap().success());
            assert_eq!(scratch.contents().len() as u64, LEN);
        });
    }

    // --- descriptor to descriptor: a file as stdin, stdout spliced into a file ---

    /// A tap that keeps what it saw, to compare against what reached the file.
    #[derive(Default)]
    struct Collect(Vec<u8>);

    impl ByteTap for Collect {
        fn update(&mut self, chunk: &[u8]) {
            self.0.extend_from_slice(chunk);
        }
    }

    /// A read-only handle on a scratch file holding `bytes`, at offset 0.
    fn input_file(scratch: &ScratchFile, bytes: &[u8]) -> File {
        std::fs::write(&scratch.0, bytes).unwrap();
        File::open(&scratch.0).unwrap()
    }

    fn path_only_env() -> ChildEnv {
        ChildEnv::new().set("PATH", std::env::var_os("PATH").unwrap_or_default())
    }

    /// Runs `program args` with `input` as stdin and a fresh scratch file as the
    /// sink; returns the run's result, what reached the file and what the tap saw.
    fn run_to_file(
        program: &'static str,
        args: &'static [&'static str],
        input: File,
        limit: u64,
        timeout: Option<Duration>,
    ) -> (Result<Transfer, PipelineError>, Vec<u8>, Vec<u8>) {
        let out = ScratchFile::new("sink");
        let mut file = out.open_rw();
        let mut tap = Collect::default();
        let result = run_filter_file_in_env(
            program,
            args,
            &path_only_env(),
            input,
            FileSink { file: &mut file, limit, tap: &mut tap },
            timeout,
        );
        (result, out.contents(), tap.0)
    }

    #[test]
    fn a_file_is_the_childs_stdin_and_its_stdout_is_spliced_into_the_output_file() {
        // Several pipe capacities, and not a multiple of any chunk size.
        let payload = patterned(3 * 1024 * 1024 + 5);
        let source = ScratchFile::new("in-cat");
        let input = input_file(&source, &payload);
        let expected = payload;
        let (result, written, seen) = within_deadline(move || run_to_file("cat", &[], input, 64 << 20, None));
        let transfer = result.unwrap();
        assert_eq!(transfer, Transfer { bytes_in: expected.len() as u64, bytes_out: expected.len() as u64 });
        assert!(written == expected, "the file differs from the input");
        assert!(seen == expected, "the tap must see exactly the bytes that were stored, once, in order");
    }

    #[test]
    fn the_child_reads_from_the_descriptors_current_offset() {
        let source = ScratchFile::new("in-offset");
        let mut input = input_file(&source, b"0123456789");
        io::Seek::seek(&mut input, io::SeekFrom::Start(4)).unwrap();
        let (result, written, seen) = run_to_file("cat", &[], input, 1 << 20, None);
        let transfer = result.unwrap();
        assert_eq!(written, b"456789");
        assert_eq!(seen, b"456789");
        // `bytes_in` is the size of the file, as documented, not what was read.
        assert_eq!((transfer.bytes_in, transfer.bytes_out), (10, 6));
    }

    #[test]
    fn a_pipe_descriptor_works_as_the_childs_stdin_too() {
        let payload = patterned(1_000_003);
        let expected = payload.clone();
        let (reader, mut writer) = io::pipe().unwrap();
        let feeder = thread::spawn(move || {
            let _ = writer.write_all(&payload);
        });
        let input = File::from(std::os::fd::OwnedFd::from(reader));
        let (result, written, seen) = within_deadline(move || run_to_file("cat", &[], input, 64 << 20, None));
        feeder.join().unwrap();
        result.unwrap();
        assert!(written == expected && seen == expected);
    }

    #[test]
    fn output_over_the_limit_kills_the_child_and_is_a_sink_error() {
        let source = ScratchFile::new("in-limit");
        let input = input_file(&source, b"ignored");
        // `yes` never stops on its own: only the limit can end this run.
        let (result, written, _) = within_deadline(move || run_to_file("yes", &[], input, 100_000, None));
        match result {
            Err(PipelineError::Sink(e)) => assert!(e.to_string().contains("100000"), "{e}"),
            other => panic!("expected a sink error, got {other:?}"),
        }
        assert!(written.len() as u64 <= 100_001, "the file holds at most the limit plus the probe byte, not {}", written.len());
    }

    #[test]
    fn an_empty_output_is_for_the_caller_to_judge_not_an_error_here() {
        let source = ScratchFile::new("in-empty");
        let input = input_file(&source, b"anything");
        let (result, written, seen) = run_to_file("true", &[], input, 1 << 20, None);
        assert_eq!(result.unwrap().bytes_out, 0);
        assert!(written.is_empty() && seen.is_empty());
    }

    #[test]
    fn a_failing_child_is_reported_as_failed_with_its_stderr_not_as_an_empty_sink() {
        // The child writes nothing to stdout: the copy sees an empty stream,
        // which must not shadow the exit status that explains it.
        let source = ScratchFile::new("in-fail");
        let input = input_file(&source, b"anything");
        let (result, written, _) = run_to_file("sh", &["-c", "echo rejected >&2; exit 3"], input, 1 << 20, None);
        match result {
            Err(PipelineError::Failed { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert_eq!(stderr, "rejected");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(written.is_empty());
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline_on_the_descriptor_road_too() {
        let source = ScratchFile::new("in-hang");
        let input = input_file(&source, b"anything");
        let started = std::time::Instant::now();
        let (result, _, _) = within_deadline(move || run_to_file("sleep", &["30"], input, 1 << 20, Some(Duration::from_millis(200))));
        assert!(matches!(result, Err(PipelineError::Timeout(_))), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "the kill must not wait out the child");
    }

    #[test]
    fn an_append_only_output_falls_back_to_the_buffered_copy_with_the_same_bytes() {
        // `splice(2)` refuses an `O_APPEND` destination: the copy must carry on
        // in user space without losing, repeating or reordering a byte.
        let payload = patterned(700_001);
        let source = ScratchFile::new("in-append");
        let input = input_file(&source, &payload);
        let out = ScratchFile::new("sink-append");
        let mut file = std::fs::OpenOptions::new().read(true).append(true).create(true).open(&out.0).unwrap();
        let mut tap = Collect::default();
        let expected = payload;
        let (result, tap) = within_deadline(move || {
            let result = run_filter_file_in_env(
                "cat",
                &[],
                &path_only_env(),
                input,
                FileSink { file: &mut file, limit: 64 << 20, tap: &mut tap },
                None,
            );
            (result, tap.0)
        });
        result.unwrap();
        assert!(out.contents() == expected && tap == expected);
    }

    #[test]
    fn a_missing_program_on_the_descriptor_road_is_classified_as_unavailable() {
        let source = ScratchFile::new("in-missing");
        let input = input_file(&source, b"x");
        let (result, _, _) = run_to_file("y4p-definitely-not-a-real-binary", &[], input, 1 << 20, None);
        assert!(result.unwrap_err().is_unavailable());
    }
}
