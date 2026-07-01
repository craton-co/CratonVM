# Hibernate `type.temporal.*` crash — moving young GC strands lambda refs in native stream/collection intrinsics

**Severity:** High (process aborts rc=1 mid-class, no `@@RESULT`; also flaky SIGSEGV / rc=139).
**Status:** 🟠 PARTIALLY FIXED. Per-native pinning (this commit) corrects the stale-Rust-local bug in the
stream/collection natives that drove the clean `java/lang/Object.<sam>` linkage-error crash — those natives
now re-read every held ref from its pin handle after each allocating dispatch, so they can no longer
dispatch on a relocated slot. The crash rate dropped markedly (repeated InstantTests runs now reach
`@@RESULT/@@DONE` instead of aborting early on the stream path). A **distinct residual** remains: a broader
GC missed-root reclamation that zeroes a connected set of *concurrency / JUnit* objects
(`AbstractQueuedSynchronizer`, `ThreadPoolExecutor`, `ScheduledFuture`, `NodeTestTask`) in a single
collection → "Stale pointer in invokevirtual receiver (all-zero header)" storm → SIGSEGV (~1 in 3 runs at
the default heap). That residual is the **GC root-coverage family** (see #15 lost-tag missed-root and #18
blocked-thread frame `Thread`-mirror reclamation), NOT a stream native — per-native pinning cannot fix it.
**Mode:** Interpreter (default and `--nojit`). **HotSpot (JDK 25):** PASS.
**Affected classes (5):** `org.hibernate.orm.test.type.temporal.{InstantTests, LocalDateTimeTest,
OffsetDateTimeTest, OffsetTimeTest, ZonedDateTimeTest}`.

This is **NOT** a java.time temporal-type binding bug (no `Timestamp`/`Calendar`/`OffsetDateTime`
conversion is involved). It is another manifestation of the GC-root-coverage family already documented in
[`hibernate-bytearraymapping-stackwalk-gc-corruption.md`](hibernate-bytearraymapping-stackwalk-gc-corruption.md):

> CV native code holds Java object refs in Rust locals/`Vec`s across allocating `ctx` calls
> (`invoke_virtual` / `create_string` / `alloc_*`) **without re-reading them from a pin** afterwards.
> `safe_native_call` pins a native's *args*, but the native's local copies still go stale when the moving
> young-gen GC relocates the object (the pin is remapped; the bare local is not).

## Abort reason

```
WARN NoSuchMethodError method="java/lang/Object.<sam>(...)" caller="…@pc=…"
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error:
    no such method: java/lang/Object.<sam>(...)
```

