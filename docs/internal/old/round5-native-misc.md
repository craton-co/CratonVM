# Round 5 review: native-collections, native-io, native-api

Reviewer: senior Rust performance / correctness review (round 5).
Scope: `native-collections/`, `native-io/`, `native-api/`.

Audit of round-4 fixes (RAF `Arc<Mutex<File>>`, path validation rewrite,
bulk byte intrinsic wiring, JDK-split `map_resize`, `by_method_desc`
O(1) index, FxHashMap migrations) — plus remaining items from
round-4 native-misc and new angles.

---

## (A) Round-4 audit

### 1. `[CRIT]` `net::net_read0` / `net_write0` hold `net_sockets` read-lock across blocking `TcpStream::read` / `write` — same anti-pattern round-4 just fixed in RAF
`native-io/src/net.rs:492-505` (read0), `native-io/src/net.rs:538-547` (write0)

Both block their entire I/O inside `let map = net_sockets().read(); match map.get(&fd) { ... s.read(&mut buf) ... }`. A long `read()` on socket A freezes every concurrent net op in the VM (accept/connect/bind/option-set/close on every other socket), including the listener-acceptor cloning fix at line 364. The RAF refactor (`random_access_file.rs:111-118`) is exactly the template — clone the `TcpStream` (`try_clone()`) under the lock, drop the guard, then read/write on the clone.

Fix: store `Arc<TcpStream>` (or keep `TcpStream` and `try_clone()` like accept already does), then `let stream = { net_sockets().read().get(&fd).and_then(|h| if let Stream(s) = h { s.try_clone().ok() } else { None }) };` drop guard, syscall on `stream`.

### 2. `[CRIT]` `map_resize` legacy-rebuild bucket guard still 1M-step with `eprintln!` — round-4 fixed it ONLY in the pre-seeded branch
`native-collections/src/lib.rs:1339-1350`

The new JDK-split branch uses the correct `size + 8` bound (line 1284). The legacy fallback (taken whenever `bulk_array_copy` returns false — i.e. **always**, because `bulk_array_copy` rejects reference arrays at `vm/src/vm/vm_exec.rs:1313`) still has the original per-bucket `steps > 1_000_000` + `eprintln!` from round-4 finding #4. The "fix" never reaches production because reference arrays disqualify the preseed path 100% of the time.

Fix: same bound as the preseed branch — `step_cap = (size as usize).saturating_add(8)`, replace `eprintln!` with `tracing::warn!` (or drop entirely; the cycle is a logic bug the VM cannot recover from anyway).

### 3. `[HIGH]` `validate_path` null-byte check now correct, but components-check misses Windows verbatim/UNC traversal
`native-io/src/lib.rs:91-130`

The round-4 rewrite is correct for POSIX and normal Windows paths (`Path::components()` returns `ParentDir` only for actual `..` segments; `foo..bar.txt` is `Normal("foo..bar.txt")`). However, on Windows a hostile `\\?\C:\foo\..\..\etc` is parsed as a verbatim prefix — `Path::components()` yields `Prefix(VerbatimDisk('C'))`, `RootDir`, `Normal("foo")`, `ParentDir`, `ParentDir`, `Normal("etc")` — which is still caught. But `\\?\GLOBALROOT\Device\HarddiskVolume1\..` and `\\.\UNC\server\share\..` may yield prefix variants where `ParentDir` after the prefix is interpreted as a normal segment by the OS APIs. Also `normalize_for_os` strips `\\?\` *after* validation, so a path that passes validation may be rewritten to one the OS treats differently.

Fix: run validation on the normalized form (`normalize_for_os(path)` first, then `validate_path`). Add a unit test for `\\?\C:\foo\..\bar` and `\\.\C:\..`.

### 4. `[MED]` `map_hash_key` already applies JDK spread (`h ^ (h>>16)`) — verify dual-write of `Node.hash` stores the spread, not the raw String.hashCode
`native-collections/src/lib.rs:1116`, `native-collections/src/lib.rs:1942`

`map_hash_key` returns `h ^ (h>>16)` and `put` stores that into `NODE_FIELD_HASH`. This matches JDK's `Node.hash = HashMap.hash(key)` (also spread). Cross-checked `vm/src/vm/vm_exec.rs:5529::java_string_hash` — it returns the raw recipe (no spread). Both sides are internally consistent **as long as no caller writes `Node.hash = String.hashCode()` directly**. Worth a regression test: insert via native `put`, then read `Node.hash` via VM reflection and compare to `String.hashCode() ^ (>>16)` — silently divergent today.

Fix: add `vm/tests/wp_native_hashmap_node_hash.rs` pinning the dual-write contract.

### 5. `[MED]` Bulk-byte fallback path silently swallows bounds errors (no caller checks the `bool` / `usize` return)
`native-api/src/registry.rs:226-246` + every call site (`native-io/src/lib.rs:670,695,882,907,956`, `random_access_file.rs:339,392`, `pipe.rs:478,522,555,591`, `zip_real_jar.rs:402`, `stream_decoder.rs:332,336,372`, `stream_encoder.rs:158`)

`write_byte_array_from` returns `false` on bounds error or non-byte-array; `read_byte_array_into` returns `0`. Every caller ignores the return — silently truncates reads to zero / drops writes. The fallback default-impl also propagates wrong array kind as `Value::Int` only (line 241 / 257), so a writer caller passing a `boolean[]` works in the VM override (line 1202 accepts `Byte | Boolean`) but the per-element fallback dispatches via `set_array_element(Value::Int(b as i8 as i32))` which most heap impls type-check as Byte only — divergent behavior across overrides.

Fix: callers should `if !ctx.write_byte_array_from(...) { throw ArrayIndexOutOfBoundsException }`; same for `read_byte_array_into(...) != n`. Or: have the trait return `Result<usize, IndexOutOfBounds>` and propagate.

---

## (B) Remaining round-4 items

### 6. `[HIGH]` `copy_file_range(2)` still missing — file→file `transferTo` falls back to `sendfile` even between regular files on the same filesystem
`native-io/src/file_channel.rs:415-494`

Same finding as round-4 #9 — no progress. `copy_file_range` enables in-kernel reflink (btrfs/xfs/CIFS server-side copy) and 0-copy across NFSv4.2. The branch already clones both files (line 434), so the check `is_regular_file(dst)` is free.

Fix: add `#[cfg(target_os = "linux")] fn transfer_via_copy_file_range(src, dst, position, count)` invoked first when both ends are regular files; fall through to `sendfile` on `ENOSYS`/`EXDEV`.

