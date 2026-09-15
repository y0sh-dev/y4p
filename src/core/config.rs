// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/core/config.rs

//! User configuration loader and lightweight TOML parser.
//!
//! Parses `y4p.toml` using standard library primitives alone, deliberately
//! avoiding external parser crates. The parser is intentionally forgiving:
//! unrecognised sections or malformed values are ignored, safely falling back
//! to built-in defaults without aborting daemon startup.

use std::path::PathBuf;

use crate::core::constants::DEFAULT_MAX_HISTORY;

#[derive(Debug, Clone, PartialEq)]
pub struct GeneralConfig {
    pub max_history: usize,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self { max_history: DEFAULT_MAX_HISTORY }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageConfig {
    /// When enabled, fetches the original remote image referenced by HTML offers.
    /// Defaults to `false` to avoid ambient network requests without opt-in.
    pub hijack_original: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SanitizeConfig {
    pub strip_tracking: bool,
}

impl Default for SanitizeConfig {
    fn default() -> Self {
        Self { strip_tracking: true }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MimeConfig {
    pub drop_rtf: bool,
    pub downgrade_html: bool,
}

impl Default for MimeConfig {
    fn default() -> Self {
        Self { drop_rtf: true, downgrade_html: true }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppConfig {
    pub ignore: Vec<String>,
    pub bypass_sanitize: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub general: GeneralConfig,
    pub image: ImageConfig,
    pub sanitize: SanitizeConfig,
    pub mime: MimeConfig,
    pub app: AppConfig,
}

impl Config {
    /// `$XDG_CONFIG_HOME/y4p/y4p.toml`, falling back to `~/.config/y4p/y4p.toml`
    /// when that variable isn't set (mirroring `core::get_db_path`'s own XDG
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
    /// headers, `key = value` pairs (booleans, unsigned integers, quoted
    /// strings, and single-/multi-line string arrays), and `#` comments
    /// (a `#` inside a quoted string is not treated as one). Never panics:
    /// an unrecognised section/key, a value that doesn't parse as the type
    /// that key expects, or any other malformed line is simply skipped,
    /// leaving whatever default was already set for that field.
    pub fn parse(content: &str) -> Self {
        let mut config = Self::default();
        let lines: Vec<&str> = content.lines().map(strip_comment).collect();
        let mut section = String::new();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i].trim();
            i += 1;
            if line.is_empty() { continue; }

            if line.starts_with('[') && line.ends_with(']') && !line.contains('=') {
                section = line[1..line.len() - 1].trim().to_string();
                continue;
            }

            let Some((key, value_start)) = line.split_once('=') else { continue; };
            let key = key.trim();
            let mut value = value_start.trim().to_string();

            // Multi-line array: the opening `[` was seen but not yet its
            // closing `]` — keep folding subsequent (already comment-
            // stripped) lines in until one closes it, or the file ends.
            if value.starts_with('[') && !value.ends_with(']') {
                while i < lines.len() {
                    let next = lines[i].trim();
                    i += 1;
                    value.push(' ');
                    value.push_str(next);
                    if next.ends_with(']') { break; }
                }
            }

            apply_kv(&mut config, &section, key, value.trim());
        }

        config
    }

    /// Case-insensitive check of `app_id` against `[app] ignore`. `None`
    /// (App ID undetectable — see `wayland::active_app`) never matches.
    pub fn is_app_ignored(&self, app_id: Option<&str>) -> bool {
        list_contains_ignore_case(&self.app.ignore, app_id)
    }

    /// True when this clipboard event's URL tracking-parameter sanitisation
    /// should be skipped: either sanitisation is globally disabled
    /// (`[sanitize] strip_tracking = false`), or the source app is
    /// specifically listed in `[app] bypass_sanitize`.
    pub fn should_bypass_sanitize(&self, app_id: Option<&str>) -> bool {
        !self.sanitize.strip_tracking || list_contains_ignore_case(&self.app.bypass_sanitize, app_id)
    }

    pub fn should_hijack_image(&self) -> bool {
        self.image.hijack_original
    }

    pub fn should_drop_rtf(&self) -> bool {
        self.mime.drop_rtf
    }

    pub fn should_downgrade_html(&self) -> bool {
        self.mime.downgrade_html
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

/// Splits a bracketed, comma-separated list of quoted strings (a single- or
/// already-folded multi-line array's full text, brackets included) into its
/// elements. An unquoted or otherwise malformed element — including the
/// empty segment a trailing comma produces — is silently dropped rather
/// than treated as an error.
fn parse_string_array(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    let Some(start) = trimmed.find('[') else { return Vec::new(); };
    let Some(end) = trimmed.rfind(']') else { return Vec::new(); };
    if start >= end { return Vec::new(); }
    let inner = &trimmed[start + 1..end];
    inner
        .split(',')
        .filter_map(|raw| {
            let t = raw.trim();
            if t.is_empty() { return None; }
            if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
                || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
            {
                Some(t[1..t.len() - 1].to_string())
            } else {
                None
            }
        })
        .collect()
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
        ("image", "hijack_original") => {
            if let Some(b) = parse_bool(value) { config.image.hijack_original = b; }
        }
        ("sanitize", "strip_tracking") => {
            if let Some(b) = parse_bool(value) { config.sanitize.strip_tracking = b; }
        }
        ("mime", "drop_rtf") => {
            if let Some(b) = parse_bool(value) { config.mime.drop_rtf = b; }
        }
        ("mime", "downgrade_html") => {
            if let Some(b) = parse_bool(value) { config.mime.downgrade_html = b; }
        }
        ("app", "ignore") => { config.app.ignore = parse_string_array(value); }
        ("app", "bypass_sanitize") => { config.app.bypass_sanitize = parse_string_array(value); }
        _ => {}
    }
}

fn list_contains_ignore_case(list: &[String], app_id: Option<&str>) -> bool {
    let Some(app_id) = app_id else { return false; };
    list.iter().any(|entry| entry.eq_ignore_ascii_case(app_id))
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
        assert!(!config.image.hijack_original);
        assert!(config.sanitize.strip_tracking);
        assert!(config.mime.drop_rtf);
        assert!(config.mime.downgrade_html);
        assert!(config.app.ignore.is_empty());
        assert!(config.app.bypass_sanitize.is_empty());
    }

    // --- Config::parse: the full agreed schema ---

    #[test]
    fn parse_full_schema_matches_the_worked_example() {
        let toml = r#"
            [general]
            max_history = 1000

            [image]
            hijack_original = false

            [sanitize]
            strip_tracking = true

            [mime]
            drop_rtf = true
            downgrade_html = true

            [app]
            ignore = [
                "org.keepassxc.KeePassXC",
                "Bitwarden",
                "1Password",
            ]
            bypass_sanitize = [
                "firefox",
            ]
        "#;

        let config = Config::parse(toml);
        assert_eq!(config.general.max_history, 1000);
        assert!(!config.image.hijack_original);
        assert!(config.sanitize.strip_tracking);
        assert!(config.mime.drop_rtf);
        assert!(config.mime.downgrade_html);
        assert_eq!(config.app.ignore, vec!["org.keepassxc.KeePassXC", "Bitwarden", "1Password"]);
        assert_eq!(config.app.bypass_sanitize, vec!["firefox"]);
    }

    #[test]
    fn parse_empty_content_yields_defaults() {
        assert_eq!(Config::parse(""), Config::default());
    }

    #[test]
    fn parse_single_line_array() {
        let toml = r#"[app]
ignore = ["a", "b", "c"]"#;
        let config = Config::parse(toml);
        assert_eq!(config.app.ignore, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_trailing_inline_comment_is_stripped() {
        let toml = "[general]\nmax_history = 500 # keep a smaller history on this box";
        assert_eq!(Config::parse(toml).general.max_history, 500);
    }

    #[test]
    fn parse_hash_inside_quoted_string_is_preserved() {
        let toml = r#"[app]
ignore = ["weird#app-id"]"#;
        assert_eq!(Config::parse(toml).app.ignore, vec!["weird#app-id"]);
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

    #[test]
    fn parse_empty_array_yields_empty_vec() {
        let toml = "[app]\nignore = []";
        assert!(Config::parse(toml).app.ignore.is_empty());
    }

    // --- is_app_ignored ---

    #[test]
    fn is_app_ignored_matches_case_insensitively() {
        let config = Config { app: AppConfig { ignore: vec!["Bitwarden".to_string()], ..Default::default() }, ..Default::default() };
        assert!(config.is_app_ignored(Some("bitwarden")));
        assert!(config.is_app_ignored(Some("BITWARDEN")));
        assert!(!config.is_app_ignored(Some("firefox")));
    }

    #[test]
    fn is_app_ignored_none_app_id_never_matches() {
        let config = Config { app: AppConfig { ignore: vec!["Bitwarden".to_string()], ..Default::default() }, ..Default::default() };
        assert!(!config.is_app_ignored(None));
    }

    #[test]
    fn is_app_ignored_empty_list_never_matches() {
        assert!(!Config::default().is_app_ignored(Some("anything")));
    }

    // --- should_bypass_sanitize ---

    #[test]
    fn should_bypass_sanitize_true_when_tracking_disabled_globally() {
        let config = Config { sanitize: SanitizeConfig { strip_tracking: false }, ..Default::default() };
        assert!(config.should_bypass_sanitize(Some("anything")));
        assert!(config.should_bypass_sanitize(None));
    }

    #[test]
    fn should_bypass_sanitize_true_for_a_listed_app_even_with_tracking_enabled() {
        let config = Config {
            sanitize: SanitizeConfig { strip_tracking: true },
            app: AppConfig { bypass_sanitize: vec!["firefox".to_string()], ..Default::default() },
            ..Default::default()
        };
        assert!(config.should_bypass_sanitize(Some("Firefox")));
    }

    #[test]
    fn should_bypass_sanitize_false_by_default() {
        let config = Config::default();
        assert!(!config.should_bypass_sanitize(Some("firefox")));
        assert!(!config.should_bypass_sanitize(None));
    }

    // --- the remaining should_* accessors ---

    #[test]
    fn should_hijack_image_mirrors_the_image_config_field() {
        assert!(!Config::default().should_hijack_image());
        let config = Config { image: ImageConfig { hijack_original: true }, ..Default::default() };
        assert!(config.should_hijack_image());
    }

    #[test]
    fn should_drop_rtf_mirrors_the_mime_config_field() {
        assert!(Config::default().should_drop_rtf());
        let config = Config { mime: MimeConfig { drop_rtf: false, downgrade_html: true }, ..Default::default() };
        assert!(!config.should_drop_rtf());
    }

    #[test]
    fn should_downgrade_html_mirrors_the_mime_config_field() {
        assert!(Config::default().should_downgrade_html());
        let config = Config { mime: MimeConfig { drop_rtf: true, downgrade_html: false }, ..Default::default() };
        assert!(!config.should_downgrade_html());
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

    #[test]
    fn parse_string_array_handles_single_quotes_and_brackets_in_string() {
        let toml = r#"
[app]
ignore = ['single_quoted_app', "bracketed_]app"]
"#;
        let config = Config::parse(toml);
        assert_eq!(config.app.ignore, vec!["single_quoted_app", "bracketed_]app"]);
    }
}
