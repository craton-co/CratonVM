# Round 4 review: native-collections, native-io, native-api

Reviewer: senior Rust performance review (round 4).
Scope: `native-collections/`, `native-io/`, `native-api/`.
Audit-fixes from rounds 1-3 (4 bulk-array intrinsics on `NativeContext`
with VM ptr-copy override, `VmHeap` override using
`ptr::copy_nonoverlapping`) are out of scope; the deferred "wire bulk-array
intrinsic callers" item is not re-stated generically, but concrete
call sites worth a one-line migration are flagged.

Findings ordered by impact.

---

## native-collections

### 1. `[CRIT]` `map_hash_key` iterates UTF-8 bytes instead of UTF-16 chars — wrong hash for any non-ASCII key
`native-collections/src/lib.rs:1086-1094`

```rust
if let Some(s) = ctx.read_string(key) {
    let mut h: i32 = 0;
    for ch in s.bytes() { ... }   // BUG: UTF-8 byte iteration
    return h ^ (h >> 16);
}
```

For ASCII this happens to match Java's `String.hashCode`, but for any
non-ASCII char (e.g. `é` = `0xC3 0xA9` in UTF-8 vs char `0x00E9`) we
mix in 2-3 bytes instead of the single UTF-16 code unit. The bucket
index computed by our native `put`/`get` therefore disagrees with the
bucket index that any bytecode path (`HashMap.resize` reachable via the
JDK class, `keySet().contains` fall-through, third-party reflection
that reads `table`) computes via `String.hashCode()` → silent
`containsKey == false` for non-ASCII keys that round-trip through both
native and bytecode paths. Also incompatible with the JDK-`Node.hash`
field we dual-write.

Fix: iterate UTF-16 code units —
`s.encode_utf16().for_each(|cu| h = h.wrapping_mul(31).wrapping_add(cu as i32));`
This is what `java.lang.String.hashCode` does on the real JVM.

### 2. `[HIGH]` `al_ensure_capacity` per-element growth copy — should be `bulk_array_copy`
`native-collections/src/lib.rs:337-343` (also `native_al_trim_to_size`
807-818, `native_al_sub_list` 891-895, `native_al_add_all` 865-868,
`map_resize` re-bucketing 1234-1274)

Every ArrayList grow path loops via `get_array_element` /
`set_array_element` — virtual-dispatch + `Value` boxing per element.
A 1M-entry `ArrayList.addAll` triggers `log2(N)` grows, each copying
N/2 to 2N elements through the trait. `NativeContext::bulk_array_copy`
already has a `copy_nonoverlapping` override in `VmHeap`; using it
collapses each grow into one memcpy.

Fix: replace each `for i in 0..n { set_array_element(.., get_array_element(..)) }`
with `ctx.bulk_array_copy(old_buf, 0, new_buf, 0, copy_len);` (and
`my_size` for `addAll`).

### 3. `[MED]` `native_al_to_string` materialises every element as `String` then `join`s — quadratic for large lists, plus per-element `obj_to_display_string` invokes `toString` virtually
`native-collections/src/lib.rs:824-841`

For an N-element list of String values the path is:
N virtual `toString` invokes → N heap Strings → N `Vec<String>` ->
`Vec::join` (which allocates a fresh String, walks every byte twice
because it cannot size-hint accurately). For N=10K this is the
hottest user-facing perf cliff after Logger.

Fix: pre-size a `String::with_capacity(size * 16)`, push `"["`, then
`if i > 0 { push_str(", ") }; push_str(&obj_to_display_string(...))`
in the loop, push `"]"`. Saves the intermediate `Vec<String>` plus
`join`'s pre-pass.

### 4. `[MED]` `map_resize` cycle-guard prints + 1M-step bound is per-bucket, allowing 2^30 total iterations
`native-collections/src/lib.rs:1234-1275`

