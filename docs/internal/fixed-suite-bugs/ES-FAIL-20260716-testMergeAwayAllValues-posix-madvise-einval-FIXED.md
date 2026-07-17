# 2026-07-16/17: FIXED — root-caused via live gdb capture on the real `posix_madvise` syscall

**Status: FIXED.** `testMergeAwayAllValues` now passes reliably.

## 2026-07-17 Windows host re-verification and residual closure

The repair was re-verified from a clean, dev-based worktree with the uniquely
named binary `cratonvm-es-posix-madvise-einval-20260717.exe`.  The full
`IVFKnnFloatVectorQueryTests` class passed **3/3** under CratonVM JIT-on
(28/28 tests each; 42.053 s, 38.382 s, and 32.663 s), and the same class
passed on Temurin 25.0.3 HotSpot (28/28, 4.131 s).  This Windows fixture does
not invoke Linux `posix_madvise`, but it exercises the same real mapped-segment
and FFM bridge representations that caused the Linux failure.

Residual audit extended the representation repair to every Panama bridge that
can receive a real JDK MemorySegment: `byteSize`, `address`, `asSlice`,
`reinterpret`, copy/fill/access and UTF-8 helpers, and Linker downcall-handle
address extraction now share the named `min`/`length` resolution rather than
reading synthetic field slots from real mapped instances.

The re-verification also fixed three Windows runner defects: PowerShell 5.1
now chooses `bin\\java.exe`; log parent directories are created at the write
boundary; and overlong log paths use a compact deterministic work-root
directory.  Those repairs were exercised by all four host runs above.

## Root cause, confirmed empirically

A live `gdb` breakpoint on the real libc `posix_madvise` symbol (bypassing
the need for further rebuild-instrumentation cycles, given this host's heavy
build contention that day) captured the ACTUAL arguments reaching the
syscall for the failing call: `posix_madvise(addr=0x114, len=276,
advice=0)`. `0x114` is exactly `276` in decimal — **the segment's own byte
length**, not a real address. Lucene's own exception message reports a
different, plausible-looking address (`0x7FFFF77E3000`) only because it
separately calls `MemorySegment.address()` *after* the failed downcall,
purely to format the error string — and that native method already uses a
different, correct implementation.

