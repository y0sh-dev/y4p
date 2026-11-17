// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/daemon/src/test_support.rs

//! Test-only helpers shared by the daemon's unit tests.
//!
//! `ClipboardDb::open` resolves its database, cache and socket paths from the
//! process's `XDG_*` environment, and the daemon's tests need a real store (the
//! storage layer exposes no way to open one elsewhere). The environment is
//! process-wide, so this module redirects it under a mutex: only one test at a
//! time can own a [`PrivateStore`], and the previous values are restored — and
//! the tree removed — when it drops.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::storage::ClipboardDb;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Serialises the tests that redirect the process environment.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The four XDG roots a `PrivateStore` redirects, and the subdirectory each
/// gets inside its private tree.
const XDG_VARS: [(&str, &str); 4] = [
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_RUNTIME_DIR", "run"),
];

/// A private `XDG_*` tree, alive (and owning the environment) until dropped.
pub(crate) struct PrivateStore {
    root: PathBuf,
    previous: Vec<(&'static str, Option<OsString>)>,
    // Held for the lifetime of the store: released after `Drop` has restored
    // the environment, since fields drop after the `Drop` impl has run.
    _serialised: MutexGuard<'static, ()>,
}

impl PrivateStore {
    /// Redirects the XDG roots at a fresh tree unique to `tag`, waiting for any
    /// other test that holds them.
    pub(crate) fn new(tag: &str) -> Self {
        let serialised = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("y4p-daemon-unit-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&root);

        let mut previous = Vec::with_capacity(XDG_VARS.len());
        for (var, sub) in XDG_VARS {
            let _ = std::fs::create_dir_all(root.join(sub));
            previous.push((var, std::env::var_os(var)));
            // SAFETY: no other thread in this binary reads or writes these
            // variables, and `ENV_LOCK` keeps every other `PrivateStore` — the
            // only writer — out until this one has restored them on drop.
            unsafe { std::env::set_var(var, root.join(sub)) };
        }
        Self { root, previous, _serialised: serialised }
    }

    /// The writer connection the daemon would own.
    pub(crate) fn open(&self) -> ClipboardDb {
        ClipboardDb::open().unwrap()
    }

    /// IDs currently stored, most recent first.
    pub(crate) fn rows(&self) -> Vec<i64> {
        self.open().fetch_metadata(100).into_iter().map(|row| row.0).collect()
    }

    /// Where the daemon's control socket lives inside this tree.
    pub(crate) fn socket_path(&self) -> PathBuf {
        crate::core::get_socket_path()
    }
}

impl Drop for PrivateStore {
    fn drop(&mut self) {
        for (var, value) in self.previous.drain(..) {
            match value {
                Some(value) => {
                    // SAFETY: this thread still holds `ENV_LOCK`, so no test
                    // that reads these variables can be running.
                    unsafe { std::env::set_var(var, value) }
                }
                None => {
                    // SAFETY: as above: the variables belong to this thread
                    // until the guard is released after this `Drop`.
                    unsafe { std::env::remove_var(var) }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
