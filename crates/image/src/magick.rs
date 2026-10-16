// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/magick.rs

use std::fs::File;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use crate::core::constants::MAGICK_PROGRAM;
use crate::core::splice::ByteTap;
use crate::pipeline::{self, FileSink, PipelineError, Transfer};
use crate::sandbox::{self, ScratchDir};

/// True when `program -version` can be spawned and exits successfully.
///
/// Never panics: a missing binary, a non-executable file or a non-zero exit
/// all collapse to `false`, which is exactly the signal a caller needs to
/// choose a fallback. All three std streams are nulled so the probe can
/// neither block on a pipe nor write into the caller's terminal. It runs in
/// the same sanitised environment as a real conversion, so the answer
/// describes the tool as conversions will see it.
pub fn probe(program: &str) -> bool {
    probe_with(program, &["-version"])
}

fn probe_with(program: &str, args: &[&str]) -> bool {
    let mut command = Command::new(program);
    sandbox::child_env(None).apply(&mut command);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Whether ImageMagick's `magick` is usable on this system.
///
/// Probed once per process and cached: the answer only changes if the
/// operator installs or removes the tool, and re-spawning a process for
/// every conversion request would dwarf the cost of small conversions.
pub fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| probe(MAGICK_PROGRAM))
}

/// Streams `input` through `magick <args>` into `output`.
///
/// Deliberately does not consult [`is_available`] first: a missing binary
/// already surfaces as [`PipelineError::Spawn`], which
/// [`PipelineError::is_unavailable`] classifies, and a pre-check would only
/// add a second process spawn plus a check-then-use race. Callers that want
/// to avoid even attempting the call can gate on [`is_available`] themselves.
///
/// Every run is isolated (see [`sandbox`]): `magick` gets a private scratch
/// directory for its disk spill and an environment of exactly `PATH` plus the
/// temp variables, so neither the shared `/tmp` nor the daemon's own
/// `MAGICK_*` settings can reach it. If no private directory can be made,
/// nothing is started and the call ends with [`PipelineError::Isolation`].
///
/// On `Err`, `output` may hold partial bytes and must be discarded.
pub fn run<R, W>(args: &[&str], input: R, output: &mut W) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    run_isolated(MAGICK_PROGRAM, sandbox::create_scratch_dir(), args, input, output, None)
}

/// [`run`] under a wall-clock budget: a `magick` that hangs past `timeout`
/// is killed and the call ends with [`PipelineError::Timeout`].
pub fn run_with_timeout<R, W>(args: &[&str], input: R, output: &mut W, timeout: Duration) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    run_isolated(MAGICK_PROGRAM, sandbox::create_scratch_dir(), args, input, output, Some(timeout))
}

/// [`run_with_timeout`] descriptor to descriptor: `input` is handed to `magick`
/// as its stdin and its output is moved into the sink's file by `splice(2)`,
/// fingerprinted on the way (see [`pipeline::run_filter_file_in_env`] for the
/// contract). The same isolation applies: a private scratch directory and the
/// minimal environment, and nothing is started if no scratch directory can be
/// made ([`PipelineError::Isolation`]). On `Err` the sink's file may hold
/// partial bytes and must be discarded.
pub fn run_file_with_timeout<T: ByteTap>(
    args: &[&str],
    input: File,
    sink: FileSink<'_, T>,
    timeout: Duration,
) -> Result<Transfer, PipelineError> {
    let scratch = sandbox::create_scratch_dir().map_err(PipelineError::Isolation)?;
    let env = sandbox::child_env(Some(scratch.path()));
    pipeline::run_filter_file_in_env(MAGICK_PROGRAM, args, &env, input, sink, Some(timeout))
}

