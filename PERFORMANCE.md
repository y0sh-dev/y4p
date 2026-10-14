# PERFORMANCE — v0.7.1 Baseline for the Zero-Copy Cycle

This document records the quantitative baseline against which the zero-copy milestones (v0.7.2 - v0.8.0) are judged: what the ingestion path costs today in time, allocations and memory, which bounds hold, and which targets the next milestones aim at. Numbers are measurements, not claims; every figure below was produced by the harness described in section 1 on the machine in section 2.

---

## 1. Reproducing the Numbers

```bash
cargo bench -p y4p-daemon --bench baseline     # full measurement (well under a minute)
cargo test  --workspace --all-targets          # runs the same harness in smoke mode: one pass, no timing
```

The harness ([`crates/daemon/benches/baseline.rs`](crates/daemon/benches/baseline.rs)) uses `std` only (`harness = false`); no benchmarking crate is added to the workspace.

| Property | Behaviour |
| :--- | :--- |
| Timing | Calibrated tight loop (pass >= 40 ms), 9 rounds, **median** and minimum reported; `black_box` around every result |
| Allocations | A counting `GlobalAlloc` wraps `System`; per-operation calls and bytes come from one isolated call (micro cases) or the first round (batched cases) |
| State | Stateful cases (SQLite, queue) reset their state before **every** round, and assert the resulting row count so a benchmark cannot silently measure the wrong path |
| Fixtures | Payload and hash buffers are reused, so building a unique row never allocates inside a timed region: allocation columns describe the subject, not the harness |
| Isolation | The binary re-executes itself with `XDG_*` pointing into a fresh private directory (`$Y4P_BENCH_DIR`, default the system temp directory), refuses to run unless the database path lies inside it, and deletes it afterwards. The real clipboard history is never opened |

---

## 2. Environment of the Recorded Run

