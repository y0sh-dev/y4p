// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/wayland/src/handlers/data_control/source.rs

use wayland_client::{Dispatch, Connection, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{self, ExtDataControlSourceV1};
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use crate::state::{WaylandState, SourceMetadata};
use super::mime_is_compatible;
use crate::core::constants::*;
use crate::core::payload::{Payload, PayloadReader};
use crate::core::utils::{is_html_mime, strip_html_tags};

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
                    // Rich markup requested as plain text: strip tags so a
                    // non-HTML consumer gets readable text instead of raw
                    // markup; requesting the markup mime itself still gets the
                    // original bytes untouched.
                    let strip_markup = is_html_mime(&meta.mime) && !is_html_mime(&mime_type);

                    // S-07: uri-list is normalised to plain paths at ingest
                    // (see core::utils::normalize_uri_list), so no per-offer
                    // file:// rewrite is needed here -- every consumer gets the
                    // same already-clean bytes.
                    if let Some(data) = meta.payload.as_bytes() {
                        // In memory: the one unavoidable copy (the source is
                        // served again for every paste), and none at all when
                        // stripping builds a fresh buffer anyway.
                        let data_to_send = if strip_markup { strip_html_tags(data) } else { data.to_vec() };
                        spawn_egress(move || write_bytes(fd, &data_to_send));
                    } else {
                        // Descriptor- or file-backed: never copied into this
                        // process's heap on the plain path (see `transfer`).
                        match meta.payload.try_clone() {
                            Ok(payload) => spawn_egress(move || serve_descriptor_payload(payload, strip_markup, fd)),
                            Err(e) => eprintln!("{}egress: could not duplicate the payload source: {}", LOG_ERROR, e),
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

/// Runs one egress on its own thread, so a slow or stalled reader on the other
/// end of the pipe never blocks the event loop. `Builder::spawn` reports a
/// refused thread instead of panicking; the closure, and the destination fd it
/// owns, is then dropped, which closes the pipe and lets the reader see EOF.
fn spawn_egress<F: FnOnce() + Send + 'static>(job: F) {
    if let Err(e) = std::thread::Builder::new().name("y4p-egress".to_owned()).spawn(job) {
        eprintln!("{}egress: could not start a transfer thread: {}", LOG_ERROR, e);
    }
}

/// Whether `e` only says the receiving client went away mid-paste (for example
/// `head -n 1` closing the pipe after the first line). That is the reader's
/// choice, not a failure of ours, so every egress path stays quiet about it.
fn receiver_left(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset)
}

fn write_bytes(dest: OwnedFd, data: &[u8]) {
    let mut file = std::fs::File::from(dest);
    if let Err(e) = file.write_all(data)
        && !receiver_left(&e)
    {
        eprintln!("{}egress transmission failure: {}", LOG_ERROR, e);
    }
    let _ = file.flush();
}

/// Serves a `Stream` or `File` payload to `dest`.
///
/// The plain path hands the open descriptor to `transfer`, which moves the
/// bytes kernel-side. Only the markup-as-plain-text request must see the bytes,
/// so only that one materialises them (bounded, as at ingest) before stripping.
fn serve_descriptor_payload(payload: Payload, strip_markup: bool, dest: OwnedFd) {
    if strip_markup {
        match payload.read_bounded(MAX_PAYLOAD_BYTES) {
            Ok(data) => write_bytes(dest, &strip_html_tags(&data)),
            Err(e) => eprintln!("{}egress: could not read the payload for markup stripping: {}", LOG_ERROR, e),
        }
        return;
    }

    match payload.into_reader() {
        Ok(reader) => transfer(reader, dest),
        Err(e) => eprintln!("{}egress: failed to open the payload: {}", LOG_ERROR, e),
    }
}

/// Transfers `src`'s remaining bytes into `dest` (a Wayland-provided pipe).
///
/// When the reader has a descriptor (`PayloadReader::raw_fd`) the bytes go
/// through `sendfile(2)`, entirely kernel-side: no read into a `Vec<u8>`, no
/// clone, and no MIME-dependent re-encoding, so the stored bytes go out exactly
/// as received. The transfer runs to EOF; a length hint is never trusted to cap
/// it (it is advisory, and a short one would otherwise truncate the paste).
///
/// `sendfile(2)` is called with an explicit offset, so a seekable source's own
/// file position is left alone (the same file can be served again). A source
/// that cannot be addressed that way -- a pipe or socket, `ESPIPE` -- or a
/// kernel/target that refuses it (`EINVAL`/`ENOSYS`) falls back to a userspace
/// copy of whatever is left, so a paste is never silently empty or partial just
/// because the fast path was unavailable. A reader without a descriptor (an
/// in-memory source) takes that same copy.
fn transfer(mut src: PayloadReader, dest: OwnedFd) {
    let mut dest_file = std::fs::File::from(dest);
    let dest_fd = dest_file.as_raw_fd();

    let Some(src_fd) = src.raw_fd() else {
        copy_remaining(&mut src, &mut dest_file, 0);
        return;
    };

    let mut offset: libc::off_t = 0;
    loop {
        // sendfile(2) caps a single call around 0x7ffff000 bytes on Linux;
        // chunk defensively rather than assume any particular kernel's ceiling.
        let chunk = 1usize << 30;
        // SAFETY: `dest_fd` is owned by `dest_file` and `src_fd` by `src`, both
        // alive and unmoved for this whole call; `offset` is a valid
        // `&mut libc::off_t` for `sendfile(2)` to advance.
        let n = unsafe { libc::sendfile(dest_fd, src_fd, &mut offset, chunk) };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // Receiver closed its end mid-transfer: not our failure to report.
                Some(libc::EPIPE) | Some(libc::ECONNRESET) => {}
                Some(libc::EINTR) => continue,
                Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::ESPIPE) => {
                    copy_remaining(&mut src, &mut dest_file, offset as u64);
                }
                _ => eprintln!("{}egress: sendfile failed: {}", LOG_ERROR, err),
            }
            return;
        }
        if n == 0 { break; } // EOF
    }

    let _ = dest_file.flush();
}

