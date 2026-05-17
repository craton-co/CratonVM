# Round 9 — native-io / native-collections / native-api audit

Round-8 regression audit + remaining items + new angles.

## native-collections

### CRIT-1 — Per-segment RwLock gated by a global Mutex; serializes all CHM ops
`lib.rs:145-163` — `chm_seg_lock_for` takes a global `Mutex` on every
read+write (`chm_seg_get:14591`, `map_resize:1373`). Two threads `put`-ing
on **different** segments contend on this Mutex just to look up their
per-segment RwLock — worse than a single global RwLock.
**Fix:** store `Arc<RwLock<()>>` on the segment object (side overlay at
alloc); or swap the lookup Mutex for `DashMap`.

### CRIT-2 — Resize-lock keyed by heap pointer; reuse aliases unrelated CHMs
`lib.rs:156-163` — key is `seg.as_ptr() as usize`; never pruned. A GC'd
segment's address reused for a new object inherits the old RwLock —
unrelated ops serialize; plus unbounded growth.
**Fix:** clear entries via object-finalization hook, or store the lock
on the segment.

### HIGH-3 — `map_resize` taxes plain `HashMap` with global mutex
`lib.rs:1373` — every `java.util.HashMap.put` triggering a resize goes
through the CHM global Mutex. Single-threaded HashMap pays it per resize.
**Fix:** gate `chm_seg_lock_for` behind a CHM-segment class-id check;
skip for plain HashMap/LinkedHashMap/TreeMap receivers.

### HIGH-4 — `ConcurrentSkipListMap` not concurrent
`lib.rs:17202-17428` — sorted array, shift-insert/shift-remove, **no
synchronization** (no monitor_enter, no atomics). Concurrent put/remove
corrupts the array. Also O(N) not O(log N).
**Fix:** wrap put/remove/get in `monitor_enter(this)` (correctness
floor); real skip-list later.

### HIGH-5 — `BufferedReader.read()` issues one native call per byte
`native-io/src/lib.rs:1343-1354` — `native_br_read` dispatches a native +
`fd_table().read_byte(fd)` per byte. No internal buffer; defeats
"Buffered". `while (br.read()!=-1)` = N native crossings + N FD calls.
**Fix:** per-this `Vec<u8>` refill buffer in a side overlay; add bulk
`read(char[],int,int)` via `read_byte_array_into` +
`write_char_array_from`.

### MED-6 — `HashMap(int initialCapacity)` ignores load factor
`lib.rs:1853-1861` — uses `max(c,1).next_power_of_two()`. JDK does
`tableSizeFor((int)(initialCapacity/loadFactor + 1f))`.
`new HashMap<>(1_000_000)` gets 2^20 here vs 2^21 in JDK → an extra
full resize at 786 K.
**Fix:** `((cap as f32)/0.75).ceil() as u32` before `next_power_of_two`.

### MED-7 — `LinkedHashMap` access-order is a no-op; overlay leaks
`lib.rs:9839-9891` — no `accessOrder` field, no move-to-tail on get.
LRU-cache `removeEldestEntry` evicts the wrong entry. Overlay never
cleaned on GC.
**Fix:** add `access_order: bool` to overlay; on `native_lhm_get` hit
unlink+re-link node at tail; drop overlay on finalization.

### MED-8 — `TreeMap` uses sorted array; put/remove O(N)
`lib.rs:12443-12515` — binary-search + shift-insert in a parallel
(key,value) array. JDK is red-black O(log N). At N=100k each put shifts
up to 50k slots through the VM trait.
**Fix:** real red-black via allocated `TreeMap$Entry` nodes; keep array
path as fallback for N<32.

## native-io

### CRIT-9 — `freed_addrs` FIFO eviction enables pool double-put under churn
`direct_buffer.rs:537-565,574-595` — cap 4096. >4096 frees evict the
oldest; a stale `dbb_free_explicit(addr,size)` (e.g. Cleaner racing
`Unsafe.freeMemory`) finds `take_unsafe_alloc` empty AND
`mark_freed_or_check` empty → `dbb_free` runs twice, same addr in two
pool buckets. Round-8 fix voids itself exactly when churn is highest.
**Fix:** make the set unbounded (1M frees = 16 MiB — acceptable) OR
drive entry lifetime from `take_unsafe_alloc` (entry stays until both
paths observe it).

### LOW-10 — `connect_pool` workers leak on VM teardown
`socket_channel.rs:163-201` — round-7+ TODO unresolved. `SENDER` in
`OnceLock` never disconnects; 32 threads leak per shutdown.
**Fix:** `Mutex<Option<Sender>>`; `take()` on shutdown hook; join
with 1 s deadline.

### LOW-11 — FileLock still process-local
`lib.rs:8974-8991,9460` — no real `fcntl(F_SETLK)` / `LockFileEx`.
Cross-process locks silently unhonoured.
**Fix:** add `windows-sys`; thread `as_raw_fd`/`as_raw_handle` through
`FdTable`; keep in-process registry as fast-path.

### LOW-12 — `ZipEntry.method` collapses BZIP2/LZMA to DEFLATED
`zip_real_jar.rs:260-263` — `_ => 8` reports every non-`Stored` as
DEFLATE; `getInputStream` then runs inflate on bzip2 bytes → corruption.
**Fix:** map each `zip::CompressionMethod` explicitly; return the real
numeric code or fail with `ZipException`.

## Verified OK (round-8 fixes intact)

`ChmMonitorGuard::drop` (lib.rs:205-231) — `AssertUnwindSafe` wrap
sound. `regions_overlap` (lib.rs:9033-9053) — empty short-circuit +
`None=+∞` correct. `atomic_fetch_add_int/long` panics carry field +
variant. `write_byte/char_array_from` upfront `checked_add` covers
all paths.

## Open carryovers

DBB PhantomReference Cleaner queue; ZipFile parallel decompress;
Windows IOCP `AsynchronousSocketChannel`; CHM clone-resize (replaces
CRIT-1/2 kludge); `AtomicReferenceArray` natives.