/// Runs `program` inside `scratch`, which is removed once the child is gone
/// (the pipeline reaps it, killed or not, before returning). Program and
/// scratch result are parameters so the environment handed to the child and
/// the "could not isolate" path are testable without ImageMagick.
fn run_isolated<R, W>(
    program: &str,
    scratch: std::io::Result<ScratchDir>,
    args: &[&str],
    input: R,
    output: &mut W,
    timeout: Option<Duration>,
) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    let scratch = scratch.map_err(PipelineError::Isolation)?;
    let env = sandbox::child_env(Some(scratch.path()));
    pipeline::run_filter_in_env(program, args, &env, input, output, timeout)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::{Cursor, empty};

    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn magick_or_skip() -> bool {
        if is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    /// Generates a solid-colour PNG of `size`x`size` through the pipeline itself.
    fn solid_png(size: u32) -> Vec<u8> {
        let geometry = format!("{size}x{size}");
        let mut png = Vec::new();
        run(&["-size", &geometry, "xc:red", "png:-"], empty(), &mut png).unwrap();
        png
    }

    #[test]
    fn probe_reports_false_for_a_missing_binary() {
        assert!(!probe("y4p-definitely-not-a-real-binary"));
    }

    #[test]
    fn probe_reports_false_for_a_non_zero_exit() {
        // `false` ignores its arguments and always exits 1.
        assert!(!probe("false"));
    }

    #[test]
    fn probe_reports_true_for_a_working_binary() {
        // `true` ignores its arguments and always exits 0.
        assert!(probe("true"));
    }

    #[test]
    fn identity_conversion_round_trips_a_png() {
        if !magick_or_skip() { return; }
        let source = solid_png(16);
        assert!(source.starts_with(PNG_SIGNATURE));

        let mut converted = Vec::new();
        let transfer = run(&["-", "png:-"], Cursor::new(source.clone()), &mut converted).unwrap();
        assert!(converted.starts_with(PNG_SIGNATURE));
        assert_eq!(transfer.bytes_in, source.len() as u64);
        assert_eq!(transfer.bytes_out, converted.len() as u64);

        // Dimensions survive the round trip.
        let mut info = Vec::new();
        run(&["-", "-format", "%wx%h", "info:"], Cursor::new(converted), &mut info).unwrap();
        assert_eq!(String::from_utf8_lossy(&info).trim(), "16x16");
    }

    #[test]
    fn large_image_exceeding_the_pipe_buffer_converts_without_deadlock() {
        if !magick_or_skip() { return; }
        // Incompressible noise: the PNG alone is far beyond 64KiB, and the
        // BMP it converts to is ~786KiB, so both pipe directions overflow.
        let mut noisy = Vec::new();
        run(&["-size", "512x512", "xc:", "+noise", "Random", "png:-"], empty(), &mut noisy).unwrap();
        assert!(noisy.len() > 64 * 1024);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bmp = Vec::new();
            let result = run(&["-", "bmp:-"], Cursor::new(noisy), &mut bmp);
            let _ = tx.send((result, bmp));
        });
        let (result, bmp) = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("magick pipeline deadlocked");
        result.unwrap();
        assert!(bmp.starts_with(b"BM"));
        assert!(bmp.len() > 512 * 512 * 3);
    }

    #[test]
    fn invalid_input_yields_a_failed_error_with_diagnostics() {
        if !magick_or_skip() { return; }
        let mut out = Vec::new();
        let err = run(&["-", "png:-"], Cursor::new(b"this is not an image".to_vec()), &mut out)
            .unwrap_err();
        match err {
            PipelineError::Failed { code, stderr } => {
                assert_ne!(code, Some(0));
                assert!(!stderr.is_empty(), "magick should explain why it rejected the input");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn large_invalid_input_fails_cleanly_instead_of_hanging_on_a_broken_pipe() {
        if !magick_or_skip() { return; }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let result = run(&["-", "png:-"], Cursor::new(vec![0xA5u8; 4 * 1024 * 1024]), &mut out);
            let _ = tx.send(result);
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("magick pipeline hung on early child exit");
        assert!(matches!(result, Err(PipelineError::Failed { .. })));
    }

    #[test]
    fn a_failure_to_isolate_starts_nothing_and_is_reported_as_such() {
        let refusal = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no private directory");
        let mut out = Vec::new();
        let result = run_isolated("env", Err(refusal), &[], empty(), &mut out, None);
        assert!(matches!(result, Err(PipelineError::Isolation(_))), "{result:?}");
        assert!(out.is_empty(), "magick must not have run");
        // A shortage of scratch space says nothing about whether the tool exists.
        assert!(!result.unwrap_err().is_unavailable());
    }

    #[test]
    fn the_child_runs_in_the_scratch_directory_environment_and_nothing_else() {
        let root = scratch_root("env");
        let scratch = sandbox::create_scratch_dir_under(&root).unwrap();
        let inside = scratch.path().to_path_buf();

        let mut out = Vec::new();
        run_isolated("env", Ok(scratch), &[], empty(), &mut out, None).unwrap();
        let seen: std::collections::BTreeMap<String, String> = String::from_utf8(out)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
            .filter(|(key, _)| key != "_" && key != "PWD")
            .collect();

        let dir = inside.display().to_string();
        assert_eq!(seen.get("MAGICK_TEMPORARY_PATH"), Some(&dir));
        assert_eq!(seen.get("TMPDIR"), Some(&dir));
        let keys: Vec<&str> = seen.keys().map(String::as_str).collect();
        assert!(keys.iter().all(|key| ["MAGICK_TEMPORARY_PATH", "PATH", "TMPDIR"].contains(key)), "unexpected variables: {keys:?}");
        assert!(!inside.exists(), "removed once the child is gone");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_availability_probe_runs_without_the_inherited_environment() {
        // `HOME` is set in practically every test environment; the probe must
        // not pass it on. (Where it is unset the check is vacuously true.)
        assert!(probe_with("sh", &["-c", "[ -z \"${HOME+x}\" ] && [ -z \"${LD_PRELOAD+x}\" ]"]));
        // Control: `PATH`, the one variable that is passed on, is really there.
        assert!(probe_with("sh", &["-c", "[ -n \"${PATH+x}\" ]"]));
    }

    /// A throwaway root under `target/`, so the real runtime directory is not involved.
    fn scratch_root(tag: &str) -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("magick-test-tmp")
            .join(format!("{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn entries_under(root: &std::path::Path) -> usize {
        // The per-user base directory holds one subdirectory per run.
        std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|base| std::fs::read_dir(base.path()).unwrap().count())
            .sum()
    }

    #[test]
    fn the_scratch_directory_is_removed_after_success_and_after_failure() {
        if !magick_or_skip() { return; }
        let root = scratch_root("cleanup");

        let mut png = Vec::new();
        let scratch = sandbox::create_scratch_dir_under(&root).unwrap();
        let inside = scratch.path().to_path_buf();
        run_isolated(MAGICK_PROGRAM, Ok(scratch), &["-size", "8x8", "xc:red", "png:-"], empty(), &mut png, None).unwrap();
        assert!(!inside.exists(), "left behind after a successful run");
        assert_eq!(entries_under(&root), 0);

        let scratch = sandbox::create_scratch_dir_under(&root).unwrap();
        let inside = scratch.path().to_path_buf();
        let mut out = Vec::new();
        let failure = run_isolated(MAGICK_PROGRAM, Ok(scratch), &["png:-", "png:-"], Cursor::new(b"not an image".to_vec()), &mut out, None);
        assert!(matches!(failure, Err(PipelineError::Failed { .. })), "{failure:?}");
        assert!(!inside.exists(), "left behind after a failed run");
        assert_eq!(entries_under(&root), 0);

        let _ = std::fs::remove_dir_all(&root);
    }
}
