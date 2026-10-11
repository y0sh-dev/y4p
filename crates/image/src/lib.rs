// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/image/src/lib.rs

//! Local Image Pipeline: runs external converters (ImageMagick's `magick`)
//! as filters over `Read`/`Write` streams.
//!
//! Layering: depends only on `std` and `core`; nothing here knows about
//! SQLite, Wayland or the daemon, so any layer may adopt it later.

// Only ingestion (`route::route_for_ingest`) consumes this module so far;
// parts of the public surface (e.g. PNG output, egress-side conversion) are
// exercised by the unit tests only until later v0.5.x milestones adopt them,
// so `dead_code` is allowed module-wide rather than sprinkled per item.
#![allow(dead_code)]

// Aliased to `core` so the `crate::core::...` paths in the modules below
// resolve exactly as they did inside the monolith (import resolution only).
use y4p_core as core;

pub mod breaker;
pub mod magick;
pub mod pipeline;
pub mod route;
pub mod sandbox;
pub mod throttle;
pub mod transcode;
