// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/wayland/handlers/data_control/source.rs

use wayland_client::{Dispatch, Connection, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{self, ExtDataControlSourceV1};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};
use crate::wayland::state::{WaylandState, SourceMetadata, SourcePayload};
use super::mime_is_compatible;
use crate::core::constants::*;
use crate::core::utils::strip_html_tags;

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

                if mime_is_compatible(&mime_type, &meta.mime) {
                    match &meta.payload {
                        SourcePayload::File(path) => {
                            let path = path.clone();

                            // On-demand PNG transcode: the stored bytes are
                            // never re-encoded except for this one case —
                            // a paste target asking specifically for
                            // image/png against a non-PNG original (see
                            // send_transcoded_png).
                            let req_base = crate::core::utils::parse_mime(&mime_type).0;
                            let meta_base = crate::core::utils::parse_mime(&meta.mime).0;
                            let needs_png_transcode = req_base == "image/png"
                                && meta_base.starts_with("image/")
                                && meta_base != "image/png";

                            std::thread::spawn(move || {
                                if needs_png_transcode {
                                    send_transcoded_png(&path, fd);
                                } else {
                                    // Kernel-level egress: hand the
                                    // destination pipe straight to
                                    // sendfile(2) against the cache file.
                                    // The payload's bytes never pass
                                    // through this process's userspace
                                    // heap — no read-into-Vec<u8>, no clone.
                                    send_via_sendfile(&path, fd);
                                }
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

/// On-demand PNG transcode for Egress, streamed with zero userspace copy:
/// each attempt's stdout is wired directly to the Wayland-provided
/// destination pipe (`Stdio::from`), so a possibly-large transcoded image
/// never passes through this process's own heap the way `Command::output()`
/// would. Tries `magick` first, then `ffmpeg`; if neither is installed (or
/// both fail on this particular file), falls back to the untouched original
/// via `send_via_sendfile` rather than leave the requesting app's pipe
/// hanging with nothing ever written to it.
///
/// A converter that spawns successfully but exits non-zero partway through
/// may already have written a partial stream straight to `dest` before
/// failing — an accepted tradeoff of piping directly through rather than
/// buffering first and validating before committing to send anything.
fn send_transcoded_png(path: &Path, dest: OwnedFd) {
    let arg = path.to_string_lossy().into_owned();

    // `dest` is consumed (and, if `magick` isn't installed, closed) the
    // moment it's wired into a child's stdout below — this dup is the only
    // way to still have something to try `ffmpeg` (or the raw fallback)
    // with afterward.
    let Ok(after_magick) = dup_owned_fd(&dest) else {
        send_via_sendfile(path, dest);
        return;
    };
    if try_transcode("magick", &[arg.clone(), "png:-".to_string()], dest) {
        return;
    }

    let Ok(after_ffmpeg) = dup_owned_fd(&after_magick) else {
        send_via_sendfile(path, after_magick);
        return;
    };
    let ffmpeg_args: Vec<String> = [
        "-nostdin", "-loglevel", "error", "-i", arg.as_str(),
        "-f", "image2pipe", "-vcodec", "png", "-",
    ].into_iter().map(str::to_string).collect();
    if try_transcode("ffmpeg", &ffmpeg_args, after_magick) {
        return;
    }

    send_via_sendfile(path, after_ffmpeg);
}

/// Spawns `cmd` with `dest` wired directly as its stdout and waits for it to
/// exit. `child.wait()` (not a bare `spawn()` left to its own devices) is
/// what keeps a finished converter from lingering as a zombie process —
/// this thread owns the child for its entire lifetime.
fn try_transcode(cmd: &str, args: &[String], dest: OwnedFd) -> bool {
    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(dest))
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(child.wait(), Ok(status) if status.success())
}

/// Duplicates `fd` with `O_CLOEXEC` enabled, so a fallback attempt still has a
/// live descriptor to write to after an earlier attempt's `Stdio::from` consumed
/// — and, on a failed spawn, closed — its own copy.
fn dup_owned_fd(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    fd.try_clone()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A minimal, valid 1x1 lossy WebP (VP8) — small enough to embed
    /// directly, real enough for an actual system decoder to accept.
    const MINIMAL_WEBP: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x3C, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50,
        0x56, 0x50, 0x38, 0x20, 0x30, 0x00, 0x00, 0x00, 0xD0, 0x01, 0x00, 0x9D,
        0x01, 0x2A, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x34, 0x25, 0xA0, 0x02,
        0x74, 0xBA, 0x01, 0xF8, 0x00, 0x03, 0xB0, 0x00, 0xFE, 0xF0, 0xC4, 0x0B,
        0xFF, 0x20, 0xB9, 0x61, 0x75, 0xC8, 0xD7, 0xFF, 0x20, 0x3F, 0xE4, 0x07,
        0xFC, 0x80, 0xFF, 0xF8, 0xF2, 0x00, 0x00, 0x00,
    ];

    fn find_any_converter() -> Option<&'static str> {
        ["magick", "ffmpeg"].into_iter().find(|&cmd| {
            Command::new(cmd).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
        })
    }

    #[test]
    fn send_transcoded_png_produces_valid_png_signature() {
        // Environment-dependent by nature (needs an actual system
        // converter installed): skip rather than fail where neither is
        // available, instead of asserting on a tool this test doesn't control.
        if find_any_converter().is_none() {
            eprintln!("skipping: neither magick nor ffmpeg is installed");
            return;
        }

        let tmp_path = std::env::temp_dir().join(format!("y4p-test-{:?}.webp", std::thread::current().id()));
        std::fs::write(&tmp_path, MINIMAL_WEBP).unwrap();

        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid, correctly-sized `&mut [c_int; 2]` for
        // `pipe(2)` to write its two returned descriptors into.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: `fds[0]` is a freshly-created, open, unique descriptor
        // `pipe(2)` just returned above, wrapped exactly once.
        let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: `fds[1]` is a freshly-created, open, unique descriptor
        // `pipe(2)` just returned above, wrapped exactly once.
        let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        send_transcoded_png(&tmp_path, write_fd);

        let mut out = Vec::new();
        std::fs::File::from(read_fd).read_to_end(&mut out).unwrap();
        let _ = std::fs::remove_file(&tmp_path);

        assert!(out.len() >= 8, "expected at least a PNG signature, got {} bytes", out.len());
        assert_eq!(&out[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    }
}
