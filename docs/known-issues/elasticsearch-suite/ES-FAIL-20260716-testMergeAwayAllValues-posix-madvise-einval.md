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
