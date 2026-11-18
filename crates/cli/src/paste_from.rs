// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/cli/src/paste_from.rs

use crate::core::constants::*;
use crate::utils::ArgContext;

/// What `paste-from` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum PasteFromRequest {
    /// Fetch the selection under this MIME type (`-l` was not given).
    Payload(String),
    /// Print the MIME types the current selection advertises, and nothing else.
    ListTypes,
}

/// Validate `paste-from` arguments and resolve the request.
/// Returns `None` on validation failure. The actual Wayland fetch and
/// stdout write happen in `main.rs` — this frontend module must not depend
/// on `wayland`.
pub fn run(args: &[String]) -> Option<PasteFromRequest> {
    let ctx = ArgContext::parse(args);

    // `-l` / `--list-types` are the only flags `paste-from` accepts. The
    // global parser has already put them in `unknown_flags` (it serves every
    // subcommand), so they are separated out here instead of widening the
    // shared parser and changing what the other commands reject.
    let mut list_types = false;
    for flag in &ctx.unknown_flags {
        match flag.as_str() {
            "-l" | "--list-types" => list_types = true,
            _ => {
                eprintln!("{}command 'paste-from' does not support options.", LOG_ERROR);
                return None;
            }
        }
    }

    // The remaining flags are global ones that make no sense for a direct
    // stream operation.
    if ctx.raw || ctx.full || ctx.force || ctx.verbose || ctx.use_id {
        eprintln!("{}command 'paste-from' does not support options.", LOG_ERROR);
        return None;
    }

    // Arity enforcement: a list request takes no MIME, and a payload request
    // takes at most one.
    if list_types && !ctx.positionals.is_empty() {
        eprintln!("{}command 'paste-from --list-types' accepts no MIME type argument.", LOG_ERROR);
        return None;
    }
    if ctx.positionals.len() > 1 {
        eprintln!("{}command 'paste-from' accepts at most one MIME type argument.", LOG_ERROR);
        return None;
    }

    if list_types {
        return Some(PasteFromRequest::ListTypes);
    }

    // Resolve target MIME from the first positional argument or use system default
    let mime = ctx.positionals.first().map(|s| s.as_str()).unwrap_or(DEFAULT_MIME);

    Some(PasteFromRequest::Payload(mime.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        let mut all = vec!["y4p".to_owned(), "paste-from".to_owned()];
        all.extend(rest.iter().map(|s| (*s).to_owned()));
        all
    }

    #[test]
    fn a_short_list_flag_asks_for_the_offered_types() {
        assert_eq!(run(&args(&["-l"])), Some(PasteFromRequest::ListTypes));
    }

    #[test]
    fn a_long_list_flag_asks_for_the_offered_types() {
        assert_eq!(run(&args(&["--list-types"])), Some(PasteFromRequest::ListTypes));
    }

    #[test]
    fn repeated_list_flags_are_still_only_a_list_request() {
        assert_eq!(run(&args(&["-l", "--list-types"])), Some(PasteFromRequest::ListTypes));
    }

    #[test]
    fn a_list_flag_with_a_mime_is_rejected() {
        assert_eq!(run(&args(&["-l", "text/plain"])), None);
        assert_eq!(run(&args(&["--list-types", "image/png"])), None);
    }

    #[test]
    fn an_unknown_flag_is_rejected() {
        assert_eq!(run(&args(&["--nope"])), None);
        assert_eq!(run(&args(&["-x"])), None);
    }

    #[test]
    fn a_global_flag_is_rejected_even_next_to_the_list_flag() {
        assert_eq!(run(&args(&["-l", "--verbose"])), None);
        assert_eq!(run(&args(&["--list-types", "-R"])), None);
    }

    #[test]
    fn a_lone_mime_asks_for_that_payload() {
        assert_eq!(run(&args(&["text/plain"])), Some(PasteFromRequest::Payload("text/plain".to_owned())));
    }

    #[test]
    fn no_arguments_ask_for_the_default_payload() {
        assert_eq!(run(&args(&[])), Some(PasteFromRequest::Payload(DEFAULT_MIME.to_owned())));
    }

    #[test]
    fn more_than_one_mime_is_rejected() {
        assert_eq!(run(&args(&["text/plain", "image/png"])), None);
    }
}
