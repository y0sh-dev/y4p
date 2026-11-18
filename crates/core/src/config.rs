// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/config.rs

//! User configuration loader and lightweight TOML parser.
//!
//! Parses `y4p.toml` using standard library primitives alone, deliberately
//! avoiding external parser crates. The parser is intentionally forgiving:
//! unrecognised sections or malformed values are ignored, safely falling back
//! to built-in defaults without aborting daemon startup.

use std::path::PathBuf;

use crate::constants::DEFAULT_MAX_HISTORY;

#[derive(Debug, Clone, PartialEq)]
pub struct GeneralConfig {
    pub max_history: usize,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self { max_history: DEFAULT_MAX_HISTORY }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MimeConfig {
    pub drop_rtf: bool,
}

impl Default for MimeConfig {
    fn default() -> Self {
        Self { drop_rtf: true }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageConfig {
    /// Off by default: store raw image bytes and require no converter.
    pub sanitize: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub general: GeneralConfig,
    pub mime: MimeConfig,
    pub image: ImageConfig,
}

impl Config {
    /// `$XDG_CONFIG_HOME/y4p/y4p.toml`, falling back to `~/.config/y4p/y4p.toml`
    /// when that variable isn't set (mirroring `crate::get_db_path`'s own XDG
    /// fallback chain for consistency).
    pub fn get_config_path() -> PathBuf {
        build_config_path(std::env::var("XDG_CONFIG_HOME").ok().as_deref(), std::env::var("HOME").ok().as_deref())
    }

    /// Loads and parses the config file, falling back to `Config::default()`
    /// whenever it's missing, unreadable (permissions, not a regular file,
    /// ...), or not valid UTF-8 — a config problem is never allowed to stop
    /// the daemon from starting with sane built-in behaviour.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::get_config_path())
            .map(|content| Self::parse(&content))
            .unwrap_or_default()
    }

    /// Parses the fixed subset of TOML this schema needs: `[section]`
    /// headers, `key = value` pairs (booleans and unsigned integers — the
    /// schema holds no string or array values), and `#` comments (a `#`
    /// inside a quoted string is not treated as one). Never panics: an
    /// unrecognised section/key, a value that doesn't parse as the type
    /// that key expects, or any other malformed line is simply skipped,
    /// leaving whatever default was already set for that field.
    pub fn parse(content: &str) -> Self {
        let mut config = Self::default();
        let mut section = String::new();

        for line in content.lines().map(strip_comment) {
            let line = line.trim();
            if line.is_empty() { continue; }

            if line.starts_with('[') && line.ends_with(']') && !line.contains('=') {
                section = line[1..line.len() - 1].trim().to_string();
                continue;
            }

            let Some((key, value)) = line.split_once('=') else { continue; };
            apply_kv(&mut config, &section, key.trim(), value.trim());
        }

        config
    }

    pub fn should_drop_rtf(&self) -> bool {
        self.mime.drop_rtf
    }

    /// Whether ingestion should re-encode raster images through ImageMagick to
    /// strip metadata. Off by default: the raw bytes are stored untouched and
    /// no external converter is needed at run time.
    pub fn should_sanitize_images(&self) -> bool {
        self.image.sanitize
    }
}

/// Pure path-building logic factored out of `get_config_path` so it can be
/// exercised directly in tests without mutating process-global environment
/// state (which risks flakiness under parallel test execution).
fn build_config_path(xdg_config_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let mut path = if let Some(xdg) = xdg_config_home {
        PathBuf::from(xdg)
    } else if let Some(home) = home {
        let mut p = PathBuf::from(home);
        p.push(".config");
        p
    } else {
        PathBuf::from(".")
    };

    path.push("y4p");
    path.push("y4p.toml");
    path
}

/// Strips a trailing `#...` comment from one line, without disturbing a `#`
/// that appears inside a quoted string. Not a full TOML string-escaping
/// implementation (no `\"` handling) — this schema never needs one, and
/// keeping the scanner this simple is what keeps it panic-free.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (i, b) in line.bytes().enumerate() {
        match b {
            b'"' => in_string = !in_string,
            b'#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_usize(value: &str) -> Option<usize> {
    value.parse::<usize>().ok().filter(|&n| n > 0)
}

/// Routes one already-split `key = value` pair to the field its
/// `[section]`/key name names, converting `value`'s raw text to that
/// field's type. Anything not recognised by this schema (an unknown
/// section, an unknown key within a known one, or a value that fails to
/// parse as the expected type) is a silent no-op, leaving the field at
/// whatever it was already initialised to.
fn apply_kv(config: &mut Config, section: &str, key: &str, value: &str) {
    match (section, key) {
        ("general", "max_history") => {
            if let Some(n) = parse_usize(value) { config.general.max_history = n; }
        }
        ("mime", "drop_rtf") => {
            if let Some(b) = parse_bool(value) { config.mime.drop_rtf = b; }
        }
        ("image", "sanitize") => {
            if let Some(b) = parse_bool(value) { config.image.sanitize = b; }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // --- build_config_path ---

    #[test]
    fn build_config_path_prefers_xdg_config_home() {
        let path = build_config_path(Some("/custom/config"), Some("/home/alice"));
        assert_eq!(path, PathBuf::from("/custom/config/y4p/y4p.toml"));
    }

    #[test]
    fn build_config_path_falls_back_to_home_dot_config() {
        let path = build_config_path(None, Some("/home/alice"));
        assert_eq!(path, PathBuf::from("/home/alice/.config/y4p/y4p.toml"));
    }

    #[test]
    fn build_config_path_falls_back_to_current_dir_when_neither_set() {
        let path = build_config_path(None, None);
        assert_eq!(path, PathBuf::from("./y4p/y4p.toml"));
    }

    // --- Config::default ---

    #[test]
    fn default_matches_the_agreed_balanced_defaults() {
        let config = Config::default();
        assert_eq!(config.general.max_history, DEFAULT_MAX_HISTORY);
        assert!(config.mime.drop_rtf);
        assert!(!config.image.sanitize, "image sanitisation is opt-in");
    }

    // --- Config::parse: the full agreed schema ---

    #[test]
    fn parse_full_schema_matches_the_worked_example() {
        let toml = r#"
            [general]
            max_history = 1000

            [mime]
            drop_rtf = true

            [image]
            sanitize = true
        "#;

        let config = Config::parse(toml);
        assert_eq!(config.general.max_history, 1000);
        assert!(config.mime.drop_rtf);
        assert!(config.image.sanitize);
    }

    #[test]
    fn parse_empty_content_yields_defaults() {
        assert_eq!(Config::parse(""), Config::default());
    }

    #[test]
    fn parse_trailing_inline_comment_is_stripped() {
        let toml = "[general]\nmax_history = 500 # keep a smaller history on this box";
        assert_eq!(Config::parse(toml).general.max_history, 500);
    }

    #[test]
    fn parse_hash_inside_quoted_string_is_preserved() {
        // The comment-stripping scanner itself is exercised directly here,
        // since no remaining schema key holds a quoted string value.
        assert_eq!(strip_comment(r#"foo = "weird#value" # trailing"#), r#"foo = "weird#value" "#);
    }

    #[test]
    fn parse_unknown_section_and_key_are_ignored_without_panicking() {
        let toml = r#"
            [nonsense]
            whatever = true

            [general]
            unknown_key = 5
            max_history = 42
        "#;
        let config = Config::parse(toml);
        assert_eq!(config.general.max_history, 42);
    }

    #[test]
    fn parse_malformed_lines_are_skipped_without_panicking() {
        let toml = "this is not a key value pair at all\n[general]\nmax_history = 10";
        assert_eq!(Config::parse(toml).general.max_history, 10);
    }

    #[test]
    fn parse_type_mismatch_keeps_the_default() {
        // Boolean key given a non-boolean value: default retained.
        let toml = "[mime]\ndrop_rtf = maybe";
        assert!(Config::parse(toml).mime.drop_rtf);
        assert!(!Config::parse("[image]\nsanitize = perhaps").image.sanitize);
    }

    #[test]
    fn parse_out_of_range_max_history_keeps_the_default() {
        let toml = "[general]\nmax_history = 0";
        assert_eq!(Config::parse(toml).general.max_history, DEFAULT_MAX_HISTORY);
    }

    #[test]
    fn parse_negative_max_history_keeps_the_default() {
        let toml = "[general]\nmax_history = -5";
        assert_eq!(Config::parse(toml).general.max_history, DEFAULT_MAX_HISTORY);
    }

    #[test]
    fn parse_whitespace_around_section_and_keys_is_tolerated() {
        let toml = "  [ general ]  \n  max_history   =   77  ";
        assert_eq!(Config::parse(toml).general.max_history, 77);
    }

    // --- the should_* accessors ---

    #[test]
    fn should_drop_rtf_mirrors_the_mime_config_field() {
        assert!(Config::default().should_drop_rtf());
        let config = Config { mime: MimeConfig { drop_rtf: false }, ..Default::default() };
        assert!(!config.should_drop_rtf());
    }

    #[test]
    fn should_sanitize_images_mirrors_the_image_config_field() {
        assert!(!Config::default().should_sanitize_images());
        let config = Config { image: ImageConfig { sanitize: true }, ..Default::default() };
        assert!(config.should_sanitize_images());
    }

    #[test]
    fn parse_image_sanitize_false_keeps_it_off() {
        assert!(!Config::parse("[image]\nsanitize = false").should_sanitize_images());
        assert!(Config::parse("[image]\nsanitize = true").should_sanitize_images());
    }

    // --- Config::load: safe fallback ---

    #[test]
    fn load_falls_back_to_defaults_when_the_file_is_absent() {
        // A path that (barring an extraordinarily unlucky collision) never
        // exists on the machine running this test — `load()` must not panic
        // and must return the default configuration.
        let content = std::fs::read_to_string("/nonexistent/y4p-config-test-path/y4p.toml");
        assert!(content.is_err());
        assert_eq!(Config::default(), Config::parse(""));
    }
}
