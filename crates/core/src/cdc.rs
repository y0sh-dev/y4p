// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/cdc.rs

//! Content-defined chunking (FastCDC with normalised chunking).
//!
//! Fixed-size chunking ties every boundary to an absolute offset, so a single
//! inserted byte shifts all later boundaries and no later chunk matches its
//! old self (the byte-shift problem). Here a boundary depends only on the last
//! few dozen bytes: after an edit the cut points re-synchronise within a chunk
//! or two and every untouched chunk keeps its exact bytes, hence its hash.
//!
//! [`FastCdc`] walks a slice and yields sub-slices of it: no allocation, no
//! copy, and the chunks tile the input exactly (concatenated they are the
//! input).
//!
//! # Algorithm
//!
//! A 64-bit Gear hash `fp = (fp << 1) + GEAR[byte]` is rolled over the data.
//! Each step shifts old bytes towards the top, so bit `p` of `fp` is a
//! function of only the last `p + 1` bytes; the masks below select the top
//! bits, which gives a window of up to 64 bytes without ever subtracting a
//! byte that left it. A boundary is declared where `fp & mask == 0`.
//!
//! * Bytes below [`CDC_MIN_CHUNK_BYTES`] are skipped outright (no boundary can
//!   be there, so hashing them would be wasted work); the hash starts at zero
//!   at the minimum, which keeps a boundary a function of local content only.
//! * Normalised chunking: up to the average size a *stricter* mask (more bits)
//!   is used, past it a *looser* one. Chunks are pushed towards the average
//!   from both sides, so the size distribution is tight and few chunks end at
//!   the hard maximum.
//! * At [`CDC_MAX_CHUNK_BYTES`] the chunk is cut regardless of content.
//!
//! The gear table is generated at compile time from a fixed seed (SplitMix64),
//! so boundaries are identical on every build and platform: required for a
//! chunk hash to mean the same thing across runs.

use crate::constants::{CDC_AVG_CHUNK_BYTES, CDC_MAX_CHUNK_BYTES, CDC_MIN_CHUNK_BYTES};

/// Mask strictness is `log2(average) +/- NORMALISATION_LEVEL` bits. Level 2 is
/// the FastCDC paper's recommendation.
const NORMALISATION_LEVEL: u32 = 2;

const AVG_BITS: u32 = CDC_AVG_CHUNK_BYTES.trailing_zeros();

/// `bits` ones at the top of the word, where the Gear hash has the longest memory.
const fn top_mask(bits: u32) -> u64 {
    (!0u64) << (64 - bits)
}

/// Used from the minimum up to the average size: harder to hit.
const MASK_STRICT: u64 = top_mask(AVG_BITS + NORMALISATION_LEVEL);
/// Used from the average up to the maximum size: easier to hit.
const MASK_LOOSE: u64 = top_mask(AVG_BITS - NORMALISATION_LEVEL);

const _: () = {
    assert!(CDC_MIN_CHUNK_BYTES < CDC_AVG_CHUNK_BYTES);
    assert!(CDC_AVG_CHUNK_BYTES < CDC_MAX_CHUNK_BYTES);
    // The mask arithmetic assumes the average is an exact power of two.
    assert!(CDC_AVG_CHUNK_BYTES.is_power_of_two());
    assert!(AVG_BITS > NORMALISATION_LEVEL);
    assert!(AVG_BITS + NORMALISATION_LEVEL < 64);
};

/// 256 pseudo-random 64-bit words, one per byte value (SplitMix64, fixed seed).
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
};

/// Length of the first chunk of `data`: always in `1..=CDC_MAX_CHUNK_BYTES`
/// for non-empty input, and never longer than `data`.
fn cut_point(data: &[u8]) -> usize {
    let limit = data.len().min(CDC_MAX_CHUNK_BYTES);
    if limit <= CDC_MIN_CHUNK_BYTES {
        return limit;
    }
    let normal = CDC_AVG_CHUNK_BYTES.min(limit);

    let mut fp: u64 = 0;
    let mut pos = CDC_MIN_CHUNK_BYTES;
    for (mask, end) in [(MASK_STRICT, normal), (MASK_LOOSE, limit)] {
        for &byte in data.get(pos..end).unwrap_or_default() {
            fp = (fp << 1).wrapping_add(GEAR[usize::from(byte)]);
            pos += 1;
            if fp & mask == 0 {
                return pos;
            }
        }
    }
    limit
}

/// Iterator over the content-defined chunks of a byte slice.
///
/// Yields borrowed sub-slices in order; the empty slice yields nothing. Only
/// the last chunk can be shorter than [`CDC_MIN_CHUNK_BYTES`].
#[derive(Debug, Clone)]
pub struct FastCdc<'a> {
    rest: &'a [u8],
}

impl<'a> FastCdc<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }
}

impl<'a> Iterator for FastCdc<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        // `cut_point` never exceeds `rest.len()`; the `None` arm is unreachable
        // and only drains the remainder rather than panic.
        let (chunk, rest) = self.rest.split_at_checked(cut_point(self.rest)).unwrap_or((self.rest, &[]));
        self.rest = rest;
        Some(chunk)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Each chunk is at least one byte and at most the maximum; the last
        // one is the only one allowed to be short.
        let len = self.rest.len();
        (len.div_ceil(CDC_MAX_CHUNK_BYTES), Some(len))
    }
}

