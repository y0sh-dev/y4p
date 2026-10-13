// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/route.rs

//! MIME-aware dispatch for ingestion: decides whether a clipboard payload
//! should be transcoded (privacy-stripped and compressed) and, if so, runs
//! the transcoder with a guaranteed fall-back to the untouched original.
//!
//! The contract is "never lose data, never make things worse": whatever goes
//! wrong, the caller still gets back a usable `(mime, bytes)` pair.
//!
//! Conversions run behind a [`CircuitBreaker`]: a converter that keeps
//! failing is skipped for a cooldown instead of being re-spawned per image.
//! They also run under a [`ProcessThrottle`]: only a few converter processes
//! exist at once, and an image that cannot get a slot in time is stored
//! unmodified. The two are orthogonal: a busy converter is not a failing one.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use std::time::Duration;

use crate::core::constants::{
    IMAGE_BREAKER_COOLDOWN_SECS, IMAGE_BREAKER_THRESHOLD, IMAGE_CONCURRENCY_LIMIT,
    IMAGE_OUTPUT_HINT_CAP_BYTES, IMAGE_THROTTLE_WAIT_MS, LOG_WARN,
    MSG_IMAGE_TOOL_MISSING, log_image_breaker_closed, log_image_breaker_open,
    log_image_scratch_unavailable, log_image_throttled,
};
use crate::core::utils::{detect_image_mime, is_animated_webp};
use crate::breaker::{CircuitBreaker, PermitOutcome};
use crate::magick;
use crate::pipeline::{PipelineError, Transfer};
use crate::throttle::ProcessThrottle;
use crate::transcode::{self, InputFormat, OutputFormat, Transcode};

/// Bytes inspected when classifying a payload. Binary image signatures fit
/// in 12; the rest is slack for leading whitespace before an SVG prolog.
const SNIFF_HEAD_BYTES: usize = 512;

/// What an ingested image of format `input` is converted to, or `None` to
/// store it untouched.
///
/// - PNG, BMP, JPEG and static WebP all become lossless PNG: pixels bit-exact
///   (the decode of a JPEG or WebP is kept as is, with no further generation
///   loss), EXIF orientation baked in, every metadata block removed. PNG is
///   the one raster MIME every Wayland paste target requests, so a stored
///   `image/webp` could not be pasted into most applications.
/// - GIF and animated WebP are left alone: transcoding keeps only the first frame
///   (a safety property for untrusted input), which would silently destroy
///   animations the user copied.
///
/// A JPEG or WebP source grows when re-encoded losslessly (several times for
/// a photo); that is the price of an encoding every client accepts.
pub fn ingest_target(input: InputFormat) -> Option<OutputFormat> {
    match input {
        InputFormat::Png | InputFormat::Bmp | InputFormat::Jpeg | InputFormat::Webp => Some(OutputFormat::Png),
        InputFormat::Gif => None,
    }
}

/// Decides which decoder (if any) a payload belongs to. The bytes outrank
/// the label: when the leading bytes identify an image, that identification
/// is final — so an SVG announced as `image/png` is recognised as SVG and
/// bypassed instead of being handed to a raster decoder. Only when the bytes
/// are not recognised (BMP has no short signature; corrupt data has none)
/// does the declared MIME decide. Non-images yield `None` and cost nothing.
fn classify(mime: &str, payload: &[u8]) -> Option<InputFormat> {
    let head = payload.get(..SNIFF_HEAD_BYTES).unwrap_or(payload);
    match detect_image_mime(head) {
        Some(detected) => InputFormat::from_mime(detected),
        None => InputFormat::from_mime(mime),
    }
}

/// A converter must return a plausible file of the promised type; anything
/// else (empty, or the wrong signature) is treated as a failed conversion
/// rather than stored.
fn is_valid_output(target: OutputFormat, converted: &[u8]) -> bool {
    !converted.is_empty() && detect_image_mime(converted) == Some(target.mime())
}

/// Whether a failed conversion says something about the converter's health.
/// Crashes, non-zero exits, deadline overruns and broken pipes do. A missing
/// tool (`Spawn`) is a different, static condition handled by the
/// availability probe, `Isolation` (no private scratch directory) is a
/// property of the host, and `Source`/`Sink` errors concern the caller's own
/// streams; counting any of them would let an unrelated problem open the circuit.
fn is_converter_fault(error: &PipelineError) -> bool {
    matches!(
        error,
        PipelineError::Failed { .. } | PipelineError::Timeout(_) | PipelineError::Child(_)
    )
}

