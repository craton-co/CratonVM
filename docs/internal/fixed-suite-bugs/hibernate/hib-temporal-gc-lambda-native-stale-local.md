# Hibernate `type.temporal.*` crash — moving young GC strands lambda refs in native stream/collection intrinsics

**Severity:** was High (process aborts rc=1 mid-class, no `@@RESULT`; also flaky SIGSEGV / rc=139).
**Status:** ✅ **FIXED on dev, ARCHIVED 2026-07-07.** The native-stale-local family this doc identified was
closed at three levels: (1) the per-native `pin_native_root`/`read_native_pin` sweeps
(native-collections + scheduled pump, then the 242-site `phases_late.rs` sweep, merged `3240cb75`),
(2) the interpreter-lost-tag missed-root fix (`0abb64ba` — the likely mechanism of the "broader
concurrency/JUnit missed-root residual" this doc described: all-zero-header storm over AQS/TPE/
ScheduledFuture/NodeTestTask), and (3) the keystone `invoke_shared`/`invoke_special_shared` fix
(`5732a1e1`): object args are now pinned across the class-load + `<clinit>` window, the stale-args hole
no per-native pin could cover.
**Acceptance validation (2026-07-07, Linux probe host, `hibernate-orm-harness`, default heap, default
JIT):** ZERO instances of THIS doc's crash signatures anywhere (`Object.<sam>` linkage error,
all-zero-header stale-receiver storm, SIGSEGV, rc=1 mid-class abort). Per-class:
`InstantTests`/`LocalDateTimeTest`/`OffsetTimeTest` completed to `@@RESULT` fully clean;
`OffsetDateTimeTest` completed (exit=0) with 2 gracefully-degraded `mark_young … implausible extent`
guard rejections; `ZonedDateTimeTest` never crashes but LIVELOCKS in a continuous
`mark_young: rejecting object … implausible extent` + `[A2] BREADCRUMB — NO allocation record` loop —
that was a **distinct** corruption face (garbage-header object repeatedly reachable by the
young mark), now fixed and archived in
[`../gcstress-residual-corruption-faces-FIXED.md`](../gcstress-residual-corruption-faces-FIXED.md)
(see its 2026-07-07 repro note), NOT this doc's (fixed) dispatch-crash family. The classes also surface
a **functional, non-GC bug** — duplicated JDBC `?` placeholders in generated SQL — tracked as
[`hib-temporal-sql-parameter-placeholder-duplication-FIXED.md`](hib-temporal-sql-parameter-placeholder-duplication-FIXED.md) (FIXED 2026-07-07 — it was the reopened JIT reason-8 imprecise-resume corruption, not a string bug).
**Mode:** Interpreter (default and `--nojit`). **HotSpot (JDK 25):** PASS.
**Affected classes (5):** `org.hibernate.orm.test.type.temporal.{InstantTests, LocalDateTimeTest,
OffsetDateTimeTest, OffsetTimeTest, ZonedDateTimeTest}`.

**2026-07-04 OSR-default/residual sweep (historical):** was left OPEN pending exactly the validation that
has now run (see Status above).

This is **NOT** a java.time temporal-type binding bug (no `Timestamp`/`Calendar`/`OffsetDateTime`
conversion is involved). It is another manifestation of the GC-root-coverage family already documented in
[`hibernate-bytearraymapping-stackwalk-gc-corruption.md`](../hibernate-bytearraymapping-stackwalk-gc-corruption.md):

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
`../../../../native-collections/src/lib.rs` (new `read_pinned_elem` helper + index-permutation sort to keep pinned
elements stable across a reorder): `native_stream_for_each` (eager path), `native_stream_sorted` /
`native_stream_sorted_cmp`, `native_spliterator_for_each_remaining`, `native_stream_filter`,
`native_stream_map`, `native_map_for_each`, `native_hs_for_each`. Pattern: pin the lambda + every
materialized element (and freshly-produced results), re-read each from its handle before the (allocating)
dispatch, and re-read `this` before any post-loop `set_field`. This removes the `[lambda-stray]` crash.

**Additional sweep (2026-07-01):** the previously lower-priority siblings with the same stale-native-local
pattern are now pinned as well in `../../../../native-collections/src/lib.rs`: `make_stream` / derived-stream construction,
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
`../../../../native-collections/tests/gc_native_pins.rs`.

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

**Additional scheduled-pump sweep (2026-07-03, follow-up):** `../../../../native-builtins/src/scheduled_pump.rs`
now pins the stored periodic-task runnable before pumping accrued ticks, re-reads it from the pin before and
after each `Runnable.run()` dispatch, and writes the current address back to the task record. This covers the
case where a pump call fires more than one accrued tick and the first callback allocates/triggers a moving GC:
the old loop reconstructed the runnable once, then reused that Rust local for every later callback in the same
pump. Regression `scheduled_pump::tests::pump_re_reads_runnable_pin_after_callback_gc` simulates relocation
during the first callback and asserts the second callback receives the remapped object.

## Validation (2026-07-03 follow-up)

- `cargo test -p cratonvm-native-builtins scheduled_pump::tests::pump_re_reads_runnable_pin_after_callback_gc -- --nocapture`
- `cargo test -p cratonvm-native-builtins scheduled_pump::tests -- --nocapture`
- `cargo build -p cratonvm-cli --bin cratonvm`
- Unique binary smoke: `target/debug/cratonvm-hib-temporal-gc-lambda-20260703.exe --version` => `cratonvm 0.3.0`
- Hibernate runner validation was **not run in this worktree**: `../../../../apps/hib-suite-runner` / `common.args` are absent.

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
