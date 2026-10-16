// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/transcode.rs

//! Privacy-stripping, resource-bounded image transcoding on top of the
//! streaming `magick` runner.
//!
//! Every argument handed to `magick` is derived from the closed enums below
//! and from compile-time constants, never from caller-supplied text, so
//! there is no way to smuggle an option, a `coder:` prefix or a filename
//! through clipboard-controlled data.

use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::time::Duration;

use crate::core::constants::{
    IMAGE_LIMIT_DISK, IMAGE_LIMIT_HEIGHT_PX, IMAGE_LIMIT_MAP, IMAGE_LIMIT_MEMORY,
    IMAGE_LIMIT_TIME_SECS, IMAGE_LIMIT_WIDTH_PX, IMAGE_QUALITY_DEFAULT, IMAGE_QUALITY_MAX,
    IMAGE_QUALITY_MIN, IMAGE_TRANSCODE_TIMEOUT_SECS,
};
use crate::core::splice::ByteTap;
use crate::core::utils::{detect_image_mime, mime_base};
use crate::magick;
use crate::pipeline::{FileSink, PipelineError, Transfer};

/// Raster formats accepted as *input*. The decoder is pinned explicitly
/// (`png:-`) instead of letting ImageMagick sniff the stream: auto-detection
/// can route attacker-chosen bytes into any of its ~100 coders, including
/// scriptable ones (SVG, MVG, MSL, ...) with a long history of
/// vulnerabilities. A pinned coder makes a mislabelled or disguised payload
/// fail to decode rather than reach a different parser. Vector and
/// scriptable formats are deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
}

impl InputFormat {
    /// The pinned-decoder input argument: `coder:-` (stdin) with `[0]`
    /// selecting the first frame, so an animated GIF/WebP cannot expand into
    /// a frame sequence. A static string per format, hence no allocation.
    fn pinned_input(self) -> &'static str {
        match self {
            Self::Png => "png:-[0]",
            Self::Jpeg => "jpeg:-[0]",
            Self::Gif => "gif:-[0]",
            Self::Webp => "webp:-[0]",
            Self::Bmp => "bmp:-[0]",
        }
    }

    /// Maps a clipboard MIME type (parameters and case ignored) to a
    /// supported input format; `None` for anything else, SVG included.
    pub fn from_mime(mime: &str) -> Option<Self> {
        let base = mime_base(mime);
        [
            ("image/png", Self::Png),
            ("image/jpeg", Self::Jpeg),
            ("image/jpg", Self::Jpeg),
            ("image/gif", Self::Gif),
            ("image/webp", Self::Webp),
            ("image/bmp", Self::Bmp),
            ("image/x-ms-bmp", Self::Bmp),
        ]
        .into_iter()
        .find(|(name, _)| base.eq_ignore_ascii_case(name))
        .map(|(_, format)| format)
    }

    /// Identifies the format from a payload's leading bytes (the same
    /// magic-byte check ingestion already trusts over sender labels).
    /// Unsupported formats, BMP (no reliable short signature) and
    /// unrecognised data yield `None`.
    pub fn sniff(header: &[u8]) -> Option<Self> {
        detect_image_mime(header).and_then(Self::from_mime)
    }
}

/// Encoder quality, always within `IMAGE_QUALITY_MIN..=IMAGE_QUALITY_MAX`.
/// Construction clamps instead of failing: a caller passing 0 or 255 gets
/// the nearest valid value, so no input can make `magick` reject the option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality(u8);

impl Quality {
    pub fn new(raw: u8) -> Self {
        Self(raw.clamp(IMAGE_QUALITY_MIN, IMAGE_QUALITY_MAX))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self::new(IMAGE_QUALITY_DEFAULT)
    }
}

/// Target encodings. Quality exists only where it means something: the two
/// lossless variants carry none, so "lossless at quality 40" is not a state
/// the type can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Lossless PNG at the fastest zlib effort (`PNG_COMPRESSION_ARG`): the
    /// ingestion target. Pixels are bit-exact at every level; only the file
    /// size and encode time differ, and a clipboard copy is latency-bound.
    /// PNG is also the one raster encoding every paste target accepts. The
    /// output is a pure function of the input (no `tIME` stamp, see
    /// `PNG_EXCLUDE_TIME_ARG`), which deduplication by digest relies on.
    Png,
    /// Lossless WebP: bit-exact pixels, typically smaller than PNG on
    /// screenshots. Always encoded at ImageMagick quality 100, because in
    /// lossless mode ImageMagick maps any lower quality onto libwebp's
    /// near-lossless pre-processing, which *does* alter pixels (measured:
    /// 59 of 16384 pixels differ at quality 75, 118 at 50). Only 100 is exact.
    WebpLossless,
    /// Lossy WebP: the "small and good enough" choice, tuned by `Quality`.
    WebpLossy(Quality),
}

impl OutputFormat {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::WebpLossless | Self::WebpLossy(_) => "image/webp",
        }
    }
}

/// One transcoding job: which decoder to pin and which encoding to produce.
/// Metadata stripping is not a switch — every transcode removes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transcode {
    pub input: InputFormat,
    pub output: OutputFormat,
}

/// Parses a plain decimal number of seconds in a `const` context; anything
/// else yields `u64::MAX`, which makes the assertion below fail loudly.
const fn parse_secs(text: &str) -> u64 {
    let mut rest = text.as_bytes();
    if rest.is_empty() {
        return u64::MAX;
    }
    let mut secs: u64 = 0;
    while let [digit, tail @ ..] = rest {
        if !digit.is_ascii_digit() {
            return u64::MAX;
        }
        secs = secs.saturating_mul(10).saturating_add((*digit - b'0') as u64);
        rest = tail;
    }
    secs
}

