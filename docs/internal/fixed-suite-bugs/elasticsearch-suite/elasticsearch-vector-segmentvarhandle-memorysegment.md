# Elasticsearch vector codecs missing SegmentVarHandle MemorySegment access

Status: FIXED on `dev` (2026-07-03) — see "What actually merged" below

Date observed: 2026-07-02

## Summary

Vector codec and vector query tests failed under CratonVM when code reached
JDK foreign-memory var-handle accessors. HotSpot passed the same
representative classes.

Observed signatures:

```text
java.lang.NoSuchMethodError:
java/lang/invoke/SegmentVarHandle.set(Ljava/lang/foreign/MemorySegment;JI)V
```

```text
java.lang.NoSuchMethodError:
java/lang/invoke/SegmentVarHandle.get(Ljava/lang/foreign/MemorySegment;J)B
```

## Root cause

`ValueLayout.OfXxx.varHandle()` (`java.lang.foreign`) runs genuine real-JDK
bytecode and returns a real, final, package-private
`java.lang.invoke.SegmentVarHandle` — never one of CratonVM's synthetic
`VarHandle` objects. `SegmentVarHandle` declares no `get`/`set`/etc. itself;
those are inherited, `native`, signature-polymorphic methods declared once on
the abstract `VarHandle` base and dispatched by the JVM using the *call
site's* concrete descriptor (e.g. `(Ljava/lang/foreign/MemorySegment;J)B`).
CratonVM had zero code referencing `SegmentVarHandle` anywhere, so method
resolution fell through to `NoSuchMethodError`.

## What actually merged

**Two independent sessions fixed this in parallel** — discovered when
verifying my own fix (branch `fix/es-vector-segmentvarhandle-memorysegment`,
never merged) against current `dev`, which by then already had commit
`76d20637` ("fix(ffm): implement SegmentVarHandle MemorySegment access, fixing
TSDB docvalues crash + storedfields hang") from a concurrent session. That
fix is structurally *better* than mine in two ways I hadn't handled:

- Resolves the real JDK classes' fields **by name** (`ctx.resolve_field_index`)
  rather than hardcoded numeric slots — more robust against JDK version drift.
- Reads the `SegmentVarHandle`'s own `enclosing`/`offset`/`be` fields, so it
  correctly supports **byte-order-swapped** (`ByteOrder.BIG_ENDIAN`) layouts
  and enforces **read-only segment** writes throwing
  `IllegalStateException` — my version silently assumed native byte order and
  never checked read-only.

However, testing dev's `76d20637` against my own verification battery (built
from real-JDK-25-matching test programs — see below) found a **real,
reproducible bug**: `segment_raw_access`'s heap-segment branch read
`HeapMemorySegmentImpl.offset` and used it directly as a 0-based
`ctx.get/set_array_element` index. That field is `Unsafe`-style and carries
`Unsafe.arrayBaseOffset(elementType)` baked in (CratonVM's `Unsafe` reports 16
for every array type), so a fresh full-array segment's `offset` is 16, not 0.
Every `MemorySegment.ofArray(byte[])`-backed access landed 16 bytes past its
intended target — silently corrupting adjacent data when still in-bounds,
throwing `IllegalStateException: Out of bound access on heap MemorySegment`
when it overflowed the backing array. Confirmed with `HeapCheck`/`SliceCheck`
(below): both FAILED against dev's `76d20637` and PASSED against my
now-abandoned parallel implementation.

Fixed with a small follow-up (`fix/segmentvarhandle-heap-abase-offset`,
merged into `dev`): subtract the `ABASE` (16) constant from
`HeapMemorySegmentImpl.offset` before using it as an array index, in
`segment_raw_access`. My own parallel implementation (exact-descriptor
registration directly on `SegmentVarHandle`, described below for reference)
was **not merged** — dev's fix, once corrected, is the better design.

## My abandoned approach (reference only — not what's in dev)