impl std::iter::FusedIterator for FastCdc<'_> {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Deterministic incompressible bytes (xorshift64*), so the tests need no
    /// RNG crate and fail identically everywhere.
    fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    fn chunks(data: &[u8]) -> Vec<&[u8]> {
        FastCdc::new(data).collect()
    }

    /// The invariants every input must satisfy, whatever its content.
    fn assert_well_formed(data: &[u8]) {
        let parts = chunks(data);
        let rejoined: Vec<u8> = parts.iter().flat_map(|c| c.iter().copied()).collect();
        assert!(rejoined == data, "chunks must tile the input exactly");
        for (i, c) in parts.iter().enumerate() {
            assert!(!c.is_empty());
            assert!(c.len() <= CDC_MAX_CHUNK_BYTES);
            if i + 1 < parts.len() {
                assert!(c.len() >= CDC_MIN_CHUNK_BYTES, "only the last chunk may be short");
            }
        }
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert_eq!(FastCdc::new(&[]).count(), 0);
    }

    #[test]
    fn input_below_the_minimum_is_one_chunk() {
        for len in [1, 100, CDC_MIN_CHUNK_BYTES - 1, CDC_MIN_CHUNK_BYTES] {
            let data = random_bytes(len, 7);
            let parts = chunks(&data);
            assert_eq!(parts.len(), 1, "len {len}");
            assert!(parts[0] == data);
        }
    }

    #[test]
    fn input_up_to_the_maximum_never_splits_past_it() {
        let data = random_bytes(CDC_MAX_CHUNK_BYTES, 11);
        assert_well_formed(&data);
        let data = random_bytes(CDC_MAX_CHUNK_BYTES + 1, 11);
        assert_well_formed(&data);
    }

    #[test]
    fn uniform_data_is_cut_at_bounded_sizes() {
        // A constant byte drives the hash to a fixed point; whether or not it
        // ever matches a mask, the bounds must hold and the input be tiled.
        assert_well_formed(&vec![0x00; 1 << 20]);
        assert_well_formed(&vec![0xFF; 1 << 20]);
        assert_well_formed(&vec![0x00; CDC_MIN_CHUNK_BYTES + 1]);
    }

    #[test]
    fn random_data_is_well_formed_at_awkward_lengths() {
        for (i, len) in [4097, 16_384, 16_385, 65_535, 65_536, 65_537, 200_003, 1 << 20].into_iter().enumerate() {
            assert_well_formed(&random_bytes(len, 100 + i as u64));
        }
    }

    #[test]
    fn chunking_is_deterministic() {
        let data = random_bytes(1 << 20, 3);
        let a: Vec<usize> = chunks(&data).iter().map(|c| c.len()).collect();
        let b: Vec<usize> = chunks(&data).iter().map(|c| c.len()).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn chunks_borrow_from_the_input_without_copying() {
        let data = random_bytes(100_000, 5);
        let range = data.as_ptr_range();
        for c in FastCdc::new(&data) {
            assert!(range.contains(&c.as_ptr()), "a chunk must point into the source slice");
        }
    }

    #[test]
    fn the_average_chunk_size_is_near_the_target() {
        let data = random_bytes(8 << 20, 42);
        let parts = chunks(&data);
        let mean = data.len() / parts.len();
        assert!(
            (CDC_AVG_CHUNK_BYTES / 2..=CDC_AVG_CHUNK_BYTES * 2).contains(&mean),
            "mean chunk size {mean} is far from the {CDC_AVG_CHUNK_BYTES}-byte target"
        );
        // Normalisation should keep forced cuts at the maximum rare.
        let forced = parts.iter().filter(|c| c.len() == CDC_MAX_CHUNK_BYTES).count();
        assert!(forced * 10 < parts.len(), "{forced} of {} chunks hit the hard maximum", parts.len());
    }

    /// Share of `before`'s chunks that reappear byte-for-byte in `after`.
    fn survival(before: &[u8], after: &[u8]) -> f64 {
        let kept: HashSet<&[u8]> = chunks(after).into_iter().collect();
        let old = chunks(before);
        old.iter().filter(|c| kept.contains(*c)).count() as f64 / old.len() as f64
    }

    #[test]
    fn boundaries_resynchronise_after_an_insertion() {
        let original = random_bytes(2 << 20, 9);
        let mid = original.len() / 2;
        for inserted in [1usize, 7, 4096] {
            let mut edited = original[..mid].to_vec();
            edited.extend(std::iter::repeat_n(0xA5, inserted));
            edited.extend_from_slice(&original[mid..]);
            let share = survival(&original, &edited);
            assert!(share >= 0.90, "inserting {inserted} byte(s) kept only {:.1}% of the chunks", share * 100.0);
        }
    }

    #[test]
    fn boundaries_resynchronise_after_a_deletion() {
        let original = random_bytes(2 << 20, 13);
        let mid = original.len() / 2;
        let mut edited = original[..mid].to_vec();
        edited.extend_from_slice(&original[mid + 1..]);
        assert!(survival(&original, &edited) >= 0.90);
    }

    #[test]
    fn an_insertion_at_the_front_still_resynchronises() {
        let original = random_bytes(2 << 20, 21);
        let mut edited = vec![0x5A];
        edited.extend_from_slice(&original);
        assert!(survival(&original, &edited) >= 0.90);
    }

    #[test]
    fn the_size_hint_brackets_the_real_count() {
        let data = random_bytes(500_000, 8);
        let (lo, hi) = FastCdc::new(&data).size_hint();
        let n = FastCdc::new(&data).count();
        assert!(lo <= n && n <= hi.unwrap());
    }
}