/// Core routing, with the converter injected so every branch — including
/// "no converter was even invoked" — is deterministic to test.
///
/// Returns `(mime, bytes)` to persist: the converted pair on success, the
/// original pair, byte for byte, on bypass or on any failure.
///
/// Breaker accounting happens only around a real conversion attempt, so the
/// counters cannot be polluted by what never reaches the converter: payloads
/// bypassed by classification or policy never touch the breaker, a missing
/// tool is released neutrally, and while the circuit is open the original is
/// returned without calling `convert` at all.
///
/// Concurrency is capped by `throttle`. The breaker is consulted first so an
/// open circuit never makes anyone wait for a slot. If no slot frees up within
/// the throttle's budget the original is returned and the breaker permit is
/// released neutrally: a busy converter is not a broken one, and counting it
/// would let a burst of images open the circuit and suspend conversion for
/// the cooldown even though the converter is healthy.
pub fn route_with<F>(
    mime: String,
    payload: Vec<u8>,
    breaker: &CircuitBreaker,
    throttle: &ProcessThrottle,
    convert: F,
) -> (String, Vec<u8>)
where
    F: FnOnce(Transcode, &[u8], &mut Vec<u8>) -> Result<Transfer, PipelineError>,
{
    // Nothing to convert (ingestion already rejects empty reads; this keeps
    // the function safe on its own): never spawn a process for zero bytes.
    if payload.is_empty() {
        return (mime, payload);
    }
    let Some(input) = classify(&mime, &payload) else { return (mime, payload) };
    if input == InputFormat::Webp && is_animated_webp(&payload) {
        return (mime, payload);
    }
    let Some(output) = ingest_target(input) else { return (mime, payload) };

    let Some(permit) = breaker.acquire() else { return (mime, payload) };

    let Some(slot) = throttle.acquire() else {
        permit.release();
        eprintln!("{}", log_image_throttled(duration_ms(throttle.wait())));
        return (mime, payload);
    };

    let mut converted = Vec::with_capacity(output_capacity_hint(payload.len()));
    let result = convert(Transcode { input, output }, &payload, &mut converted);
    // The process is gone: hand the slot to the next waiter before doing the
    // (cheap, but lock-taking) bookkeeping below.
    drop(slot);

    let (outcome, result_pair) = match result {
        Ok(_) if is_valid_output(output, &converted) => {
            (PermitOutcome::Success, (output.mime().to_owned(), converted))
        }
        // "Success" that produced no usable file is a converter fault too.
        Ok(_) => (PermitOutcome::Failure, (mime, payload)),
        Err(error) if is_converter_fault(&error) => (PermitOutcome::Failure, (mime, payload)),
        Err(_) => (PermitOutcome::Release, (mime, payload)),
    };

    let (tripped, recovered) = permit.resolve(outcome);
    if recovered {
        eprintln!("{}", log_image_breaker_closed());
    } else if tripped {
        report_failure(true);
    }

    result_pair
}

fn report_failure(tripped: bool) {
    if tripped {
        eprintln!("{}", log_image_breaker_open(IMAGE_BREAKER_THRESHOLD, IMAGE_BREAKER_COOLDOWN_SECS));
    }
}

/// How much output buffer to reserve before streaming a conversion of
/// `input_len` bytes. Without a hint the sink grows by repeated doubling while
/// the child streams, copying everything written so far at each step; with an
/// estimate close to the final size that is one allocation. The input length
/// is that estimate (see `IMAGE_OUTPUT_HINT_CAP_BYTES`), clamped so one huge
/// input cannot reserve more than the cap.
fn output_capacity_hint(input_len: usize) -> usize {
    input_len.min(IMAGE_OUTPUT_HINT_CAP_BYTES)
}

