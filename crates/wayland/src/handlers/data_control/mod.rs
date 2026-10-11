// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/handlers/data_control/mod.rs

pub mod device;
pub mod source;

use wayland_client::{Dispatch, Connection, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};
use std::os::fd::{FromRawFd, OwnedFd, AsRawFd};
use crate::state::{WaylandState, OfferData};
use crate::core::constants::*;

/// Evaluates if the requested MIME type is compatible with the target type.
/// Supports category-level matching for the text group. Both sides are
/// compared via their normalized base type (case- and whitespace-
/// insensitive, parameters stripped — see `core::utils::mime_base`), so
/// `TEXT/PLAIN`, `text/plain; charset=utf-8` and `text/plain;charset=UTF-8`
/// are all treated identically. Zero allocations throughout.
///
/// Every rule here is applied symmetrically — `mime_is_compatible(a, b)` and
/// `mime_is_compatible(b, a)` always agree — since nothing about "requested"
/// vs. "target" makes one side a more authoritative judge of compatibility
/// than the other (see PR #40 review: the previous text-alias rule checked
/// `requested` only against the fixed alias list while letting `target` also
/// match via `text/*`/XHTML, so `text/plain` was compatible with
/// `application/xhtml+xml` only in one direction, and `text/html` with
/// `application/xhtml+xml` in neither).
///
/// Images are deliberately *not* given the same category-level treatment:
/// PNG, JPEG, WebP, GIF, ... are distinct binary encodings, and without
/// dynamic transcoding (removed — see `wayland::handlers::data_control::source`)
/// y4p cannot turn one into another on request. Cross-matching them here
/// would let a target's `Send` handler believe it can satisfy a request for
/// a format it's never actually going to produce, silently sending the
/// wrong bytes (or crashing a strict decoder) instead of this failing
/// cleanly earlier. Images therefore only ever match via the exact-base-type
/// check below.
fn mime_is_compatible(requested: &str, target: &str) -> bool {
    let req_base = crate::core::utils::mime_base(requested);
    let tgt_base = crate::core::utils::mime_base(target);

    if req_base.eq_ignore_ascii_case(tgt_base) { return true; }

    let req_is_text_cat = crate::core::utils::starts_with_ignore_ascii_case(req_base, "text/");
    let tgt_is_text_cat = crate::core::utils::starts_with_ignore_ascii_case(tgt_base, "text/");
    if req_is_text_cat && tgt_is_text_cat { return true; }

    // Symmetric HTML/XHTML markup match: text/html and application/xhtml+xml
    // are both HTML markup, just wrapped differently, so either side being
    // either variant is enough — independent of which one is "requested".
    if crate::core::utils::is_html_mime(req_base) && crate::core::utils::is_html_mime(tgt_base) {
        return true;
    }

    // Symmetric plain-text alias negotiation: X11/Wayland legacy string
    // atoms (TEXT_ALIASES) are requestable wherever any text-compatible MIME
    // (text/*, HTML markup, or another alias) sits on the other side — and,
    // crucially, that "other side" check is applied to whichever of the two
    // is the alias, not fixed to `requested`.
    const TEXT_ALIASES: &[&str] = &[
        "text/plain",
        "utf8_string",
        "string",
        "text",
        "compound_text",
    ];
    let req_is_alias = TEXT_ALIASES.iter().any(|&alias| req_base.eq_ignore_ascii_case(alias));
    let tgt_is_alias = TEXT_ALIASES.iter().any(|&alias| tgt_base.eq_ignore_ascii_case(alias));

    let req_is_text_compatible = req_is_text_cat || req_is_alias || crate::core::utils::is_html_mime(req_base);
    let tgt_is_text_compatible = tgt_is_text_cat || tgt_is_alias || crate::core::utils::is_html_mime(tgt_base);

    (req_is_alias && tgt_is_text_compatible) || (tgt_is_alias && req_is_text_compatible)
}

/// Checks if any offered MIME type matches the sensitive hints blacklist.
///
/// BUGFIX: both the haystack (offered MIME) and the hint now go through
/// `to_ascii_lowercase()`. Previously only the MIME was lowercased while
/// `SENSITIVE_MIME_HINTS` mixes case (`x-kde-passwordManagerHint`) — a
/// lowercased haystack can never contain that hint's own uppercase letters
/// verbatim, so this specific hint could never actually match anything,
/// silently defeating the KDE password-manager filter it exists for.
pub(crate) fn is_sensitive<S: AsRef<str>>(mimes: &[S]) -> bool {
    SENSITIVE_MIME_HINTS.iter().any(|&hint| {
        let hint_lower = hint.to_ascii_lowercase();
        mimes.iter().any(|m| m.as_ref().to_ascii_lowercase().contains(&hint_lower))
    })
}

