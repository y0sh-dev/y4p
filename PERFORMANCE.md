# PERFORMANCE — Baseline for the Zero-Copy Cycle (v0.7.1, extended in v0.7.2 - v0.7.7)

This document records the quantitative baseline against which the zero-copy milestones (v0.7.2 - v0.8.0) are judged: what the ingestion path costs today in time, allocations and memory, which bounds hold, and which targets the next milestones aim at. Numbers are measurements, not claims; every figure below was produced by the harness described in section 1 on the machine in section 2.

---

## 1. Reproducing the Numbers

```bash
cargo bench -p y4p-daemon --bench baseline     # full measurement (about 2 minutes since v0.7.7)
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
| Context switches | The `pipe` group also reports context switches per operation: `getrusage(RUSAGE_SELF)`, voluntary plus involuntary, all threads, averaged over 20 extra calls of the same case |
| Smoke mode | `cargo test` runs every case once on an unoptimised build; since v0.7.7 it stops at 1 MiB payloads (the 16 MiB cases and the deduplication facts, which hash about 100 MiB, run in the full measurement only) |

---

## 2. Environment of the Recorded Run

| Item | Value |
| :--- | :--- |
| Date | 2026-10-14 |
| Code | Sections 3.1-3.4: v0.7.0 ingestion, SQLite, hashing and MIME code with the v0.7.1 bounded queue and reader gate (recorded 2026-10-14). Section 3.5 and the v0.7.2 re-check at its end: v0.7.2 payload abstraction (recorded 2026-10-15, three full runs). Section 3.6 and its re-check: v0.7.3 streaming ingestion (recorded 2026-10-15, three full runs). Section 3.7: v0.7.4 Part 1, descriptor-to-descriptor transcoding (recorded 2026-10-16, three full runs; ImageMagick 7.1.2-34). Sections 3.9 - 3.13: v0.7.5 (FastCDC), v0.7.6 (chunked storage) and v0.7.7 (pipe tuning), recorded 2026-10-21: three full runs plus a fourth after the harness was corrected, and a same-day control, the v0.7.5 tree (`7c9ff52`) built and run with the same payloads (the `chunked` group's cases and the old `baseline` harness) |
| CPU | AMD Ryzen 7 9700X (16 threads), cpufreq governor `powersave` |
| OS / toolchain | Linux 7.2.9-zen1, rustc 1.97.1 (sections 3.1 - 3.8); Linux 7.2.11-zen1, rustc 1.97.1 (sections 3.9 - 3.13) |
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
| insert 1 MiB BLOB (`application/octet-stream`) | 452 us (2.2 GiB/s) at v0.7.0; **6.8 ms since v0.7.6**, which chunks it (section 3.10) | 0 |
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

### 3.8 Direct file adoption and memory reclamation on wipe (v0.7.4 Part 2)

**Adoption.** The worker used to map a spooled file (`mmap`), rename image files into the cache itself and hand storage a `&[u8]`. Now `ClipboardDb::insert_spooled` takes the `SpooledFile`: an image is renamed into `<hash>.cache` by `FileCache::adopt` and its row is built from the length alone; anything else is read once into its row. Two full runs, the `ingest` group (a real `DbWorker`, database and cache directory; GIF so the converter does not touch it):

| Payload | Road | v0.7.3 (section 3.6) | v0.7.4 Part 2, run 1 / run 2 | Heap bytes / op |
| :--- | :--- | ---: | ---: | ---: |
| 1 MiB GIF | spooled, splice + adopt | 6.82 ms | 6.91 / 6.92 ms | 6,477 -> **5,994** |
| 16 MiB GIF | spooled, splice + adopt | 105.7 ms | 107.0 / 106.8 ms | 6,477 -> **5,994** |
| 1 MiB GIF | `Vec` in the ingestion thread (control, untouched code) | 6.89 ms | 6.96 / 6.93 ms | 4,195,072 (unchanged) |
| 16 MiB GIF | `Vec` in the ingestion thread (control, untouched code) | 111.5 ms | 112.4 / 112.7 ms | 67,110,011 (unchanged) |

The spooled rows read 1.0-1.4 % slower than the figures recorded at v0.7.3, and the control rows, whose code did not change, read 0.6-1.1 % slower on the same runs: the difference is the machine's drift between sessions, inside the 3 % noise floor, not a cost of the new path. What the path saves is not on the wall clock: the mapping's `mmap` and `munmap` calls, one `unsafe` pair (the tree's only one of that kind), and 483 heap bytes per operation. The old rows of sections 3.1-3.6 reproduce (queue 77.8-78.2 ns and 96.8-98.0 ns, shed 87.6-88.7 ns; v0.7.3: 77.7, 96.5-97.0, 86.4-87.2).

A non-image payload is now read into the heap once (one `Vec` of exactly its length) before SQLite copies it into the row; the mapping avoided that read, but rusqlite binds a blob with `SQLITE_TRANSIENT`, so SQLite copied from the mapping into its own buffer all the same, so the peak is one payload higher than it was for large inline blobs (a PDF, an archive). It is bounded by `MAX_PAYLOAD_BYTES` and by the recorded length. Reading it through an incremental `sqlite3_blob` write would remove the copy, but needs rusqlite's `blob` feature, a manifest change that is not made here (section 6).

**`wipe`.** After `VACUUM` the process kept its pages: SQLite's page cache and the allocator's free lists stay resident. A throwaway program (not part of the tree) inserted 40,000 text rows of about 2 KiB, listed and searched the whole history and then called `ClipboardDb::wipe`; resident set size from `/proc/self/statm`, release build, two runs of each variant (identical to 0.1 MiB):

| Variant | Before wipe | After wipe |
| :--- | ---: | ---: |
| `VACUUM` only (v0.7.3) | 233.5 MiB | 74.4 MiB |
| + `PRAGMA shrink_memory` | 233.5 MiB | 74.5 MiB |
| + `malloc_trim(0)` | 233.5 MiB | **4.2 MiB** |
| both (shipped) | 233.5 MiB | **4.2 MiB** |

At this scale the whole effect is the trim: freed small blocks in the middle of the heap stay resident until the allocator is asked to release them, and 70 MiB of them did. The pragma alone changes nothing measurable here (SQLite's default page cache is only about 2 MiB, below the run-to-run spread of the other figures); it stays because it is the documented way to release the connection's own memory and costs one statement. The trim is a hint: on a target whose C library has no `malloc_trim` it is a no-op, and the unit test (`core::memory`) frees 96 MiB of 4 KiB blocks and requires at least half of it back.

### 3.9 FastCDC chunking engine (v0.7.5)

`core::cdc::FastCdc` splits a slice into content-defined chunks (4 / 16 / 64 KiB minimum / average / maximum) and yields borrowed sub-slices. The `cdc` group walks the iterator and sums the chunk lengths over printable-ASCII prose. Three full runs, spread under 1 %.

| Payload | Walk (median) | Throughput | Chunks | Heap |
| ---: | ---: | ---: | ---: | :--- |
| 4 KiB | 5.9 ns | - | 1 | 0 |
| 64 KiB | 19.2-19.3 us | 3.23 GiB/s | 4 | 0 |
| 1 MiB | 301-302 us | 3.23 GiB/s | 56 | 0 |
| 16 MiB | 4.84 ms | 3.23 GiB/s | 893 (mean 18,787 B, min 4,131 B, max 49,487 B) | 0 |

- **Zero allocation at every size.** The 4 KiB case does not scan at all: a payload not above the minimum chunk is one chunk by construction, so the walk is a length comparison.
- **The engine is not the cost.** Chunking *and* hashing every chunk with SHA3-256 (what the storage write path does, minus SQL) takes 6.66-6.70 ms per MiB and 106.6-107.2 ms for 16 MiB; the chunking walk is 4.5 % of that, SHA3's 154-158 MiB/s is the rest. Section 3.10 shows what that means for an insert.
- **Deduplication after an edit** (deterministic, identical in every run): a 16 MiB original and an edited copy offer 33,554,433 bytes; a store keeps 16,796,246 when one byte is inserted mid-way (the copy adds **19,030 B, 0.113 %** of itself), 16,799,413 when it is inserted at the front (22,197 B, 0.132 %), 16,800,341 when 4 KiB is inserted mid-way (23,125 B, 0.138 %). Boundaries re-synchronise within a chunk or two, so an edit costs a few chunks wherever it falls. No chunk in the 16 MiB walk reached the 64 KiB maximum.

### 3.10 Chunked storage in SQLite (v0.7.6)

A payload above 4 KiB that is not cache class is split, each chunk is stored once under its SHA3-256 digest with a reference count, and a manifest lists a row's chunks in order. The `chunked` group inserts and reads such payloads (text, so the preview and the search path are real). The control is the **v0.7.5 tree run the same day** with the same payloads (inline storage in the row): the "v0.7.5" column. Three runs of each; ranges are min-max.

| Case | v0.7.5 (inline) | v0.7.7 (chunked) | Factor |
| :--- | ---: | ---: | ---: |
| insert 64 KiB text, all chunks new | 70.5-73.3 us | 484-495 us | 6.8x |
| insert 1 MiB text, all chunks new | 0.866-0.928 ms | 7.41-7.58 ms | 8.3x |
| insert 16 MiB text, all chunks new | 16.0-16.2 ms | 122.2-124.6 ms | 7.7x |
| insert 1 MiB text, edited copy of a stored one | 0.908-0.914 ms | 7.11-7.28 ms | 7.9x |
| insert 16 MiB text, edited copy of a stored one | 15.7-16.4 ms | 116.7-119.1 ms | 7.3x |
| reassemble 64 KiB (`get_content_by_id`) | 3.47-3.52 us | 7.43-7.61 us | 2.1x |
| reassemble 1 MiB | 38.1-38.3 us | 50.7-52.3 us | 1.35x |
| reassemble 16 MiB | 0.860-0.882 ms | 1.256-1.269 ms | 1.45x |
| `search_metadata`, 16 x 1 MiB text, no hit | 5.33-5.36 ms | 9.44-9.48 ms | 1.77x |
| `sqlite` group: insert 1 MiB BLOB (`application/octet-stream`) | 0.555-0.593 ms (452 us when recorded at v0.7.0) | 6.82-6.84 ms | about 11.7x |

Heap on the Rust side stays flat: an insert is 2 allocations and 200 bytes at every size (the payload is borrowed; digests live on the stack), and a reassembly allocates exactly the payload plus 74 bytes, one buffer, like the inline read it replaces.

Reading the numbers:

- **The price of chunking is the hash.** A fresh 1 MiB insert is 7.4 ms, of which 6.66 ms is chunking plus SHA3 per chunk (section 3.9) and about 0.7 ms is the SQL work, which is what the whole inline insert cost (0.87-0.93 ms). The write path became hash-bound, as the ingestion path already was (section 3.6), but now it hashes a payload **twice**: once for the row's fingerprint and once chunk by chunk. For a payload above 4 KiB that is not an image, insertion costs 7-12x more CPU than at v0.7.5, about 6.3 ms more per MiB. The insert runs on the daemon's storage worker, so a 16 MiB text occupies it for 122 ms instead of 16 ms. Section 6, item 7, names the levers; none is pulled here.
- **Deduplication saves space, not time.** An edited 1 MiB copy inserts 4 % faster than a fresh one (7.1 against 7.4 ms): it skips writing the shared chunks but still hashes every chunk to learn that they are shared. What it saves is storage: after a 16 MiB text and an edited copy the distinct chunks come to 16.80 MB instead of 33.55 MB (section 3.9), and the storage tests assert that an edit adds at most three maximal chunks.
- **The "1 MiB BLOB insert" row of section 3.4 changed meaning.** Its payload is uniform bytes with an 8-byte counter, so since v0.7.6 it is chunked: sixteen 64 KiB chunks, fifteen identical, all hashed. The 6.8 ms is hashing, not storage; the same-day v0.7.5 control (0.56 ms) is the like-for-like figure. The image row (cache file) is unaffected: 128.7-131.4 us.
- **Reads cost 1.35-1.45x** from the join and the per-chunk copy into one buffer; at 16 MiB that is still 12.6 GiB/s. **Deep search costs 1.77x** because a chunked body is concatenated in SQL before `LIKE` (a keyword may straddle a chunk boundary). It is linear in the chunked text, 1.7 GiB/s here.
- **Small rows pay a little too.** The same-day control against v0.7.7: rotation insert 25.0 -> 25.5 us (11 -> 12 allocations, 1,117 -> 1,325 bytes: the rotation query now also returns the row id and the chunked flag), duplicate hit 11.65 -> 12.1 us (+4 %), insert without rotation 41.4 -> 42.1 us (+1.5 %), `search_metadata(1 keyword, 100)` over 1000 rows 101.7 -> 110.4 us (+8.5 %, 216 -> 219 allocations: the search expression is a `CASE` with a correlated sub-select). `fetch_metadata` is unchanged.

### 3.11 Pipe buffer tuning (v0.7.7)

`core::pipe::grow` raises a pipe to 1 MiB with `fcntl(F_SETPIPE_SZ)` (the unprivileged ceiling, `/proc/sys/fs/pipe-max-size`; this machine granted all 1,048,576 bytes). It is applied to: the pipe handed to the compositor for an incoming selection (`make_pipe`), the destination pipe of a paste (`write_bytes` above 64 KiB, `transfer` for descriptor payloads), the stdin and stdout pipes of the image converter's child, and the scratch pipe `tee(2)` fills while a payload is spooled. `core::splice` moves up to 1 MiB per `tee`/`splice` round instead of 64 KiB and shows the tap the bytes 64 KiB at a time, so the fingerprint's stack buffer did not grow.

**Fallback, tested.** A descriptor that is not a pipe (`/dev/null`, a socket, a regular file, `-1`) returns `None` and changes nothing. `EPERM` (above `pipe-max-size`, or the user's pipe-page budget spent) and `ENOMEM` halve the request and retry, at most 16 attempts; any other error ends the attempt; a pipe already large enough is not touched, and one is never shrunk. A request of 256 MiB as an unprivileged user lands within a factor of two of the system limit. A pipe that stays at 64 KiB is moved exactly as before, in more rounds, and the splice tests also run on both sizes.

The `pipe` group moves the same payload through a pipe left at the kernel default and through one grown to 1 MiB, with a writer thread feeding it (or, for egress, `sendfile(2)` from a file into the pipe and a reader thread draining it in 64 KiB reads, like a paste target). Three full runs, ranges min-max:

| Case | 64 KiB pipe: time / context switches | 1 MiB pipe: time / context switches | Time |
| :--- | ---: | ---: | ---: |
| write -> `read(64 KiB)` loop, 1 MiB | 116-118 us / 31 | 105-107 us / 1.0-1.2 | -9 % |
| pipe -> file, `splice` only, 1 MiB | 202-206 us / 31 | 183-192 us / 1.0-1.2 | -8 % |
| pipe -> file, `splice` + `tee` + SHA3, 1 MiB | 6.52-6.63 ms / 16 | 6.59-6.73 ms / 1.0-2.5 | +1 % (noise) |
| egress `sendfile` -> pipe -> reader, 1 MiB | 59.4-60.6 us / 31 | 37.6-38.7 us / 1.0-1.1 | **-36 %** |
| write -> `read(64 KiB)` loop, 16 MiB | 2.08-2.15 ms / 512 | 1.77-1.86 ms / 120-123 | -15 % |
| pipe -> file, `splice` only, 16 MiB | 4.16-4.34 ms / 511-513 | 4.02-4.16 ms / 31-32 | -5 % |
| pipe -> file, `splice` + `tee` + SHA3, 16 MiB | 104.5-106.4 ms / 258-264 | 105.2-106.9 ms / 18-20 | 0 |
| egress `sendfile` -> pipe -> reader, 16 MiB | 825-862 us / 386-451 | 533-564 us / 2.1-2.2 | **-35 %** |

Reading the numbers:

- **Context switches fall 16-250x**: 31 to 1 per MiB moved, 512 to 31 for 16 MiB of splice, 451 to 2 for the egress. That is the quantity the milestone targets, and it follows from the capacity (a writer sleeps once per full pipe: 16 times per MiB at 64 KiB, once at 1 MiB).
- **Wall-clock gains are real but modest where the copy is cheap** (5-15 % for read loops and `splice`) and **large where the reader is the bottleneck** (35 % for the paste direction, where the file side no longer waits for the reader every 64 KiB).
- **Hash-bound paths do not move.** With a digest in the loop the time is SHA3's (section 3.6): 104.5-106.9 ms for 16 MiB either way. Tuning removes 240 context switches from that path and not one millisecond, which is the expected result, not a failure.
- **Control against drift.** The `splice only` rows of the old `splice` group, whose source pipes are not tuned, read 4.28 ms (16 MiB) against 4.19-4.25 ms for the same-day v0.7.5 tree: within 2 %, so the larger splice round costs nothing on a pipe that cannot hold it. (The 4.0 ms recorded on 2026-10-15 is a different session.)
- **What this does not show.** The reader in the harness is a thread of the same process; a real paste target (`wl-paste`, a terminal) and a real compositor were not measured. A pipe's page buffers are allocated as it fills, not when it is enlarged; at most 8 ingestion readers and the egress threads hold such pipes at once.

### 3.12 Evolution, v0.7.0 to v0.7.7

Figures are the medians recorded in the sections named; "-" means the milestone did not touch the metric.

| Metric | Before (the v0.7.0 - v0.7.2 road) | Milestone and result |
| :--- | :--- | :--- |
| Fingerprint of a 64 B payload | 2.04 us, 35 allocations | v0.7.3: **0.89 us, 1 allocation** (3.6) |
| Memory to ingest a 16 MiB GIF | 67.1 MB allocated, 32 MiB peak | v0.7.3: **7.1 KB, 0.00 MiB peak** (3.6) |
| Time to ingest a 16 MiB GIF end to end | 111.5 ms | v0.7.3: 105.7 ms; hash-bound (SHA3 6.3 ms per MiB) |
| Pipe -> file, 16 MiB, no digest | - | v0.7.3: **4.0-4.3 ms** against 109.6 ms for the `Vec` road (3.6) |
| Convert a 12 MiB image | 300-305 ms, 36 MiB peak | v0.7.4: **215 ms, 0.00 MiB peak** (3.7) |
| Resident memory after `wipe` | 74.4 MiB | v0.7.4: **4.2 MiB** (3.8) |
| Content-defined chunking | - | v0.7.5: **3.23 GiB/s, 0 allocations** (3.9) |
| Storage of an edited 16 MiB text | 33.55 MB | v0.7.6: **16.80 MB** (50 %) (3.9, 3.10) |
| CPU to insert 1 MiB of text | 0.87-0.93 ms | v0.7.6: **7.4 ms** (8.3x, hash-bound; 3.10) |
| Context switches per MiB through a pipe | 31 | v0.7.7: **1** (3.11) |
| Paste direction (`sendfile` to a pipe), 16 MiB | 825-862 us | v0.7.7: **533-564 us** (3.11) |
| Queue enqueue + dequeue, 64 B / 4 KiB | 77-79 / 95 ns | 78.0-78.2 / 96.8-97.2 ns: unchanged (3.13) |

### 3.13 Re-check of sections 3.1 - 3.8 at v0.7.7

Three full runs. SHA3 digest of 1 MiB 6.32-6.33 ms (158 MiB/s; 6.31 ms recorded) and the 64 B fingerprint 0.885-0.888 us (0.90): unchanged. Single-thread queue rows 78.0-78.2 ns (64 B), 96.8-97.2 ns (4 KiB), 88.1-88.4 ns (shed), against 77.8-78.3, 96.8-98.8 and 86.4-88.7 at v0.7.4: unchanged. The spooled ingest rows reproduce (1 MiB 6.79-6.93 ms, 16 MiB 105.2-106.9 ms; same-day v0.7.5 control 6.88-6.91 and 106.6-106.8), and so does the 12 MiB descriptor transcode (216.8-221.2 ms; control 215.2-216.9, recorded 214.6-216.9 at v0.7.4: 0-3 %, one run at the top of the range). The SQLite rows that changed, and by how much, are in section 3.10. Every row that spawns a thread allocates 393 bytes more than the figures of sections 3.6 - 3.8 (644 against 251 bytes for `splice only`); the v0.7.5 tree run the same day shows the identical 644, so the difference is the toolchain's thread bookkeeping and not a change of these milestones.

---

## 4. Memory Bounds

### 4.1 Measured

| Experiment | Result |
| :--- | :--- |
| Offer 128 x 1 MiB to a queue of capacity 64 with zero patience | **64 queued, 64 shed**; peak heap growth **65.0 MiB** (64 queued + the one payload in flight). The surplus is dropped, not held |
| The same, with spooled payloads (v0.7.3) | **64 queued, 64 shed**; peak heap growth **0.010 MiB**; 64 spool files while queued, 0 afterwards (shed jobs unlink theirs) |
| Convert one spooled 12 MiB raster image (v0.7.4) | peak heap growth **0.00 MiB**, against **36.12 MiB** when it is read back into memory first (v0.7.3); section 3.7 |
| `y4p wipe` after a 40,000-row history was listed and searched (v0.7.4 Part 2) | resident set **233.5 MiB -> 4.2 MiB** (74.4 MiB with `VACUUM` alone); section 3.8 |

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
| 3'. spooled, not transcoded: persist | 0 for an image (digest already known; `insert_spooled` renames the file into the cache: same inode, and the row needs only its length); 1 for a payload stored in its row (read once into a `Vec`; v0.7.4 Part 2 replaced the v0.7.3 mapping) |
| 4. raster image under the spool threshold (a few KiB): `Vec` to `magick` stdin, `magick` stdout to a new `Vec` | 2 (pipe relays) |
| 4'. spooled raster image (v0.7.4): spool file to `magick` stdin, `magick` stdout to a new spool file | 0 (the file is the child's stdin; `splice(2)` into the new file; the digest reads the `tee`d duplicate through the stack buffer) |
| 4''. converted spooled image: persist (v0.7.4) | 0 (as 3': digest known, renamed into the cache) |
| 5. egress (paste): cached image via `sendfile(2)` | 0 (kernel to kernel; into a pipe enlarged to 1 MiB since v0.7.7, section 3.11) |
| 6. chunked payload persist (v0.7.6): read into its row's chunks | 0 extra on the Rust heap (chunks borrow the payload; digests on the stack); SQLite copies each chunk into its page cache; reassembly is one buffer of exactly the payload |

Ingestion now has no heap copy for a large payload whether or not the converter rewrites it, one for small payloads and text, and two more for a small raster image; egress has none. The mapping and the mirrored layout rule that a spooled payload used to cost are gone (v0.7.4 Part 2); the one copy left is a non-image payload read into its database row.

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
| `unsafe` mapping and mirrored storage rules in the daemon | `MappedFile` (`mmap`/`munmap`), `core::cache_file_path`, `worker::stored_in_cache_file` | none | **v0.7.4 Part 2: met** (section 3.8) |
| Resident memory after `wipe` | 74.4 MiB (VACUUM only) | back to the process's idle footprint | **v0.7.4 Part 2: met** (4.2 MiB; section 3.8) |
| Queue memory bound | 64 jobs, any size | byte budget (for example 64 MiB) in addition to the count | v0.7.5 - v0.7.8 (hardening; for spooled payloads the budget is disk, see section 4.2) |
| SHA3 throughput | 158 MiB/s | no regression; overlap with the transfer so that hashing no longer serialises behind it | **v0.7.3: met in the sense that matters**: no regression (154-159 MiB/s), and hashing runs on the ingestion threads in parallel instead of the single worker; a single stream is still hash-bound (section 3.6) |
| Allocations to read a small text payload from a pipe | 3-9 | 1 | **v0.7.2: met** (section 3.5) |
| Context switches per MiB through a pipe | 31 | <= 2 | **v0.7.7: met** (1.0-1.2; section 3.11) |
| Paste direction, 16 MiB `sendfile` into a pipe | 825-862 us | no regression; ideally lower | **v0.7.7: met** (533-564 us; section 3.11) |
| Storage of an edited copy | whole payload again | only the changed chunks | **v0.7.6: met** (0.11-0.14 % new; section 3.9) |
| CPU to insert a large non-image payload | 0.87 ms per MiB | no regression beyond the 3 % floor | **v0.7.6: not met** (7.4 ms per MiB, hash-bound; section 3.10, open question 7) |
| Existing rows above | as measured | no regression beyond the 3 % noise floor (25 % for short uri-list and cross-thread cases) | every milestone |

One consequence to plan around: because SHA3-256 costs about 6.3 ms per MiB against about 0.1-0.5 ms for the writes, removing a memcpy saves little wall-clock time for large payloads by itself. The zero-copy gain is chiefly **memory** (no whole-payload buffer) and **latency hiding** (hash while receiving), not raw copy cost. The SHA3 digest is also what deduplication keys on, so replacing it is a storage-format decision and out of scope for the harness.

---

## 6. Open Questions Carried Forward

1. **Byte budget for the queue.** `INGEST_QUEUE_CAPACITY` bounds the count only, and since v0.7.3 the products in section 4.2 bound disk (the spool directory) rather than heap for large payloads. A byte budget (an atomic counter of queued bytes) has what it needs: `Payload::len_hint` reports the size without I/O, and a `SpooledFile` knows its length. It is not implemented.
2. **Hash cost.** At 154-159 MiB/s with `opt-level = "s"`, hashing dominates large payloads, and v0.7.3 shows it is the *only* thing that does (the kernel copy is 26-34 times faster). Overlapping it with the transfer is done; the remaining lever is a per-package `opt-level = 3` override for the Keccak crates in `[profile.release.package]`, which adds no crate but changes release-profile settings and therefore needs approval; the second has not been measured.
3. **Disk-backed numbers.** The recorded run is on tmpfs. A run with `Y4P_BENCH_DIR` on the real data disk should be added once, because `fsync` behaviour changes the SQLite rows by orders of magnitude.
4. **A large non-image payload meets the heap once.** `insert_spooled` reads a spooled PDF or archive into one `Vec` before SQLite copies it into the row. An incremental BLOB write (`zeroblob(n)` then `sqlite3_blob_open`, in 64 KiB steps) would keep it flat, but rusqlite's `blob` feature is a manifest change and was not requested; large non-image, non-text payloads are rare in a clipboard.
5. **Two splice primitives.** `image::pipeline::{try_splice, splice_all}` (bare, tap-less, tested, unused by production code) and `core::splice` (bounded, fingerprinting, with the buffered fallback) overlap. Folding the first into the second is a refactoring for a `.N.5`-`.N.8` release, not a feature.
6. **Realistic images.** Section 3.7 uses incompressible noise to make the byte volume the label says. A screenshot-like image spends proportionally more of its time in `magick`'s encoder and less in the plumbing; the memory result is independent of content, the time result is not.
7. **Chunk hashing doubles the hash cost of a large non-image payload** (section 3.10). The fingerprint and the per-chunk digests are both SHA3-256 over the same bytes. Levers, none applied: compute the chunk digests where the fingerprint is computed (the ingestion threads, in parallel, while the data streams) and pass them to storage; use a cheaper chunk key such as a 128-bit non-cryptographic hash with a byte comparison on a hit (no new crate, but a storage-format and trust decision); or raise the 4 KiB threshold so mid-sized payloads stay inline. Each needs approval because it changes the storage contract or the format.
8. **Pipe tuning against a real compositor and a real paste target.** Section 3.11 measures the kernel and the daemon's own code with both ends in one process. An end-to-end run with a live Wayland session (`wl-copy` of a multi-megabyte selection into the daemon, `wl-paste` out of it) would confirm the egress figures; it needs a compositor and is not part of the harness.
9. **The pipe budget of a user.** Each enlarged pipe counts against `/proc/sys/fs/pipe-user-pages-soft` (16,384 pages on this machine, 64 MiB); a pipe is 256 pages, so about 64 simultaneous 1 MiB pipes would exhaust it, after which the kernel refuses growth (handled: `EPERM` halves the request down to the default) and limits further pipes. With at most 8 readers plus the egress threads that is not reachable in normal use, but it has not been tested under a pipe-heavy load from other programs.