// Defence in depth: ImageMagick's own time limit must fire before the outer
// watchdog, so a runaway conversion ends with the converter's diagnostic and
// only a child that ignores its limit meets the process-group SIGKILL.
const _: () = assert!(
    parse_secs(IMAGE_LIMIT_TIME_SECS) >= 1 && parse_secs(IMAGE_LIMIT_TIME_SECS) < IMAGE_TRANSCODE_TIMEOUT_SECS,
    "IMAGE_LIMIT_TIME_SECS must be a whole number of seconds below IMAGE_TRANSCODE_TIMEOUT_SECS",
);

/// Resource ceilings passed to every run as `-limit <name> <value>`.
const LIMITS: [(&str, &str); 6] = [
    ("width", IMAGE_LIMIT_WIDTH_PX),
    ("height", IMAGE_LIMIT_HEIGHT_PX),
    ("memory", IMAGE_LIMIT_MEMORY),
    ("map", IMAGE_LIMIT_MAP),
    ("disk", IMAGE_LIMIT_DISK),
    ("time", IMAGE_LIMIT_TIME_SECS),
];

/// Longest possible argument vector (a WebP encode): 3 tokens per limit,
/// the input, `-auto-orient`, `-strip`, then `-define k=v`, `-quality q`
/// and the output. A capacity hint only: exceeding it would merely
/// reallocate, and a test pins it to the real maximum.
const ARGS_CAPACITY: usize = LIMITS.len() * 3 + 3 + 5;

/// zlib effort of the PNG encoder. Level 1 keeps pixels bit-exact like any
/// other level but encodes roughly 2-4x faster than level 9 (measured with
/// ImageMagick 7.1: 1080p photo-like 137 vs 257 ms, flat screenshot 65 vs 81 ms),
/// at the price of a larger file on flat content.
const PNG_COMPRESSION_ARG: &str = "png:compression-level=1";

/// Keeps ImageMagick from writing a `tIME` chunk. Left to itself the PNG
/// encoder stamps the output with the *current* time (to the second), even after
/// `-strip`, so the same image converted a second apart is a different file
/// with a different SHA3-256, and the database key, which deduplicates by that
/// digest, stops recognising it. The chunk holds the time of the conversion,
/// not of anything the user copied: dropping it loses no pixel (measured: the
/// pixel hash of the output is identical) and no information worth keeping, and
/// makes the conversion a pure function of its input.
const PNG_EXCLUDE_TIME_ARG: &str = "png:exclude-chunk=tIME";

/// `IMAGE_QUALITY_MAX` as an argument, spelled out so the lossless path
/// borrows it instead of formatting it per run (a test ties the two together).
const LOSSLESS_QUALITY_ARG: &str = "100";

impl Transcode {
    /// The complete `magick` argument vector, in the one order that is safe:
    ///
    /// 1. `-limit ...` first, so the ceilings are active before any byte is
    ///    decoded (a bomb is rejected at the header, not after allocation).
    /// 2. The pinned-decoder input `fmt:-[0]`. `[0]` selects the first frame,
    ///    so an animated GIF/WebP cannot expand into a frame sequence.
    /// 3. `-auto-orient` *before* `-strip`: orientation lives in EXIF, and
    ///    stripping it first would silently turn a rotated photo sideways.
    ///    The rotation is baked into the pixels, then the tag is dropped.
    /// 4. `-strip` removes every profile and comment (EXIF incl. GPS and
    ///    camera/timestamp tags, XMP, IPTC, ICC, text chunks).
    /// 5. Encoder settings, then the output `fmt:-`.
    ///
    /// Every token is a borrowed `'static` string except the quality of a
    /// lossy encode, the one value computed at run time: the vector is the
    /// only allocation besides it (sized up front, never regrown).
    fn args(&self) -> Vec<Cow<'static, str>> {
        let mut args: Vec<Cow<'static, str>> = Vec::with_capacity(ARGS_CAPACITY);
        for (name, value) in LIMITS {
            args.extend([Cow::Borrowed("-limit"), Cow::Borrowed(name), Cow::Borrowed(value)]);
        }
        args.push(Cow::Borrowed(self.input.pinned_input()));
        args.push(Cow::Borrowed("-auto-orient"));
        args.push(Cow::Borrowed("-strip"));
        match self.output {
            OutputFormat::Png => {
                args.extend([Cow::Borrowed("-define"), Cow::Borrowed(PNG_COMPRESSION_ARG)]);
                args.extend([Cow::Borrowed("-define"), Cow::Borrowed(PNG_EXCLUDE_TIME_ARG)]);
                args.push(Cow::Borrowed("png:-"));
            }
            OutputFormat::WebpLossless => {
                args.extend([Cow::Borrowed("-define"), Cow::Borrowed("webp:lossless=true")]);
                args.extend([Cow::Borrowed("-quality"), Cow::Borrowed(LOSSLESS_QUALITY_ARG)]);
                args.push(Cow::Borrowed("webp:-"));
            }
            OutputFormat::WebpLossy(quality) => {
                args.extend([Cow::Borrowed("-define"), Cow::Borrowed("webp:lossless=false")]);
                args.extend([Cow::Borrowed("-quality"), Cow::Owned(quality.get().to_string())]);
                args.push(Cow::Borrowed("webp:-"));
            }
        }
        args
    }
}