/// Creates a Unix pipe and configures it for safe, high-throughput I/O.
/// Implements immediate RAII wrapping to prevent FD leaks during setup failures.
fn make_pipe() -> Option<(std::fs::File, OwnedFd)> {
    let mut fds = [0i32; 2];

    // SAFETY: `fds` is a valid, correctly-sized `&mut [c_int; 2]` for
    // `pipe(2)` to write its two returned descriptors into.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return None;
    }

    // RAII Safety: Immediately wrap raw FDs.
    // If the function returns early after this point, FDs are automatically closed.
    // SAFETY: `fds[0]` is a freshly-created, open, unique descriptor `pipe(2)`
    // just returned above, wrapped here exactly once.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: same as `read_fd` above, for the pipe's other (write) end.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // SAFETY: every `libc` call below only reads/sets flags on the two FDs
    // owned above (still valid — neither `OwnedFd` has been dropped yet).
    unsafe {
        for fd in &[read_fd.as_raw_fd(), write_fd.as_raw_fd()] {
            let flags = libc::fcntl(*fd, libc::F_GETFL, 0);
            if flags >= 0 {
                // Force blocking mode to ensure complete data transfer for large buffers
                libc::fcntl(*fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn is_sensitive_detects_all_configured_hints() {
        assert!(is_sensitive(&["x-kde-passwordManagerHint"]));
        assert!(is_sensitive(&["X-KDE-PASSWORDMANAGERHINT"]));
        assert!(is_sensitive(&["application/x-keepassxc-selection"]));
        assert!(is_sensitive(&["application/x-vnd.1password"]));
        assert!(is_sensitive(&["application/x-bitwarden"]));
        assert!(is_sensitive(&["x-gnome-cliptrace"]));
        assert!(is_sensitive(&["org.nspasteboard.ConcealedType"]));
        assert!(is_sensitive(&["custom/secret-payload"]));
        assert!(is_sensitive(&["text/plain", "application/x-keepassxc-selection"]));
    }

    #[test]
    fn is_sensitive_clean_mimes_pass() {
        assert!(!is_sensitive(&["text/plain"]));
        assert!(!is_sensitive(&["text/plain;charset=utf-8"]));
        assert!(!is_sensitive(&["text/html"]));
        assert!(!is_sensitive(&["image/png"]));
        assert!(!is_sensitive(&["text/uri-list"]));
    }

    #[test]
    fn is_sensitive_empty_list_returns_false() {
        let empty: &[&str] = &[];
        assert!(!is_sensitive(empty));
    }

    // --- mime_is_compatible ---

    #[test]
    fn mime_is_compatible_exact_match() {
        assert!(mime_is_compatible("text/plain", "text/plain"));
        assert!(mime_is_compatible("image/png", "image/png"));
    }

    #[test]
    fn mime_is_compatible_exact_match_is_case_and_param_insensitive() {
        assert!(mime_is_compatible("TEXT/Plain", "text/plain; charset=utf-8"));
    }

    #[test]
    fn mime_is_compatible_distinct_image_formats_never_cross_match() {
        // The behaviour this whole refactor exists to enforce: without
        // dynamic transcoding, a request for one image format must never
        // be satisfied by a different one.
        assert!(!mime_is_compatible("image/png", "image/jpeg"));
        assert!(!mime_is_compatible("image/webp", "image/png"));
        assert!(!mime_is_compatible("image/gif", "image/svg+xml"));
    }

    #[test]
    fn mime_is_compatible_text_category_cross_matches() {
        assert!(mime_is_compatible("text/plain", "text/html"));
        assert!(mime_is_compatible("text/markdown", "text/plain"));
    }

    #[test]
    fn mime_is_compatible_text_aliases_match_plain_text_family() {
        assert!(mime_is_compatible("UTF8_STRING", "text/plain"));
        assert!(mime_is_compatible("text/plain", "STRING"));
        assert!(mime_is_compatible("TEXT", "compound_text"));
    }

    #[test]
    fn mime_is_compatible_text_alias_matches_xhtml_target() {
        assert!(mime_is_compatible("text/plain", "application/xhtml+xml"));
    }

    #[test]
    fn mime_is_compatible_unrelated_non_text_types_do_not_match() {
        assert!(!mime_is_compatible("application/json", "application/pdf"));
        assert!(!mime_is_compatible("image/png", "text/plain"));
    }

    // --- Bidirectional regression tests (PR #40 review) ---

    #[test]
    fn mime_is_compatible_html_and_xhtml_match_in_both_directions() {
        assert!(mime_is_compatible("text/html", "application/xhtml+xml"));
        assert!(mime_is_compatible("application/xhtml+xml", "text/html"));
    }

    #[test]
    fn mime_is_compatible_html_and_xhtml_match_with_parameters_in_both_directions() {
        assert!(mime_is_compatible("text/html; charset=utf-8", "application/xhtml+xml"));
        assert!(mime_is_compatible("application/xhtml+xml", "TEXT/HTML; charset=utf-8"));
    }

    #[test]
    fn mime_is_compatible_text_plain_alias_and_xhtml_match_in_both_directions() {
        assert!(mime_is_compatible("text/plain", "application/xhtml+xml"));
        assert!(mime_is_compatible("application/xhtml+xml", "text/plain"));
    }

    #[test]
    fn mime_is_compatible_utf8_string_alias_and_xhtml_match_in_both_directions() {
        assert!(mime_is_compatible("UTF8_STRING", "application/xhtml+xml"));
        assert!(mime_is_compatible("application/xhtml+xml", "UTF8_STRING"));
    }

    #[test]
    fn mime_is_compatible_alias_and_text_category_match_in_both_directions() {
        assert!(mime_is_compatible("STRING", "text/markdown"));
        assert!(mime_is_compatible("text/markdown", "STRING"));
    }

    #[test]
    fn mime_is_compatible_two_aliases_match_each_other() {
        assert!(mime_is_compatible("utf8_string", "compound_text"));
        assert!(mime_is_compatible("compound_text", "utf8_string"));
    }
}