`native-builtins/src/panama_libffi.rs::segment_address()` (used by
`marshal_arg`'s `LAYOUT_ADDRESS`/struct-by-value arms to compute the native
pointer for any `MemorySegment` downcall argument) unconditionally read
`field 0` as the base pointer and `field 5` as a slice offset — the layout
CratonVM's own synthetic `"java/lang/foreign/MemorySegment"` class uses
(built by `ofAddress`/`asSlice`'s native handlers via
`alloc_concurrent_synthetic(..., 6)`). But a memory-mapped file segment from
real `FileChannel.map()` is a genuine, bytecode/JDK-constructed
`jdk.internal.foreign.MappedMemorySegmentImpl` instance, whose REAL field
order (confirmed via `javap` against the real JDK) is completely different:
`AbstractMemorySegmentImpl{length, readOnly, scope}` then
`NativeMemorySegmentImpl{min}` then `MappedMemorySegmentImpl{unmapper}` — 5
fields total (matching the `num_slots=5` seen in the `gen_heap::get_field`
out-of-bounds WARN this doc's original report flagged). **Field 0 there is
the segment's byte length, not its address** — `min` (the real address) is
at a different index entirely.

This exact bug was already fixed once, correctly, for the *other* native
method that needs a `MemorySegment`'s address —
`MemorySegment.address()` itself, implemented by `p67_segment_address`
(`native-builtins/src/phases_late.rs`), which resolves the `min` field
**by name** first (`ctx.get_field_by_name(this, "min")`), falling back to
the synthetic 6-field scheme only if that lookup misses. `segment_address()`
in `panama_libffi.rs` (used only for the native-downcall marshaling path,
a different call site) had never been updated to match.

## Fix

`segment_address()` now mirrors `p67_segment_address`'s exact resolution
order: resolve `min` by name first (correct for any real JDK
Native/MappedMemorySegmentImpl instance), then fall back to the synthetic
`base@0 + offset@5` scheme for CratonVM's own `ofAddress`/`asSlice`-built
segments (`object_num_fields(seg) >= 6`), then a final `field 0` fallback.

## Verification

- `testMergeAwayAllValues`: 3/3 clean (`OK (1 test)`, ~2s each, was a
  deterministic failure before).
- Full `IVFKnnFloatVectorQueryTests` class: `OK (28 tests)`, ~28s.
- `cratonvm-native-builtins --lib`: 2999 passed, 0 failed.
- Live gdb re-confirmation not repeated post-fix (the failing syscall
  argument was the direct target of the fix and the Java-level test now
  passes deterministically instead of failing deterministically).

## Notes for anyone touching `panama.rs`/`panama_libffi.rs` again

The generic `asSlice` native handler
(`native-builtins/src/panama.rs`) and any OTHER call site reading
`ctx.get_field(seg, 0)`/`ctx.get_field(seg, 5)` directly on an arbitrary
`MemorySegment` argument carries the same latent hazard for real
Native/MappedMemorySegmentImpl receivers — this fix only touched
`segment_address()`, the one confirmed to matter for this doc's failure.
`asSlice`'s own field-0/field-5 read on `this` happened to be harmless in
this specific repro (the observed `base_off` read was `0`, and the
resulting slice's base_ptr, while wrong, was never actually dereferenced
before `segment_address()`'s OWN fix corrected the final address) — but it
is not verified safe in general. Worth a dedicated look if a similar
address-confusion bug surfaces again for a sliced (not root) mapped
segment.

---

# ES FAIL - server org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests#testMergeAwayAllValues — `posix_madvise` returns EINVAL via Panama FFI downcall

Status: OPEN

## Discovery context

Found 2026-07-16 while re-verifying
[the IVFKnnFloatVectorQueryTests hang doc](ES-HANG-20260709-server-org-elasticsearch-search-vectors-ivfknnfloatvectorquerytests-565afb965e.md)
against current `dev` (that doc's own hang, `testRandomWithFilter`, is now
FIXED — see that doc). Running the whole class (not just the single named
method) turned up this separate, previously-undocumented failure. Not
mentioned in either ES-HANG doc for this cluster and not caused by the
`GAP_FILLER_CLASS_ID` GC bug fixed alongside this discovery (0 GC-corruption
warnings in every repro run below) — a distinct bug in a different subsystem
(the Panama/FFM native-downcall path), filed separately rather than folded
into the GC-focused docs.

## Repro

Worktree `/data/wt/wt-es-ivfknn-20260716` (Azure host
`victor@20.83.144.174`), binary
`/data/wt/target-es-ivfknn-20260716/release/cratonvm-es-ivfknn-20260716-fix1`,
built off `origin/dev` at `04e70345` + this session's `GAP_FILLER_CLASS_ID`
fix (see the sibling docs for that fix). Fixture:
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`.

```bash
ES=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch
CP=$(tr -d '\r' < "$ES/server/build/craton-testcp.txt" | tr '\n' ':' | sed 's/:$//')
"$EXE" --java-home /home/victor/jdk25 --Xmx 2g \
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home="$ES" -Djava.awt.headless=true \
  -Dtests.method=testMergeAwayAllValues \
  <standard ES test JVM args, see run-elasticsearch-suite.ps1 Get-EsJavaArgs> \
  -cp "$CP" org.junit.runner.JUnitCore \
  org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests
```

**Deterministic 3/3** (same seed):

```
WARN [RunnerThreadGroup] Uncaught exception in thread: Thread[#5,Lucene Merge Thread #0,...]
org.apache.lucene.index.MergePolicy$MergeException: java.io.IOException: Call to posix_madvise with address=0x76B8BF309000 and byteSize=276 failed with return code 22.
	at org.apache.lucene.index.MergePolicy$MergeException.<init>(MergePolicy.java:561)
	at org.apache.lucene.index.ConcurrentMergeScheduler.handleMergeException(ConcurrentMergeScheduler.java:770)
...
1) testMergeAwayAllValues(org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests)
java.lang.AssertionError: expected:<2147483647> but was:<1>
2) testMergeAwayAllValues(org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests)
com.carrotsearch.randomizedtesting.UncaughtExceptionError: Captured an uncaught exception in thread: Thread[id=5, name=Lucene Merge Thread #0, ...]
Caused by: org.apache.lucene.index.MergePolicy$MergeException: java.io.IOException: Call to posix_madvise with address=0x76B8BF309000 and byteSize=276 failed with return code 22.
```

Return code 22 is `EINVAL`. The address printed is always page-aligned
(`...000`, a multiple of 0x1000) across all 3 runs (different absolute
addresses each run, same low-order alignment); `byteSize` is always exactly
`276`. Not an address-alignment problem on its face.

**Confirmed NOT environmental / NOT a CratonVM-generic FFM problem:** the
identical repro against real HotSpot (`/home/victor/jdk25/bin/java`, same
seed, same classpath, same JVM args minus the CratonVM-only flags) passes
cleanly (`OK (1 test)`, 3.7s). This is a genuine CratonVM-specific defect,
not a kernel/host/Lucene-version issue.

**Confirmed unrelated to this session's `GAP_FILLER_CLASS_ID` GC fix:** `grep
-c 'implausible extent\|GAP-filler sentinel'` on every repro log is 0. The
one-off `gen_heap::get_field` OOB WARN seen moments earlier in a full-class
run (`obj=... class_name=jdk/internal/foreign/MappedMemorySegmentImpl
index=5 num_slots=5`) is a candidate lead (see below) but is a *different*
code path (a live interpreter field read, not the young-GC exact-walk).

## Analysis (source-level, not yet fully root-caused)

`posix_madvise` is not a CratonVM native builtin — `grep -rn madvise` across
`vm/`, `native-io/`, `native-builtins/` returns nothing. Lucene's own
`MemorySegmentIndexInput`/`NativeAccess` machinery calls the *real* libc
`posix_madvise` directly through the real JDK Foreign Function & Memory
(FFM/Panama) API — CratonVM's role is only to execute that downcall
correctly (`native-builtins/src/panama.rs::pe_downcall_invoke`, which uses
real `libffi` to call the real C symbol — `native-builtins/src/panama_libffi.rs`).
A bug in how CratonVM marshals the `MemorySegment` argument into the real
call would produce exactly this shape: a real OS-level `EINVAL` from a
genuinely-wrong argument, not a synthetic/emulated error.

One confirmed, but likely NOT sufficient, lead: `panama_libffi.rs::segment_address`
(used by `marshal_arg`'s `LAYOUT_ADDRESS`/struct-by-value arms to compute the
native pointer for a `MemorySegment` arg) does:

```rust
pub fn segment_address(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    let base = match ctx.get_field(seg, 0) { Value::Long(n) => n, _ => 0 };
    let off = match ctx.get_field(seg, 5) { Value::Long(n) => n, _ => 0 };
    base.wrapping_add(off)
}
```

This hardcodes field index 5 as a "slice offset" with no by-name resolution
and no existence check — unlike the established, more careful pattern
elsewhere in this codebase (`lang_invoke.rs::segment_raw_access`, which uses
`ctx.resolve_field_index(NATIVE_SEGMENT, "min")` and documents that the real
JDK's `AbstractMemorySegmentImpl`/`NativeMemorySegmentImpl`/
`MappedMemorySegmentImpl` have no separate slice-offset field at all —
`asSlice()` recomputes `min` directly in the real implementation). For a
`MappedMemorySegmentImpl` object (confirmed `num_slots=5`, i.e. only indices
0-4 exist), `get_field(seg, 5)` is a genuine out-of-bounds read, caught and
dropped by the `gen_heap::guard` (the WARN seen in the full-class run,
`real_field_count=Some(5)`), and `off` falls back to `0` via the `_ => 0`
arm — meaning, as far as this function's own logic goes, the OOB read
appears to be a harmless no-op (base + 0 = base) rather than the source of
a wrong address. **This dead-code-shaped hazard was not modified this
session** (out of scope; a hardcoded, unchecked field index used broadly
by every `LAYOUT_ADDRESS`/struct-by-value downcall argument is not something
to patch speculatively without full characterization — consistent with this
codebase's established policy for unproven fast paths). It is flagged here
as the most promising existing lead, not a confirmed root cause: the actual
wrong value fed to `posix_madvise` (address, length, or the `advice` enum
argument) has not yet been captured live (e.g. via a temporary print in
`marshal_arg`/`pe_downcall_invoke` for this exact call site, or by
comparing the real Lucene-computed `long` values against what libffi
actually receives).

The assertion failure (`expected:<2147483647> but was:<1>`,
i.e. `Integer.MAX_VALUE` vs `1`) accompanying the merge exception in one of
the two reported failures suggests a downstream miscount (possibly a
doc-count or generation field) consequent to the merge aborting abnormally,
rather than a second, independent bug — not separately investigated.

## Next steps for whoever picks this up

1. Instrument `pe_downcall_invoke`/`marshal_arg` (temporarily, gated behind
   an env var per this codebase's convention) to print the exact
   `(address, byteSize, advice)` triple reaching libffi for this specific
   `posix_madvise` call site, and compare against what real Lucene bytecode
   computed (its own `MemorySegment.address()`/`.byteSize()` reads) — this
   pins whether the bug is in Lucene's inputs (unlikely, since real HotSpot
   passes) or in CratonVM's marshaling/address computation.
2. If `segment_address`'s field-5 read turns out NOT to be fully inert in
   this exact call shape (e.g. a different, larger `MappedMemorySegmentImpl`
   layout variant does have 6+ fields, or the OOB-guard's fallback interacts
   with `wrapping_add` in a way not covered by the "always 0" assumption
   above), fix by resolving the field by name/existence via
   `ctx.resolve_field_index`, matching `lang_invoke.rs::segment_raw_access`'s
   established pattern, instead of a bare `get_field(seg, 5)`.
3. Re-run this doc's repro (`testMergeAwayAllValues`, seed
   `B17AC9D3E1F2A0C4`) 3+ times after any fix attempt to confirm the
   `posix_madvise EINVAL` is gone, and diff against real HotSpot's clean
   pass as the acceptance bar.
