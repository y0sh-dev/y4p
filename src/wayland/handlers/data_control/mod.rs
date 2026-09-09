// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/handlers/data_control/mod.rs

pub mod device;
pub mod source;

use wayland_client::{Dispatch, Connection, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};
use std::os::fd::{FromRawFd, OwnedFd, AsRawFd};
use std::alloc::{alloc, dealloc, Layout};
use crate::wayland::state::{WaylandState, OfferData};
use crate::core::constants::*;

/// RAII wrapper for page-aligned memory allocation.
/// Ensures optimal performance for kernel-to-user data transfers.
pub struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuffer {
    pub fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("invalid alignment layout");
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }

    /// SOUNDNESS BUGFIX: the previous signature was
    /// `fn as_mut_slice<'a>(&mut self) -> &'a mut [u8]`. `'a` is an
    /// unconstrained lifetime with no relationship to `&mut self`'s
    /// lifetime — the compiler infers it independently at each call site
    /// (commonly as `'static`), so the returned slice is *not* borrow-
    /// checked against `self` at all. Callers are free to let the
    /// `AlignedBuffer` drop (freeing the allocation via `dealloc` in `Drop`)
    /// while still holding the slice, producing a dangling pointer and a
    /// use-after-free that the borrow checker will not catch — precisely
    /// the kind of bug that "compiles cleanly" while being unsound. The
    /// current call site in `device.rs` happens to keep both alive in the
    /// same scope, so it doesn't manifest today, but the API itself is a
    /// live landmine for any future refactor (e.g. buffer pooling/reuse for
    /// the planned Incremental BLOB I/O streaming path) that hands the
    /// slice to a different scope.
    ///
    /// Removing the free lifetime parameter ties the returned slice's
    /// lifetime to `&mut self` as normal Rust elision would, which is what
    /// was clearly intended.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout); }
    }
}

/// F_SETPIPE_SZ: Linux-specific fcntl command to change pipe capacity.
const F_SETPIPE_SZ: libc::c_int = 1031;
/// Default capacity for binary data pipes (4MB) to prevent stalls.
const PIPE_CAPACITY_IMAGE: libc::c_int = 4 * 1024 * 1024;

/// Evaluates if the requested MIME type is compatible with the target type.
/// Supports category-level matching for text and image groups. Both sides
/// are compared via their normalized base type (case- and
/// whitespace-insensitive, parameters stripped — see
/// `core::utils::parse_mime`), so `TEXT/PLAIN`, `text/plain; charset=utf-8`
/// and `text/plain;charset=UTF-8` are all treated identically.
fn mime_is_compatible(requested: &str, target: &str) -> bool {
    let req_base = crate::core::utils::parse_mime(requested).0;
    let tgt_base = crate::core::utils::parse_mime(target).0;

    if req_base == tgt_base { return true; }
    if req_base.starts_with("text/") && tgt_base.starts_with("text/") { return true; }
    if req_base.starts_with("image/") && tgt_base.starts_with("image/") { return true; }

    // Already-lowercased base forms — the old list's charset-suffixed
    // variants are redundant now that params are stripped before comparing.
    const TEXT_ALIASES: &[&str] = &[
        "text/plain",
        "utf8_string",
        "string",
        "text",
        "compound_text",
    ];
    // text/html already matches via the text/* rule above; application/xhtml+xml
    // is HTML in an XML wrapper and needs the same "requestable as plain text" treatment.
    let req_is_text_alias = TEXT_ALIASES.contains(&req_base.as_str());
    let tgt_is_text_alias = TEXT_ALIASES.contains(&tgt_base.as_str())
        || tgt_base.starts_with("text/")
        || tgt_base == "application/xhtml+xml";

    req_is_text_alias && tgt_is_text_alias
}

/// Checks if any offered MIME type matches the sensitive hints blacklist.
///
/// BUGFIX: both the haystack (offered MIME) and the hint now go through
/// `to_ascii_lowercase()`. Previously only the MIME was lowercased while
/// `SENSITIVE_MIME_HINTS` mixes case (`x-kde-passwordManagerHint`) — a
/// lowercased haystack can never contain that hint's own uppercase letters
/// verbatim, so this specific hint could never actually match anything,
/// silently defeating the KDE password-manager filter it exists for.
fn is_sensitive(mimes: &[String]) -> bool {
    SENSITIVE_MIME_HINTS.iter().any(|&hint| {
        let hint_lower = hint.to_ascii_lowercase();
        mimes.iter().any(|m| m.to_ascii_lowercase().contains(&hint_lower))
    })
}

/// Creates a Unix pipe and configures it for safe, high-throughput I/O.
/// Implements immediate RAII wrapping to prevent FD leaks during setup failures.
fn make_pipe(is_image: bool) -> Option<(std::fs::File, OwnedFd)> {
    let mut fds = [0i32; 2];
    
    // Attempt to create the raw pipe
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return None; 
    }

    // RAII Safety: Immediately wrap raw FDs.
    // If the function returns early after this point, FDs are automatically closed.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    unsafe {
        for fd in &[read_fd.as_raw_fd(), write_fd.as_raw_fd()] {
            let flags = libc::fcntl(*fd, libc::F_GETFL, 0);
            if flags >= 0 {
                // Force blocking mode to ensure complete data transfer for large buffers
                libc::fcntl(*fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }

        if is_image {
            // Increase buffer size to prevent deadlock when transferring massive bitmaps
            let _ = libc::fcntl(read_fd.as_raw_fd(), F_SETPIPE_SZ, PIPE_CAPACITY_IMAGE);
        }
    }

    // Convert read side to File for standard I/O compatibility
    Some((std::fs::File::from(read_fd), write_fd))
}

// --- ExtDataControlManagerV1 ---

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(_: &mut Self, _: &ExtDataControlManagerV1, _: ext_data_control_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// --- ExtDataControlOfferV1 ---

impl Dispatch<ExtDataControlOfferV1, OfferData> for WaylandState {
    fn event(_: &mut Self, _: &ExtDataControlOfferV1, ev: ext_data_control_offer_v1::Event, data: &OfferData, _: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = ev
            && let Ok(mut mimes) = data.mimes.lock()
            && !mimes.contains(&mime_type) {
            mimes.push(mime_type);
        }
    }
}
