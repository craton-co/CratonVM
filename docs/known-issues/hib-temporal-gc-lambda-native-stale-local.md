# Hibernate `type.temporal.*` crash — moving young GC strands lambda refs in native stream/collection intrinsics

**Severity:** High (process aborts rc=1 mid-class, no `@@RESULT`; also flaky SIGSEGV / rc=139).
**Status:** 🟠 OPEN (root-caused; fix in progress — per-native pinning).
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

**Path 1 (in progress) — per-native pinning.** Apply the established
`pin_native_root` / `read_native_pin` / `unpin_native_roots` pattern to every stream/collection native that
holds a ref across `ctx.invoke_virtual` (the lazy `native_stream_for_each` path, `drain_spliterator_to_array`
and `materialize_lazy_stream` already do this correctly — use them as the template). Pin the lambda, the
backing array, `this`, and every materialized element; re-read each from its pin handle on every loop
iteration; re-read `this` before any post-loop `set_field`.

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
