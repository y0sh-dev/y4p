# Wayland Protocol & Streaming I/O

`y4p` talks to the compositor through `ext-data-control-v1`.

This protocol hands you clipboard data one file descriptor at a time. Every decision in this document comes back to the same constraint: a clipboard manager is background infrastructure. It has to move that data — sometimes 70MB+ of lossless image — without becoming the reason the desktop feels slow.

---

## Why one thread watches both Wayland and IPC

The daemon has two independent sources of work.

Wayland protocol events: a new clipboard offer just arrived. IPC commands: someone ran `y4p copy-to 3` on the socket.

The obvious design spawns a thread per source. `y4p` doesn't. Both are watched on the *same* thread, with a single `libc::poll` call:

```rust
// src/daemon/mod.rs — the daemon's main loop
let mut poll_fds = [
    libc::pollfd { fd: conn.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
    libc::pollfd { fd: listener.as_fd().as_raw_fd(),  events: libc::POLLIN, revents: 0 },
];

if unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 500) } < 0 { continue; }
```

Why does this matter?

A thread that only exists to block on `recv()` still costs something. A kernel stack. A scheduler entry. A context switch, every time it wakes up. That's pure overhead for a process that's idle almost all the time.

`poll(2)` puts the kernel in charge of watching both descriptors at once. It only wakes the process when one of them actually has data.

That's what lets the daemon sit at genuinely 0% CPU between clipboard events — instead of spending cycles just to discover there's nothing to do.

The 500ms timeout isn't a busy-loop interval, either. It's a watchdog. It makes sure `is_exiting()`, and the seat-rebind self-heal check in `bind_data_device`, still get re-evaluated periodically — even with zero I/O activity.

One tradeoff this buys: no locking between "Wayland event" and "IPC command" handling. They can never run concurrently with each other, because they're the same thread. Anything that *does* need real concurrency — disk I/O, hashing — is pushed off this thread entirely instead. More on that in [03](03_concurrency_and_memory.md).

---

## Why ingestion is one pass, not two

The naive way to "read data, then hash it" is two separate steps.

Read it all into a buffer. Then loop over that buffer again to compute the hash. For a 70MB image, that's a second full sweep across tens of thousands of cache lines — memory that may have already been evicted by the time the second pass gets to it.

`y4p` updates the hasher inline, inside the same read loop. Each chunk gets hashed exactly once, while it's still hot in cache:

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

The 4096-byte alignment isn't cosmetic, either.

It matches the Linux kernel's page size. Each `read()` from the compositor's pipe lands cleanly on a page boundary, instead of straddling two. That keeps the kernel-to-userspace copy on the cheapest path available, for the pipe sizes `y4p` configures — see `make_pipe`'s `F_SETPIPE_SZ` bump to 4MB for image transfers.

There's one deliberate exception: `text/uri-list`.

A uri-list has to be normalized before it's persisted — `core::utils::normalize_uri_list` strips `file://` prefixes and re-encodes escaped paths. So the fingerprint needs to match what actually ends up in the database, not the raw pre-normalization bytes.

Hashing the raw bytes could let two different offers that normalize to the same paths fail to dedupe correctly, or the reverse. So uri-list buffers first, normalizes, and hashes once, after the fact. It's the only MIME type on that slower path. Everything else keeps the single-pass loop above.

---

## Why `provider_locks` exists

`ext-data-control-v1` can't tell "the user copied something new" apart from "the compositor is re-offering content the daemon itself just set."

Without a way to distinguish those, restoring a history entry (`copy-to`) would immediately trigger the daemon's own ingestion handler. It would re-hash and re-store the exact content it just placed on the clipboard. A feedback loop — one that does nothing useful, and just touches a timestamp on every restore.

`y4p` closes that loop with a plain counter.

Right before handing data to the compositor, it increments the lock:

```rust
// src/daemon/mod.rs — handle_restore_request
state.provider_locks += 1;
```

And the ingestion handler checks it first, before doing any work at all:

```rust
// src/wayland/handlers/data_control/device.rs
if state.provider_locks > 0 {
    state.provider_locks -= 1;
    return;
}
```

This works cleanly because both sides run on the same single-threaded event loop described above. There's no race between "increment" and "check" to guard against — so a plain `usize` is enough. No atomics. No mutex.

That's the point, really. Use the simplest tool that actually fits the concurrency model you have. Adding synchronization the design doesn't need is just more surface area to get wrong later.

---

## Zero-copy egress for cached payloads

Restoring a *text* entry is cheap either way — it's already a small `Vec<u8>` sitting in the process.

Restoring an *image* from `~/.cache/y4p/<hash>.cache` is a different story. Naively: `read()` the whole file into a `Vec<u8>`, then `write()` that `Vec` back out to the requesting client's pipe. The full payload crosses the userspace boundary twice, for a transfer that never needed to touch this process's heap in the first place.

`storage::ContentLocation` exists to route around that.

A record's payload is either `InlineBlob` — small, lives in the SQLite row — or `CacheFile` — large, lives on disk. Egress branches on which one it is. A `CacheFile` payload goes to `sendfile(2)`, not a read/write pair:

```rust
// src/wayland/handlers/data_control/source.rs
SourcePayload::File(path) => {
    let path = path.clone();
    std::thread::spawn(move || {
        send_via_sendfile(&path, fd);
    });
}
```

`sendfile(2)` copies file-to-pipe entirely inside the kernel. This process's heap never holds the image — not even briefly.

The transfer runs on its own spawned thread, same as the owned-payload path already does. That way, a slow reader on the other end can never stall the daemon's main poll loop.

A userspace copy loop still exists as a fallback (`fallback_copy`). It only kicks in if `sendfile(2)` itself reports it can't be used against this destination — `EINVAL` or `ENOSYS`. Rare, for a plain pipe. But cheap insurance against an exotic kernel silently producing a truncated paste instead of a working one.