The `steps > 1_000_000` cycle break is *per bucket*. With `MAP_MAX_CAPACITY
= 1<<30` and a pathological single-bucket chain (e.g. all keys colliding
on the legacy synthetic layout), this loop walks up to one billion
nodes per bucket before bailing — but only after `eprintln!` per
break, polluting stderr in long-running services. The `1_000_000`
also exceeds plausible bucket lengths by 5+ orders of magnitude and
turns a true cycle into a 1M-step + stderr-spam pattern instead of
fast detection.

Fix: replace per-step counter with a tortoise/hare cycle detector
(O(N) but with no fixed bound, terminates exactly at first revisit)
or with a guard of `2 * old_cap` steps (the chain in any HashMap
bucket is bounded by total entries, so total steps over all buckets is
trivially `size`). Drop `eprintln!` to a `tracing::warn!`-gated path so
production logs aren't spammed.

### 5. `[LOW]` `map_keys_equal` and `map_hash_key` both check `read_string` / `unbox_wrapper` on every key comparison — repeated for every chain step
`native-collections/src/lib.rs:1088,1096,1116,1124-1156`

`unbox_wrapper` walks the field table to find `Integer.value` /
`Long.value` etc.; `read_string` resolves field indices. On a 64-bucket
HashMap with a 10-deep chain, `get(key)` invokes `read_string(key)` once
and then `read_string(node_key)` 10 times. The receiver-key Strings
never change inside a single `get`/`put`/`containsKey` call.

Fix: extract the receiver-key shape once (compute a small enum
`KeyKind { Str(String), Int(i32), Long(i64), Other(ObjectRef) }`),
pass it into the bucket walk, and compare against the per-node key
shape inside the loop. Saves O(chain_len) `resolve_field_index`
walks per HashMap op.

---

## native-io

### 6. `[HIGH]` FileInputStream / FileOutputStream / RandomAccessFile bulk read/write loop element-by-element through `set_array_element`/`get_array_element`
`native-io/src/lib.rs:651-653` (FIS `readBytes`),
`native-io/src/lib.rs:677-679` (FIS `read([B)`),
`native-io/src/lib.rs:864-869` (FOS `writeBytes`),
`native-io/src/lib.rs:893-897` (FOS `write([B)`),
`native-io/src/random_access_file.rs:306-308,360-365`,
`native-io/src/stream_decoder.rs:269-272,330-334,339-343,368-370,378-380`,
`native-io/src/stream_encoder.rs:157-159,188-193`,
`native-io/src/zip_real_jar.rs:411-413` (the existing `TODO(audit MED, perf)`),
`native-io/src/pipe.rs:478-484` (Sink), `pipe.rs:556` area (Source).

Every file-and-stream native materialises a `Vec<u8>` of the requested
length and then either reads from / writes to the Java `byte[]` one
element at a time. For a 4 MiB read this is 4M `Value::Int` boxes plus
4M virtual `set_array_element` dispatches. `NativeContext` already
exposes `write_byte_array_from` / `read_byte_array_into` with a
`ptr::copy_nonoverlapping` override; the migration is essentially
mechanical.

Fix per call site:
- `native_fis_read_bytes`: replace the `for ... set_array_element` with
  `ctx.write_byte_array_from(arr, off, &buf[..n]);`
- `native_fos_write_bytes`: replace the materialisation loop with
  `ctx.read_byte_array_into(arr, off, &mut buf[..]);` then `write_bytes`.
- Same shape for RAF, StreamDecoder buffer plumbing, StreamEncoder
  byte emission, ZipFile `build_byte_array_input_stream`, and Pipe
  Sink/Source `ByteBuffer` ↔ socket fast paths.

Expected impact: linear factor 50-100x on large reads (`copy_nonoverlapping`
vs ~50 cycles/element of virtual dispatch + box/unbox).

### 7. `[HIGH]` `RandomAccessFile::with_file` holds the *global* `handle_map` mutex across the entire file I/O — serialises every RAF op in the process
`native-io/src/random_access_file.rs:84-90`

