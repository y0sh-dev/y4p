
# y4p Architecture

This document provides a detailed overview of the internal mechanisms of `y4p` and the design choices made to ensure stability and performance in a Wayland environment.

---

## 1. Process and Communication Model

The system operates on a client-server model where a single background **daemon** handles all heavy lifting. The CLI acts as a lightweight "remote control" that sends instructions to the daemon.

### Unified Event Loop
The daemon monitors both Wayland protocol events and IPC (Inter-Process Communication) commands within a single thread using a multiplexed polling mechanism.

<details>
<summary>Implementation Details: libc::poll</summary>

Instead of using multiple threads for different input sources, we utilize `libc::poll` to manage file descriptors for the Wayland connection and the IPC socket simultaneously. This ensures the daemon remains idle (consuming 0% CPU) until an actual event occurs.

```rust
// Monitoring both Wayland and Socket entry points
while !crate::core::is_exiting() {
    let mut poll_fds = [
        libc::pollfd { fd: wayland_fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: socket_fd,  events: libc::POLLIN, revents: 0 },
    ];

    // Wait for activity with a 500ms watchdog timeout
    let poll_res = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, 500) };
    
    if poll_res > 0 {
        // If socket activity: Execute IPC command
        // If Wayland activity: Process clipboard event
    }
}
```
</details>

---

## 2. Storage Strategy

To maintain high performance even with large datasets, the system employs a dual-storage approach based on the nature of the data.

### Hybrid Persistence
Text data is stored in a **SQLite 3** database for fast searching, while large binary assets (images, GIFs) are offloaded to the **filesystem cache**.

<details>
<summary>Deduplication and Cache Logic</summary>

We generate a unique "fingerprint" for every piece of data using the **SHA3-256** algorithm.
- If identical content is copied, the system simply updates the "Last Used" timestamp of the existing record (MRU promotion).
- For images, the hash value is used as the filename in `~/.cache/y4p/`. This ensures that duplicate images do not occupy redundant disk space.

```rust
// Check for existing content
let existing: Option<i64> = tx.query_row(
    "SELECT id FROM clipboard WHERE hash = ?1 LIMIT 1",
    params![hash], |row| row.get(0)
).ok();

if let Some(id) = existing {
    // Content exists: Update timestamp only
    tx.execute("UPDATE clipboard SET timestamp = ?1 WHERE id = ?2", params![ts, id])?;
} else {
    // New content: Branch storage based on MIME type
    if is_binary {
        fs::write(cache_path, data)?; // To Filesystem
    }
    tx.execute("INSERT INTO clipboard ...", ...)?; // To Database
}
```
</details>

---

## 3. Concurrency Management

To prevent the Wayland event loop from stalling during disk operations, database interactions are serialized.

### Dedicated Worker Thread
Data ingestion (reading from the pipe) and data persistence (writing to the DB) occur in separate execution contexts.

<details>
<summary>Serialization via mpsc Channels</summary>

The main thread sends "Jobs" to a dedicated worker thread via an asynchronous channel. This ensures that SQLite never encounters "Database is locked" errors, as only one thread holds the write connection at any given time.

```rust
// Communication channel between Wayland handlers and the DB worker
let (job_tx, job_rx) = mpsc::channel::<ClipboardJob>();

std::thread::spawn(move || {
    // This thread exclusively owns the write-access to the database
    while let Ok(job) = job_rx.recv() {
        db.insert_with_hash(&job.mime, &job.data, &job.hash);
        
        // On Linux, explicitly release memory back to the OS after heavy tasks
        #[cfg(target_os = "linux")]
        unsafe { libc::malloc_trim(0); }
    }
});
```
</details>

---

## 4. High-Capacity Data Handling

The system is optimized to process large payloads, such as 70MB+ lossless images, without impacting desktop responsiveness.

### Page-Aligned I/O
Memory allocation is aligned with the operating system's native memory management units (typically 4KB).

<details>
<summary>Memory Control via std::alloc</summary>

By using page-aligned buffers, we optimize data transfer from the kernel's pipe buffer to user space, reducing CPU cache misses. Additionally, hash calculation is performed in a single pass as data is being read, avoiding redundant memory traversals.

```rust
// Allocate a 64KB buffer aligned to a 4096-byte page boundary
let layout = std::alloc::Layout::from_size_align(65536, 4096).unwrap();
let ptr = unsafe { std::alloc::alloc(layout) };

if !ptr.is_null() {
    let chunk = unsafe { std::slice::from_raw_parts_mut(ptr, layout.size()) };
    while let Ok(n) = reader.read(chunk) {
        if n == 0 { break; }
        hasher.update(&chunk[..n]); // Calculate hash during read
        payload.extend_from_slice(&chunk[..n]);
    }
    unsafe { std::alloc::dealloc(ptr, layout) };
}
```
</details>

---

## 5. Wayland Protocol Safeguards

Specific mechanisms are implemented to handle the unique behaviors of the Wayland Data Control protocol.

### Self-Copy Suppression
The daemon prevents "feedback loops" where it might attempt to re-ingest data that it just restored to the clipboard.

<details>
<summary>Control via provider_locks</summary>

When the daemon initiates a "Restore" operation, it increments a lock counter. The ingestion handler checks this counter; if it is greater than zero, the handler identifies the event as self-generated and ignores it.

```rust
if let Event::Selection { id } = ev {
    // If the event was triggered by our own 'copy-to' command
    if state.provider_locks > 0 {
        state.provider_locks -= 1;
        return; // Ignore this event
    }
    // ... proceed with normal storage logic
}
```
</details>

---

## 6. Security and Privacy

Strict measures are in place to protect sensitive history data, such as passwords or authentication tokens.

### Enforced Permissions
The system enforces restrictive filesystem permissions regardless of the user's default `umask` settings.

<details>
<summary>Filesystem Hardening</summary>

Directories are created with `700` (rwx------) and database files with `600` (rw-------) permissions. This ensures that clipboard history is inaccessible to other users on the same system.

```rust
// Force owner-only permissions during directory initialization
if !path.exists() {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    let _ = builder.create(&path);
}
```
</details>