/// Whole milliseconds for log output, saturating instead of truncating.
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Ingestion entry point: [`route_with`] backed by the real `magick` and the
/// process-wide breaker and throttle.
///
/// `magick` is only probed (and only spawned) once a payload has been
/// classified as a convertible raster image, so text, HTML, SVG and URI
/// lists never pay for it. When it is missing, a single warning is printed
/// per process and images are stored unmodified. The same holds, with its own
/// single warning, when no private scratch directory can be prepared (see
/// `image::sandbox`): the converter is then not run at all rather than left to
/// spill into a shared temp directory.
pub fn route_for_ingest(mime: String, payload: Vec<u8>) -> (String, Vec<u8>) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    static WARNED_SCRATCH: AtomicBool = AtomicBool::new(false);
    static BREAKER: CircuitBreaker =
        CircuitBreaker::new(IMAGE_BREAKER_THRESHOLD, Duration::from_secs(IMAGE_BREAKER_COOLDOWN_SECS));
    static THROTTLE: ProcessThrottle =
        ProcessThrottle::new(IMAGE_CONCURRENCY_LIMIT, Duration::from_millis(IMAGE_THROTTLE_WAIT_MS));

    route_with(mime, payload, &BREAKER, &THROTTLE, |job, input, output| {
        if !magick::is_available() {
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("{}{}", LOG_WARN, MSG_IMAGE_TOOL_MISSING);
            }
            return Err(PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound)));
        }
        let result = transcode::transcode(job, input, output);
        if let Err(PipelineError::Isolation(reason)) = &result
            && !WARNED_SCRATCH.swap(true, Ordering::Relaxed)
        {
            eprintln!("{}", log_image_scratch_unavailable(&reason.to_string()));
        }
        result
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::empty;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Instant;
    use crate::breaker::BreakerState;

    fn magick_or_skip() -> bool {
        if magick::is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    fn generate(args: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        magick::run(args, empty(), &mut out).unwrap();
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn png() -> Vec<u8> {
        generate(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"])
    }

    /// A converter reply that passes `is_valid_output` for the PNG target.
    fn fake_png() -> Vec<u8> {
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(b"payload");
        data
    }

    /// A WebP payload: valid *input* for the classifier, but now a wrong-kind
    /// *reply* for the PNG target.
    fn fake_webp() -> Vec<u8> {
        let mut data = b"RIFF".to_vec();
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(b"WEBP");
        data.extend_from_slice(b"payload");
        data
    }

    fn fake_animated_webp() -> Vec<u8> {
        let mut data = vec![0u8; 24];
        data[0..4].copy_from_slice(b"RIFF");
        data[8..12].copy_from_slice(b"WEBP");
        data[12..16].copy_from_slice(b"VP8X");
        data[20] = 0x02; // Animation flag
        data.extend_from_slice(b"extra_frame_data");
        data
    }

    fn fresh_breaker() -> CircuitBreaker {
        CircuitBreaker::new(3, Duration::from_secs(30))
    }

    /// A throttle that never gets in the way of tests that are not about it.
    fn roomy_throttle() -> ProcessThrottle {
        ProcessThrottle::new(64, Duration::from_secs(30))
    }

    /// Runs `route_with` on `breaker` and `throttle` with a converter that
    /// records how it was called and answers with `reply`.
    fn route_through(
        breaker: &CircuitBreaker,
        throttle: &ProcessThrottle,
        mime: &str,
        payload: Vec<u8>,
        reply: Result<Vec<u8>, PipelineError>,
    ) -> ((String, Vec<u8>), Option<Transcode>) {
        let called = Cell::new(None);
        let routed = route_with(mime.to_owned(), payload, breaker, throttle, |job, _, out| {
            called.set(Some(job));
            let bytes = reply?;
            let len = bytes.len() as u64;
            *out = bytes;
            Ok(Transfer { bytes_in: 0, bytes_out: len })
        });
        (routed, called.get())
    }

    /// Same, on an unconstrained throttle.
    fn route_on(
        breaker: &CircuitBreaker,
        mime: &str,
        payload: Vec<u8>,
        reply: Result<Vec<u8>, PipelineError>,
    ) -> ((String, Vec<u8>), Option<Transcode>) {
        route_through(breaker, &roomy_throttle(), mime, payload, reply)
    }

    /// Same, on a throwaway breaker (for tests that are not about the breaker).
    fn route_recording(
        mime: &str,
        payload: Vec<u8>,
        reply: Result<Vec<u8>, PipelineError>,
    ) -> ((String, Vec<u8>), Option<Transcode>) {
        route_on(&fresh_breaker(), mime, payload, reply)
    }

    fn png_header() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3]
    }

    fn failed() -> PipelineError {
        PipelineError::Failed { code: Some(1), stderr: "boom".into() }
    }

    // --- bypass: the converter must not even be invoked ---

    #[test]
    fn non_raster_payloads_bypass_without_invoking_the_converter() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec();
        let xml_svg = b"<?xml version=\"1.0\"?><svg/>".to_vec();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("text/plain", b"hello world".to_vec()),
            ("text/plain;charset=utf-8", "日本語のテキスト".as_bytes().to_vec()),
            ("text/html", b"<p>hi</p>".to_vec()),
            ("text/uri-list", b"/home/user/a.png".to_vec()),
            ("application/json", b"{\"a\":1}".to_vec()),
            ("application/octet-stream", vec![0, 1, 2, 3, 4, 5]),
            ("image/svg+xml", svg.clone()),
            ("image/svg+xml", xml_svg),
            // The label lies: bytes that are SVG must still be bypassed.
            ("image/png", svg),
        ];
        for (mime, payload) in cases {
            let (routed, called) = route_recording(mime, payload.clone(), Ok(fake_png()));
            assert!(called.is_none(), "converter invoked for {mime}");
            assert_eq!(routed, (mime.to_owned(), payload));
        }
    }

    #[test]
    fn avif_is_recognised_as_unsupported_and_bypassed() {
        let mut avif = vec![0, 0, 0, 0x20];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0; 16]);
        let (routed, called) = route_recording("image/avif", avif.clone(), Ok(fake_png()));
        assert!(called.is_none());
        assert_eq!(routed, ("image/avif".to_owned(), avif));
    }

    #[test]
    fn gif_is_left_untouched_by_policy_so_its_animation_survives() {
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
        let (routed, called) = route_recording("image/gif", gif.clone(), Ok(fake_png()));
        assert!(called.is_none(), "image/gif must not be re-encoded");
        assert_eq!(routed, ("image/gif".to_owned(), gif));
    }

    #[test]
    fn animated_webp_is_left_untouched_by_policy_so_its_animation_survives() {
        let webp = fake_animated_webp();
        let (routed, called) = route_recording("image/webp", webp.clone(), Ok(fake_png()));
        assert!(called.is_none(), "animated webp must not be re-encoded");
        assert_eq!(routed, ("image/webp".to_owned(), webp));
    }

    #[test]
    fn empty_payload_bypasses_even_when_labelled_as_a_raster_image() {
        let (routed, called) = route_recording("image/png", Vec::new(), Ok(fake_png()));
        assert!(called.is_none(), "no process may be spawned for zero bytes");
        assert_eq!(routed, ("image/png".to_owned(), Vec::new()));
    }

    // --- policy and classification ---

    #[test]
    fn png_is_routed_to_png_and_the_bytes_are_replaced() {
        let original = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let (routed, called) = route_recording("image/png", original, Ok(fake_png()));
        assert_eq!(called, Some(Transcode { input: InputFormat::Png, output: OutputFormat::Png }));
        assert_eq!(routed, ("image/png".to_owned(), fake_png()));
    }

    #[test]
    fn jpeg_is_routed_to_lossless_png() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];
        let (routed, called) = route_recording("image/jpeg", jpeg, Ok(fake_png()));
        assert_eq!(called, Some(Transcode { input: InputFormat::Jpeg, output: OutputFormat::Png }));
        assert_eq!(routed, ("image/png".to_owned(), fake_png()));
    }

    #[test]
    fn webp_is_routed_to_lossless_png_so_that_it_can_be_pasted() {
        let mut webp = fake_webp();
        webp.extend_from_slice(&[0; 16]);
        let (routed, called) = route_recording("image/webp", webp, Ok(fake_png()));
        assert_eq!(called, Some(Transcode { input: InputFormat::Webp, output: OutputFormat::Png }));
        assert_eq!(routed, ("image/png".to_owned(), fake_png()));
    }

    #[test]
    fn bmp_without_a_signature_is_routed_by_its_declared_mime() {
        let bmp = b"BM\x36\x00\x00\x00".to_vec();
        let (routed, called) = route_recording("image/bmp", bmp, Ok(fake_png()));
        assert_eq!(called, Some(Transcode { input: InputFormat::Bmp, output: OutputFormat::Png }));
        assert_eq!(routed.1, fake_png());
    }

    #[test]
    fn every_raster_input_but_gif_targets_png_and_never_webp() {
        for (input, expected) in [
            (InputFormat::Png, Some(OutputFormat::Png)),
            (InputFormat::Bmp, Some(OutputFormat::Png)),
            (InputFormat::Jpeg, Some(OutputFormat::Png)),
            (InputFormat::Webp, Some(OutputFormat::Png)),
            (InputFormat::Gif, None),
        ] {
            assert_eq!(ingest_target(input), expected, "{input:?}");
        }
    }

    #[test]
    fn recognised_bytes_outrank_a_wrong_label() {
        // JPEG bytes labelled as PNG are decoded as the JPEG they are.
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];
        let (_, called) = route_recording("image/png", jpeg, Ok(fake_png()));
        assert_eq!(called.map(|job| job.input), Some(InputFormat::Jpeg));
    }

    // --- fallback: every failure returns the original, byte for byte ---

    #[test]
    fn every_converter_failure_falls_back_to_the_original() {
        let original = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 9, 9, 9];
        let replies: Vec<Result<Vec<u8>, PipelineError>> = vec![
            Err(PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound))),
            Err(failed()),
            Err(PipelineError::Source(io::Error::other("x"))),
            Err(PipelineError::Sink(io::Error::other("x"))),
            Err(PipelineError::Child(io::Error::other("x"))),
            // "Success" that produced nothing, or the wrong kind of file:
            Ok(Vec::new()),
            Ok(b"not an image at all".to_vec()),
            Ok(fake_webp()), // WebP where PNG was promised
        ];
        for reply in replies {
            let (routed, called) = route_recording("image/png", original.clone(), reply);
            assert!(called.is_some());
            assert_eq!(routed, ("image/png".to_owned(), original.clone()));
        }
    }

    // --- end to end with the real magick ---

    /// Decoded-pixel hash (`%#`) of an image, independent of its container.
    fn pixel_signature(image: Vec<u8>) -> String {
        let mut out = Vec::new();
        magick::run(&["-", "-format", "%#", "info:"], std::io::Cursor::new(image), &mut out).unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    #[test]
    fn real_png_is_rewritten_as_a_pixel_identical_stripped_png() {
        if !magick_or_skip() { return; }
        let original = generate(&[
            "-size", "32x32", "plasma:fractal", "-depth", "8",
            "-set", "comment", "SECRETCOMMENT", "png:-",
        ]);
        assert!(contains(&original, b"SECRETCOMMENT"), "fixture must carry the metadata");
        let (mime, data) = route_for_ingest("image/png".to_owned(), original.clone());
        assert_eq!(mime, "image/png");
        assert_eq!(detect_image_mime(&data), Some("image/png"));
        assert!(!contains(&data, b"SECRETCOMMENT"));
        assert_ne!(data, original);
        assert_eq!(pixel_signature(data), pixel_signature(original), "lossless means every pixel survives");
    }

    #[test]
    fn real_bmp_and_webp_are_rewritten_as_pixel_identical_png() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        let expected = pixel_signature(png.clone());
        let fixtures: [(&str, &[&str]); 2] = [
            ("image/bmp", &["-", "bmp:-"]),
            ("image/webp", &["-", "-define", "webp:lossless=true", "webp:-"]),
        ];
        for (mime, args) in fixtures {
            let label = mime;
            let mut source = Vec::new();
            magick::run(args, std::io::Cursor::new(png.clone()), &mut source).unwrap();
            assert_ne!(detect_image_mime(&source), Some("image/png"), "fixture must not already be a PNG ({label})");

            let (routed_mime, data) = route_for_ingest(mime.to_owned(), source);
            assert_eq!(routed_mime, "image/png", "{label}");
            assert_eq!(detect_image_mime(&data), Some("image/png"), "{label}");
            assert_eq!(pixel_signature(data), expected, "{label} altered pixel data");
        }
    }

    #[test]
    fn real_jpeg_loses_its_exif_and_comment_on_the_way_in() {
        if !magick_or_skip() { return; }
        let base = generate(&["-size", "32x32", "plasma:fractal", "jpg:-"]);
        // Splice a COM segment carrying a marker right after SOI.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xFE];
        jpeg.extend_from_slice(&((b"SECRETCOMMENT".len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(b"SECRETCOMMENT");
        jpeg.extend_from_slice(&base[2..]);
        assert!(contains(&jpeg, b"SECRETCOMMENT"));

        let (mime, data) = route_for_ingest("image/jpeg".to_owned(), jpeg);
        assert_eq!(mime, "image/png");
        assert_eq!(detect_image_mime(&data), Some("image/png"));
        assert!(!contains(&data, b"SECRETCOMMENT"));
    }

    #[test]
    fn real_corrupt_images_fall_back_to_the_original_bytes_and_mime() {
        if !magick_or_skip() { return; }
        let good = png();
        let truncated = good[..good.len() / 2].to_vec();
        // Valid signature, garbage body.
        let mut bogus = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bogus.extend_from_slice(&[0xA5; 4096]);

        for original in [truncated, bogus] {
            let (mime, data) = route_for_ingest("image/png".to_owned(), original.clone());
            assert_eq!(mime, "image/png");
            assert_eq!(data, original);
        }
    }

    #[test]
    fn real_text_and_svg_pass_through_identically() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"5\" height=\"5\"/>".to_vec();
        for (mime, payload) in [("text/plain", b"plain".to_vec()), ("image/svg+xml", svg)] {
            let routed = route_for_ingest(mime.to_owned(), payload.clone());
            assert_eq!(routed, (mime.to_owned(), payload));
        }
    }

    // --- circuit breaker integration ---

    fn trip(breaker: &CircuitBreaker) {
        for _ in 0..3 {
            let (routed, called) = route_on(breaker, "image/png", png_header(), Err(failed()));
            assert!(called.is_some());
            assert_eq!(routed.1, png_header());
        }
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    #[test]
    fn three_consecutive_failures_open_the_circuit_and_later_images_skip_the_converter() {
        let breaker = fresh_breaker();
        trip(&breaker);

        // Open: the converter is not even invoked, and the original is kept.
        let (routed, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_png()));
        assert!(called.is_none(), "no conversion (hence no child process) while open");
        assert_eq!(routed, ("image/png".to_owned(), png_header()));
    }

    #[test]
    fn every_kind_of_converter_fault_counts_toward_the_threshold() {
        let faults: Vec<Result<Vec<u8>, PipelineError>> = vec![
            Err(failed()),
            Err(PipelineError::Failed { code: None, stderr: "killed".into() }),
            Err(PipelineError::Timeout(Duration::from_secs(15))),
            Err(PipelineError::Child(io::Error::from(io::ErrorKind::BrokenPipe))),
            Ok(Vec::new()),
            Ok(b"not an image".to_vec()),
        ];
        for fault in faults {
            let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
            let (routed, _) = route_on(&breaker, "image/png", png_header(), fault);
            assert_eq!(routed, ("image/png".to_owned(), png_header()), "zero data loss");
            assert_eq!(breaker.state(), BreakerState::Open);
        }
    }

    #[test]
    fn bypassed_payloads_neither_advance_nor_reset_the_failure_count() {
        let breaker = fresh_breaker();
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        for _ in 0..20 {
            route_on(&breaker, "text/plain", b"hello".to_vec(), Err(failed()));
            route_on(&breaker, "image/svg+xml", b"<svg/>".to_vec(), Err(failed()));
            route_on(&breaker, "text/uri-list", b"/a".to_vec(), Err(failed()));
            route_on(&breaker, "image/gif", b"GIF89a....".to_vec(), Err(failed()));
        }
        assert_eq!(breaker.state(), BreakerState::Closed, "bypasses must not count as failures");
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert_eq!(breaker.state(), BreakerState::Open, "bypasses must not have reset the count either");
    }

    #[test]
    fn a_missing_tool_and_caller_stream_errors_do_not_touch_the_counters() {
        let breaker = fresh_breaker();
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        for _ in 0..20 {
            let missing = PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound));
            route_on(&breaker, "image/png", png_header(), Err(missing));
            route_on(&breaker, "image/png", png_header(), Err(PipelineError::Source(io::Error::other("x"))));
            route_on(&breaker, "image/png", png_header(), Err(PipelineError::Sink(io::Error::other("x"))));
        }
        assert_eq!(breaker.state(), BreakerState::Closed);
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert_eq!(breaker.state(), BreakerState::Open, "third real fault still trips: nothing was reset");
    }

    #[test]
    fn failing_to_isolate_is_neutral_and_keeps_the_original() {
        let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
        for _ in 0..20 {
            let refusal = io::Error::new(io::ErrorKind::PermissionDenied, "no private directory");
            let (routed, called) =
                route_on(&breaker, "image/png", png_header(), Err(PipelineError::Isolation(refusal)));
            assert!(called.is_some());
            assert_eq!(routed, ("image/png".to_owned(), png_header()), "zero data loss");
        }
        assert_eq!(breaker.state(), BreakerState::Closed, "a host problem must not open the circuit");
    }

    #[test]
    fn a_missing_tool_alone_can_never_open_the_circuit() {
        let breaker = fresh_breaker();
        for _ in 0..50 {
            let missing = PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound));
            let (routed, _) = route_on(&breaker, "image/png", png_header(), Err(missing));
            assert_eq!(routed, ("image/png".to_owned(), png_header()));
        }
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn a_successful_conversion_resets_the_consecutive_count() {
        let breaker = fresh_breaker();
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        let (routed, _) = route_on(&breaker, "image/png", png_header(), Ok(fake_png()));
        assert_eq!(routed.1, fake_png());
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn after_the_cooldown_a_successful_canary_conversion_closes_the_circuit() {
        let breaker = CircuitBreaker::new(3, Duration::from_millis(40));
        trip(&breaker);
        std::thread::sleep(Duration::from_millis(60));

        let (routed, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_png()));
        assert!(called.is_some(), "the canary must actually be attempted");
        assert_eq!(routed, ("image/png".to_owned(), fake_png()));
        assert_eq!(breaker.state(), BreakerState::Closed);

        let (routed, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_png()));
        assert!(called.is_some());
        assert_eq!(routed.1, fake_png(), "normal service resumed");
    }

    #[test]
    fn a_failed_canary_keeps_the_data_and_reopens_the_circuit() {
        let breaker = CircuitBreaker::new(3, Duration::from_millis(40));
        trip(&breaker);
        std::thread::sleep(Duration::from_millis(60));

        let (routed, called) = route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert!(called.is_some());
        assert_eq!(routed, ("image/png".to_owned(), png_header()));
        assert_eq!(breaker.state(), BreakerState::Open);

        let (_, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_png()));
        assert!(called.is_none(), "back in cooldown");
    }

    #[test]
    fn only_one_image_is_converted_while_the_canary_is_in_flight() {
        let breaker = CircuitBreaker::new(3, Duration::ZERO);
        trip(&breaker);

        let nested_called = Cell::new(false);
        let outer = route_with("image/png".to_owned(), png_header(), &breaker, &roomy_throttle(), |_, _, out| {
            // While this canary is running, a second image arrives.
            let (inner, called) = route_on(&breaker, "image/png", png_header(), Ok(fake_png()));
            nested_called.set(called.is_some());
            assert_eq!(inner, ("image/png".to_owned(), png_header()), "bypassed with the original intact");
            *out = fake_png();
            Ok(Transfer { bytes_in: 0, bytes_out: out.len() as u64 })
        });
        assert!(!nested_called.get(), "the second image must not start a second probe");
        assert_eq!(outer.1, fake_png());
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    // --- real magick behind the breaker ---

    #[test]
    fn real_corrupt_images_trip_the_breaker_without_losing_a_single_byte() {
        if !magick_or_skip() { return; }
        let breaker = fresh_breaker();
        let mut corrupt = png_header();
        corrupt.extend_from_slice(&[0xA5; 2048]);
        for _ in 0..5 {
            let routed = route_with("image/png".to_owned(), corrupt.clone(), &breaker, &roomy_throttle(), |job, input, out| {
                crate::transcode::transcode(job, input, out)
            });
            assert_eq!(routed, ("image/png".to_owned(), corrupt.clone()));
        }
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    // --- process throttle ---

    /// A converter reply that would convert successfully, used to prove that
    /// a refused request really never reaches the converter.
    fn would_succeed() -> Result<Vec<u8>, PipelineError> {
        Ok(fake_png())
    }

    #[test]
    fn a_saturated_throttle_falls_back_to_the_original_without_invoking_the_converter() {
        let breaker = fresh_breaker();
        let throttle = ProcessThrottle::new(1, Duration::from_millis(20));
        let _held = throttle.acquire().unwrap();

        let (routed, called) = route_through(&breaker, &throttle, "image/png", png_header(), would_succeed());
        assert!(called.is_none(), "no process may be spawned past the cap");
        assert_eq!(routed, ("image/png".to_owned(), png_header()), "zero data loss, original MIME kept");
        assert_eq!(throttle.in_use(), 1, "the refused request must not disturb the slot count");
    }

    #[test]
    fn saturation_waits_for_the_budget_before_falling_back() {
        let budget = Duration::from_millis(100);
        let throttle = ProcessThrottle::new(1, budget);
        let _held = throttle.acquire().unwrap();

        let started = Instant::now();
        let (routed, _) = route_through(&fresh_breaker(), &throttle, "image/png", png_header(), would_succeed());
        assert!(started.elapsed() >= budget, "fell back after {:?}, before the budget", started.elapsed());
        assert_eq!(routed.1, png_header());
    }

    #[test]
    fn saturation_never_counts_towards_the_breaker() {
        let throttle = ProcessThrottle::new(1, Duration::ZERO);
        let _held = throttle.acquire().unwrap();

        // A threshold of 1 is the harshest case: a single counted failure opens it.
        let strict = CircuitBreaker::new(1, Duration::from_secs(30));
        for _ in 0..50 {
            route_through(&strict, &throttle, "image/png", png_header(), would_succeed());
        }
        assert_eq!(strict.state(), BreakerState::Closed);
    }

    #[test]
    fn saturation_neither_advances_nor_resets_the_failure_count() {
        let breaker = fresh_breaker();
        route_on(&breaker, "image/png", png_header(), Err(failed()));
        route_on(&breaker, "image/png", png_header(), Err(failed()));

        let throttle = ProcessThrottle::new(1, Duration::ZERO);
        let _held = throttle.acquire().unwrap();
        for _ in 0..20 {
            route_through(&breaker, &throttle, "image/png", png_header(), would_succeed());
        }
        assert_eq!(breaker.state(), BreakerState::Closed, "saturation must not count as a failure");

        route_on(&breaker, "image/png", png_header(), Err(failed()));
        assert_eq!(breaker.state(), BreakerState::Open, "nor may it have reset the two earlier failures");
    }

    #[test]
    fn a_saturated_canary_hands_the_probe_back_instead_of_wedging_the_breaker() {
        let breaker = CircuitBreaker::new(3, Duration::ZERO);
        trip(&breaker);

        let throttle = ProcessThrottle::new(1, Duration::ZERO);
        let held = throttle.acquire().unwrap();
        let (routed, called) = route_through(&breaker, &throttle, "image/png", png_header(), would_succeed());
        assert!(called.is_none());
        assert_eq!(routed, ("image/png".to_owned(), png_header()));
        assert_eq!(breaker.state(), BreakerState::HalfOpen, "no verdict yet: neither closed nor re-opened");

        drop(held);
        let (routed, called) = route_through(&breaker, &throttle, "image/png", png_header(), would_succeed());
        assert!(called.is_some(), "the next image must be allowed to probe");
        assert_eq!(routed.1, fake_png());
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn an_open_circuit_never_waits_for_a_slot() {
        let breaker = fresh_breaker();
        trip(&breaker);
        let throttle = ProcessThrottle::new(1, Duration::from_secs(30));
        let _held = throttle.acquire().unwrap();

        let started = Instant::now();
        let (routed, called) = route_through(&breaker, &throttle, "image/png", png_header(), would_succeed());
        assert!(started.elapsed() < Duration::from_secs(10), "waited for a slot behind an open circuit");
        assert!(called.is_none());
        assert_eq!(routed, ("image/png".to_owned(), png_header()));
    }

    #[test]
    fn bypassed_payloads_never_take_or_wait_for_a_slot() {
        let throttle = ProcessThrottle::new(1, Duration::from_secs(30));
        let _held = throttle.acquire().unwrap();
        let started = Instant::now();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("text/plain", b"hello".to_vec()),
            ("image/svg+xml", b"<svg/>".to_vec()),
            ("image/gif", b"GIF89a....".to_vec()),
            ("image/png", Vec::new()),
        ];
        for (mime, payload) in cases {
            let (routed, called) =
                route_through(&fresh_breaker(), &throttle, mime, payload.clone(), would_succeed());
            assert!(called.is_none());
            assert_eq!(routed, (mime.to_owned(), payload));
        }
        assert!(started.elapsed() < Duration::from_secs(10), "a bypass waited on the throttle");
    }

    #[test]
    fn the_slot_is_released_after_every_kind_of_outcome() {
        let throttle = ProcessThrottle::new(1, Duration::ZERO);
        let replies: Vec<Result<Vec<u8>, PipelineError>> = vec![
            would_succeed(),
            Err(failed()),
            Err(PipelineError::Timeout(Duration::from_secs(15))),
            Err(PipelineError::Spawn(io::Error::from(io::ErrorKind::NotFound))),
            Ok(Vec::new()),
        ];
        for reply in replies {
            let breaker = CircuitBreaker::new(100, Duration::from_secs(30));
            route_through(&breaker, &throttle, "image/png", png_header(), reply);
            assert_eq!(throttle.in_use(), 0, "slot leaked");
        }
    }

    #[test]
    fn a_panicking_converter_still_releases_its_slot() {
        let breaker = fresh_breaker();
        let throttle = ProcessThrottle::new(1, Duration::ZERO);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            route_with("image/png".to_owned(), png_header(), &breaker, &throttle, |_, _, _| {
                panic!("converter blew up")
            })
        }));
        assert!(result.is_err());
        assert_eq!(throttle.in_use(), 0, "unwinding must free the slot");
        assert!(throttle.acquire().is_some());
    }

    #[test]
    fn a_burst_falls_back_deterministically_while_the_only_slot_is_busy() {
        let breaker = CircuitBreaker::new(1, Duration::from_secs(30));
        let throttle = ProcessThrottle::new(1, Duration::from_millis(20));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        std::thread::scope(|scope| {
            let (breaker, throttle) = (&breaker, &throttle);
            let first = scope.spawn(move || {
                route_with("image/png".to_owned(), png_header(), breaker, throttle, |_, _, out| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    *out = fake_png();
                    Ok(Transfer { bytes_in: 0, bytes_out: out.len() as u64 })
                })
            });
            entered_rx.recv().unwrap();

            // The first conversion now owns the only slot. A burst behind it
            // is stored untouched, never converted and never lost.
            for _ in 0..5 {
                let (routed, called) = route_through(breaker, throttle, "image/png", png_header(), would_succeed());
                assert!(called.is_none());
                assert_eq!(routed, ("image/png".to_owned(), png_header()));
            }

            release_tx.send(()).unwrap();
            assert_eq!(first.join().unwrap().1, fake_png(), "the admitted conversion still completes");
        });
        assert_eq!(breaker.state(), BreakerState::Closed, "a threshold-1 breaker would have opened on one counted fault");
        assert_eq!(throttle.in_use(), 0);
    }

    #[test]
    fn a_waiting_image_is_converted_once_a_slot_frees_up_in_time() {
        let breaker = fresh_breaker();
        let throttle = ProcessThrottle::new(1, Duration::from_secs(30));
        let held = throttle.acquire().unwrap();

        std::thread::scope(|scope| {
            let (breaker, throttle) = (&breaker, &throttle);
            let waiter = scope.spawn(move || {
                route_through(breaker, throttle, "image/png", png_header(), would_succeed()).0
            });
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
            assert_eq!(waiter.join().unwrap().1, fake_png());
        });
        assert_eq!(throttle.in_use(), 0);
    }

    #[test]
    fn conversions_never_exceed_the_cap_and_nothing_is_lost_in_a_burst() {
        const LIMIT: usize = 2;
        const IMAGES: usize = 12;
        let breaker = fresh_breaker();
        let throttle = ProcessThrottle::new(LIMIT, Duration::from_secs(30));
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..IMAGES {
                scope.spawn(|| {
                    let routed = route_with("image/png".to_owned(), png_header(), &breaker, &throttle, |_, _, out| {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(10));
                        running.fetch_sub(1, Ordering::SeqCst);
                        *out = fake_png();
                        Ok(Transfer { bytes_in: 0, bytes_out: out.len() as u64 })
                    });
                    assert_eq!(routed, ("image/png".to_owned(), fake_png()));
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= LIMIT, "{} converters ran at once", peak.load(Ordering::SeqCst));
        assert_eq!(throttle.in_use(), 0);
    }

    #[test]
    fn a_poisoned_throttle_still_admits_and_still_falls_back() {
        let breaker = fresh_breaker();
        let throttle = ProcessThrottle::new(1, Duration::ZERO);
        crate::throttle::poison_for_test(&throttle);

        let (routed, called) = route_through(&breaker, &throttle, "image/png", png_header(), would_succeed());
        assert!(called.is_some(), "a poisoned lock must not disable conversion");
        assert_eq!(routed.1, fake_png());

        let _held = throttle.acquire().unwrap();
        let (routed, called) = route_through(&breaker, &throttle, "image/png", png_header(), would_succeed());
        assert!(called.is_none(), "the cap still holds on a poisoned lock");
        assert_eq!(routed, ("image/png".to_owned(), png_header()));
    }

    #[test]
    fn real_magick_burst_stays_within_the_cap() {
        if !magick_or_skip() { return; }
        const LIMIT: usize = 2;
        let original = png();
        let breaker = fresh_breaker();
        let throttle = ProcessThrottle::new(LIMIT, Duration::from_secs(60));
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let routed = route_with("image/png".to_owned(), original.clone(), &breaker, &throttle, |job, input, out| {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        let result = transcode::transcode(job, input, out);
                        running.fetch_sub(1, Ordering::SeqCst);
                        result
                    });
                    assert_ne!(routed.1, original, "a real conversion rewrites the bytes");
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= LIMIT);
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert_eq!(throttle.in_use(), 0);
    }

    // --- output buffer capacity hint ---

    #[test]
    fn the_capacity_hint_follows_the_input_size_up_to_the_cap() {
        assert_eq!(output_capacity_hint(0), 0);
        assert_eq!(output_capacity_hint(1), 1);
        assert_eq!(output_capacity_hint(300_000), 300_000);
        assert_eq!(output_capacity_hint(IMAGE_OUTPUT_HINT_CAP_BYTES), IMAGE_OUTPUT_HINT_CAP_BYTES);
        assert_eq!(output_capacity_hint(IMAGE_OUTPUT_HINT_CAP_BYTES + 1), IMAGE_OUTPUT_HINT_CAP_BYTES);
        assert_eq!(output_capacity_hint(usize::MAX), IMAGE_OUTPUT_HINT_CAP_BYTES);
    }

    /// Runs a conversion of `payload` whose converter reports the capacity it
    /// was handed, then fills the buffer up to that capacity and reports
    /// whether it had to move (i.e. reallocate).
    fn capacity_seen_by_converter(payload: Vec<u8>) -> (usize, bool) {
        let seen = Cell::new((0, true));
        route_with("image/png".to_owned(), payload, &fresh_breaker(), &roomy_throttle(), |_, _, out| {
            let capacity = out.capacity();
            let before = out.as_ptr();
            out.resize(capacity, 0xAB);
            seen.set((capacity, out.as_ptr() != before));
            out.clear();
            Err(failed())
        });
        seen.get()
    }

    #[test]
    fn the_converter_receives_a_buffer_sized_from_the_input_and_filling_it_never_reallocates() {
        let mut payload = png_header();
        payload.resize(200_000, 7);
        let (capacity, moved) = capacity_seen_by_converter(payload.clone());
        assert!(capacity >= payload.len(), "reserved {capacity} for a {}-byte input", payload.len());
        assert!(!moved, "streaming up to the hint must not reallocate");
    }

    #[test]
    fn a_huge_input_reserves_no_more_than_the_cap() {
        let mut payload = png_header();
        payload.resize(IMAGE_OUTPUT_HINT_CAP_BYTES + 1_000_000, 7);
        let (capacity, _) = capacity_seen_by_converter(payload);
        assert!(capacity >= IMAGE_OUTPUT_HINT_CAP_BYTES);
        assert!(capacity < IMAGE_OUTPUT_HINT_CAP_BYTES + 1_000_000, "reserved {capacity}: the cap was ignored");
    }
}