```rust
fn with_file<F, R>(handle: i64, f: F) -> Option<R> ... {
    let mut map = handle_map().lock();        // global mutex held
    map.get_mut(&handle).map(|h| f(&mut h.file))   // ... across read/write/seek
}
```

A blocking `read` on RAF handle A freezes every concurrent RAF
operation in the VM, including independent handles B, C, ... — this is
the same anti-pattern that `FileDescriptorTable` already fixed (see
its B3/B4 audit comment at `native-api/src/fd_table.rs:57-63` and the
`Arc<FileEntry>` clone-and-release in `get_entry`).

Fix: wrap each `RafHandle.file` in `Arc<Mutex<File>>`, take a clone of
the inner `Arc` under the table lock, drop the table guard, then
lock the per-handle mutex for the actual I/O. Mirrors the
`fd_table::get_entry` pattern exactly.

### 8. `[HIGH]` `validate_path` rejects every path containing the literal substring `".."` — false-positive on legitimate filenames; security check is also bypassed entirely when validation disabled (default-on but call sites guard differently)
`native-io/src/lib.rs:81-113`

```rust
if path.contains("..") {
    return Err(... SecurityException ...);
}
```

Substring match flags `foo..bar.txt`, `..something/x` (a directory
literally named `..something`), `C:\release..2025\file` and even paths
under any directory whose name contains two consecutive dots (e.g.
`/var/log/...rotated/x`). Conversely, on Windows the check happens
*before* normalisation, so a hostile `subdir/%2e%2e/etc/passwd`
encoding survives untouched (we don't URL-decode, but the JDK File
ctor accepts Win32 `~` short names which also escape this check).

Worse: when validation is disabled (line 82), even the null-byte
check is skipped — so `Files.delete("/etc/passwd\0/safe")` would
truncate at the null inside Rust's `CString` conversion and delete
the wrong file when path validation is off.

Fix: (a) reject ".." only when it is a *path component*
(`Path::new(path).components().any(|c| matches!(c, Component::ParentDir))`),
(b) keep the null-byte check unconditional even when full validation
is disabled.

### 9. `[MED]` `file_channel::transfer_via_sendfile` uses `sendfile(2)` for file→file but Linux's preferred file-to-file zero-copy syscall is `copy_file_range(2)` — single syscall, kernel may use reflink/server-side copy
`native-io/src/file_channel.rs:415-494`

`sendfile(2)` was historically restricted to socket destinations; the
`O_LARGEFILE` and reflink-aware fast path on modern kernels (and
network filesystems) is `copy_file_range(2)`. For file→file transfers
between regular files on the same filesystem, `copy_file_range` can
avoid the userspace round-trip entirely (and falls back to in-kernel
copy on cross-fs). Our `clone_file(dst_fd)` already confirms the dst
is a regular file before entering this branch.

Fix: probe `copy_file_range` first when both src/dst are regular
files; fall back to `sendfile` for the socket-dst case (which we
don't currently hit because the `Ok(None)` returns from `clone_file`
for non-file dst, but the dispatch path leaves room for it).

### 10. `[MED]` `direct_buffer::cleaners()` uses `std::HashMap<i32, CleanerEntry>` — identity-style keys, FxHashMap is strictly better; also a single global mutex on the hot Cleaner path
`native-io/src/direct_buffer.rs:47,291-294`

The cleaner_id keyspace is a sequential `AtomicI32` counter — there's
zero adversarial-input concern that warrants SipHash. The hot path is
`register_cleaner` → `fire_cleaner` (every direct-buffer alloc + free),
and every op pays SipHash overhead today. The std HashMap import is
explicitly retained at line 47 while `native-api`/`registry.rs`/`fd_table.rs`
all migrated to FxHashMap in T10.9.B.