Registered `get`/`set`/`getVolatile`/`setVolatile`/`getOpaque`/`setOpaque`/
`getAcquire`/`setRelease` directly on `java/lang/invoke/SegmentVarHandle` with
the concrete `(MemorySegment, long) -> carrier` descriptor for every
primitive carrier, found by the VM's existing superclass-chain native lookup
with no `vm_exec.rs` changes. (Dev's merged fix instead broadens the
`is_vh` signature-polymorphic-dispatch check in `vm_exec.rs` to match any
`java/lang/invoke/*` class containing "VarHandle", mirroring the existing
`is_mh` broadening — also a valid approach, and the one that's actually live.)

Two non-obvious traps I hit, which the *other* session independently hit too
(their commit message documents the same finding):

1. **Calling back into the segment's own methods (`invoke_virtual` for
   `unsafeGetBase`/`unsafeGetOffset`/`byteSize`) from within this
   signature-polymorphic native crashed** (`SIGSEGV`, reproduced with and
   without `--nojit`). Root cause not fully chased down; both fixes work
   around it by reading the real classes' fields directly instead. Field
   slots verified against the bundled JDK 25's actual class files (`javap
   jdk.internal.foreign.{AbstractMemorySegmentImpl, NativeMemorySegmentImpl,
   HeapMemorySegmentImpl}`) and cross-checked at runtime via temporary debug
   instrumentation:
   - `AbstractMemorySegmentImpl`: `length@0` (long), `readOnly@1`, `scope@2`
   - `NativeMemorySegmentImpl` / `MappedMemorySegmentImpl`: + `min@3` (long) —
     the fully-resolved absolute address (slicing bakes the offset into `min`)
   - `HeapMemorySegmentImpl<T>`: + `offset@3` (long, `Unsafe`-style, includes
     the array-base-offset constant — see the bug this doc is about), `base@4`
     (the backing array)

2. **`Arena.allocate()` addresses are not real OS pointers.** They bottom out
   through `SegmentFactories.allocateNativeInternal` → `Unsafe.allocateMemory0`,
   which CratonVM backs with a bounds-checked *virtual arena* (tagged handles,
   bit 62 set — see `unsafe_arena_allocate`/`ARENA_TAG` in
   `../../../../native-builtins/src/lib.rs`), not a dereferenceable pointer. Casting a
   tagged handle straight to `*const u8` segfaults. Dev's merged fix detects
   this via `unsafe_arena_contains`/`copy_out`/`copy_in`; mine used
   `unsafe_arena_addr_is_tagged` + the per-width `unsafe_arena_get_*`/`put_*`
   accessors. Equivalent in effect.

## Verification

Four scenarios were exercised directly against real JDK 25 and cross-checked
against both my (abandoned) fix and dev's (merged + ABASE-corrected) fix —
matching exactly in every case for the final merged version:

- Off-heap `Arena.allocate()` segment, all 8 primitive carriers, get + set +
  getVolatile/setVolatile, plus an out-of-bounds access correctly throwing.
- Heap `MemorySegment.ofArray(byte[])` segment — **this one FAILED against
  dev's pre-ABASE-fix `76d20637`** (`arr[4]=0` instead of the written value).
- A slice (`MemorySegment.asSlice(offset, size)`) of a heap segment —
  **also FAILED pre-fix** (read/wrote 16 bytes off; threw
  `IllegalStateException` on a subsequent access that pre-fix arithmetic
  pushed out of the backing array's real bounds).
- A real memory-mapped file (`FileChannel.map(...)`) segment, full byte range
  round-tripped correctly (this path was never affected — off-heap addressing
  is untouched by the ABASE bug).

Suite repro (updated index — the original bug doc's index 1341 shifted to
1349/1373 as the sorted class list grew by 2026-07-03):

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1349 -Count 1 -Parallel 1 -TimeoutSec 700 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir <suite-runner>\.suite `
  -Exe <fixed-binary>.exe
```

`ES920DiskBBQBFloat16VectorsFormatTests`: with the ABASE fix, **17/17 real
test methods pass** (only the pre-existing, unrelated suite-level performance
timeout meta-failure remains — see Residuals).

`ES818BinaryQuantizedVectorsFormatTests`: `NoSuchMethodError` gone; 17/25
pass. The 8 non-passing are a **pre-existing, distinct** issue — already
tracked in
[elasticsearch-vector-scorer-zero-results.md](../known-issues/elasticsearch-vector-scorer-zero-results.md).

## Residuals (not blocking — tracked separately)

1. **Performance**: these vector-format test classes take ~600–620s under
   CratonVM JIT-on vs. ~18s on HotSpot (~33x), tripping the Lucene
   randomizedtesting framework's own internal ~580s suite timeout and
   producing a cascading "Test abandoned / Suite timeout exceeded" failure
   independent of correctness. A `ClassId`-keyed class-kind cache (from my
   abandoned fix) did not move this number — the per-access
   `class_name_of_id` lock+allocation was not the dominant cost. Root cause
   not investigated further (would need profiling); general
   interpreter-dispatch overhead for a very access-heavy codec is the
   working theory, consistent with other documented CratonVM interpreter
   slowdowns.
2. **Correctness**: `ES818BinaryQuantizedVectorsFormatTests` shows several
   `expected:<X> but was:<0.0>` similarity-score assertion failures once
   execution gets past the `NoSuchMethodError`. Verified NOT caused by the
   `SegmentVarHandle` memory-access arithmetic (all four scenarios above
   round-trip correctly against real JDK once the ABASE fix is applied). See
   [elasticsearch-vector-scorer-zero-results.md](../known-issues/elasticsearch-vector-scorer-zero-results.md).

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-abasefix-verify-920-20260703\all-jit\  (17/17 real methods pass)
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-segvh-verify-es818-20260703\all-jit\   (pre-ABASE-fix run, 17/25 pass, distinct scorer issue)
```

Merge commits on `dev`: `76d20637` (original SegmentVarHandle fix, TSDB
docvalues/storedfields) + the ABASE follow-up merge (2026-07-03).
