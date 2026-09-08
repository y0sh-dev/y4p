# Wayland Protocol & Streaming I/O

`y4p` talks to the compositor through `ext-data-control-v1`, a protocol that hands you clipboard data one file descriptor at a time. Every design decision in this file is about the same constraint: a clipboard manager is background infrastructure, so it has to move that data — sometimes 70MB+ of lossless image — without being the reason the desktop feels slow.

---

## Why one thread multiplexes Wayland *and* IPC

The daemon has two independent sources of work: Wayland protocol events (a new clipboard offer arrived) and IPC commands over the Unix socket (`y4p copy-to 3` was just run). The obvious design spawns a thread per source. `y4p` doesn't — both are watched on the *same* thread with a single `libc::poll` call:

```rust
// src/daemon/mod.rs — the daemon's main loop
let mut poll_fds = [
    libc::pollfd { fd: conn.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
    libc::pollfd { fd: listener.as_fd().as_raw_fd(),  events: libc::POLLIN, revents: 0 },
];

if unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 500) } < 0 { continue; }
```

A thread that exists only to block on `recv()` still costs a kernel stack, a scheduler entry, and a context switch every time it wakes up — overhead that's pure waste for a process that is idle the overwhelming majority of the time. `poll(2)` puts the *kernel* in charge of watching both file descriptors at once and only wakes the process when one of them actually has data, which is what lets the daemon sit at genuinely 0% CPU between clipboard events instead of spending cycles just to discover there's nothing to do. The 500ms timeout isn't a busy-loop interval; it's a watchdog so `is_exiting()` and the seat-rebind self-heal check (see `bind_data_device` in `daemon/mod.rs`) still get re-evaluated periodically even with no I/O activity at all.

The tradeoff this buys: no locking between "Wayland event" and "IPC command" handling, because they can never run concurrently with each other — they're the same thread. Anything that *does* need to run concurrently (disk I/O, hashing) is deliberately pushed off this thread instead (see [03](03_concurrency_and_memory.md)), so the poll loop itself never blocks on anything slower than a syscall.

## Why ingestion is one pass, not two

The naive way to "read data and compute its hash" is to read it all into a buffer, then loop over the buffer again to hash it. That's two full passes over the same memory — once to fill the CPU cache, once to re-read it after it may have already been evicted. For a 70MB image, that's a needless second sweep across tens of thousands of cache lines.

`y4p` updates the hasher inline with the read loop instead, so each chunk is hashed exactly once, while it's still hot in cache:

```rust
// src/wayland/handlers/data_control/device.rs
let mut chunk_buffer = super::AlignedBuffer::new(65536, 4096);
let chunk = chunk_buffer.as_mut_slice();
let mut hasher = (!is_uri_list).then(Sha3_256::new);

while let Ok(n) = reader.read(chunk) {
    if n == 0 { break; }
    let data = &chunk[..n];
    if let Some(h) = hasher.as_mut() { h.update(data); }
    payload.extend_from_slice(data);
}
```

The 4096-byte alignment on the read buffer isn't cosmetic either — it matches the Linux kernel's page size, so each `read()` from the compositor's pipe lands on a page boundary instead of straddling two pages. That keeps the kernel-to-userspace copy on the cheapest possible path for the pipe buffer sizes `y4p` configures (see `make_pipe`'s `F_SETPIPE_SZ` bump to 4MB for image transfers).

One deliberate exception: `text/uri-list` payloads skip the inline hasher (`is_uri_list` short-circuits it to `None`) and get hashed *after* buffering, once. That's because a uri-list has to be normalized (`core::utils::normalize_uri_list` strips `file://` prefixes and re-encodes escaped paths) before it's persisted, and the fingerprint has to match what actually ends up in the database — hashing the raw, pre-normalization bytes would let two byte-for-byte-different offers that normalize to the same paths dedupe incorrectly, or vice versa. Buffer-then-normalize-then-hash is the only order that keeps the hash meaningful for that one MIME type; every other MIME type keeps the single-pass path.

## Why `provider_locks` exists

`ext-data-control-v1` doesn't distinguish "the user copied something new" from "someone asked the compositor to re-offer clipboard content the daemon itself just set." Without a way to tell those apart, restoring a history entry (`copy-to`) would immediately trigger the daemon's own ingestion handler, which would re-hash and re-store the same content the daemon just put on the clipboard — a feedback loop that does nothing except burn CPU and touch the timestamp on every restore.

`y4p` closes that loop with a plain counter. Right before it hands data to the compositor, it increments the lock:

```rust
// src/daemon/mod.rs — handle_restore_request
state.provider_locks += 1;
```

and the ingestion handler checks it before doing any work at all:

```rust
// src/wayland/handlers/data_control/device.rs
if state.provider_locks > 0 {
    state.provider_locks -= 1;
    return;
}
```

This works because both sides run on the same single-threaded event loop described above — there's no race between "increment" and "check" to guard against, so a plain `usize` is enough; no atomics, no mutex. It's the simplest tool that fits the actual concurrency model, which is the point: adding synchronization the design doesn't need would just be more surface area to get wrong.

## Zero-copy egress for cached payloads

Restoring a *text* entry is cheap regardless — it's already a small `Vec<u8>` living in the process. Restoring an *image* that's sitting in `~/.cache/y4p/<hash>.cache` is a different story: naively, that means `read()` the whole file into a `Vec<u8>`, then `write()` that `Vec` out to the requesting client's pipe — the full payload crosses the userspace boundary twice for a transfer that never needed to touch this process's heap at all.

`storage::ContentLocation` exists to route around that. A record's payload is either `InlineBlob` (small, lives in the SQLite row) or `CacheFile` (large, lives on disk); egress branches on which one it is, and a `CacheFile` payload is handed to `sendfile(2)` instead of a read/write pair:

```rust
// src/wayland/handlers/data_control/source.rs
SourcePayload::File(path) => {
    let path = path.clone();
    std::thread::spawn(move || {
        send_via_sendfile(&path, fd);
    });
}
```

`sendfile(2)` copies file-to-pipe entirely inside the kernel — this process's heap never holds the image at all, not even briefly. The transfer runs on its own spawned thread (same as the pre-existing owned-payload path) so a slow reader on the other end can never stall the daemon's main poll loop. A userspace copy loop remains as a fallback (`fallback_copy`) only for the case where `sendfile(2)` itself reports it can't be used against the destination (`EINVAL`/`ENOSYS`) — rare for a plain pipe, but cheap insurance against an exotic kernel silently producing a truncated paste instead of a working one.
