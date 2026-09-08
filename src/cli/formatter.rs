// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/formatter.rs

use unicode_width::UnicodeWidthChar;
use crate::core::constants::*;

/// Convert a timestamp into a human-readable relative time string.
pub fn format_time(timestamp_ms: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    
    let diff_sec = now.saturating_sub(timestamp_ms) / 1000;

    if diff_sec < 60 {
        format!("{}{}", diff_sec, TIME_UNIT_SEC)
    } else if diff_sec < 3600 {
        format!("{}{}", diff_sec / 60, TIME_UNIT_MIN)
    } else {
        format!("{}{}", diff_sec / 3600, TIME_UNIT_HOUR)
    }
}

/// Generate a preview string truncated to a specific width, calculating full-width chars as 2 and half-width as 1.
pub fn preview_content(text: &str) -> String {
    let mut current_width = 0;
    let mut result = String::new();
    
    let clean_text = text.replace(['\n', '\r', '\t'], " ");
    let ellipsis_width = unicode_width::UnicodeWidthStr::width(ELLIPSIS);

    for c in clean_text.chars() {
        let w = c.width().unwrap_or(0);
        
        // Fit within the designated width while accounting for the ellipsis width
        if current_width + w > PREVIEW_WIDTH - ellipsis_width {
            result.push_str(ELLIPSIS);
            current_width += ellipsis_width;
            break;
        }
        result.push(c);
        current_width += w;
    }
    
    // Pad with spaces to keep column alignments consistent
    if current_width < PREVIEW_WIDTH {
        result.push_str(&" ".repeat(PREVIEW_WIDTH - current_width));
    }
    
    result
}

/// Retrieve the appropriate label corresponding to the given MIME type.
pub fn get_label(mime: &str) -> &'static str {
    // application/* rich-markup siblings (xhtml/json/xml) don't contain
    // "text", unlike text/html, text/rtf and text/markdown, which the
    // substring check below already catches.
    const RICH_MARKUP_ALTS: &[&str] = &["application/xhtml+xml", "application/json", "application/xml"];

    if mime == MIME_URI_LIST {
        LABEL_FILE
    } else if mime.starts_with("image/") {
        LABEL_IMAGE
    } else if mime.contains("text") || mime.contains("UTF8") || RICH_MARKUP_ALTS.contains(&mime) {
        LABEL_TEXT
    } else {
        LABEL_DATA
    }
}