Fix: switch to `FxHashMap<i32, CleanerEntry>` (already a dependency).
Optional: shard the mutex by `id & 0xF` to remove the cleaner-table
contention point under high direct-buffer churn (the WP3.5 acceptance
test exercises this).

### 11. `[MED]` `net::net_sockets()` and `net::net_opts()` use `std::HashMap<i32, ...>` — same reasoning as #10
`native-io/src/net.rs:21,158-170`

`(i32, i32, i32)` opts key and `i32` socket-id are both internal
monotonic IDs. Replace `std::collections::HashMap` with
`rustc_hash::FxHashMap`. `net_opts` is hit on every
`get/setIntOption0` from `sun.nio.ch.Net`, which Netty pings
frequently during channel setup.

---

## native-api

### 12. `[MED]` `NativeMethodRegistry::find` allocates a `Vec<String>` of descriptor variants on *every* miss, even when the descriptor is already clean
`native-api/src/registry.rs:1477-1531`

```rust
if let Some(cb) = self.methods.get(&key).copied() { return Some(cb); }
let mut variants: Vec<String> = Vec::with_capacity(4);   // allocated on every miss
let trimmed = descriptor.trim_matches(...);              // also allocates on the .to_string() that follows
```

Method-resolution misses happen constantly (e.g. when the dispatcher
probes a parent class chain looking for a native override on a class
where none exists). Every probe of a class without a native method
allocates 4-slot Vec + intermediate String. Hot path.

Fix: gate the variants block on `if descriptor.contains(|c: char|
c.is_ascii_whitespace() || c == '\0' || c == '\r' || c == '\n')
|| descriptor_needs_object_repair(descriptor)` first; only build the
Vec when we actually need to fall back. For typical clean descriptors
the function then returns `None` with zero allocation after the
single `methods.get`.

### 13. `[LOW]` `NativeMethodRegistry::find_by_method_descriptor` is O(N) over the entire registrations log on every slow-recovery dispatch
`native-api/src/registry.rs:1601-1619`

Used for synthetic-allocated objects routed through `java/lang/Object`;
documented as gated to the NSME-recovery slow path. With ~3000+
registrations and one such recovery per offending invocation, the
linear scan is meaningful when an app holds a hot `Pattern.matcher`
loop.

Fix: maintain a parallel `FxHashMap<(u64, u64), NativeCallback>` keyed
by `(fnv_pair(method), fnv_pair(descriptor))` — populated on every
`register()` (one extra insert), consulted in O(1) here. Same
collision-probability calculus as the primary table.

---

## Summary table

| # | Sev | Crate | One-liner |
|---|-----|-------|-----------|
| 1 | CRIT | native-collections | UTF-8 bytes used instead of UTF-16 chars in `map_hash_key` |
| 2 | HIGH | native-collections | ArrayList grow / addAll / sublist / trim / map_resize per-element copy |
| 6 | HIGH | native-io | FIS/FOS/RAF/StreamDecoder/StreamEncoder/ZipFile/Pipe per-byte boxing |
| 7 | HIGH | native-io | RAF holds global handle-map mutex across blocking I/O |
| 8 | HIGH | native-io | `..` substring rejection + null-byte check skipped when validation off |
| 3 | MED | native-collections | ArrayList.toString via Vec+join instead of single String::with_capacity |
| 4 | MED | native-collections | map_resize cycle-guard is per-bucket 1M with stderr spam |
| 9 | MED | native-io | Missing `copy_file_range` fast path for file→file transferTo |
| 10 | MED | native-io | direct_buffer cleaners use std HashMap + single global mutex |
| 11 | MED | native-io | net_sockets / net_opts use std HashMap (identity keys) |
| 12 | MED | native-api | NativeMethodRegistry::find allocates Vec on every miss |
| 5 | LOW | native-collections | map_keys_equal repeats receiver-key shape detection per chain step |
| 13 | LOW | native-api | find_by_method_descriptor is O(N) over registrations log |
