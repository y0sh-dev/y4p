// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// crates/core/src/memory.rs

//! Handing freed heap memory back to the kernel.
//!
//! `free(3)` returns a block to the allocator, not to the operating system:
//! glibc keeps the pages of a freed small block mapped (and resident) until the
//! top of an arena can be trimmed, which a long-lived process that once held a
//! large history may never see. After an operation that frees a great deal at
//! once (`y4p wipe`) the process should say so explicitly.

/// Asks the allocator to return every free page it holds to the kernel.
///
/// Returns whether the allocator reported that it released memory. That is
/// `false` when it had nothing to give back, and always `false` on targets whose
/// C library has no `malloc_trim(3)` (everything but Linux with glibc); there
/// the call is a no-op, never an error: this is an optimisation hint, not a
/// guarantee.
pub fn release_unused_heap() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: `malloc_trim` takes no pointer. It is thread-safe (it locks each
        // arena in turn), may be called at any time, and only returns pages that
        // `free` has already given back to the allocator, so no live allocation
        // is touched. Its argument is the padding to keep at the top of the heap;
        // zero keeps none.
        unsafe { libc::malloc_trim(0) != 0 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Resident set size in bytes, from `/proc/self/statm` (field 2, in pages).
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn resident_bytes() -> u64 {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
        let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
        // SAFETY: `sysconf` with a valid name has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        pages * u64::try_from(page).unwrap()
    }

    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn trimming_returns_freed_small_blocks_to_the_kernel() {
        // Many blocks far below the mmap threshold live in the arena, so freeing
        // all but the last one leaves them resident until the allocator is told.
        const BLOCK: usize = 4096;
        const COUNT: usize = 24 * 1024; // 96 MiB
        let mut blocks: Vec<Vec<u8>> = (0..COUNT).map(|i| vec![(i % 251) as u8; BLOCK]).collect();
        let pinned = blocks.pop().unwrap(); // keeps the top of the arena in use
        let held = resident_bytes();
        drop(blocks);
        let before = resident_bytes();

        // `malloc_trim(3)` is an advisory optimisation hint, not a guarantee.
        // Under AddressSanitizer (ASan) or non-standard allocators, `malloc_trim`
        // is typically a no-op stub that returns 0. Assert RSS reduction only
        // when the allocator actually reports successful reclamation.
        let trimmed = release_unused_heap();
        let after = resident_bytes();

        assert!(held >= (COUNT * BLOCK) as u64 / 2, "the fixture must really be resident ({held})");
        if trimmed {
            assert!(
                before.saturating_sub(after) >= (COUNT * BLOCK) as u64 / 2,
                "trimming should return at least half of the 96 MiB freed: resident {before} -> {after}"
            );
        }
        assert_eq!(pinned.len(), BLOCK, "a live block survives the trim");
    }

    #[test]
    fn trimming_is_always_safe_to_call() {
        let live = vec![7u8; 1 << 20];
        let _ = release_unused_heap();
        let _ = release_unused_heap();
        assert!(live.iter().all(|&b| b == 7));
    }
}
