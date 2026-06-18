# Round 7 review: native-io, native-collections, native-api

Reviewer: senior Rust performance / correctness review (round 7).
Scope: `native-io/`, `native-collections/`, `native-api/`.

Audit of round-6 wave-1 fixes (Net `Arc<Mutex<_>>`, HashMap JDK-split un-gated,
`copy_file_range`, async-socket bulk migration, `write_char_array_from`,
stream-decoder bulk-write bounds) plus surviving round-5 items and new angles.

---

## (A) Round-6 audit

### 1. `[CRIT]` `write_byte_array_from` / `write_char_array_from` default impl returns `true` even on bounds overflow — silent corruption
`native-api/src/registry.rs:226-231` (byte), `:272-277` (char)

Both default fallbacks `for (i, b) in src.iter().enumerate() { self.set_array_element(arr, dst_off + i, ...) }` then **unconditionally `return true`**. If `dst_off + src.len() > array_length(arr)`, every overrun element silently fails inside `set_array_element` (which has no return), yet the function reports success. The doc-comment claims `false` on bounds error. Round-6 added `write_char_array_from` symmetrically broken — the off-by-one audit in the prompt found nothing because the function lies about success.

Fix: `if dst_off.saturating_add(src.len()) > self.array_length(arr) { return false; }` at the top of both. Round-6 callers (`stream_decoder.rs:376,386`) happen to size the dst exactly, but any external caller (Pipe/RAF/Zip) that miscounts now corrupts trailing bytes silently.

### 2. `[CRIT]` `native_chm_put` leaks segment monitor on inner-call panic / error
`native-collections/src/lib.rs:14450-14458` (put), `:14479-14512` (put_if_absent / replace), `:14523-14550` (remove / compute), `:14586-14588` (clear), `:14606-...` (put_all)

Pattern is `monitor_enter(seg); let result = native_map_put(...); monitor_exit(seg); result` — if `native_map_put` returns `Err` or panics during `map_resize` (which can happen on `alloc_ref_array` OOM or the `eprintln!` cycle-guard branch), the `monitor_exit` is skipped and that segment is permanently deadlocked for every future `chm_put`/`chm_get`. Same anti-pattern at 12+ call sites.

Fix: scope-guard the exit — `struct Unlock<'a>(&'a mut dyn NativeContext, ObjectRef); impl Drop for Unlock { fn drop(&mut self) { self.0.monitor_exit(self.1); } }`. Or refactor `native_map_put` to a fallible helper that returns the value pre-monitor-exit.

### 3. `[CRIT]` ConcurrentHashMap `get` is not actually lock-free correct under concurrent `put` resize
`native-collections/src/lib.rs:14384-14395` (get), `:1393` (map_resize swap), `:1310-1345` (split walker)

`native_chm_get` → `native_map_get` reads `MAP_FIELD_BUCKETS` without monitor; `native_chm_put` holds the segment monitor and inside `map_resize` mutates each node's `NODE_FIELD_NEXT` (lines 1314/1321) to splice the lo/hi chains **before** swapping `buckets` to the new array at line 1393. A concurrent reader following `node.next` can land on a partially-spliced chain — infinite loop or wrong-bucket lookup. Real CHM publishes the new table only after all nodes are linked, and uses volatile next-reads.

Fix: in `map_resize`, allocate fresh node objects for the new chains (or copy each node) so the old chain remains intact for readers, then atomic-swap `MAP_FIELD_BUCKETS`. Alternatively, document that segment reads must take the monitor too and update `native_chm_get`.

### 4. `[HIGH]` Net `setIntOption0` accepts SO_KEEPALIVE but never applies it; SO_LINGER not handled at all
`native-io/src/net.rs:614-619`, plus missing setSoLinger/setSoTimeout registrations