`<sam>` is `accept` / `compare` / `apply`. The receiver is a lambda-proxy `Consumer`/`Comparator`/`Function`
whose header reads `class_id = 0` (→ resolves on bare `java/lang/Object`, which has no SAM) because the
young GC corrupted/zeroed it. Under different timing it instead manifests as a hard **SIGSEGV** (rc=139,
heap-corruption cascade through `gen_heap::set_field` "out-of-bounds … class_id=ClassId(0)" + "Stale pointer
in invokevirtual receiver (all-zero header)").

## Root cause

The JUnit `@ParameterizedClass` machinery and store cleanup drive lambdas through CV's synthetic
stream/collection intrinsics, which loop calling `ctx.invoke_virtual(lambda, "accept"/"compare"/"apply", …)`
while holding the lambda **and** the materialized `stream_elements()` `Vec` in plain Rust locals. The lambda
body allocates → young GC. Because these tests run in the interpreter (no JIT frame on stack), the **moving
(Cheney)** young collector runs: it relocates the lambda proxy and remaps `native_pin_roots`, but cannot
rewrite the native's Rust-local copy → the native then dispatches `accept`/`compare`/`apply` on the stale
(post-evacuation, zeroed) from-space slot. This is the exact failure mode the non-moving sweep protects JIT
register/spill slots from; a native intrinsic's Rust frame holds the same kind of un-rewritable raw pointer.

A **different culprit native each run** (the corruption is timing-dependent), confirmed via
`CRATONVM_DBG_STRAYSTACK=1` + `NativeMethodRegistry::flush_native_ring_names()` →
`native_ring::name_of(cb)` (PDB symbolization is useless — the release profile strips closure symbols):
- `java/util/stream/Stream.forEach` → `native_stream_for_each` (native-collections) — **eager** path
  (`stream_elements` Vec + consumer held across the `accept` loop). The **lazy** path is already pinned.
- `java/util/stream/Stream.sorted(Comparator)` → `native_stream_sorted_cmp` — `comparator` (arg) + `elems`
  Vec held across the merge-sort comparator dispatch.
- `java/util/Spliterator.tryAdvance` / `forEachRemaining` → `native_spliterator_try_advance` /
  `native_spliterator_for_each_remaining` (native-collections — these **win** over the native-builtins
  `phases_late.rs` duplicates via last-writer-wins registration).
- `java/util/ArrayList.forEach`, and likely other `Collection`/`Map`/`Stream` op natives.

## Evidence

- `-Xmx8g` (suppresses young GC) → **PASSES** (`@@RESULT … ok=112 failed=0 aborted=92`, rc=0).
- Default heap → ~50–70 % crash; **`--nojit` also crashes** (different site) → not JIT-specific.
- `proxies_len = 1144` ≪ `MAX_LAMBDA_PROXIES` (100 000) → **not** the lambda-proxy cap.
- Instrumented `[lambda-stray]` capture (in `invoke_virtual`'s `resolved_from_receiver &&
  class_name=="java/lang/Object"` arm): `recv_cid=0, recv_in_pins=true, native_active=true`.

## Fix

**Path 1 (DONE for the lambda-stale-local crash) — per-native pinning.** Applied the established
`pin_native_root` / `read_native_pin` / `unpin_native_roots` pattern to the stream/collection natives that
hold a ref across `ctx.invoke_virtual` (the lazy `native_stream_for_each` path, `native_al_for_each`,
`native_stream_peek`, `drain_spliterator_to_array`, `materialize_lazy_stream` already did this). Fixed in
`native-collections/src/lib.rs` (new `read_pinned_elem` helper + index-permutation sort to keep pinned
elements stable across a reorder): `native_stream_for_each` (eager path), `native_stream_sorted` /
`native_stream_sorted_cmp`, `native_spliterator_for_each_remaining`, `native_stream_filter`,
`native_stream_map`, `native_map_for_each`, `native_hs_for_each`. Pattern: pin the lambda + every
materialized element (and freshly-produced results), re-read each from its handle before the (allocating)
dispatch, and re-read `this` before any post-loop `set_field`. This removes the `[lambda-stray]` crash.

**Additional sweep (2026-07-01):** the previously lower-priority siblings with the same stale-native-local
pattern are now pinned as well in `native-collections/src/lib.rs`: `make_stream` / derived-stream construction,
`Stream.iterator` / `toArray`, `native_stream_distinct`, `native_stream_flat_map`, `native_stream_peek`,
`native_stream_reduce_*`, `native_stream_{any,all,none}_match`, `native_stream_{min,max}`,
`native_stream_map_to_int`, plus `native_al_sort_comparator`, `native_al_remove_if`, and
`native_al_replace_all`. The native-collections mock now has a callback-triggered moving-GC simulation and
regression coverage for `Stream.forEach` and `ArrayList.removeIf`. This broadens the per-native pinning fix;
it does **not** claim to close the distinct residual GC root-coverage family described above.

**Additional callback sweep (2026-07-01, follow-up):** the same pin/re-read pattern now covers more
collection callback loops that can be reached from real app code: `LinkedHashMap.forEach`,
`ArrayDeque.forEach`, `TreeMap.forEach`, `TreeSet.forEach`, both `ConcurrentHashMap.forEach` overloads
registered here, and `Collections$UnmodifiableList$ListItr.forEachRemaining`. Focused regressions now
simulate a moving GC during callbacks for `LinkedHashMap.forEach` and `TreeMap.forEach` in
`native-collections/tests/gc_native_pins.rs`.

**Additional map-functional sweep (2026-07-01, follow-up):** `HashMap.computeIfAbsent`,
`HashMap.compute`, `HashMap.computeIfPresent`, `HashMap.merge`, `HashMap.replaceAll`, plus the
analogous `TreeMap.computeIfAbsent` / `TreeMap.merge` paths now pin and re-read their map receiver,
callback, keys, old values, merge values, and callback results across `invoke_virtual`. The same
generic HashMap helpers are used by `ConcurrentHashMap.compute`, `merge`, and `replaceAll` after segment
selection. The native-collections mock relocation hook now also rewrites heap fields/array slots for
moved pins, and `gc_native_pins.rs` covers `HashMap.replaceAll` under callback-triggered moving GC.

**Additional CHM bulk sweep (2026-07-01, follow-up):** the `ConcurrentHashMap` bulk operations
registered here (`forEachEntry`, `forEachKey`, `forEachValue`, and `search`) now pin and re-read their
callback plus the collected key/value snapshots across each callback dispatch. Focused regressions cover
`ConcurrentHashMap.forEachKey` and `ConcurrentHashMap.search` under callback-triggered moving GC.

**Do NOT** try to fix this by forcing the non-moving sweep when a native is active: it re-triggers the
documented [`HIB-CV-33`](HIB-CV-33-sigsegv-execute-fault-joined-inheritance-sf-build.md) precise-root
reclaim gap (`gen_heap.rs` — the non-moving sweep without conservative roots reclaims live precise-rooted
objects), which zeroes the *pinned* proxy in place (`recv_in_pins=true` yet `recv_cid=0`). Pinning in the
moving Cheney collector itself is not possible ("a semispace cannot pin").

**Do NOT** try to fix this by forcing the non-moving sweep when a native is active: it re-triggers the
documented [`HIB-CV-33`](HIB-CV-33-sigsegv-execute-fault-joined-inheritance-sf-build.md) precise-root
reclaim gap (`gen_heap.rs` — the non-moving sweep without conservative roots reclaims live precise-rooted
objects), which zeroes the *pinned* proxy in place (`recv_in_pins=true` yet `recv_cid=0`). Pinning in the
moving Cheney collector itself is not possible ("a semispace cannot pin").

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm> --java-home "C:/Program Files/Java/jdk-25" \
    @common.args CratonRunner <listfile-with-InstantTests> 0
```
Flaky — loop ~10× at the default heap. Add `CRATONVM_DBG_STRAYSTACK=1` to name the culprit native.