/// Streams `input` through `magick` according to `job` into `output`,
/// stripping all metadata. Nothing is buffered whole: see
/// [`crate::pipeline::run_filter`] for the chunked, deadlock-free
/// relay, and for the rule that `output` must be discarded on `Err`.
///
/// A payload that does not decode as `job.input` (corrupt, truncated,
/// mislabelled, or over the resource limits) ends as
/// [`PipelineError::Failed`]; a `magick` that hangs is killed after
/// `IMAGE_TRANSCODE_TIMEOUT_SECS` of wall-clock time and ends as
/// [`PipelineError::Timeout`]; a missing `magick` ends as
/// [`PipelineError::Spawn`] (`is_unavailable()`), the cue to keep the
/// original bytes untouched instead.
pub fn transcode<R, W>(job: Transcode, input: R, output: &mut W) -> Result<Transfer, PipelineError>
where
    R: Read + Send,
    W: Write,
{
    let args = job.args();
    let refs: Vec<&str> = args.iter().map(|arg| &**arg).collect();
    magick::run_with_timeout(&refs, input, output, Duration::from_secs(IMAGE_TRANSCODE_TIMEOUT_SECS))
}

/// [`transcode`] descriptor to descriptor: `magick` reads `input` directly as
/// its stdin and its output is spliced into `output` (at most `limit` bytes),
/// with `tap` seeing every output byte once, in order, as it passes (the
/// fingerprint of what is stored). No payload byte touches this process's heap;
/// see [`crate::pipeline::run_filter_file_in_env`] for the mechanics, the
/// fallbacks and the failure semantics.
///
/// Unlike [`transcode`], success is checked here: the first bytes of `output`
/// must carry the signature of `job.output`. A converter that exits 0 having
/// written nothing, or something else, ends as [`PipelineError::Failed`] (a
/// converter fault, like a crash), because the caller has no buffer to inspect
/// and would otherwise have to re-read the file to find out. For that check
/// `output` must have been opened for reading as well as writing; a file that
/// cannot be read back ends as [`PipelineError::Sink`].
///
/// On `Err`, `output` may hold partial bytes and must be discarded.
pub fn transcode_file<T: ByteTap>(
    job: Transcode,
    input: File,
    output: &mut File,
    limit: u64,
    tap: &mut T,
) -> Result<Transfer, PipelineError> {
    let args = job.args();
    let refs: Vec<&str> = args.iter().map(|arg| &**arg).collect();
    let sink = FileSink { file: &mut *output, limit, tap };
    let transfer = magick::run_file_with_timeout(&refs, input, sink, Duration::from_secs(IMAGE_TRANSCODE_TIMEOUT_SECS))?;
    check_signature(job.output, output)?;
    Ok(transfer)
}

/// Signature bytes inspected by [`check_signature`]; binary image signatures
/// fit in 12 (see `detect_image_mime`), the rest is slack.
const SIGNATURE_PROBE_BYTES: usize = 32;