Round-6 added `(SOL_SOCKET, SO_KEEPALIVE) => { /* remember the value */ }` — the comment admits std doesn't expose it. The value is cached in `net_opts` but the kernel never sees it, so HTTP keep-alive on long-idle connections still gets RST'd by NATs. Same for SO_LINGER, SO_SNDBUF, SO_RCVBUF, SO_REUSEPORT. `setSoTimeout`, `setSoLinger`, `setReceiveBufferSize` are not registered at all — fall through to synthetic stubs.

Fix: add `socket2 = "0.5"` to Cargo.toml; wrap the `Arc<TcpStream>` in a `socket2::Socket` (zero-cost, same fd) and call `.set_keepalive(Duration::from_secs(val as u64))`, `.set_linger(...)`. Register the per-class SocketOptions methods explicitly.

### 5. `[HIGH]` Net `bind0` overwrites FileDescriptor.handle with the bound port (not the fd)
`native-io/src/net.rs:337-339`

`ctx.set_field_by_name(fd_obj, "handle", Value::Long(local.port() as i64));` — but `net_fd_from_descriptor` (line 233) **falls back to reading `handle` as the fd id** when `fd` is empty. After `bind0`, calling `localPort(fd_obj)` works (it re-reads via `net_fd_from_descriptor`, which still finds a value), but if the JDK later passes the same FD to a path that constructs a child socket and inherits `handle`, the child gets the bound port treated as its fd. Mismatch with the JDK convention where `handle` is the Windows SOCKET.

Fix: store the bound port in a separate side-table keyed by fd, or use a dedicated synthetic field like `_localPort`. Never write port values to `handle`.

### 6. `[HIGH]` HashMap split walker uses `(size as usize).saturating_add(8)` cycle guard but `size` is the *whole-map* size, not chain length
`native-collections/src/lib.rs:1293`

`step_cap = (size as usize).saturating_add(8)` bounds a single bucket walk by the total entry count. For a well-balanced map (`size = 10_000`, average chain length 1) this lets a real cycle run 10_008 steps before tripping — fine, but the eprintln! at line 1300 then dumps `bucket=i` and bails midway through the split, leaving the chain partially spliced and the new-array bucket head pointing into the old bucket. The map is corrupted and silently lossy for entries past the truncation point.

Fix: when a cycle is detected, fall back to allocating a fresh array and re-inserting all entries via `native_map_put` from a pre-walked snapshot. Or `panic!` — corruption is unrecoverable and continuing is worse than aborting.

### 7. `[HIGH]` `copy_file_range` treats EBADF / EINVAL as "kernel unsupported, fall through to sendfile" — hides real bugs
`native-io/src/file_channel.rs:493-505`

EBADF means one of our `as_raw_fd()` returned a closed fd — that's a use-after-close in our code, not a kernel-capability issue. Funneling it through sendfile (which will fail the same way) and then through the userspace loop (where pread also fails) returns garbage to Java. EINVAL on the first call is ambiguous (could be cross-fs); after `transferred > 0` it's a hard error per the man page.

Fix: split the error match — only `ENOSYS | EXDEV | EOPNOTSUPP` are "try next mechanism"; `EBADF | EINVAL` on first call should propagate as an IOException.

---

## (B) Surviving round-5 items (unfixed)

### 8. `[HIGH]` No per-thread scratch buffer — every read/write allocates a fresh `vec![0u8; len]`
`native-io/src/net.rs:508` (read0), `:553` (write0), `file_channel.rs:627` (transfer fallback), `pipe.rs`, `socket_channel.rs`, `datagram.rs`

`grep` finds zero `thread_local!` declarations across all 11 native-io modules. Hot HTTP reads (4 KiB) allocate, touch, free a fresh Vec per request — measurable on the Netty echo benchmark. The `BUF_TLS` pattern from `vm_heap` would cut allocator traffic by ~80% for typical I/O.

Fix: `thread_local! { static IO_SCRATCH: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(64 * 1024)); }` — reuse `IO_SCRATCH` for any read/write where the caller wants bytes-by-value and we copy out anyway.