/// Userspace copy for `transfer`, resuming after the `already_sent` bytes the
/// kernel path managed. `sendfile(2)` with an explicit offset never moves the
/// source's position, so those bytes are read and discarded rather than sent
/// twice (they are only ever non-zero after a mid-transfer `EINVAL`, which is
/// rare, so the simple correct skip beats a `Seek` bound on every source).
fn copy_remaining(src: &mut PayloadReader, dest: &mut std::fs::File, already_sent: u64) {
    use std::io::Read;
    if already_sent > 0 {
        match std::io::copy(&mut src.by_ref().take(already_sent), &mut std::io::sink()) {
            Ok(skipped) if skipped == already_sent => {}
            _ => return,
        }
    }
    if let Err(e) = std::io::copy(src, dest)
        && !receiver_left(&e)
    {
        eprintln!("{}egress: copy failed: {}", LOG_ERROR, e);
    }
    let _ = dest.flush();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::payload::StreamDescriptor;
    use std::io::Read;
    use std::path::PathBuf;

    /// Runs `serve` on a thread (the pipe holds only ~64 KiB, so a larger
    /// payload would block a same-thread writer) and returns what arrived.
    fn received_from(serve: impl FnOnce(OwnedFd) + Send + 'static) -> Vec<u8> {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let handle = std::thread::spawn(move || serve(OwnedFd::from(writer)));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        handle.join().unwrap();
        out
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn temp_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("y4p-source-test-{}-{tag}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn stream_of(bytes: &[u8], hint: Option<u64>) -> Payload {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = writer.write_all(&bytes);
        });
        Payload::Stream(StreamDescriptor::new(OwnedFd::from(reader), hint))
    }

    #[test]
    fn a_file_payload_arrives_intact_through_sendfile() {
        let data = pattern(3 * 1024 * 1024 + 17);
        let path = temp_file("file", &data);
        let got = received_from({
            let path = path.clone();
            move |dest| serve_descriptor_payload(Payload::File(path), false, dest)
        });
        assert_eq!(got.len(), data.len());
        assert!(got == data, "file bytes must arrive unchanged");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_seekable_file_can_be_served_twice_with_the_same_bytes() {
        let data = pattern(70_000);
        let path = temp_file("twice", &data);
        for _ in 0..2 {
            let got = received_from({
                let path = path.clone();
                move |dest| serve_descriptor_payload(Payload::File(path), false, dest)
            });
            assert!(got == data);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_stream_payload_falls_back_to_a_userspace_copy_and_arrives_intact() {
        // A pipe cannot be addressed by offset, so `sendfile(2)` refuses it
        // (`ESPIPE`) and the fallback has to carry every byte.
        let data = pattern(200_000);
        let payload = stream_of(&data, Some(data.len() as u64));
        let got = received_from(move |dest| serve_descriptor_payload(payload, false, dest));
        assert!(got == data);
    }

    #[test]
    fn a_wrong_length_hint_neither_truncates_nor_extends_a_stream() {
        let data = pattern(10_000);
        let short = received_from({
            let payload = stream_of(&data, Some(10));
            move |dest| serve_descriptor_payload(payload, false, dest)
        });
        assert!(short == data, "an under-stated hint must not cap the transfer");

        let long = received_from({
            let payload = stream_of(&data, Some(u64::MAX));
            move |dest| serve_descriptor_payload(payload, false, dest)
        });
        assert!(long == data, "an over-stated hint must not pad or hang the transfer");
    }

    #[test]
    fn an_empty_file_and_an_empty_stream_send_nothing_and_do_not_hang() {
        let path = temp_file("empty", b"");
        let got = received_from({
            let path = path.clone();
            move |dest| serve_descriptor_payload(Payload::File(path), false, dest)
        });
        assert!(got.is_empty());
        std::fs::remove_file(path).unwrap();

        let payload = stream_of(b"", None);
        assert!(received_from(move |dest| serve_descriptor_payload(payload, false, dest)).is_empty());
    }

    #[test]
    fn a_missing_file_closes_the_destination_instead_of_hanging_the_reader() {
        let got = received_from(|dest| {
            serve_descriptor_payload(Payload::File(PathBuf::from("/definitely/not/there")), false, dest);
        });
        assert!(got.is_empty());
    }

    #[test]
    fn a_reader_without_a_descriptor_takes_the_copy_path() {
        let data = pattern(5000);
        let reader = Payload::memory(data.clone()).into_reader().unwrap();
        assert!(reader.raw_fd().is_none());
        let got = received_from(move |dest| transfer(reader, dest));
        assert!(got == data);
    }

    #[test]
    fn markup_requested_as_plain_text_is_stripped_for_a_file_payload_too() {
        let path = temp_file("html", b"<p>hello <b>world</b></p>");
        let got = received_from({
            let path = path.clone();
            move |dest| serve_descriptor_payload(Payload::File(path), true, dest)
        });
        assert_eq!(String::from_utf8(got).unwrap().trim(), "hello world");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn markup_requested_as_markup_is_sent_untouched() {
        let path = temp_file("html-raw", b"<p>hi</p>");
        let got = received_from({
            let path = path.clone();
            move |dest| serve_descriptor_payload(Payload::File(path), false, dest)
        });
        assert_eq!(got, b"<p>hi</p>");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_receiver_that_leaves_early_is_not_an_error_or_a_crash() {
        let data = pattern(4 * 1024 * 1024);
        let path = temp_file("early", &data);
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        serve_descriptor_payload(Payload::File(path.clone()), false, OwnedFd::from(writer));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn write_bytes_delivers_an_in_memory_payload() {
        let got = received_from(|dest| write_bytes(dest, b"inline"));
        assert_eq!(got, b"inline");
    }

    /// A pipe whose read end is already closed: every write meets `EPIPE`.
    fn pipe_without_reader() -> OwnedFd {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        OwnedFd::from(writer)
    }

    #[test]
    fn a_vanished_receiver_is_recognised_whichever_way_it_reports_it() {
        use std::io::{Error, ErrorKind};
        assert!(receiver_left(&Error::from(ErrorKind::BrokenPipe)));
        assert!(receiver_left(&Error::from(ErrorKind::ConnectionReset)));
        assert!(!receiver_left(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!receiver_left(&Error::from(ErrorKind::WriteZero)));
    }

    #[test]
    fn write_bytes_survives_a_receiver_that_left_before_the_first_byte() {
        // Larger than a pipe, so the write cannot slip into the buffer first.
        write_bytes(pipe_without_reader(), &pattern(1024 * 1024));
        write_bytes(pipe_without_reader(), b"x");
        write_bytes(pipe_without_reader(), b"");
    }

    #[test]
    fn write_bytes_survives_a_receiver_that_leaves_mid_stream() {
        // `head -n 1` shape: take a little, then close while the sender still
        // has most of the payload to push.
        let (mut reader, writer) = std::io::pipe().unwrap();
        let handle = std::thread::spawn(move || write_bytes(OwnedFd::from(writer), &pattern(4 * 1024 * 1024)));
        let mut first = [0u8; 16];
        reader.read_exact(&mut first).unwrap();
        drop(reader);
        handle.join().unwrap();
    }

    #[test]
    fn markup_stripping_survives_a_receiver_that_left() {
        let path = temp_file("html-early", b"<p>hello</p>");
        serve_descriptor_payload(Payload::File(path.clone()), true, pipe_without_reader());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_copy_fallback_survives_a_receiver_that_left() {
        // A stream source forces the userspace copy (`ESPIPE` from sendfile).
        let data = pattern(1024 * 1024);
        serve_descriptor_payload(stream_of(&data, None), false, pipe_without_reader());
        let reader = Payload::memory(data).into_reader().unwrap();
        transfer(reader, pipe_without_reader());
    }
}
