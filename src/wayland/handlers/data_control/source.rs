// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/handlers/data_control/source.rs

use wayland_client::{Dispatch, Connection, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{self, ExtDataControlSourceV1};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use crate::wayland::state::{WaylandState, SourceMetadata, SourcePayload};
use super::mime_is_compatible;
use crate::core::constants::*;
use crate::core::utils::{strip_html_tags, mime_base_eq};

// --- ExtDataControlSourceV1 ---

impl Dispatch<ExtDataControlSourceV1, SourceMetadata> for WaylandState {
    fn event(state: &mut Self, _source: &ExtDataControlSourceV1, ev: ext_data_control_source_v1::Event, meta: &SourceMetadata, _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                // SAFETY: `raw` is the valid, open fd the compositor just
                // handed us in this `Send` event; `fcntl` only reads/sets
                // its status flags.
                unsafe {
                    let raw = fd.as_raw_fd();
                    let flags = libc::fcntl(raw, libc::F_GETFL, 0);
                    if flags >= 0 {
                        libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK);
                    }
                }

                // Multi-layer defense (image egress): images get their own
                // dispatch — true MIME and on-demand image/png are both
                // satisfiable from one stored payload, neither of which
                // `mime_is_compatible`'s any-image-to-any-image rule (built
                // for the old, since-removed alt-Offer scheme) models
                // correctly any more.
                if meta.mime.starts_with("image/") {
                    handle_image_send(&meta.mime, &meta.payload, &mime_type, fd);
                } else if mime_is_compatible(&mime_type, &meta.mime) {
                    match &meta.payload {
                        // Kernel-level egress: hand the destination pipe
                        // straight to sendfile(2) against the cache file.
                        // The payload's bytes never pass through this
                        // process's userspace heap — no read-into-Vec<u8>,
                        // no clone.
                        SourcePayload::File(path) => {
                            let path = path.clone();
                            std::thread::spawn(move || {
                                send_via_sendfile(&path, fd);
                            });
                        }
                        // S-07: uri-list is normalized to plain paths at ingest
                        // (see core::utils::normalize_uri_list), so the
                        // per-offer file:// rewrite this used to do on egress
                        // is no longer needed — every consumer gets the same
                        // already-clean bytes.
                        SourcePayload::Owned(data) => {
                            let mut file = std::fs::File::from(fd);

                            // Rich markup requested as plain text: strip tags
                            // so a non-HTML consumer gets readable text instead
                            // of raw markup; requesting the markup mime itself
                            // still gets the original bytes untouched.
                            let is_rich_markup = meta.mime == "text/html" || meta.mime == "application/xhtml+xml";
                            let wants_markup = mime_type.starts_with("text/html") || mime_type.starts_with("application/xhtml");
                            let data_to_send = if is_rich_markup && !wants_markup {
                                strip_html_tags(data)
                            } else {
                                data.clone()
                            };

                            std::thread::spawn(move || {
                                if let Err(e) = file.write_all(&data_to_send) {
                                    eprintln!("{}egress transmission failure: {}", LOG_ERROR, e);
                                }
                                let _ = file.flush();
                            });
                        }
                    }
                } else {
                    drop(std::fs::File::from(fd));
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.current_source = None;
                
                if state.verbose {
                    println!("{}clipboard ownership relinquished.", LOG_INFO);
                }
            }
            _ => {}
        }
    }
}