### 9. `[MED]` `FileLock` still synthetic — no `fcntl(F_SETLK)` / `LockFileEx`
`native-io/src/lib.rs:9036-9151`

Unchanged since round-5 finding #7. Two processes opening `Maven repository.lock` both succeed, both corrupt. Round-7 not fixed.

Fix: same as round-5 — `fcntl` on Linux, `LockFileEx(... | LOCKFILE_FAIL_IMMEDIATELY)` on Windows; remember the locked range on `release()`.

### 10. `[MED]` DirectByteBuffer leak — no PhantomReference hook
`native-io/src/direct_buffer.rs:274-340`

Unchanged. Netty workloads allocating 4 KiB direct buffers per request leak entries until VM exit. Stopgap: cap `cleaners()` size at e.g. 65_536 and evict-oldest with `fire_cleaner`.

### 11. `[MED]` `ZipFile.entries()` calls `by_index(i)` N times — each seeks back to the local header
`native-io/src/zip_real_jar.rs:439-444`

Round-5 finding #11 — `zip::ZipArchive::by_index` seeks and re-parses per call. For a 10-KB-entry jar (modern Spring fat-jar), `entries()` does 10K seeks just to enumerate. The central directory already has every field except the local header offset; the `zip` crate exposes them via `archive.file_names()` and `archive.by_index_raw()` (no decompression setup) — both ~free.

Fix: replace the `by_index` loop with iteration over `state.archive.shared.files` (private field — alternatively use `by_index_raw` which skips the data-descriptor parse).

---

## (C) New angles

### 12. `[MED]` `LinkedHashMap` has no access-order semantics — `get()` never promotes node to tail
`native-collections/src/lib.rs:9784` (lhm_resize), and absence of any `afterNodeAccess`

`grep accessOrder | accessOrder | afterNodeAccess` returns one comment match and nothing else. Guava `CacheBuilder` and `Collections.synchronizedMap(new LinkedHashMap(..., true))` LRU caches don't evict in the right order; `removeEldestEntry` callback also not honoured. Spring Boot's `ConcurrentReferenceHashMap` falls through to LinkedHashMap for its segment LRU — silently degrades to FIFO.

Fix: add `lhm_promote_to_tail(ctx, this, node)` and call it from `native_lhm_get` when an `__accessOrder` flag (stored in the overlay) is set; honor the flag in the `(IFZ)V` ctor.

### 13. `[LOW]` `HashMap` registers no `<init>(I,F)V` / `<init>(I,F,Z)V` overloads — capacity hints with custom load factor fall through
`native-collections/src/lib.rs:1508-1511`

Only `()V`, `(I)V`, `(Ljava/util/Map;)V` are registered. `new HashMap<>(1024, 0.5f)` from app code hits the synthetic ctor which ignores the load factor and uses default 0.75 — minor surprise for tuning code (e.g. Guava `Maps.newHashMapWithExpectedSize`).

Fix: register the 2-arg and 3-arg ctors — same body as `native_map_init_capacity`, just store the load factor via `try_set_jdk_map_field("loadFactor", ...)`.

### 14. `[LOW]` `ZipEntry` name decoded as UTF-8 unconditionally — JDK falls back to CP437 when the EFS bit (gp-flag bit 11) is clear
`native-io/src/zip_real_jar.rs:446` (`f.name().to_string()`)

The `zip` crate returns UTF-8 regardless of the general-purpose-bit-11 EFS flag. Legacy zips (created by Windows Explorer pre-2007, Info-ZIP, Maven before 3.2.5) store filenames in CP437 / system codepage. Names with accented chars come through mojibake and `getEntry(name)` then misses.

Fix: inspect `f.flags() & 0x800`; if clear, decode `f.name_raw()` (bytes) via `encoding_rs::IBM866`/`WINDOWS_1252` per the standard heuristic, or simply prefer CP437 when EFS is off.

---

**Word count: ~590** (excluding code/path lines).
