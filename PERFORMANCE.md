# PERFORMANCE — Baseline for the Zero-Copy Cycle (v0.7.1, extended in v0.7.2, v0.7.3 and v0.7.4)

This document records the quantitative baseline against which the zero-copy milestones (v0.7.2 - v0.8.0) are judged: what the ingestion path costs today in time, allocations and memory, which bounds hold, and which targets the next milestones aim at. Numbers are measurements, not claims; every figure below was produced by the harness described in section 1 on the machine in section 2.

---

## 1. Reproducing the Numbers

```bash
cargo bench -p y4p-daemon --bench baseline     # full measurement (about 80 seconds since v0.7.3)
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
| Code | Sections 3.1-3.4: v0.7.0 ingestion, SQLite, hashing and MIME code with the v0.7.1 bounded queue and reader gate (recorded 2026-10-14). Section 3.5 and the v0.7.2 re-check at its end: v0.7.2 payload abstraction (recorded 2026-10-15, three full runs). Section 3.6 and its re-check: v0.7.3 streaming ingestion (recorded 2026-10-15, three full runs). Section 3.7: v0.7.4 Part 1, descriptor-to-descriptor transcoding (recorded 2026-10-16, three full runs; ImageMagick 7.1.2-34) |
| CPU | AMD Ryzen 7 9700X (16 threads), cpufreq governor `powersave` |
| OS / toolchain | Linux 7.2.9-zen1, rustc 1.97.1 |
| Profile | `bench` inheriting `release`: `opt-level = "s"`, `lto = true`, `codegen-units = 1` (the shipped binary's settings; `"s"` favours size over speed) |
| Storage medium | **tmpfs** (`/tmp`): SQLite and cache-file costs exclude device latency and `fsync` to disk. Set `Y4P_BENCH_DIR` to a real disk to include them |
| Run-to-run spread | at most about 3 % on SHA3, SQLite and the single-thread queue cases; up to about 25 % on `normalize_uri_list` (short loops) and on cross-thread hand-off, where scheduler wake-ups dominate |

Limits of the method: the counting allocator sees Rust allocations only. SQLite and ImageMagick allocate through C `malloc` and are not counted, so the SQLite rows understate total allocations. Until v0.7.3 the image conversion itself (process spawn plus ImageMagick) was not in this harness; it had been measured separately when the PNG pipeline was introduced (1080p, zlib level 1: about 65 ms for a flat screenshot, about 110-140 ms for photo-like content, about 100 ms for a JPEG source; [`docs/06_local_image_pipeline.md`](docs/06_local_image_pipeline.md) section 2.1) and dominates every image ingest by two to three orders of magnitude. Since v0.7.4 the `transcode` group (section 3.7) runs it, on *incompressible noise* (the worst case for `magick`), so its absolute times are not those of a screenshot; what it compares is the plumbing around a conversion, not the conversion. ImageMagick's own allocations are still invisible to the counting allocator.

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

### 3.5 Payload abstraction (v0.7.2)

`core::payload` ([`crates/core/src/payload.rs`](crates/core/src/payload.rs)) is the shared vocabulary for a payload's bytes: `Payload::{Memory, Stream, File}`, a `PayloadReader` enum that implements `Read`, `route_payload`, and `read_bounded`. Three full runs, spread under 2 % on every row below.

| Case | Median | Allocations |
| :--- | ---: | :--- |
| `route_payload`, length hint | 0.5 ns | 0 |
| `route_payload`, no hint, text-like MIME | 9.3 ns | 0 |
| `route_payload`, no hint, binary MIME | 7.2 ns | 0 |
| reader built from a `Vec` + read 4 KiB, **`PayloadReader` enum** | 15.7 ns | **0** |
| the same through **`Box<dyn Read>`** (what the enum replaces) | 31.7 ns | 1 call, 16 B |
| `Payload::Memory` `read_bounded`, 4 KiB (hand-over) | 4.9 ns | 0 |
| `read_bounded`, 64 B, no hint / 4 KiB hint | 92 / 32.6 ns | 3 / **1** calls |
| `read_bounded`, 300 B, no hint / 4 KiB hint | 137 / 33.0 ns | 5 / **1** calls |
| `read_bounded`, 4 KiB, no hint / 4 KiB hint | 251 / 48.8 ns | 9 / **1** calls |
| pipe stream `read_bounded`, 4 KiB (incl. creating the pipe) | 1.03 us | 1 call |
| pipe stream `read_bounded`, 32 KiB (incl. creating the pipe) | 2.36 us | 1 call |

- **The dispatch is free**: the enum builds and reads with zero allocations and in half the time of the boxed reader; routing costs one comparison (0.5 ns) with a hint and one allocation-free MIME scan (7-9 ns) without.
- **An in-memory payload is never copied on its way to the worker**: `read_bounded` over `Payload::Memory` validates and returns the same `Vec` (4.9 ns, 0 allocations, and a unit test asserts pointer equality).
- **Pre-sizing the read buffer is the measurable win.** A Wayland pipe carries no length, so `device.rs` takes the hint from the MIME alone: a text-like type reserves one page (`INLINE_PAYLOAD_THRESHOLD_BYTES`) up front. That replaces the read loop's regrowth (3, 5 and 9 allocations for 64 B, 300 B and 4 KiB) by exactly one, and is 2.8-5.1x faster on the read itself. Non-text types start empty and grow, because they may be large.
- The pipe rows are the **syscall floor** for a streamed payload (pipe creation, write, close, read to EOF). They are the figure the v0.7.3 `splice(2)` path has to be judged against, not a regression.

**Re-check of sections 3.1-3.4 at v0.7.2** (same machine, three runs): SHA3, MIME, reader gate and SQLite rows are unchanged within the noise floor; allocation counts are identical everywhere; the memory-bound experiment still reports **64 queued, 64 shed, 65.0 MiB**. One measurable cost: the single-thread queue cases are 3-7 % slower (77-79 ns against 72 ns for 64 B, 95 against 92 ns for 4 KiB, 86 against 82 ns for a shed). `ClipboardJob` now holds a `Payload` enum (56 bytes) instead of a bare `Vec` (48 bytes), so each move through the channel is eight bytes larger and the job is constructed through a wrapper; the allocation count (2 calls, 74 B) is unchanged. About 5 ns per clipboard event is far below anything observable and buys a queue that can carry a descriptor or a file.

### 3.6 Streaming ingestion (v0.7.3)

`core::splice::copy_to_file` moves a pipe into a file with `splice(2)`, optionally `tee(2)`-ing the stream so a SHA3-256 digest is computed on the way; `FileSpooler` and the worker's `store_spooled` build on it ([`docs/01`](docs/01_wayland_streaming_io.md) section 2.4). Three full runs (the `splice` and `ingest` groups run last), spread under 2 % on every row quoted. Each operation creates a pipe, a feeding thread (above 32 KiB), and a fresh destination file, and unlinks it.

**Pipe to file** (`splice` group; the digest is SHA3-256 in every row that has one):

| Payload | Road | Median | Heap bytes / op | Heap calls |
| :--- | :--- | ---: | ---: | ---: |
| 1 MiB | `Vec` + SHA3 + write (the v0.7.2 road) | 6.60 ms | 4,194,523 | 24 |
| 1 MiB | buffered copy + SHA3, 64 KiB stack buffer (the fallback) | 6.51 ms | 251 | 7 |
| 1 MiB | `splice` only, no digest | **0.19 ms** (5.2 GiB/s) | 251 | 7 |
| 1 MiB | `splice` + `tee` + SHA3 (the v0.7.3 road) | 6.52 ms | **251** | 7 |
| 16 MiB | `Vec` + SHA3 + write (the v0.7.2 road) | 109.6 ms | 67,109,083 | 28 |
| 16 MiB | `splice` only | **4.0 ms** (3.9 GiB/s) | 251 | 7 |
| 16 MiB | `splice` + `tee` + SHA3 (the v0.7.3 road) | 105.2 ms | **251** | 7 |

(The 251 bytes and 7 calls are the feeding thread and bookkeeping of the benchmark itself, constant in the payload size. The 32 KiB rows are the same story: 131,135 heap bytes for the `Vec` road against 123.)

**Pipe to database, end to end** (`ingest` group; a real `DbWorker`, a real database and cache directory; every payload unique, GIF so the image pipeline does not touch it):

| Payload | Road | Median | Heap bytes / op | Peak heap growth |
| :--- | :--- | ---: | ---: | ---: |
| 1 MiB | `Vec` in the ingestion thread (v0.7.2) | 6.89 ms | 4,195,072 | 2.00 MiB |
| 1 MiB | spooled: `splice` + adopt (v0.7.3) | 6.82 ms | **6,477** | **0.00 MiB** |
| 16 MiB | `Vec` in the ingestion thread (v0.7.2) | 111.5 ms | 67,110,011 | 32.00 MiB |
| 16 MiB | spooled: `splice` + adopt (v0.7.3) | **105.7 ms** | **6,477** | **0.00 MiB** |

**Fingerprint encoding** (`sha3` group, 64 B payload): `to_hex` **0.90 us, 1 allocation** against the previous per-byte `format!` **2.03 us, 35 allocations**: the worker's hex encoder was replaced (the digest itself is unchanged).

**Queue memory bound with spooled payloads**: offered 128 x 1 MiB into a queue of 64 with zero patience: **64 queued, 64 shed, peak heap growth 0.010 MiB** (about 160 bytes per queued job), and exactly 64 spool files on disk while queued and 0 once the queue is dropped. The in-memory experiment of section 4.1 still reports **65.0 MiB**.

Reading the numbers:

- **The kernel does the copying 26-34 times faster than SHA3 can hash** (5.2 GiB/s against 154 MiB/s on 1 MiB, 3.9 GiB/s on 16 MiB). With a digest in the loop, wall-clock time is the hash's: `splice` + `tee` + SHA3 is within 1 % of the buffered copy and 1.3-4 % faster than the `Vec` road. This is exactly what section 5 predicted, and it fixes what the milestone buys: **memory, not speed**. The heap cost of a 16 MiB payload falls from 64 MiB allocated (32 MiB peak, the doubling `Vec`) to a constant 251 bytes.
- **`tee` costs nothing measurable here**: the two SHA3 rows with and without `tee` (kernel path against user-space buffered path) differ by less than 1 % because the stack-buffer copy (about 10 GB/s) is invisible next to 154 MiB/s of hashing. `tee` is kept because it leaves the *destination* write entirely in the kernel and is free when no digest is wanted (`NoTap` creates no scratch pipe): `splice` only is 34x faster than the `Vec` road at 1 MiB.
- **Hashing moves off the single worker thread.** The digest is computed by the ingestion threads (up to 8, in parallel) while the data streams; the worker no longer hashes a spooled payload at all (it only maps the file and renames it). The end-to-end table measures one payload at a time, so it cannot show this effect; it shows that the extra machinery (file create, rename, mapping) costs less than the allocation it replaces (6.82 against 6.89 ms at 1 MiB).
- **A small payload pays nothing**: one 4 KiB head read decides, and a stream that ends inside it never creates a file.

**Re-check of sections 3.1-3.5 at v0.7.3** (same machine, three runs): SHA3 digest, MIME, reader gate, payload and SQLite rows are unchanged within the noise floor, and every allocation count is identical. Single-thread queue rows: 77.7 ns (64 B), 96.5-97.0 ns (4 KiB), 86.4-87.2 ns (shed), against 77-79, 95 and 86 at v0.7.2: unchanged (`Payload` is still 32 bytes; a test asserts it, so the boxed `Spooled` variant widens no queue slot). A first ordering of the harness, with the new groups *before* the queue group, measured the 4 KiB enqueue at 108-110 ns: the new groups start threads and push tens of megabytes through the allocator, which changes the allocator state later rows run in. They now run last, and that row is back to 97 ns, which is why the harness comment says so. The cross-thread `producer/consumer` rows vary between 4 and 16 us per message across builds *and* orders (v0.7.2 itself: 7.1-9.0 us at 64 B); they are scheduling-bound, sit outside the noise-floor claim of section 5, and no conclusion is drawn from them.

### 3.7 Descriptor-to-descriptor transcoding (v0.7.4 Part 1)

A spooled raster image the converter would rewrite used to be read back into a `Vec`, fed to `magick` through the chunk-buffered relay, collected in a second `Vec` and hashed afterwards. Now `spool::transcode_spooled` hands the spool file to `magick` as its stdin and splices `magick`'s stdout into a new spool file while `tee(2)` feeds SHA3-256 ([`docs/06`](docs/06_local_image_pipeline.md) section 6.4). The `transcode` group runs both from the same spool file and ends both with the SHA3-256 of the converted bytes (the harness first checks that the two roads produce the *same digest*, and fails the run if not). Three full runs; the images are incompressible noise, so the PNG is as large as its pixels.

| Image (PNG -> PNG) | Road | Median (3 runs) | Heap bytes / op | Heap calls | Peak heap growth |
| :--- | :--- | ---: | ---: | ---: | ---: |
| 769 KiB | read into `Vec` + buffered relay + SHA3 (v0.7.3) | 23.2-23.4 ms | 3,290,706 | 92 | 2.32 MiB |
| 769 KiB | spool file -> spool file, descriptors + `splice` + `tee` + SHA3 (v0.7.4) | 17.9-18.9 ms | **5,778** | 85 | **0.00 MiB** |
| 12 MiB | read into `Vec` + buffered relay + SHA3 (v0.7.3) | 300.2-305.3 ms | 50,540,594 | 92 | 36.12 MiB |
| 12 MiB | spool file -> spool file, descriptors + `splice` + `tee` + SHA3 (v0.7.4) | **214.6-216.9 ms** | **5,781** | 85 | **0.00 MiB** |

The spread is under 2 % on every row except the 769 KiB descriptor row (5 %). The remaining 5.8 KB and 85 calls are the process spawn (argument vector, environment, scratch directory, the stderr and watchdog threads), the same on both roads bar the old road's feeder thread and 64 KiB chunk buffers; the 50.5 MB the old road allocated at 12 MiB (input, output and its doubling) is gone.

Reading the numbers:

- **Heap: from the image's size to a constant.** The old road held the input (12 MiB), the output (13 MiB, plus doubling) and the hash input at once: 36 MiB peak for a 12 MiB image, 50.5 MB allocated per conversion. The new road holds none of it: the input is the child's own descriptor, the output goes from the pipe into the file inside the kernel, and the digest reads the `tee`d duplicate through a 64 KiB stack buffer.
- **Time: the conversion's own floor.** Measured separately in a shell (`magick` with this milestone's arguments, output to `/dev/null`, 7 runs each, medians): **215-217 ms with a file on stdin and 215-216 ms with a pipe** (two passes each) on the 12 MiB image. The descriptor road's 215-217 ms is that floor: the SHA3 pass and the copies add nothing to the wall clock. The old road took about 85 ms more, which is what hashing 13.3 MB costs at the 155 MiB/s of section 3.1 (about 82 ms) done *after* the conversion instead of during it (215 + 82 = 297 against the measured 300-305 ms). That is consistent with the explanation; the harness does not isolate it further. It also means the gain is not "a seekable stdin is faster for `magick`": the two stdin kinds cost the same here.
- **Smaller images gain less in absolute terms** (about 5 ms of 23 at 769 KiB): the process spawn and ImageMagick's start-up are the same on both roads and dominate.
- **A screenshot-sized conversion (tens of milliseconds of `magick`) is dominated by `magick`**, as section 2 says; this group shows what the plumbing around it costs and that on the new road the plumbing costs nothing in memory and no wall-clock time.

**A defect found on the way, and fixed.** The harness's digest check failed on the first full run: converting the *same* image twice produced different bytes. ImageMagick's PNG encoder stamps its output with a `tIME` chunk holding the *current time* (to the second), even after `-strip` (offset 47 of the file, the seconds byte, differed in every case). The same image copied a second apart therefore had a different SHA3-256 and was not recognised as a duplicate, on the in-memory road too (`memory == memory again: false` in a probe), contrary to the "deterministic conversion" the dedupe relies on. Three conversions of one noise image 1.1 s apart gave three different file hashes; with `-define png:exclude-chunk=tIME` they gave one, and the decoded pixel hash was identical. The argument is now part of every PNG conversion (`transcode.rs`), pinned by three tests that fail without it (one converts the same image either side of a second boundary). Stored images no longer carry the conversion time; copies made before this version keep their old digest and will be stored once more if copied again.

**Re-check of sections 3.1-3.6 at v0.7.4** (four full runs, the new group last in the order): single-thread queue rows 77.8-78.3 ns (64 B), 96.8-98.8 ns (4 KiB) and 86.4-88.7 ns (shed), against 77.7, 96.5-97.0 and 86.4-87.2 at v0.7.3: within the 3 % noise floor; the splice and ingest rows of section 3.6 reproduce (the memory bound with spooled payloads again 64 queued, 64 shed, 0.011 MiB). The cross-thread producer/consumer rows swing 5.9-7.5 us (64 B) and 15.2-19.9 us (4 KiB) between runs, as they did at v0.7.2 and v0.7.3: no conclusion is drawn from them. A full run takes 79 s.

---

## 4. Memory Bounds

### 4.1 Measured

| Experiment | Result |
| :--- | :--- |
| Offer 128 x 1 MiB to a queue of capacity 64 with zero patience | **64 queued, 64 shed**; peak heap growth **65.0 MiB** (64 queued + the one payload in flight). The surplus is dropped, not held |
| The same, with spooled payloads (v0.7.3) | **64 queued, 64 shed**; peak heap growth **0.010 MiB**; 64 spool files while queued, 0 afterwards (shed jobs unlink theirs) |
| Convert one spooled 12 MiB raster image (v0.7.4) | peak heap growth **0.00 MiB**, against **36.12 MiB** when it is read back into memory first (v0.7.3); section 3.7 |

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

Since v0.7.3 that ceiling is a *heap* ceiling only for text, small payloads and the in-memory fallback. A large non-text payload is spooled to disk and queued as a path and a digest: the same product (72 x 256 MiB) then bounds **disk** space in the cache directory's `spool/`, and the heap stays flat (section 3.6). Disk is the cheaper resource, but the bound is as loose as the heap one was; a spool byte budget belongs with section 6, item 1. Since v0.7.4 the worker converting a spooled image briefly holds a second file besides the queued ones (the converted output, bounded by the same `MAX_PAYLOAD_BYTES`; the original is unlinked once it is in place), so the ceiling is (64 + 8 + 1) x 256 MiB. The converter's own scratch directory is separate and bounded by `-limit disk 1GiB`.

File descriptors: at most 8 ingestion readers at any time, independent of how fast selections change. A reader that spools holds four (pipe read end, spool file, the two ends of the `tee` scratch pipe), so at most 32; a queued job holds none.

### 4.3 Payload copies on the current ingest path

By inspection of the code (not measured):

| Step | Userspace copy of the payload bytes |
| :--- | :--- |
| 1. compositor pipe to `Vec<u8>` (`read_to_end`): text, payloads under one page, and the fallback | 1; for text-like MIMEs the buffer is pre-sized to one page (v0.7.2), so a typical payload is read with no reallocation copy (section 3.5) |
| 1'. compositor pipe to spool file: large non-text payloads (v0.7.3) | 0 into the heap: `splice(2)` into the file; the digest reads the `tee`d duplicate through a 64 KiB **stack** buffer (the one copy SHA3 itself needs) |
| 2. `ClipboardJob` through the queue, and the worker's `into_bytes` | 0 (moved; `Payload::Memory` is handed over without a copy; a `Spooled` job is a path) |
| 3. text and unsupported types: hash, then insert | 0 (hashed in place; SQLite takes a borrowed slice) |
| 3'. spooled, not transcoded: persist (v0.7.3) | 0 (digest already known; the file is mapped for the storage layer, and an image is renamed into the cache: same inode) |
| 4. raster image under the spool threshold (a few KiB): `Vec` to `magick` stdin, `magick` stdout to a new `Vec` | 2 (pipe relays) |
| 4'. spooled raster image (v0.7.4): spool file to `magick` stdin, `magick` stdout to a new spool file | 0 (the file is the child's stdin; `splice(2)` into the new file; the digest reads the `tee`d duplicate through the stack buffer) |
| 4''. converted spooled image: persist (v0.7.4) | 0 (as 3': digest known, file mapped, renamed into the cache) |
| 5. egress (paste): cached image via `sendfile(2)` | 0 (kernel to kernel) |

Ingestion now has no heap copy for a large payload whether or not the converter rewrites it, one for small payloads and text, and two more for a small raster image; egress has none. The remaining per-payload costs of a spooled one are the mapping and the mirrored layout rule, both removed by v0.7.4 Part 2 (section 6).

---

## 5. Targets for the Zero-Copy Milestones

Proposals derived from the baseline above, to be agreed per milestone. "Baseline" is the value in this document; a target is met only by the same harness on the same machine.

| Metric | Baseline | Target | Milestone |
| :--- | :--- | :--- | :--- |
| Allocations per fingerprint | 35 | <= 1 (the result `String`) | **v0.7.3: met** (1; section 3.6) |
| Fingerprint of a 64 B payload | 2.04 us | <= 1.0 us | **v0.7.3: met** (0.90 us) |
| Peak payload buffer per in-flight reader | whole payload (up to 256 MiB) | <= 64 KiB chunk for streamed payloads | **v0.7.3: met** for spooled payloads (64 KiB stack buffer, 0 bytes of heap) |
| Userspace copies, large payload that bypasses the converter | 1 | 0 (`splice(2)` into the cache file, hash streamed alongside) | **v0.7.3: met** (section 4.3) |
| Userspace copies, raster image | 3 | <= 1 (descriptor to `magick`) | **v0.7.4 Part 1: met** for spooled images (0; section 4.3), and time falls to the converter's own floor (section 3.7); a raster under the 4 KiB spool threshold keeps the in-memory road |
| Queue memory bound | 64 jobs, any size | byte budget (for example 64 MiB) in addition to the count | v0.7.5 - v0.7.8 (hardening; for spooled payloads the budget is disk, see section 4.2) |
| SHA3 throughput | 158 MiB/s | no regression; overlap with the transfer so that hashing no longer serialises behind it | **v0.7.3: met in the sense that matters**: no regression (154-159 MiB/s), and hashing runs on the ingestion threads in parallel instead of the single worker; a single stream is still hash-bound (section 3.6) |
| Allocations to read a small text payload from a pipe | 3-9 | 1 | **v0.7.2: met** (section 3.5) |
| Existing rows above | as measured | no regression beyond the 3 % noise floor (25 % for short uri-list and cross-thread cases) | every milestone |

One consequence to plan around: because SHA3-256 costs about 6.3 ms per MiB against about 0.1-0.5 ms for the writes, removing a memcpy saves little wall-clock time for large payloads by itself. The zero-copy gain is chiefly **memory** (no whole-payload buffer) and **latency hiding** (hash while receiving), not raw copy cost. The SHA3 digest is also what deduplication keys on, so replacing it is a storage-format decision and out of scope for the harness.

---

## 6. Open Questions Carried Forward

1. **Byte budget for the queue.** `INGEST_QUEUE_CAPACITY` bounds the count only, and since v0.7.3 the products in section 4.2 bound disk (the spool directory) rather than heap for large payloads. A byte budget (an atomic counter of queued bytes) has what it needs: `Payload::len_hint` reports the size without I/O, and a `SpooledFile` knows its length. It is not implemented.
2. **Hash cost.** At 154-159 MiB/s with `opt-level = "s"`, hashing dominates large payloads, and v0.7.3 shows it is the *only* thing that does (the kernel copy is 26-34 times faster). Overlapping it with the transfer is done; the remaining lever is a per-package `opt-level = 3` override for the Keccak crates in `[profile.release.package]`, which adds no crate but changes release-profile settings and therefore needs approval; the second has not been measured.
3. **Disk-backed numbers.** The recorded run is on tmpfs. A run with `Y4P_BENCH_DIR` on the real data disk should be added once, because `fsync` behaviour changes the SQLite rows by orders of magnitude.
4. **The cost left on a spooled payload.** `MappedFile` (the only `mmap`/`munmap` `unsafe` in the tree) exists because the frozen storage layer takes a `&[u8]`, and `core::cache_file_path` / `worker::stored_in_cache_file` mirror its private layout rule. A storage entry point that takes a file (v0.7.4 Part 2, under a lifted storage invariant) removes both; it is not measured here because nothing in this harness would change by it except the page-cache mapping.
5. **Two splice primitives.** `image::pipeline::{try_splice, splice_all}` (bare, tap-less, tested, unused by production code) and `core::splice` (bounded, fingerprinting, with the buffered fallback) overlap. Folding the first into the second is a refactoring for a `.N.5`-`.N.8` release, not a feature.
6. **Realistic images.** Section 3.7 uses incompressible noise to make the byte volume the label says. A screenshot-like image spends proportionally more of its time in `magick`'s encoder and less in the plumbing; the memory result is independent of content, the time result is not.