/// Transfer `path`'s entire contents into `dest` (a Wayland-provided pipe
/// fd) via `sendfile(2)`, entirely kernel-side. Runs on its own spawned
/// thread (see caller), same as the pre-existing `write_all` path, so a
/// slow/stalled reader on the other end never blocks the event loop.
///
/// Falls back to a bounded userspace copy loop only if `sendfile(2)` itself
/// reports it can't be used for this destination (EINVAL/ENOSYS) — rare for
/// a plain pipe, but this keeps a `copy-to` of a large image from silently
/// producing empty/partial clipboard content on an exotic kernel/target
/// instead of just failing outright.
fn send_via_sendfile(path: &Path, dest: OwnedFd) {
    let mut dest_file = std::fs::File::from(dest);
    let dest_fd = dest_file.as_raw_fd();

    let src_file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}egress: failed to open cached payload {}: {}", LOG_ERROR, path.display(), e);
            return;
        }
    };
    let src_fd = src_file.as_raw_fd();

    let total_len = match src_file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("{}egress: failed to stat cached payload {}: {}", LOG_ERROR, path.display(), e);
            return;
        }
    };

    let mut offset: libc::off_t = 0;
    let mut remaining = total_len as usize;

    while remaining > 0 {
        // sendfile(2) caps a single call around 0x7ffff000 bytes on Linux;
        // clipboard payloads are far below that, but chunk defensively
        // rather than assume any particular kernel's exact ceiling.
        let chunk = remaining.min(1usize << 30);
        // SAFETY: `dest_fd`/`src_fd` are valid, open descriptors owned by
        // `dest_file`/`src_file` (alive for this whole call), and `offset`
        // is a valid `&mut libc::off_t` for `sendfile(2)` to advance.
        let n = unsafe { libc::sendfile(dest_fd, src_fd, &mut offset, chunk) };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // Receiver closed its end mid-transfer: not our failure to report.
                Some(libc::EPIPE) | Some(libc::ECONNRESET) => {}
                Some(libc::EINTR) => continue,
                Some(libc::EINVAL) | Some(libc::ENOSYS) => {
                    fallback_copy(src_file, dest_file, offset as u64, total_len);
                }
                _ => eprintln!("{}egress: sendfile failed for {}: {}", LOG_ERROR, path.display(), err),
            }
            return;
        }
        if n == 0 { break; } // Shouldn't happen before `remaining` hits 0; stop cleanly rather than spin.
        remaining -= n as usize;
    }

    let _ = dest_file.flush();
}

/// Userspace copy fallback for `send_via_sendfile`, resuming from wherever
/// `sendfile(2)` left off (`start_offset`) rather than restarting the
/// transfer from byte zero.
fn fallback_copy(mut src: std::fs::File, mut dest: std::fs::File, start_offset: u64, total_len: u64) {
    if src.seek(SeekFrom::Start(start_offset)).is_err() { return; }
    let mut buf = vec![0u8; 65536];
    let mut copied = start_offset;
    while copied < total_len {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if dest.write_all(&buf[..n]).is_err() { break; }
        copied += n as u64;
    }
    let _ = dest.flush();
}

/// Routes a `Send` request for an image record to one of the two MIMEs
/// `daemon::handle_restore_request` actually offers for it: the true format,
/// or the `image/png` compatibility layer. Any other request (a client
/// asking for something never offered) is refused, same as the pre-existing
/// `mime_is_compatible` fallthrough.
fn handle_image_send(true_mime: &str, payload: &SourcePayload, requested: &str, fd: OwnedFd) {
    if mime_base_eq(requested, true_mime) {
        send_raw(payload, fd);
    } else if mime_base_eq(requested, "image/png") {
        send_as_png(payload, fd);
    } else {
        drop(std::fs::File::from(fd));
    }
}

fn clone_payload(payload: &SourcePayload) -> SourcePayload {
    match payload {
        SourcePayload::File(path) => SourcePayload::File(path.clone()),
        SourcePayload::Owned(data) => SourcePayload::Owned(data.clone()),
    }
}

/// Spawns the thread that actually performs `send_raw_blocking` — the entry
/// point used directly from the `Send` dispatch (not yet on a thread of its
/// own). `send_as_png`'s no-converter fallback calls the blocking variant
/// directly instead, since it's already running on its own spawned thread.
fn send_raw(payload: &SourcePayload, fd: OwnedFd) {
    let owned = clone_payload(payload);
    std::thread::spawn(move || send_raw_blocking(&owned, fd));
}