| Item | Value |
| :--- | :--- |
| Date | 2026-10-14 |
| Code | v0.7.0 ingestion, SQLite, hashing and MIME code unchanged; v0.7.1 bounded queue and reader gate |
| CPU | AMD Ryzen 7 9700X (16 threads), cpufreq governor `powersave` |
| OS / toolchain | Linux 7.2.9-zen1, rustc 1.97.1 |
| Profile | `bench` inheriting `release`: `opt-level = "s"`, `lto = true`, `codegen-units = 1` (the shipped binary's settings; `"s"` favours size over speed) |
| Storage medium | **tmpfs** (`/tmp`): SQLite and cache-file costs exclude device latency and `fsync` to disk. Set `Y4P_BENCH_DIR` to a real disk to include them |
| Run-to-run spread | at most about 3 % on SHA3, SQLite and the single-thread queue cases; up to about 25 % on `normalize_uri_list` (short loops) and on cross-thread hand-off, where scheduler wake-ups dominate |

Limits of the method: the counting allocator sees Rust allocations only. SQLite and ImageMagick allocate through C `malloc` and are not counted, so the SQLite rows understate total allocations. The image conversion itself (process spawn plus ImageMagick) is not in this harness; it was measured separately when the PNG pipeline was introduced (1080p, zlib level 1: about 65 ms for a flat screenshot, about 110-140 ms for photo-like content, about 100 ms for a JPEG source; [`docs/06_local_image_pipeline.md`](docs/06_local_image_pipeline.md) section 2.1) and dominates every image ingest by two to three orders of magnitude.

---

## 3. Baseline Results

### 3.1 SHA3-256 fingerprinting

`digest only` is the hash; `fingerprint (hex)` is what the worker does (`worker.rs`: digest, then lowercase hex through `format!("{:02x}")` per byte).

| Payload | digest only | fingerprint (hex) | Throughput | Allocations (hex) |
| ---: | ---: | ---: | ---: | :--- |
| 64 B | 0.86 us | 2.04 us | 71 / 30 MiB/s | 35 calls, 368 B |
| 4 KiB | 25.6 us | 26.7 us | 152 / 146 MiB/s | 35 calls, 368 B |
| 64 KiB | 395 us | 399 us | 158 / 157 MiB/s | 35 calls, 368 B |
| 1 MiB | 6.31 ms | 6.34 ms | 158 / 158 MiB/s | 35 calls, 368 B |

- **Hashing is the dominant per-byte cost of ingestion**: about 6.3 ms per MiB, against about 0.12 ms for writing the same MiB to a cache file and about 0.45 ms into a BLOB. A maximal 256 MiB payload spends about 1.6 s in SHA3 alone on this machine (with the shipped `opt-level = "s"`).
- The hex conversion costs a fixed 35 allocations and about 1.2 us, visible only for tiny payloads (2.04 us against 0.86 us at 64 B); the digest itself never allocates.

### 3.2 MIME normalisation and URI-list parsing

| Case | Median | Allocations |
| :--- | ---: | :--- |
| `mime_base_eq` (parameters + case) | 13 ns | 0 |
| `is_html_mime` | 10 ns | 0 |
| `is_text_like_mime` | 64 ns | 0 |
| `detect_image_mime`, PNG header | 1.1 ns | 0 |
| `detect_image_mime`, non-image 512 B | 1.5 ns | 0 |
| `normalize_uri_list`, 1 path (53 B) | 194 ns | 4 calls, 165 B |
| `normalize_uri_list`, 100 paths (5 KiB) | 16.5 us | 301 calls, 16.5 KB |
| `normalize_uri_list`, 1000 paths (51 KiB) | 177 us | 3001 calls, 165 KB |

Every classification primitive is allocation-free and negligible. `normalize_uri_list` allocates three times per path (each line is percent-decoded into an intermediate `Vec` that grows in steps, then copied into the output) and runs at about 260-310 MiB/s: irrelevant at clipboard rates, but the only normalisation step that scales with the payload.

### 3.3 Bounded queue, load shedding and the reader gate

| Case | Median | Allocations |
| :--- | ---: | :--- |
| enqueue + dequeue, 64 B job (one thread) | 72 ns | 2 calls, 74 B |
| enqueue + dequeue, 4 KiB job (one thread) | 92 ns | 2 calls, 4106 B |
| shed on a full queue (zero patience) | 82 ns | 2 calls, 74 B |
| reader gate acquire + release | 14.6 ns | 0 |
| producer to consumer thread, 64 B job | 18.6 us per message | 2 calls |
| producer to consumer thread, 4 KiB job | 15.8 us per message | 2 calls |

- The two allocations are the **job itself** (`mime` `String` and `data` `Vec`), built inside the timed region; the channel allocates nothing per message and the payload is **moved, never copied** (92 ns for 4 KiB against 72 ns for 64 B is bookkeeping, not a memcpy).
- Cross-thread hand-off costs 8-23 us per message: it is dominated by the consumer's futex wake-up and, under the `powersave` governor, by frequency ramp-up, not by the queue. At clipboard rates (a handful of copies per second) it is immaterial.
- Refusing work is as cheap as accepting it (82 ns), so overload cannot become a CPU problem.

### 3.4 SQLite (`y4p-storage`, WAL, tmpfs)

| Case | Median | Allocations (Rust only) |
| :--- | ---: | :--- |
| insert 64 B text, no rotation (table growing to 2000 rows) | 41.0 us | 5 calls, 274 B |
| insert 64 B text + rotation (table held at 256 rows) | 24.8 us | 12 calls, 848 B |
| duplicate hit (same hash) | 11.7 us | 0 |
| `fetch_metadata(100)` over 1000 rows | 36.6 us | 206 calls, 25 KB |
| `fetch_metadata(1000)` over 1000 rows | 297 us | 2009 calls, 217 KB |
| `search_metadata` (1 keyword, 100) over 1000 rows | 102 us | 216 calls, 28 KB |
| insert 1 MiB BLOB (`application/octet-stream`) | 452 us (2.2 GiB/s) | 0 |
| insert 1 MiB image (cache file) | 123 us (8.2 GiB/s) | 5 calls, 409 B |

- A duplicate costs 11.7 us against 25-41 us for a fresh insert: deduplication is cheap, and hashing first is the right order.
- A fresh insert into the growing table is slower than one into the rotating table (41 against 25 us): index and WAL growth, not rotation, are the expensive part at this scale.
- Neither large insert allocates payload-sized memory on the Rust side: the BLOB is bound by reference, and the cache file is written from the borrowed slice.
- Metadata scans cost 0.3-0.4 us and 2 allocations per row returned (the row's strings); a 100-row list is 37 us.

---

## 4. Memory Bounds

### 4.1 Measured

| Experiment | Result |
| :--- | :--- |
| Offer 128 x 1 MiB to a queue of capacity 64 with zero patience | **64 queued, 64 shed**; peak heap growth **65.0 MiB** (64 queued + the one payload in flight). The surplus is dropped, not held |

### 4.2 Worst case, by construction

The bounds are **counts**, not byte budgets; the worst-case resident payload memory is therefore a product, reached only if every slot holds a maximal payload:

| Quantity | Constant | Value | Bound |
| :--- | :--- | ---: | :--- |
| Single payload | `MAX_PAYLOAD_BYTES` | 256 MiB | read with a +1 probe byte, oversized streams discarded |
| Queued payloads | `INGEST_QUEUE_CAPACITY` | 64 | `sync_channel` capacity; the worker holds one more while it processes |
| Concurrent readers | `INGEST_MAX_CONCURRENT_READERS` | 8 | one thread, one pipe read end and one buffer each; checked **before** the pipe is created |
| Queue patience | `INGEST_ENQUEUE_WAIT_MS` | 2000 ms | a reader keeps its gate slot while waiting, then sheds |
| Image conversion | `IMAGE_CONCURRENCY_LIMIT` | 2 | plus the converter's own `-limit memory 256MiB` |

Theoretical ceiling: (64 + 8 + 1) x 256 MiB. In practice clipboard payloads are kilobytes (text) to a few megabytes (screenshots), where the same bounds mean tens of megabytes. **The count bound is not a byte bound**; section 6 names the follow-up.

File descriptors: at most 8 pipe read ends from ingestion at any time, independent of how fast selections change.

### 4.3 Payload copies on the current ingest path

By inspection of the code (not measured):

| Step | Userspace copy of the payload bytes |
| :--- | :--- |
| 1. compositor pipe to `Vec<u8>` (`read_to_end`, grows by doubling) | 1, plus reallocation copies while it grows |
| 2. `ClipboardJob` through the queue | 0 (moved) |
| 3. text and unsupported types: hash, then insert | 0 (hashed in place; SQLite and the cache file take a borrowed slice) |
| 4. raster image: `Vec` to `magick` stdin, `magick` stdout to a new `Vec` | 2 (pipe relays) |
| 5. egress (paste): cached image via `sendfile(2)` | 0 (kernel to kernel) |

Ingestion has one unavoidable userspace copy for every payload and two more for images; egress already has none. That asymmetry is what v0.7.2 - v0.7.4 address.

---

## 5. Targets for the Zero-Copy Milestones

Proposals derived from the baseline above, to be agreed per milestone. "Baseline" is the value in this document; a target is met only by the same harness on the same machine.

| Metric | Baseline | Target | Milestone |
| :--- | :--- | :--- | :--- |
| Allocations per fingerprint | 35 | <= 1 (the result `String`) | v0.7.2 |
| Fingerprint of a 64 B payload | 2.04 us | <= 1.0 us | v0.7.2 |
| Peak payload buffer per in-flight reader | whole payload (up to 256 MiB) | <= 64 KiB chunk for streamed payloads | v0.7.3 |
| Userspace copies, large non-image payload | 1 | 0 (`splice(2)` into the cache file, hash streamed alongside) | v0.7.3 |
| Userspace copies, raster image | 3 | <= 1 (descriptor to `magick`) | v0.7.4 |
| Queue memory bound | 64 jobs, any size | byte budget (for example 64 MiB) in addition to the count | v0.7.2 |
| SHA3 throughput | 158 MiB/s | no regression; overlap with the transfer so that hashing no longer serialises behind it | v0.7.3 |
| Existing rows above | as measured | no regression beyond the 3 % noise floor (25 % for short uri-list and cross-thread cases) | every milestone |

One consequence to plan around: because SHA3-256 costs about 6.3 ms per MiB against about 0.1-0.5 ms for the writes, removing a memcpy saves little wall-clock time for large payloads by itself. The zero-copy gain is chiefly **memory** (no whole-payload buffer) and **latency hiding** (hash while receiving), not raw copy cost. The SHA3 digest is also what deduplication keys on, so replacing it is a storage-format decision and out of scope for the harness.

---

## 6. Open Questions Carried Forward

1. **Byte budget for the queue.** `INGEST_QUEUE_CAPACITY` bounds the count only. A byte budget (an atomic counter of queued bytes) fits naturally with the descriptor abstraction of v0.7.2, where the size is known before the payload is read.
2. **Hash cost.** At 158 MiB/s with `opt-level = "s"`, hashing dominates large payloads. The levers are hashing overlapped with the transfer (v0.7.3's streaming hash) and a per-package `opt-level = 3` override for the Keccak crates in `[profile.release.package]`, which adds no crate but changes release-profile settings and therefore needs approval; the second has not been measured.
3. **Disk-backed numbers.** The recorded run is on tmpfs. A run with `Y4P_BENCH_DIR` on the real data disk should be added once, because `fsync` behaviour changes the SQLite rows by orders of magnitude.
