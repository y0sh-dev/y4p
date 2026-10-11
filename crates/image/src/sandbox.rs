// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/sandbox.rs

//! Execution environment for the external image converter: where it may
//! spill to disk, and what it may inherit.
//!
//! # Scratch directory
//!
//! With `-limit disk` set, ImageMagick moves its pixel cache to temporary
//! files once memory and map limits are exhausted. In a shared `/tmp` those
//! files are readable-by-name side channels and a target for symlink races,
//! and they contain a decoded clipboard image. So the converter is pointed
//! (via `MAGICK_TEMPORARY_PATH` and `TMPDIR`) at a directory only the current
//! user can enter:
//!
//! 1. `$XDG_RUNTIME_DIR/<SCRATCH_DIR_NAME>`, provided `$XDG_RUNTIME_DIR` is an
//!    absolute path to a real directory (not a symlink) owned by the current
//!    user with no group/other permissions; else
//! 2. `<SCRATCH_FALLBACK_ROOT>/<SCRATCH_DIR_NAME>-<uid>`.
//!
//! Nothing is taken on trust: every candidate is `lstat`ed, must be a real
//! directory owned by the effective uid, and is tightened to 0700 if it was
//! ours but looser. A candidate that fails (someone else's directory, a
//! symlink, a plain file) is never used and never followed. If neither
//! location qualifies, creation fails and the caller must not run the
//! converter: falling back to the shared default would defeat the purpose.
//!
//! Each conversion gets a fresh subdirectory ([`ScratchDir`]) that is removed
//! when the guard is dropped. That also cleans up after a child killed by the
//! watchdog, which cannot clean up after itself, and keeps concurrent
//! conversions out of each other's files.
//!
//! # Environment
//!
//! [`child_env`] builds the complete environment of the converter: `PATH`
//! (to find the program) plus the two temp variables, nothing else. In
//! particular `MAGICK_CONFIGURE_PATH`, `MAGICK_HOME`, `MAGICK_CODER_MODULE_PATH`,
//! `HOME` and `XDG_CONFIG_HOME` (all of which can substitute ImageMagick's
//! security policy or coders) and `LD_PRELOAD`/`LD_LIBRARY_PATH` are not
//! passed on. The system-wide policy in `/etc/ImageMagick-*` still applies.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::constants::{SCRATCH_DIR_NAME, SCRATCH_FALLBACK_ROOT};
use crate::pipeline::ChildEnv;

/// Owner-only: read, write, search.
const PRIVATE_MODE: u32 = 0o700;
/// Any permission bit for group or others.
const GROUP_OTHER_BITS: u32 = 0o077;

