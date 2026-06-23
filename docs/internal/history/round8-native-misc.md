# Round 8 — native-io / native-collections / native-api audit

Round-7 wave 1+2 regression audit plus follow-ups.

## CRIT

### CRIT-1 — `map_resize` in-place chain mutation breaks lock-free CHM readers
`native-collections/src/lib.rs:1370,1377,1420,1423` — lo/hi split rewrites
`NEXT` on OLD chain nodes. `chm_seg_get` reads OLD buckets and walks NEXT
volatilely, but during resize the OLD bucket still points to N1 while
N1.NEXT has been rewritten to skip "hi" nodes → reader sees truncated chain,
spurious miss. The Acquire/Release fence on buckets-publication covers only
the new array; in-place mutation races readers still on the OLD array.
**Fix:** clone nodes into the new array (allocate fresh `HashMap$Node`s for
lo/hi) instead of mutating `NEXT` on the original chain.

### CRIT-2 — `dbb_free_explicit` can double-free after `Unsafe.freeMemory`
`native-io/src/direct_buffer.rs:471,523` — `unsafe_free_memory` calls
`dbb_free` and removes the unsafe_allocs entry. A follow-up
`freeMemoryExplicit(addr,size)` has `take_unsafe_alloc` return None but still
calls `dbb_free` → second `pool_put` of the same address. Subsequent
`dbb_allocate` of that bucket hands the pointer to two live DirectByteBuffers.
**Fix:** gate `dbb_free` in `dbb_free_explicit` behind `take_unsafe_alloc
.is_some()` OR keep a per-address `freed_set` no-op set.

### CRIT-3 — `atomic_fetch_add_int` silently corrupts non-Int fields
`native-api/src/registry.rs:778-790,795-807` — match on `Value::Int(v)` falls
back to `0` on any non-Int (incl. `Value::Long`). The CAS then writes
`Value::Int(0+delta)` over a Long slot, type-mutating it; every later read
returns 0 because CAS compares bit patterns.
**Fix:** on type mismatch return `0` without performing the CAS, or assert —
silent CAS on wrong type is always wrong.

## HIGH

### HIGH-1 — `ChmMonitorGuard` aborts VM on `monitor_exit` panic
`native-collections/src/lib.rs:160-167` — guard's Drop calls `monitor_exit`
directly. Sites hold the guard while invoking `native_map_put` → arbitrary
`<clinit>`. A clinit OOM unwinds through Drop; if `monitor_exit` itself
panics (wrong owner) double-panic aborts the VM.
**Fix:** wrap `monitor_exit` in `catch_unwind`; eprintln + leak on failure.

### HIGH-2 — Connect-pool workers leak on VM teardown
`native-io/src/socket_channel.rs:163-186` — workers detached, `Sender` is in
`OnceLock` forever so `recv()` never returns Err. Hosted/embedded JVM
teardown leaves workers alive and in-flight connects un-cancelled.
**Fix:** stash `JoinHandle`s + `shutdown: AtomicBool`; on shutdown set flag,
drop sender so workers exit.

### HIGH-3 — `regions_overlap` saturates into false-positive conflicts
`native-io/src/lib.rs:9023-9028` — `size == i64::MAX` (whole-file) makes
`saturating_add` clamp any non-zero position to `i64::MAX`, so `[5..MAX) ∩
[0..3)` reports overlap when disjoint.
**Fix:** special-case `size == i64::MAX`: use `a_pos < b_end` only.

## MED

### MED-1 — `chm_seg_get` reads HASH/KEY non-volatilely
`native-collections/src/lib.rs:14524,14529,14530` — `get_node_key`/
`get_field(NODE_FIELD_HASH)` use plain `get_field`. Writer node-init writes
are not synchronized; reader may observe a half-initialized node.
**Fix:** read HASH/KEY via `get_field_volatile`, or publish nodes via
volatile NEXT write last (Release fence).

### MED-2 — `unsafe_free_memory` silently leaks unknown-size allocations
`direct_buffer.rs:471-484` — when `take_unsafe_alloc` returns None
(reflective bypass) no `release(size)` runs; `Bits.reserved` inflates
permanently → spurious OOM.
**Fix:** log + best-effort `release(1)` on unknown path.

### MED-3 — `by_index_raw` perf claim overstated
`native-io/src/zip_real_jar.rs:464-478` — `by_index_raw` skips decompressor
setup but `find_content` (local-header seek) still runs per entry.
**Fix:** use central-directory accessors when available; fall back only as
last resort.

## LOW

### LOW-1 — `dbb_allocate` accounting drifts for sub-64B sizes
`direct_buffer.rs:162` — sizes <64 collapse to bucket 0 (64B slots) but
`try_reserve(size)` reserves only `size`. RSS vs `Bits.reserved` diverges.
**Fix:** round to bucket size before `try_reserve`.

### LOW-2 — `allocateDirect` lacks page-alignment guarantee
`direct_buffer.rs:230` uses 8-byte alignment; `bb.alignmentOffset(0, 4096)`
returns 0 only by chance. `MapMode.READ_WRITE_SYNC` callers misalign.
**Fix:** align to `page_size()` for sizes ≥ page size.

### LOW-3 — Missing carryover natives
No impls for `ConcurrentLinkedQueue/Deque`, `AtomicReferenceArray`,
`CopyOnWriteArrayList`, `Phaser`/`CompletableFuture`, `Selector.wakeup`
self-pipe. WildFly/EJBCA hit CompletableFuture heavily — schedule round-9.