/// Whether the first bytes of `file` identify an image of `target`'s type.
fn check_signature(target: OutputFormat, file: &File) -> Result<(), PipelineError> {
    let mut head = [0u8; SIGNATURE_PROBE_BYTES];
    let read = file.read_at(&mut head, 0).map_err(PipelineError::Sink)?;
    if read > 0 && detect_image_mime(&head[..read]) == Some(target.mime()) {
        return Ok(());
    }
    Err(PipelineError::Failed {
        code: Some(0),
        stderr: format!("converter exited successfully but its output is not a {} image", target.mime()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::{Cursor, empty};
    use std::sync::mpsc;
    use std::time::Duration;

    const MARKER_CAMERA: &[u8] = b"SECRETCAM";
    const MARKER_COMMENT: &[u8] = b"SECRETCOMMENT";
    const MARKER_AUTHOR: &[u8] = b"SECRETAUTHOR";

    fn magick_or_skip() -> bool {
        if magick::is_available() {
            true
        } else {
            eprintln!("skipping: `magick` is not available on this system");
            false
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// The argument vector as plain `String`s, for assertions that read better that way.
    fn owned_args(job: &Transcode) -> Vec<String> {
        job.args().into_iter().map(Cow::into_owned).collect()
    }

    fn lossy(quality: u8) -> OutputFormat {
        OutputFormat::WebpLossy(Quality::new(quality))
    }

    /// Runs `magick` directly (no stripping, no pinned decoder) to build
    /// fixtures and to take measurements; the code under test is `transcode`.
    fn raw(args: &[&str], input: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        magick::run(args, Cursor::new(input), &mut out).unwrap();
        out
    }

    fn generate(args: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        magick::run(args, empty(), &mut out).unwrap();
        out
    }

    fn dimensions(image: Vec<u8>) -> String {
        String::from_utf8(raw(&["-", "-format", "%wx%h", "info:"], image)).unwrap().trim().to_owned()
    }

    fn pixel_signature(image: Vec<u8>) -> String {
        String::from_utf8(raw(&["-", "-format", "%#", "info:"], image)).unwrap().trim().to_owned()
    }

    /// Completes within `secs` or fails the test instead of hanging the suite.
    fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(secs)).expect("transcode did not finish in time")
    }

    fn run_job(job: Transcode, input: Vec<u8>) -> (Result<Transfer, PipelineError>, Vec<u8>) {
        within(60, move || {
            let mut out = Vec::new();
            let result = transcode(job, Cursor::new(input), &mut out);
            (result, out)
        })
    }

    /// A PNG carrying plain-text metadata chunks (tEXt: comment + author).
    fn png_with_text_metadata() -> Vec<u8> {
        generate(&[
            "-size", "64x64", "plasma:fractal",
            "-set", "comment", "SECRETCOMMENT",
            "-set", "Author", "SECRETAUTHOR",
            "png:-",
        ])
    }

    /// Minimal little-endian EXIF (TIFF) blob: Make = "SECRETCAM" plus an
    /// optional Orientation tag.
    fn exif_blob(orientation: Option<u16>) -> Vec<u8> {
        let make = b"SECRETCAM\0";
        let entries: u16 = if orientation.is_some() { 2 } else { 1 };
        let data_off = 8 + 2 + 12 * u32::from(entries) + 4;
        let mut tiff = b"II*\0".to_vec();
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&entries.to_le_bytes());
        // IFD entries must be sorted by tag: Make (0x010F) < Orientation (0x0112).
        tiff.extend_from_slice(&0x010Fu16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&(make.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&data_off.to_le_bytes());
        if let Some(value) = orientation {
            tiff.extend_from_slice(&0x0112u16.to_le_bytes());
            tiff.extend_from_slice(&3u16.to_le_bytes());
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(make);
        tiff
    }

    /// A JPEG of `size` with an EXIF APP1 segment and a COM comment spliced
    /// in directly after SOI (pure byte surgery, no external helper).
    fn jpeg_with_metadata(size: &str, orientation: Option<u16>) -> Vec<u8> {
        let base = generate(&["-size", size, "plasma:fractal", "jpg:-"]);
        assert_eq!(&base[..2], &[0xFF, 0xD8]);
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&exif_blob(orientation));
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&exif);
        jpeg.extend_from_slice(&[0xFF, 0xFE]);
        jpeg.extend_from_slice(&((MARKER_COMMENT.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(MARKER_COMMENT);
        jpeg.extend_from_slice(&base[2..]);
        jpeg
    }

    // --- argument construction (needs no magick) ---

    // --- allocation behaviour of argument construction ---

    const ALL_INPUTS: [InputFormat; 5] =
        [InputFormat::Png, InputFormat::Jpeg, InputFormat::Gif, InputFormat::Webp, InputFormat::Bmp];

    fn all_jobs() -> Vec<Transcode> {
        let outputs = [OutputFormat::Png, OutputFormat::WebpLossless, lossy(1), lossy(90), lossy(100)];
        ALL_INPUTS.iter().flat_map(|&input| outputs.iter().map(move |&output| Transcode { input, output })).collect()
    }

    fn owned_count(args: &[Cow<'static, str>]) -> usize {
        args.iter().filter(|arg| matches!(arg, Cow::Owned(_))).count()
    }

    #[test]
    fn lossless_argument_vectors_are_built_entirely_from_borrowed_static_tokens() {
        for input in ALL_INPUTS {
            for output in [OutputFormat::Png, OutputFormat::WebpLossless] {
                let args = Transcode { input, output }.args();
                assert_eq!(owned_count(&args), 0, "{input:?} -> {output:?} allocated a token");
            }
        }
    }

    #[test]
    fn a_lossy_encode_allocates_exactly_one_token_the_quality_value() {
        for input in ALL_INPUTS {
            let args = Transcode { input, output: lossy(70) }.args();
            let owned: Vec<&str> = args.iter().filter(|arg| matches!(arg, Cow::Owned(_))).map(|arg| &**arg).collect();
            assert_eq!(owned, ["70"], "only the run-time quality may be allocated");
        }
    }

    #[test]
    fn the_capacity_hint_equals_the_longest_vector_so_construction_never_reallocates() {
        let longest = all_jobs().iter().map(|job| job.args().len()).max().unwrap();
        assert_eq!(longest, ARGS_CAPACITY, "the hint must stay tight as arguments are added or removed");
        for job in all_jobs() {
            let args = job.args();
            assert_eq!(args.capacity(), ARGS_CAPACITY, "{job:?} outgrew its up-front allocation");
        }
    }

    #[test]
    fn the_internal_time_limit_is_shorter_than_the_outer_watchdog() {
        let internal = parse_secs(IMAGE_LIMIT_TIME_SECS);
        assert!((1..IMAGE_TRANSCODE_TIMEOUT_SECS).contains(&internal), "{internal}s vs {IMAGE_TRANSCODE_TIMEOUT_SECS}s");
        // ...and it is the value that really reaches the command line.
        let args = owned_args(&Transcode { input: InputFormat::Png, output: OutputFormat::Png });
        let at = args.iter().position(|a| a == "time").unwrap();
        assert_eq!(args[at - 1], "-limit");
        assert_eq!(args[at + 1], internal.to_string());
    }

    #[test]
    fn the_seconds_parser_accepts_plain_numbers_and_rejects_everything_else() {
        assert_eq!(parse_secs("10"), 10);
        assert_eq!(parse_secs("0"), 0);
        assert_eq!(parse_secs("007"), 7);
        for bad in ["", "ten", "10s", "-1", "1.5", " 10", "1e3"] {
            assert_eq!(parse_secs(bad), u64::MAX, "{bad:?}");
        }
        assert_eq!(parse_secs("99999999999999999999999"), u64::MAX, "overflow saturates and fails the assertion");
    }

    #[test]
    fn the_png_compression_argument_names_a_valid_zlib_level() {
        let level = PNG_COMPRESSION_ARG.strip_prefix("png:compression-level=").and_then(|n| n.parse::<u8>().ok());
        assert!(matches!(level, Some(1..=9)), "{PNG_COMPRESSION_ARG}");
    }

    #[test]
    fn the_spelled_out_lossless_quality_matches_the_constant() {
        assert_eq!(LOSSLESS_QUALITY_ARG, IMAGE_QUALITY_MAX.to_string());
    }

    #[test]
    fn every_pinned_input_names_its_own_coder_and_selects_the_first_frame() {
        let expected = [
            (InputFormat::Png, "png:-[0]"),
            (InputFormat::Jpeg, "jpeg:-[0]"),
            (InputFormat::Gif, "gif:-[0]"),
            (InputFormat::Webp, "webp:-[0]"),
            (InputFormat::Bmp, "bmp:-[0]"),
        ];
        for (input, token) in expected {
            assert_eq!(input.pinned_input(), token);
            assert!(owned_args(&Transcode { input, output: OutputFormat::Png }).iter().any(|a| a == token));
        }
    }

    #[test]
    fn args_apply_limits_before_the_pinned_input_then_strip_then_encode() {
        let job = Transcode { input: InputFormat::Jpeg, output: lossy(70) };
        let args = owned_args(&job);
        let pos = |needle: &str| args.iter().position(|a| a == needle).unwrap();

        for name in ["width", "height", "memory", "map", "disk", "time"] {
            let at = args.iter().position(|a| a == name).unwrap();
            assert_eq!(args[at - 1], "-limit");
            assert!(at < pos("jpeg:-[0]"), "{name} limit must precede the input");
        }
        assert!(pos("jpeg:-[0]") < pos("-auto-orient"));
        assert!(pos("-auto-orient") < pos("-strip"), "orientation must be applied before EXIF is stripped");
        assert!(pos("-strip") < pos("webp:lossless=false"));
        assert_eq!(args.last().map(String::as_str), Some("webp:-"));
        assert_eq!(args[pos("-quality") + 1], "70");
    }

    #[test]
    fn args_for_png_output_pin_no_webp_settings() {
        let args = owned_args(&Transcode { input: InputFormat::Png, output: OutputFormat::Png });
        assert_eq!(args.last().map(String::as_str), Some("png:-"));
        assert!(args.iter().all(|a| !a.contains("webp")));
        assert!(args.iter().any(|a| a == "-strip"));
    }

    #[test]
    fn png_output_selects_the_fast_zlib_level_and_no_quality_flag() {
        for input in ALL_INPUTS {
            let args = owned_args(&Transcode { input, output: OutputFormat::Png });
            assert!(args.windows(2).any(|w| w == ["-define", "png:compression-level=1"]), "{input:?}");
            assert!(args.iter().all(|a| a != "-quality"), "-quality would override the zlib level for PNG");
            assert!(args.iter().all(|a| a != "png:compression-level=9"));
        }
    }

    #[test]
    fn lossless_webp_args_ignore_quality_and_pin_the_exact_setting() {
        let args = owned_args(&Transcode { input: InputFormat::Png, output: OutputFormat::WebpLossless });
        assert!(args.windows(2).any(|w| w == ["-define", "webp:lossless=true"]));
        assert!(args.windows(2).any(|w| w == ["-quality", "100"]), "only quality 100 is bit-exact");
        assert_eq!(args.last().map(String::as_str), Some("webp:-"));
    }

    #[test]
    fn quality_is_clamped_into_range_at_both_ends() {
        assert_eq!(Quality::new(0).get(), IMAGE_QUALITY_MIN);
        assert_eq!(Quality::new(1).get(), 1);
        assert_eq!(Quality::new(100).get(), 100);
        assert_eq!(Quality::new(101).get(), IMAGE_QUALITY_MAX);
        assert_eq!(Quality::new(255).get(), IMAGE_QUALITY_MAX);
        assert_eq!(Quality::default().get(), IMAGE_QUALITY_DEFAULT);
    }

    #[test]
    fn extreme_quality_values_reach_magick_already_clamped() {
        let low = owned_args(&Transcode { input: InputFormat::Png, output: lossy(0) });
        let high = owned_args(&Transcode { input: InputFormat::Png, output: lossy(255) });
        assert!(low.windows(2).any(|w| w == ["-quality", "1"]));
        assert!(high.windows(2).any(|w| w == ["-quality", "100"]));
    }

    #[test]
    fn from_mime_ignores_case_and_parameters_and_rejects_vector_formats() {
        assert_eq!(InputFormat::from_mime("image/png"), Some(InputFormat::Png));
        assert_eq!(InputFormat::from_mime("IMAGE/JPEG; q=1"), Some(InputFormat::Jpeg));
        assert_eq!(InputFormat::from_mime("image/jpg"), Some(InputFormat::Jpeg));
        assert_eq!(InputFormat::from_mime("image/x-ms-bmp"), Some(InputFormat::Bmp));
        assert_eq!(InputFormat::from_mime("image/svg+xml"), None);
        assert_eq!(InputFormat::from_mime("application/pdf"), None);
        assert_eq!(InputFormat::from_mime("text/plain"), None);
        assert_eq!(InputFormat::from_mime(""), None);
    }

    #[test]
    fn sniff_trusts_bytes_and_refuses_svg_and_junk() {
        assert_eq!(InputFormat::sniff(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]), Some(InputFormat::Png));
        assert_eq!(InputFormat::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(InputFormat::Jpeg));
        assert_eq!(InputFormat::sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"), None);
        assert_eq!(InputFormat::sniff(b"plain text"), None);
        assert_eq!(InputFormat::sniff(b""), None);
    }

    #[test]
    fn output_mime_matches_the_encoding() {
        assert_eq!(OutputFormat::Png.mime(), "image/png");
        assert_eq!(OutputFormat::WebpLossless.mime(), "image/webp");
    }

    // --- privacy stripping ---

    #[test]
    fn png_text_chunks_are_removed_for_both_output_formats() {
        if !magick_or_skip() { return; }
        let source = png_with_text_metadata();
        assert!(contains(&source, MARKER_COMMENT) && contains(&source, MARKER_AUTHOR), "fixture must carry the metadata");

        for output in [OutputFormat::Png, lossy(80), OutputFormat::WebpLossless] {
            let (result, out) = run_job(Transcode { input: InputFormat::Png, output }, source.clone());
            result.unwrap();
            assert!(!contains(&out, MARKER_COMMENT), "{output:?} kept the comment");
            assert!(!contains(&out, MARKER_AUTHOR), "{output:?} kept the author");
        }
    }

    #[test]
    fn exif_and_jpeg_comments_are_removed_and_the_control_proves_the_test_can_fail() {
        if !magick_or_skip() { return; }
        let source = jpeg_with_metadata("32x32", None);
        assert!(contains(&source, MARKER_CAMERA) && contains(&source, MARKER_COMMENT));

        // Control: the same conversion WITHOUT `-strip` keeps the EXIF, so
        // the absence asserted below is caused by the stripping, not by the
        // fixture or the encoder happening to drop it.
        let unstripped_webp = raw(&["jpeg:-", "-quality", "80", "webp:-"], source.clone());
        let unstripped_png = raw(&["jpeg:-", "png:-"], source.clone());
        assert!(contains(&unstripped_webp, MARKER_CAMERA), "control: webp should carry EXIF without -strip");
        assert!(contains(&unstripped_png, MARKER_CAMERA), "control: png should carry EXIF without -strip");
        assert!(contains(&unstripped_png, MARKER_COMMENT), "control: png should carry the comment without -strip");

        for output in [OutputFormat::Png, lossy(80), OutputFormat::WebpLossless] {
            let (result, out) = run_job(Transcode { input: InputFormat::Jpeg, output }, source.clone());
            result.unwrap();
            assert!(!contains(&out, MARKER_CAMERA), "{output:?} kept EXIF");
            assert!(!contains(&out, MARKER_COMMENT), "{output:?} kept the comment");
        }
    }

    #[test]
    fn exif_orientation_is_baked_into_the_pixels_before_it_is_stripped() {
        if !magick_or_skip() { return; }
        // Stored 8x4 with Orientation=6 (rotate 90° CW to display): shown as 4x8.
        let source = jpeg_with_metadata("8x4", Some(6));
        assert_eq!(dimensions(source.clone()), "8x4");
        let (result, out) = run_job(Transcode { input: InputFormat::Jpeg, output: OutputFormat::Png }, source);
        result.unwrap();
        assert_eq!(dimensions(out), "4x8");
    }

    // --- encoding ---

    #[test]
    fn webp_output_has_a_riff_webp_signature_and_is_smaller_than_the_png() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "512x512", "plasma:fractal", "-depth", "8", "png:-"]);
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: lossy(80) }, png.clone());
        let transfer = result.unwrap();
        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[8..12], b"WEBP");
        assert_eq!(transfer.bytes_in, png.len() as u64);
        assert_eq!(transfer.bytes_out, out.len() as u64);
        assert!(out.len() < png.len(), "lossy webp ({}) should be smaller than png ({})", out.len(), png.len());
        assert_eq!(dimensions(out), "512x512");
    }

    #[test]
    fn lower_quality_yields_a_smaller_lossy_file() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "256x256", "plasma:fractal", "-depth", "8", "png:-"]);
        let (low, low_out) = run_job(Transcode { input: InputFormat::Png, output: lossy(10) }, png.clone());
        let (high, high_out) = run_job(Transcode { input: InputFormat::Png, output: lossy(95) }, png);
        low.unwrap();
        high.unwrap();
        assert!(low_out.len() < high_out.len());
    }

    #[test]
    fn lossless_webp_and_png_preserve_every_pixel() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "128x128", "plasma:fractal", "-depth", "8", "png:-"]);
        let original = pixel_signature(png.clone());
        for output in [OutputFormat::Png, OutputFormat::WebpLossless] {
            let (result, out) = run_job(Transcode { input: InputFormat::Png, output }, png.clone());
            result.unwrap();
            assert_eq!(pixel_signature(out), original, "{output:?} altered pixel data");
        }
    }

    #[test]
    fn lossy_webp_is_not_pixel_identical_which_distinguishes_the_two_modes() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "128x128", "plasma:fractal", "-depth", "8", "png:-"]);
        let original = pixel_signature(png.clone());
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: lossy(30) }, png);
        result.unwrap();
        assert_ne!(pixel_signature(out), original);
    }

    #[test]
    fn extreme_quality_inputs_still_produce_valid_webp() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "32x32", "plasma:fractal", "-depth", "8", "png:-"]);
        for q in [0u8, 255] {
            let (result, out) = run_job(Transcode { input: InputFormat::Png, output: lossy(q) }, png.clone());
            result.unwrap();
            assert_eq!(&out[8..12], b"WEBP");
        }
    }

    #[test]
    fn animated_input_yields_a_single_static_frame() {
        if !magick_or_skip() { return; }
        let gif = generate(&["-size", "8x4", "xc:red", "xc:blue", "xc:green", "-set", "delay", "10", "gif:-"]);
        let frames = raw(&["-", "-format", "%n,", "info:"], gif.clone());
        assert!(String::from_utf8_lossy(&frames).starts_with('3'), "fixture should have 3 frames");
        let (result, out) = run_job(Transcode { input: InputFormat::Gif, output: OutputFormat::Png }, gif);
        result.unwrap();
        let out_frames = raw(&["-", "-format", "%n,", "info:"], out);
        assert!(String::from_utf8_lossy(&out_frames).starts_with('1'));
    }

    // --- hostile and malformed input ---

    #[test]
    fn declared_format_is_enforced_rather_than_auto_detected() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "8x8", "xc:red", "png:-"]);
        let (result, out) = run_job(Transcode { input: InputFormat::Jpeg, output: OutputFormat::Png }, png);
        assert!(matches!(result, Err(PipelineError::Failed { .. })), "a PNG declared as JPEG must not decode");
        assert!(out.is_empty());
    }

    #[test]
    fn svg_cannot_be_smuggled_in_under_a_raster_label() {
        if !magick_or_skip() { return; }
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"5\" height=\"5\"><rect width=\"5\" height=\"5\"/></svg>".to_vec();
        for input in [InputFormat::Png, InputFormat::Jpeg, InputFormat::Gif, InputFormat::Webp, InputFormat::Bmp] {
            let (result, out) = run_job(Transcode { input, output: OutputFormat::Png }, svg.clone());
            assert!(matches!(result, Err(PipelineError::Failed { .. })), "{input:?} accepted SVG");
            assert!(out.is_empty());
        }
    }

    #[test]
    fn image_wider_than_the_resource_limit_is_rejected() {
        if !magick_or_skip() { return; }
        // 17000 > 16384 pixels wide, yet only a few hundred bytes of PNG.
        let wide = generate(&["-size", "17000x1", "xc:red", "png:-"]);
        assert!(wide.len() < 4096);
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, wide);
        match result {
            Err(PipelineError::Failed { stderr, .. }) => assert!(stderr.contains("limit"), "stderr: {stderr}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(out.is_empty());
    }

    #[test]
    fn image_at_the_width_limit_is_still_accepted() {
        if !magick_or_skip() { return; }
        let edge = generate(&["-size", "16384x1", "xc:red", "png:-"]);
        let (result, out) = run_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, edge);
        result.unwrap();
        assert_eq!(dimensions(out), "16384x1");
    }

    #[test]
    fn garbage_empty_and_truncated_inputs_fail_without_panicking_or_hanging() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "128x128", "plasma:fractal", "-depth", "8", "png:-"]);
        let truncated = png[..png.len() / 2].to_vec();
        for input in [Vec::new(), b"definitely not an image".to_vec(), truncated, vec![0xA5u8; 4 * 1024 * 1024]] {
            let (result, _) = run_job(Transcode { input: InputFormat::Png, output: lossy(80) }, input);
            assert!(matches!(result, Err(PipelineError::Failed { .. })));
        }
    }

    // --- determinism: the output is a pure function of the input ---

    /// The chunk types of a PNG, in order, with consecutive `IDAT`s merged.
    fn chunk_types(png: &[u8]) -> Vec<String> {
        let mut types: Vec<String> = Vec::new();
        let mut offset = 8;
        while let Some(header) = png.get(offset..offset + 8) {
            let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let kind = String::from_utf8_lossy(&header[4..8]).into_owned();
            if types.last() != Some(&kind) || kind != "IDAT" {
                types.push(kind);
            }
            offset += 12 + len;
        }
        types
    }

    #[test]
    fn png_output_excludes_the_time_chunk_and_no_other_output_needs_it() {
        for input in ALL_INPUTS {
            let png = owned_args(&Transcode { input, output: OutputFormat::Png });
            assert!(png.windows(2).any(|w| w == ["-define", "png:exclude-chunk=tIME"]), "{input:?}");
            for output in [OutputFormat::WebpLossless, lossy(80)] {
                let webp = owned_args(&Transcode { input, output });
                assert!(webp.iter().all(|a| !a.contains("exclude-chunk")), "{input:?} -> {output:?}");
            }
        }
    }

    #[test]
    fn a_converted_png_carries_no_timestamp() {
        if !magick_or_skip() { return; }
        // The generator's own output has a `tIME`, so this also shows that the
        // input's stamp is not carried over and a fresh one is not added.
        let source = generate(&["-size", "64x64", "plasma:fractal", "-depth", "8", "png:-"]);
        assert!(chunk_types(&source).iter().any(|t| t == "tIME"), "fixture must carry a timestamp: {:?}", chunk_types(&source));
        let (result, converted) = run_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, source);
        result.unwrap();
        let types = chunk_types(&converted);
        assert!(!types.iter().any(|t| t == "tIME"), "{types:?}");
        assert_eq!(types.first().map(String::as_str), Some("IHDR"));
        assert_eq!(types.last().map(String::as_str), Some("IEND"));
    }

    #[test]
    fn the_same_image_converted_a_second_apart_is_the_same_file() {
        // Without the exclusion the two outputs differ in the seconds byte of
        // the timestamp, and so do their digests (the database key).
        if !magick_or_skip() { return; }
        let source = generate(&["-size", "64x64", "plasma:fractal", "-depth", "8", "png:-"]);
        let job = Transcode { input: InputFormat::Png, output: OutputFormat::Png };
        let second = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let (first_result, first) = run_job(job, source.clone());
        first_result.unwrap();
        let started = second();
        while second() == started {
            std::thread::sleep(Duration::from_millis(20));
        }
        let (later_result, later) = run_job(job, source);
        later_result.unwrap();
        assert!(first == later, "the conversion depends on the time of day");
    }

    // --- descriptor to descriptor: `transcode_file` ---

    /// A file under `target/` removed on drop (no temp-dir crate).
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(tag: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("transcode-file-test-tmp");
            std::fs::create_dir_all(&dir).unwrap();
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(dir.join(format!("{}-{id}-{tag}", std::process::id())))
        }

        fn with(tag: &str, bytes: &[u8]) -> Self {
            let file = Self::new(tag);
            std::fs::write(&file.0, bytes).unwrap();
            file
        }

        /// A fresh handle open for reading and writing, as the converter's sink needs.
        fn open_rw(&self) -> File {
            std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&self.0).unwrap()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[derive(Default)]
    struct Collect(Vec<u8>);

    impl ByteTap for Collect {
        fn update(&mut self, chunk: &[u8]) {
            self.0.extend_from_slice(chunk);
        }
    }

    /// Runs `transcode_file` from `input` bytes; returns the result, the stored
    /// bytes and what the tap saw.
    fn run_file_job(job: Transcode, input: &[u8], limit: u64) -> (Result<Transfer, PipelineError>, Vec<u8>, Vec<u8>) {
        let source = TempFile::with("in", input);
        let sink = TempFile::new("out");
        let mut output = sink.open_rw();
        let mut tap = Collect::default();
        let result = transcode_file(job, File::open(&source.0).unwrap(), &mut output, limit, &mut tap);
        (result, std::fs::read(&sink.0).unwrap(), tap.0)
    }

    #[test]
    fn the_descriptor_road_produces_exactly_the_bytes_of_the_buffered_road() {
        // Same arguments, same decoder: only the plumbing differs, so the output
        // (and with it the fingerprint that keys deduplication) must be identical.
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "96x64", "plasma:fractal", "-depth", "8", "-set", "comment", SECRET, "png:-"]);
        let jpeg = generate(&["-size", "96x64", "plasma:fractal", "jpg:-"]);
        let webp = generate(&["-size", "96x64", "plasma:fractal", "webp:-"]);
        let bmp = generate(&["-size", "96x64", "plasma:fractal", "bmp:-"]);
        for (input, source) in [(InputFormat::Png, png), (InputFormat::Jpeg, jpeg), (InputFormat::Webp, webp), (InputFormat::Bmp, bmp)] {
            let job = Transcode { input, output: OutputFormat::Png };
            let (buffered, expected) = run_job(job, source.clone());
            buffered.unwrap();

            let (result, stored, seen) = run_file_job(job, &source, 64 << 20);
            let transfer = result.unwrap();
            assert!(stored == expected, "{input:?}: the two roads disagree");
            assert!(seen == expected, "{input:?}: the tap must have seen the stored bytes");
            assert_eq!(transfer.bytes_in, source.len() as u64, "{input:?}");
            assert_eq!(transfer.bytes_out, expected.len() as u64, "{input:?}");
        }
    }

    const SECRET: &str = "SECRETCOMMENT";

    #[test]
    fn the_descriptor_road_strips_metadata_like_the_buffered_road() {
        if !magick_or_skip() { return; }
        let source = png_with_text_metadata();
        assert!(contains(&source, MARKER_COMMENT), "fixture must carry the metadata");
        let (result, stored, _) = run_file_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, &source, 64 << 20);
        result.unwrap();
        assert!(!contains(&stored, MARKER_COMMENT));
    }

    #[test]
    fn undecodable_files_fail_cleanly_on_the_descriptor_road() {
        if !magick_or_skip() { return; }
        let png = generate(&["-size", "128x128", "plasma:fractal", "-depth", "8", "png:-"]);
        let truncated = png[..png.len() / 2].to_vec();
        for input in [Vec::new(), b"definitely not an image".to_vec(), truncated, vec![0xA5u8; 4 * 1024 * 1024]] {
            let (result, _, _) = run_file_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, &input, 64 << 20);
            assert!(matches!(result, Err(PipelineError::Failed { .. })), "{result:?}");
        }
    }

    #[test]
    fn an_output_over_the_limit_is_a_sink_error_not_a_truncated_image() {
        if !magick_or_skip() { return; }
        let source = generate(&["-size", "64x64", "plasma:fractal", "-depth", "8", "png:-"]);
        let (result, stored, _) = run_file_job(Transcode { input: InputFormat::Png, output: OutputFormat::Png }, &source, 100);
        assert!(matches!(result, Err(PipelineError::Sink(_))), "{result:?}");
        assert!(stored.len() <= 101);
    }

    #[test]
    fn the_signature_check_accepts_only_the_promised_type() {
        let file = TempFile::new("signature");
        let reader = || File::open(&file.0).unwrap();
        std::fs::write(&file.0, b"definitely not a png").unwrap();
        assert!(matches!(check_signature(OutputFormat::Png, &reader()), Err(PipelineError::Failed { code: Some(0), .. })));
        std::fs::write(&file.0, b"\x89PNG\r\n\x1a\nrest").unwrap();
        assert!(check_signature(OutputFormat::Png, &reader()).is_ok());
        // A valid image of another kind is still not what was promised.
        assert!(check_signature(OutputFormat::WebpLossless, &reader()).is_err());
        std::fs::write(&file.0, b"").unwrap();
        assert!(matches!(check_signature(OutputFormat::Png, &reader()), Err(PipelineError::Failed { .. })), "an empty output is not an image");
    }

    #[test]
    fn a_sink_that_cannot_be_read_back_is_reported_as_a_sink_error() {
        let file = TempFile::new("writeonly");
        let writer = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&file.0).unwrap();
        assert!(matches!(check_signature(OutputFormat::Png, &writer), Err(PipelineError::Sink(_))));
    }
}