/// A per-conversion directory, removed (best effort) on drop.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // `remove_dir_all` does not follow symlinks. Failure leaves a stray
        // directory inside a private location; there is nothing better to do.
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid(2)` takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// Creates a private scratch directory for one conversion (see the module docs).
pub fn create_scratch_dir() -> io::Result<ScratchDir> {
    create_scratch_dir_in(
        std::env::var_os("XDG_RUNTIME_DIR"),
        Path::new(SCRATCH_FALLBACK_ROOT),
        effective_uid(),
    )
}

/// A scratch directory under `root` only (no `$XDG_RUNTIME_DIR`), for tests
/// elsewhere that need to look at where it lives.
#[cfg(test)]
pub(crate) fn create_scratch_dir_under(root: &Path) -> io::Result<ScratchDir> {
    create_scratch_dir_in(None, root, effective_uid())
}

/// [`create_scratch_dir`] with its three inputs injected, so every branch can
/// be tested without touching the process environment.
fn create_scratch_dir_in(
    runtime_dir: Option<OsString>,
    fallback_root: &Path,
    uid: u32,
) -> io::Result<ScratchDir> {
    let base = private_base(runtime_dir.as_deref().map(Path::new), fallback_root, uid)?;
    unique_child(&base)
}

fn private_base(runtime_dir: Option<&Path>, fallback_root: &Path, uid: u32) -> io::Result<PathBuf> {
    if let Some(runtime) = runtime_dir.filter(|path| path.is_absolute() && is_private_dir(path, uid)) {
        let base = runtime.join(SCRATCH_DIR_NAME);
        if ensure_private_dir(&base, uid).is_ok() {
            return Ok(base);
        }
    }
    let base = fallback_root.join(format!("{SCRATCH_DIR_NAME}-{uid}"));
    ensure_private_dir(&base, uid)?;
    Ok(base)
}

/// True for a real directory (a symlink does not count, even to a directory)
/// owned by `uid` that group and others cannot access at all.
fn is_private_dir(path: &Path, uid: u32) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.file_type().is_dir() && meta.uid() == uid && meta.mode() & GROUP_OTHER_BITS == 0
    })
}

/// Makes `path` an owner-only directory of `uid`, creating it if absent.
///
/// An existing entry is accepted only if it is a real directory of `uid`;
/// its mode is then tightened. Anything else, including a symlink planted in
/// a world-writable parent, is refused rather than followed.
fn ensure_private_dir(path: &Path, uid: u32) -> io::Result<()> {
    match DirBuilder::new().mode(PRIVATE_MODE).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() || meta.uid() != uid {
        return Err(refused(path));
    }
    if meta.mode() & GROUP_OTHER_BITS != 0 {
        // Safe to follow: the `lstat` above showed a real directory of ours.
        fs::set_permissions(path, Permissions::from_mode(PRIVATE_MODE))?;
    }
    if is_private_dir(path, uid) { Ok(()) } else { Err(refused(path)) }
}

fn refused(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} is not a private directory of the current user", path.display()),
    )
}

/// A new, empty, owner-only subdirectory of `base` (which is already private,
/// so the name needs to be unique but not unguessable).
fn unique_child(base: &Path) -> io::Result<ScratchDir> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    unique_child_with(base, || NEXT.fetch_add(1, Ordering::Relaxed))
}

/// [`unique_child`] with the source of sequence numbers injected.
fn unique_child_with(base: &Path, mut next: impl FnMut() -> u64) -> io::Result<ScratchDir> {
    for _ in 0..16 {
        let name = format!("run-{}-{}", std::process::id(), next());
        let path = base.join(name);
        match DirBuilder::new().mode(PRIVATE_MODE).create(&path) {
            Ok(()) => return Ok(ScratchDir { path }),
            // A leftover from an earlier process with the same pid: skip it.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no unused scratch directory name"))
}

/// The converter's whole environment: `PATH` (so the program can be found)
/// and, when a scratch directory is given, `MAGICK_TEMPORARY_PATH` and
/// `TMPDIR` pointing at it. Nothing else is inherited.
pub fn child_env(scratch: Option<&Path>) -> ChildEnv {
    child_env_with(std::env::var_os("PATH"), scratch)
}

fn child_env_with(path: Option<OsString>, scratch: Option<&Path>) -> ChildEnv {
    let mut env = ChildEnv::new();
    if let Some(path) = path {
        env = env.set("PATH", path);
    }
    if let Some(dir) = scratch {
        env = env.set("MAGICK_TEMPORARY_PATH", dir).set("TMPDIR", dir);
    }
    env
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::pipeline::run_filter_in_env;
    use std::collections::BTreeSet;
    use std::io::{Read, empty};

    /// A directory under `target/` that the test owns and can set the mode of,
    /// removed on drop. Stands in for `$XDG_RUNTIME_DIR` and `/tmp`.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("sandbox-test-tmp")
                .join(format!("{}-{}-{tag}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            DirBuilder::new().recursive(true).mode(PRIVATE_MODE).create(&path).unwrap();
            let dir = Self(path);
            dir.set_mode(PRIVATE_MODE);
            dir
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn set_mode(&self, mode: u32) {
            fs::set_permissions(&self.0, Permissions::from_mode(mode)).unwrap();
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, Permissions::from_mode(PRIVATE_MODE));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    fn me() -> u32 {
        effective_uid()
    }

    fn create(runtime: Option<&Path>, fallback: &Path) -> io::Result<ScratchDir> {
        create_scratch_dir_in(runtime.map(|path| path.as_os_str().to_owned()), fallback, me())
    }

    // --- location choice ---

    #[test]
    fn a_private_runtime_dir_hosts_the_scratch_space_with_mode_0700() {
        let runtime = TestDir::new("runtime");
        let fallback = TestDir::new("fallback");
        let scratch = create(Some(runtime.path()), fallback.path()).unwrap();

        let base = runtime.path().join(SCRATCH_DIR_NAME);
        assert!(scratch.path().starts_with(&base), "{:?} is not under {base:?}", scratch.path());
        assert_eq!(mode_of(&base), 0o700);
        assert_eq!(mode_of(scratch.path()), 0o700);
        assert_eq!(fs::symlink_metadata(scratch.path()).unwrap().uid(), me());
        assert!(fs::read_dir(fallback.path()).unwrap().next().is_none(), "the fallback must stay untouched");
    }

    #[test]
    fn a_runtime_dir_open_to_group_or_others_is_not_trusted() {
        for mode in [0o755, 0o750, 0o705, 0o770, 0o777] {
            let runtime = TestDir::new("loose-runtime");
            let fallback = TestDir::new("fallback");
            runtime.set_mode(mode);
            let scratch = create(Some(runtime.path()), fallback.path()).unwrap();
            assert!(scratch.path().starts_with(fallback.path()), "mode {mode:o} was trusted");
            assert!(!runtime.path().join(SCRATCH_DIR_NAME).exists(), "nothing may be created inside it");
        }
    }

    #[test]
    fn an_unusable_runtime_dir_value_falls_back_instead_of_failing() {
        let fallback = TestDir::new("fallback");
        let real = TestDir::new("real-runtime");
        let link = fallback.path().join("link-to-runtime");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let file = fallback.path().join("a-file");
        fs::write(&file, b"x").unwrap();

        let candidates: Vec<Option<OsString>> = vec![
            None,
            Some(OsString::new()),
            Some("relative/dir".into()),
            Some(fallback.path().join("does-not-exist").into_os_string()),
            Some(file.into_os_string()),
            // Even a symlink to a perfectly private directory is refused.
            Some(link.into_os_string()),
        ];
        for runtime in candidates {
            let scratch = create_scratch_dir_in(runtime.clone(), fallback.path(), me()).unwrap();
            assert!(scratch.path().starts_with(fallback.path().join(format!("{SCRATCH_DIR_NAME}-{}", me()))), "{runtime:?}");
        }
        assert!(!real.path().join(SCRATCH_DIR_NAME).exists(), "the symlink target must not have been used");
    }

    #[test]
    fn a_relative_runtime_dir_is_refused_even_if_it_resolves_to_a_private_directory() {
        let runtime = TestDir::new("runtime");
        let fallback = TestDir::new("fallback");
        let cwd = std::env::current_dir().unwrap();
        let relative = runtime.path().strip_prefix(&cwd).unwrap().to_path_buf();
        assert!(relative.is_relative() && is_private_dir(&relative, me()), "precondition: it does resolve from here");

        let scratch = create(Some(&relative), fallback.path()).unwrap();
        assert!(scratch.path().starts_with(fallback.path()));
        assert!(!runtime.path().join(SCRATCH_DIR_NAME).exists());
    }

    #[test]
    fn the_fallback_is_a_per_user_0700_directory() {
        let fallback = TestDir::new("fallback");
        let scratch = create(None, fallback.path()).unwrap();
        let base = fallback.path().join(format!("{SCRATCH_DIR_NAME}-{}", me()));
        assert!(scratch.path().starts_with(&base));
        assert_eq!(mode_of(&base), 0o700);
        assert_eq!(mode_of(scratch.path()), 0o700);
    }

    #[test]
    fn a_loose_fallback_directory_of_ours_is_tightened_not_trusted_as_is() {
        let fallback = TestDir::new("fallback");
        let base = fallback.path().join(format!("{SCRATCH_DIR_NAME}-{}", me()));
        DirBuilder::new().mode(0o755).create(&base).unwrap();
        fs::set_permissions(&base, Permissions::from_mode(0o755)).unwrap();

        let scratch = create(None, fallback.path()).unwrap();
        assert_eq!(mode_of(&base), 0o700, "mode must be tightened before use");
        assert_eq!(mode_of(scratch.path()), 0o700);
    }

    #[test]
    fn a_directory_that_belongs_to_someone_else_is_never_used() {
        // The test cannot become another user, so the roles are swapped: the
        // directories really belong to us, and we claim to be another uid.
        let runtime = TestDir::new("runtime");
        let fallback = TestDir::new("fallback");
        let other = me().wrapping_add(1);

        let result = create_scratch_dir_in(Some(runtime.path().as_os_str().to_owned()), fallback.path(), other);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(!runtime.path().join(SCRATCH_DIR_NAME).exists(), "a foreign runtime dir must not be written to");
    }

    #[test]
    fn a_planted_symlink_or_file_in_the_fallback_location_is_refused_and_not_followed() {
        let fallback = TestDir::new("fallback");
        let victim = TestDir::new("victim");
        let name = format!("{SCRATCH_DIR_NAME}-{}", me());

        let planted = fallback.path().join(&name);
        std::os::unix::fs::symlink(victim.path(), &planted).unwrap();
        let result = create(None, fallback.path());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(fs::read_dir(victim.path()).unwrap().next().is_none(), "the symlink target was written to");

        fs::remove_file(&planted).unwrap();
        fs::write(&planted, b"not a directory").unwrap();
        assert_eq!(create(None, fallback.path()).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn no_usable_location_is_an_error_rather_than_a_silent_default() {
        let missing = TestDir::new("missing-parent").path().join("gone").join("deeper");
        assert!(create(None, &missing).is_err());
    }

    // --- per-conversion directories ---

    #[test]
    fn every_conversion_gets_its_own_empty_private_directory() {
        let runtime = TestDir::new("runtime");
        let fallback = TestDir::new("fallback");
        let first = create(Some(runtime.path()), fallback.path()).unwrap();
        let second = create(Some(runtime.path()), fallback.path()).unwrap();

        assert_ne!(first.path(), second.path());
        for scratch in [&first, &second] {
            assert_eq!(mode_of(scratch.path()), 0o700);
            assert!(fs::read_dir(scratch.path()).unwrap().next().is_none());
        }
    }

    #[test]
    fn dropping_the_guard_removes_the_directory_with_everything_in_it() {
        let runtime = TestDir::new("runtime");
        let fallback = TestDir::new("fallback");
        let scratch = create(Some(runtime.path()), fallback.path()).unwrap();
        let path = scratch.path().to_path_buf();
        fs::write(path.join("magick-spill.pixels"), vec![0u8; 4096]).unwrap();
        fs::create_dir(path.join("nested")).unwrap();
        fs::write(path.join("nested").join("more"), b"x").unwrap();

        drop(scratch);
        assert!(!path.exists(), "the scratch directory outlived its guard");
        assert!(runtime.path().join(SCRATCH_DIR_NAME).exists(), "only the per-run directory goes away");
    }

    #[test]
    fn a_leftover_with_the_same_name_is_skipped_not_reused() {
        let base = TestDir::new("base");
        let squatter = base.path().join(format!("run-{}-5", std::process::id()));
        DirBuilder::new().mode(PRIVATE_MODE).create(&squatter).unwrap();
        fs::write(squatter.join("stale"), b"x").unwrap();

        let mut numbers = [5u64, 6].into_iter();
        let scratch = unique_child_with(base.path(), || numbers.next().unwrap()).unwrap();
        assert_eq!(scratch.path(), base.path().join(format!("run-{}-6", std::process::id())));
        assert!(fs::read_dir(scratch.path()).unwrap().next().is_none());
        assert!(squatter.join("stale").exists(), "another run's directory must be left alone");
    }

    #[test]
    fn running_out_of_free_names_is_an_error_not_a_loop() {
        let base = TestDir::new("base");
        let squatter = base.path().join(format!("run-{}-0", std::process::id()));
        DirBuilder::new().mode(PRIVATE_MODE).create(&squatter).unwrap();
        let result = unique_child_with(base.path(), || 0);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }

    // --- environment ---

    #[test]
    fn the_child_environment_holds_only_path_and_the_two_temp_variables() {
        let dir = Path::new("/run/user/1000/y4p-magick/run-1-0");
        let env = child_env_with(Some("/usr/bin:/bin".into()), Some(dir));
        let vars: Vec<(String, String)> = env
            .vars()
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            vars,
            [
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("MAGICK_TEMPORARY_PATH".to_owned(), dir.display().to_string()),
                ("TMPDIR".to_owned(), dir.display().to_string()),
            ]
        );
    }

    #[test]
    fn without_a_scratch_directory_no_temp_variable_is_set() {
        let env = child_env_with(Some("/usr/bin".into()), None);
        assert_eq!(env.vars().len(), 1);
        assert!(child_env_with(None, None).vars().is_empty());
    }

    /// Variables a hostile or merely unlucky parent environment could carry.
    const HOSTILE: [&str; 9] = [
        "MAGICK_CONFIGURE_PATH",
        "MAGICK_HOME",
        "MAGICK_CODER_MODULE_PATH",
        "MAGICK_TIME_LIMIT",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "HOME",
        "XDG_CONFIG_HOME",
        "Y4P_SECRET",
    ];

    fn child_sees(env: &ChildEnv) -> BTreeSet<String> {
        let mut out = Vec::new();
        run_filter_in_env("env", &[], env, empty(), &mut out, None).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.to_owned()))
            .collect()
    }

    #[test]
    fn a_real_child_sees_exactly_the_sanitised_environment() {
        let runtime = TestDir::new("runtime");
        let fallback = TestDir::new("fallback");
        let scratch = create(Some(runtime.path()), fallback.path()).unwrap();
        let env = child_env_with(std::env::var_os("PATH"), Some(scratch.path()));

        let seen = child_sees(&env);
        let expected: BTreeSet<String> =
            ["PATH", "MAGICK_TEMPORARY_PATH", "TMPDIR"].into_iter().map(str::to_owned).collect();
        // `env` itself may add `_` or `PWD` depending on the shell wrapper.
        let seen: BTreeSet<String> = seen.into_iter().filter(|key| key != "_" && key != "PWD").collect();
        assert_eq!(seen, expected);

        // Everything the test process itself inherited is absent, whatever it is.
        let inherited: BTreeSet<String> = std::env::vars_os().map(|(key, _)| key.to_string_lossy().into_owned()).collect();
        for key in inherited.difference(&expected) {
            assert!(!seen.contains(key), "{key} leaked into the child");
        }
    }

    #[test]
    fn variables_set_on_the_environment_object_replace_rather_than_accumulate() {
        let env = ChildEnv::new().set("A", "1").set("B", "2").set("A", "3");
        let pairs: Vec<(String, String)> = env
            .vars()
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(pairs, [("B".to_owned(), "2".to_owned()), ("A".to_owned(), "3".to_owned())]);
    }

    #[test]
    fn hostile_variables_cannot_be_smuggled_through_the_environment_object() {
        // Even variables explicitly present on a *Command* before the policy is
        // applied are discarded: the policy is "exactly these", not "also these".
        let env = child_env_with(std::env::var_os("PATH"), None);
        let seen = child_sees(&env);
        for key in HOSTILE {
            assert!(!seen.contains(key), "{key} reached the child");
        }
        let mut command = std::process::Command::new("env");
        for key in HOSTILE {
            command.env(key, "/evil");
        }
        env.apply(&mut command);
        let output = command.output().unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        for key in HOSTILE {
            assert!(!text.contains(&format!("{key}=")), "{key} survived the policy");
        }
    }

    /// An input stream that stays open (blocks) until released, then ends.
    struct HeldOpen(std::sync::mpsc::Receiver<()>);

    impl Read for HeldOpen {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    #[test]
    fn real_magick_spills_into_the_private_scratch_directory() {
        use crate::magick;
        use std::time::{Duration, Instant};
        if !magick::is_available() {
            eprintln!("skipping: `magick` is not available on this system");
            return;
        }
        let runtime = TestDir::new("runtime");
        let fallback = TestDir::new("fallback");
        let scratch = create(Some(runtime.path()), fallback.path()).unwrap();
        let env = child_env_with(std::env::var_os("PATH"), Some(scratch.path()));
        let (release, held) = std::sync::mpsc::channel();

        // With memory and map limits of zero the first image's pixel cache can
        // only live on disk; reading the second image from the held-open stdin
        // keeps `magick` alive while the spill file is inspected.
        let args = [
            "-limit", "memory", "0", "-limit", "map", "0", "-limit", "disk", "1GiB",
            "-size", "400x400", "xc:red", "png:-", "-append", "null:",
        ];
        std::thread::scope(|scope| {
            let runner = scope.spawn(|| {
                let mut sink = Vec::new();
                run_filter_in_env("magick", &args, &env, HeldOpen(held), &mut sink, Some(Duration::from_secs(60)))
            });

            let deadline = Instant::now() + Duration::from_secs(20);
            let spilled = loop {
                let found = fs::read_dir(scratch.path()).unwrap().count();
                if found > 0 || Instant::now() > deadline {
                    break found;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let _ = release.send(());
            // The held stream then yields an empty (invalid) second image: an error is expected.
            let _ = runner.join().unwrap();
            assert!(spilled > 0, "no spill file appeared in the private scratch directory");
        });
    }
}
