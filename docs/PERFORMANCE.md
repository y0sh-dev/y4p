
# y4p: Optimization Philosophy and Implementation

This document defines the concept of "Optimization" within the `y4p` project and details how the system interacts with hardware to minimize friction when handling massive datasets (e.g., 70MB+ lossless images).

---

## 1. Definition: Frictionless Infrastructure

In this project, optimization is not merely about "speed." It is defined as **"respecting the management units of the Operating System and physically eliminating redundant round-trips between the CPU and Memory."**

As a clipboard manager is a piece of system infrastructure, any resource waste acts as "noise" for every other application running on the desktop. Our implementation follows three core principles to ensure a silent, efficient presence.

---

## 2. Strategy: Single-pass Logic

We do not treat data ingestion and verification (hashing) as separate stages. Typically, a system reads data into memory and then scans it again to apply a hash function. This effectively means scanning the same memory region twice.

### 2.1 Streaming Hashing
Within the ingestion loop, the buffer received from the kernel is immediately fed into the SHA3-256 context while it still resides in the CPU cache.

```rust
// Ingest and Hash in a single pass
while let Ok(n) = reader.read(chunk) {
    if n == 0 { break; }
    let data = &chunk[..n];
    
    // Update SHA3-256 state while the data is still in CPU cache
    hasher.update(data); 
    
    // Accumulate into memory for final persistence
    payload.extend_from_slice(data);
}
```
This approach maximizes CPU cache hits and conserves memory bandwidth, which is critical when processing high-resolution assets.

---

## 3. Strategy: Mechanical Sympathy

We align software logic with the physical realities of the hardware and the OS kernel.

### 3.1 Page-aligned I/O
To match the Linux kernel's memory management unit (typically 4KB pages), we use `std::alloc` to force the starting address of our buffers to a page boundary.

```rust
// Aligning memory to 4096-byte boundaries to minimize TLB misses
let layout = std::alloc::Layout::from_size_align(65536, 4096).unwrap();
let ptr = unsafe { std::alloc::alloc(layout) };
// ... perform direct I/O operation ...
```
Avoiding memory accesses that cross page boundaries reduces the overhead of system calls and shaves off microseconds of latency during large binary transfers.

### 3.2 Proactive Memory Reclamation (malloc_trim)
Standard allocators often retain freed memory for future use. In a long-running daemon, this can lead to an unnecessarily high Resident Set Size (RSS).

```rust
// Forcing the heap to shrink after processing large assets
#[cfg(target_os = "linux")]
unsafe { libc::malloc_trim(0); }
```
By explicitly signaling the OS to reclaim unused heap segments immediately after a large ingestion, we maintain a minimal memory footprint.

---

## 4. Architecture: Hybrid Persistence

We protect the SQLite B-Tree structure from being "polluted" by massive binary blobs. By storing metadata and text in the database while offloading images to the filesystem, we ensure that the database page cache remains filled with searchable information. This keeps the response time for `list` and `search` operations near constant ($O(1)$) regardless of the total history size.

---

## 5. Visualization: Data Flow Integrity

The integration of these strategies results in the following streamlined data path:

```text
[ Wayland Pipe ]
       |
       | (4KB Aligned Stream)
       v
[ CPU Cache / Ingestion ] ----------------+
       |                                  |
       | (Update SHA3-256 state)          | (Accumulate)
       v                                  v
[ Final Fingerprint ]              [ Memory Payload ]
       |                                  |
       +-----------------+----------------+
                         |
                         v
          [ Hybrid Storage Decision ]
          /                         \
    (Text/Meta)                 (Binary Assets)
         |                             |
    [ SQLite 3 ]               [ ~/.cache/y4p/ ]
```

---

## 6. Roadmap: Challenging Theoretical Limits

We continue to push the boundaries of what is possible for a Wayland clipboard manager.

### [x] Current Milestones (Implemented)
- [x] **SHA3-256 Single-pass Hashing**: Concurrent ingestion and fingerprinting.
- [x] **Page-aligned Buffering**: Optimized kernel-to-user memory copies.
- [x] **Hybrid Storage Engine**: Physical separation of metadata and binary assets.
- [x] **Worker Thread Serialization**: Elimination of database lock contention.
- [x] **Proactive Resource Reclamation**: RSS management via `malloc_trim`.

### [ ] The Next Frontier (Planned)
- [x] **Kernel-level Egress**: `sendfile(2)` for `copy-to` operations on cache-backed
  (image) entries, bypassing user-space memory entirely for both the read and the
  now-eliminated duplicate `.clone()` that previously happened on every send.
  Implemented in `wayland/handlers/data_control/source.rs`; see
  `storage::ContentLocation` / `ClipboardDb::locate_content` for how a record is
  routed to this path vs. the existing in-memory path.
- [ ] ~~**Incremental BLOB I/O**~~ — reviewed, not pursued. The stated goal (fix
  memory usage at 64KB regardless of file size) is already met for the case that
  matters: large binaries never go through the SQLite BLOB column at all — the
  Hybrid Storage Engine (§4) already routes them to the on-disk cache, which is
  exactly what Kernel-level Egress above now reads from directly. `sqlite3_blob_open`
  incremental I/O would only affect small inline (text) rows, and additionally
  conflicts with the current unknown-length pipe-streaming ingestion model: `zeroblob`
  requires the final size up front, which would mean buffering to learn the size
  before writing the blob — reintroducing the double-read this architecture exists
  to avoid.
- [ ] ~~**SIMD-accelerated Fingerprinting**~~ — reviewed, not pursued. SHA-NI is an
  Intel extension for SHA-1/SHA-256; it does not apply to SHA3 (Keccak) at all, which
  is what this project hashes with. An AVX-512 Keccak path exists in principle but
  isn't available in the Rust crates this project depends on, and would require
  unsafe hand-written intrinsics or a algorithm change (breaking existing hashes/
  cache filenames) to obtain. Software SHA3-256 already hashes a 70MB payload in
  the tens-of-milliseconds range — below what's perceptible for a one-shot clipboard
  event — so the cost doesn't clear the bar for this project's actual workload.