### 7. `[MED]` `FileLock` is purely synthetic — no `fcntl(F_SETLK)` / `LockFileEx`, so cross-process locking is a no-op
`native-io/src/lib.rs:9060-9151`

`FileLock.<init>` populates five fields and returns. No OS lock is acquired. `release()` clears the validity flag. Two processes calling `FileChannel.tryLock()` on the same region both succeed and overwrite each other. JDBC drivers, embedded databases (H2/SQLite via JDBC), and Maven's `repository.lock` rely on real OS locks.

Fix: on Linux call `fcntl(fd, F_SETLK, &flock { l_type: F_WRLCK, l_start: position, l_len: size, ... })`; on Windows `LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY, ...)`. Store the locked range so `release()` can call `F_UNLCK` / `UnlockFileEx`.

### 8. `[MED]` DirectByteBuffer real Cleaner phantom-ref hook still deferred — memory leaks if user never calls `cleaner().clean()`
`native-io/src/direct_buffer.rs:274-340`

The registry holds `(addr, size)` until `fire_cleaner(id)` is called explicitly or `dbb_drain_pool` runs at VM exit. With no PhantomReference plumbing, a Netty workload doing `ByteBuffer.allocateDirect(4*1024)` per request leaks the registry entry (and the off-heap memory once the pool bucket is full at 32 retained / `pool_put` returns false at line 193).

Fix: hook the existing PhantomReference queue (gc crate) so reaping a DirectByteBuffer enqueues a `fire_cleaner(id)`. Alternative stopgap: cap registry size, evict oldest with `fire_cleaner`.

### 9. `[LOW]` `async_socket::read_buffer_bytes` array path still per-element `get_array_element` loop (round-4 missed this site)
`native-io/src/async_socket.rs:919-925` (read), `:608-615` (write)

Round-4 wired bulk intrinsics into FIS/FOS/RAF/Decoder/Encoder/Pipe/Zip but missed `AsynchronousSocketChannel` byte-buffer ↔ socket path. With Netty's `Aio*` channels this is the hottest path post-pool.

Fix: replace with `ctx.read_byte_array_into(a, off, &mut v)` (read) and `ctx.write_byte_array_from(p.arr, pos, &payload)` (write).

---

## (C) Stubs / unimplemented

### 10. `[MED]` `StreamDecoder` per-char fallback at `char_arr` fill — no `write_char_array_from` intrinsic exists
`native-io/src/stream_decoder.rs:360-363`

`for (i, &c) in chars.iter().enumerate() { ctx.set_array_element(char_arr, i, Value::Int(c as i32)); }` — UTF-8 decode of a 4 MiB body produces ~4M virtual dispatches.

Fix: add `write_char_array_from(&mut self, arr, off, &[u16]) -> bool` to `NativeContext` (symmetrical with `read_char_array_into` already present) and a `vm/src/vm/vm_exec.rs` override using `copy_nonoverlapping`.

### 11. `[LOW]` `FileChannel.read(buf, position)` / `pread` / async paths not split — current path takes the per-fd mutex even when a pread is sufficient
`native-io/src/file_channel.rs` (whole crate)

Real JDK uses `pread64`/`pwrite64` for position-qualified ops, avoiding the per-fd seek+read pair (and the implicit lock around the file's cursor). Worth a follow-up so concurrent `FileChannel.read(buf, pos)` from many threads on one fd is genuinely parallel.

---

## (D) New angles

### 12. `[LOW]` `io_uring` is too ambitious right now (Linux 5.1+, requires async runtime); skip in favor of `copy_file_range` + `pread`/`pwrite` (findings #6 and #11). Direct buffer pooling already exists (`direct_buffer.rs:147-208`) and is well-bounded; no action needed.

---

## Summary table

| #  | Sev  | Crate              | One-liner |
|----|------|--------------------|-----------|
| 1  | CRIT | native-io          | `net::read0/write0` hold map lock across syscall |
| 2  | CRIT | native-collections | `map_resize` legacy branch keeps 1M/eprintln guard (taken 100% today) |
| 3  | HIGH | native-io          | path validation should run on normalized form (Windows `\\?\` skew) |
| 6  | HIGH | native-io          | `copy_file_range` still missing |
| 4  | MED  | native-collections | dual-write of `Node.hash` (spread) needs a regression test |
| 5  | MED  | native-api / -io   | bulk byte intrinsics swallow bounds errors (no caller checks return) |
| 7  | MED  | native-io          | `FileLock` is synthetic — no real `fcntl`/`LockFileEx` |
| 8  | MED  | native-io          | DirectByteBuffer Cleaner has no phantom-ref hook |
| 10 | MED  | native-io          | StreamDecoder char-array fill still per-element (need `write_char_array_from`) |
| 9  | LOW  | native-io          | async_socket byte-buffer paths missed in round-4 bulk wiring |
| 11 | LOW  | native-io          | `FileChannel.read(buf, pos)` should use `pread`/`pwrite` |