fn send_raw_blocking(payload: &SourcePayload, fd: OwnedFd) {
    match payload {
        SourcePayload::File(path) => send_via_sendfile(path, fd),
        SourcePayload::Owned(data) => {
            let mut file = std::fs::File::from(fd);
            if let Err(e) = file.write_all(data) {
                eprintln!("{}egress transmission failure: {}", LOG_ERROR, e);
            }
            let _ = file.flush();
        }
    }
}

/// `image/png` compatibility Offer: every major Wayland toolkit (GTK/Qt/
/// Chromium) hardcodes PNG as the one bitmap format it will even ask for, so
/// a non-PNG stored image has to become one on demand here, or paste fails
/// outright regardless of what else was Offered. Runs entirely on its own
/// spawned thread — converter discovery and the conversion itself are both
/// child-process calls, neither of which may ever touch the daemon's main
/// poll loop.
fn send_as_png(payload: &SourcePayload, fd: OwnedFd) {
    let owned = clone_payload(payload);
    std::thread::spawn(move || {
        match find_png_converter().and_then(|c| convert_to_png(c, &owned)) {
            Some(png) => {
                let mut file = std::fs::File::from(fd);
                if let Err(e) = file.write_all(&png) {
                    eprintln!("{}egress transmission failure: {}", LOG_ERROR, e);
                }
                let _ = file.flush();
            }
            // No converter installed, or it failed on this particular
            // input: send the untouched original rather than fail the
            // paste outright — a lenient app parsing it anyway beats a
            // guaranteed failure (see the requirement doc's §2's rationale).
            None => send_raw_blocking(&owned, fd),
        }
    });
}

/// Checks for a system image converter in the priority order the
/// requirement specifies. A `-version` invocation that spawns at all is
/// treated as "installed" — the exit code isn't checked, since a bare
/// version query returning non-zero on some builds wouldn't mean the tool
/// is actually missing.
fn find_png_converter() -> Option<&'static str> {
    ["magick", "convert", "dwebp"].into_iter().find(|&cmd| {
        std::process::Command::new(cmd)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

/// Runs `converter` against `payload`'s bytes and returns the resulting PNG,
/// or `None` on any failure (missing input, non-zero exit, empty output) —
/// every failure mode collapses to the same "couldn't convert" signal so the
/// caller's raw-bytes fallback is the only place that has to reason about why.
fn convert_to_png(converter: &str, payload: &SourcePayload) -> Option<Vec<u8>> {
    match payload {
        SourcePayload::File(path) => run_converter(converter, path),
        SourcePayload::Owned(data) => {
            // No cache-file path to hand the converter directly — images
            // are always cache-backed in practice (see
            // storage::upsert_record), so this is a defensive spill-to-disk
            // fallback, not the expected case. Unique per call (pid +
            // thread id) since concurrent Send events each run their own
            // conversion on their own thread.
            let tmp = std::env::temp_dir().join(format!(
                "y4p-egress-{}-{:?}.tmp",
                std::process::id(),
                std::thread::current().id()
            ));
            if std::fs::write(&tmp, data).is_err() { return None; }
            let result = run_converter(converter, &tmp);
            let _ = std::fs::remove_file(&tmp);
            result
        }
    }
}

fn run_converter(converter: &str, input: &Path) -> Option<Vec<u8>> {
    // ImageMagick's `magick`/`convert` take an output *format*, not a path,
    // to write to stdout (`png:-`); dwebp instead takes a literal `-o -`.
    let args: Vec<String> = if converter == "dwebp" {
        vec![input.to_string_lossy().into_owned(), "-o".into(), "-".into()]
    } else {
        vec![input.to_string_lossy().into_owned(), "png:-".into()]
    };

    let output = std::process::Command::new(converter).args(&args).output().ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}
