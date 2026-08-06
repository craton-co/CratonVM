# `BeanRegistrationsAotContributionTests` — the 10001-definition test exhausts the heap in javac

| | |
|---|---|
| **Status** | ✅ **FIXED 2026-08-06.** `applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles` passes; the class matches HotSpot. |
| **Scope** | `org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests` (spring-framework, `spring-beans`). |
| **Fix** | `native_unmod_get` (`native-collections/src/lib.rs`) bounds-checked against `al_state`'s *"layout I cannot read"* sentinel. Landed on `dev` as `3d6af4029` → `5d265dbeb` → `de3c4d35b` by a concurrent session that found the same defect from a different starting point; this page contributes the end-to-end regression test `native-collections/tests/unmod_list_get_foreign_backing.rs` and the reduction below. |
| **Also closed** | Both compressed-oops correctness holes this page named as residuals — see [§2026-08-06](#2026-08-06-resolved-the-last-failure-was-never-about-the-heap). |
| **Measured** | 2026-08-02, 2026-08-05 and 2026-08-06, Azure host `20.83.144.174`, real JDK 25. |

---

## 2026-08-06 RESOLVED — the last failure was never about the heap

**Read this section; everything below it is the derivation that got here, kept
because four separate causes were mistaken for one and the record of how each
was ruled out is worth more than the conclusion.**

### The failure on 2026-08-06 `dev`

Re-run on `dev` @ `5081aa095`, the test no longer OOMs, no longer reaches javac,
and fails in **67 seconds** — a fifth distinct failure, and much earlier than
any before it:

```
java.lang.IllegalStateException: Unable to parse source file content:
  <the 20-line generated TestTarget__BeanFactoryRegistrations dispatcher>
  at org.springframework.core.test.tools.SourceFile.getClassName(SourceFile.java:188)
Caused by: java.lang.ArrayIndexOutOfBoundsException
  at org.springframework.core.test.tools.SourceFile.getClassName(SourceFile.java:183)
```

`SourceFile.java:183` is `javaSource.getClasses().get(0)`, one line after an
`Assert.state(javaSource.getClasses().size() == 1)` that **passed**. A list
whose `size()` is 1 and whose `get(0)` throws is not a Spring bug.

Deterministic, and `--nojit` reproduces it, so not a JIT miscompile.

### Root cause

QDox's `DefaultJavaSource.getClasses()` returns
`Collections.unmodifiableList(<a LinkedList>)`. `Collections.unmodifiableList`
is a CratonVM native returning a `cratonvm/internal/UnmodifiableList` wrapper,
and `native_unmod_get` (`native-collections/src/lib.rs`) bounds-checks the index
*before* delegating, so that an out-of-range `get` keeps raising
`ArrayIndexOutOfBoundsException` rather than the plain `IndexOutOfBoundsException`
the backing `ArrayList` would raise. That check took its size from `al_state`:

```rust
let (_, n) = al_state(ctx, backing);
if *index < 0 || *index >= n { return Err(AIOOBE) }
```

`al_state` returns `(None, 0)` for **any receiver whose layout it cannot read** —
a real-JDK `java/util/LinkedList`, a `cratonvm/internal/ArrayListSubList`, any
foreign `AbstractSequentialList`. That sentinel is indistinguishable from a
genuinely empty `ArrayList`, so every index was out of range and `get` threw for
**every** element of a list whose `size()`, `iterator()`, `toString()`,
`indexOf()`, `contains()`, `toArray()` and `listIterator()` all answered
correctly. The fix is to stop reading a size out of that sentinel: apply the
pre-check only when `al_state` actually read the backing — `unmod_view_size`
keys on the DATA slot being `Some` — and delegate otherwise, so the backing's
own `get` raises the bounds error HotSpot would.

The `ArrayList` backing is the reason this was not noticed: it is `RandomAccess`,
so HotSpot's own `getClass()` names it `Collections$UnmodifiableRandomAccessList`
and it is the shape almost every caller in the corpus has.

**Found twice, independently.** While this page was being reduced, another
session hit the same defect from a different direction and landed the fix on
`dev` first: `3d6af4029` *"an unmodifiable view's get() threw on a VALID
index"*, then `5d265dbeb` *"fix the unmodifiable get() in the native that
actually runs"* (there are two registrations; the first patch fixed the one
that does not dispatch — see [[duplicate-native-registrations-verify-which-wins]]),
then `de3c4d35b` *"use the DATA slot, not the size, to decide the unmod
pre-check"*. `de3c4d35b`'s `unmod_view_size` is the better-factored form of the
same idea — it keys on `al_state`'s DATA slot rather than on a layout predicate,
which also covers an `ArrayList` whose `elementData` is null — so the merge took
theirs and dropped this branch's implementation. What survives from here is the
reduction, the regression test, and the measurement below.

**Provenance.** The pre-check is 19 hours old:
`026ba86c9` *"fix(nio,collections): ByteBuffer had NO bounds check, and two more
shadowed overrides"*, 2026-08-05 22:15. That is why the 2026-08-05 runs recorded
below sail past `SourceFile.getClassName` and die 55 minutes later inside javac,
while the 2026-08-06 run dies in 67 seconds — and it is why chasing this page's
listed failure 3 would have been wasted work. `026ba86c9` is a good change; only
its size source was wrong.

### Reduction

The 55-minute Spring run reduces to a 3-second pure-JDK probe. On CratonVM
before the fix, on HotSpot 25 and on CratonVM after it:

```java
List<String> ll = new LinkedList<>(List.of("a", "b", "c"));
List<String> u  = Collections.unmodifiableList(ll);
u.size();    // 3      — correct on both
u.get(0);    // "a" on HotSpot; ArrayIndexOutOfBoundsException on CratonVM
Collections.unmodifiableList(new ArrayList<>(List.of("a","b","c"))).subList(1,3).get(0);
             // "b" on HotSpot; ArrayIndexOutOfBoundsException on CratonVM
```

Probes used, in the order they narrowed it:
`SkProbe` (ruled out the `Resolve.staticKind` stream shape — see failure 3
below, which is **not** what was failing here), `QdoxProbe` → `QdoxLex`
(lexer is fine) → `QdoxStep` (`Parser`/`ModelBuilder`/`addSource` all fine) →
`QdoxClasses` (pinned it to `JavaSource.getClasses()`) → `UnmodList` /
`UnmodPeel` / `LinkedGet` (pure JDK, no QDox).

### Verification

The whole class, `KRun org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests`,
on the fixed binary, real JDK 25, Azure host:

```
RESULT ... found=14 succ=14 fail=0 skip=0 abort=0 ms=1634398 status=OK
```

**14/14, rc=0** — parity with HotSpot, which is what the original Status line
asked for. Run twice: 4254 s on the pre-merge binary at host load ~50, and
1635 s on the final merged binary at load ~15. The 2.6x is the shared box, not
the change; this page's own "Measuring here at all" section below is why no
absolute time from this host means much.

Two notes on running it here at all, both learned the hard way this session:

* **The host OOM killer, not the VM.** Three runs died at 562 / 1137 / 1566 s
  with `rc=137` and no VM output. That is the *kernel* killing the largest-RSS
  process while other sessions held 28 of 31 GB in 3-4 GB `rustc` processes —
  not an `OutOfMemoryError`, and nothing to do with this page's footprint story.
  `rc=137` with no Java-level message is that, every time. The runner now
  self-shields with `oom_score_adj=-300` after launch (needs the host's
  passwordless `sudo`), which is enough to lose the coin flip against a
  comparable `rustc` without making the VM immune.
* A `RESULT` line is the only trustworthy signal — see
  [[timeout-is-often-a-sigsegv-with-no-result-line]].

### The residuals this page named, and what happened to them

| residual | outcome |
|---|---|
| *"hole 1 needs a proper narrow arm in `emit_load_string_value_ptr` rather than the blanket refusal"* | **Done.** `emit_load_narrow_ref_field` (`jit/src/x64/objects.rs`), selected per call site by the new `StringFieldLayout::value_compact_is_narrow`. The refusal in `try_resolve_string_intrinsic` is gone. Verified with `StringHot`, a hot loop over `charAt`/`length`/`isEmpty`/`hashCode`/`indexOf`/`equals`/`compareTo` across LATIN1, UTF-16 and empty receivers: byte-identical to HotSpot under `CRATONVM_COMPRESSED_OOPS=1`. |
| *"hole 2 is untouched, so the gate must stay off"* | **Done** — and, like the collections fix, found twice: `b50b71595` *"the conservative young-from rescan was blind to narrow oops"* landed on `dev` concurrently, factoring both scans through one `for_each_conservative_ref_slot` helper so the mark walk and the rewrite walk cannot disagree about width. The merge took that form. |
| *"do not cite compressed oops as the fix for this page without re-measuring after hole 1 has a proper narrow arm"* | **Re-measured.** The throughput penalty was the stopgap, not compression, and is gone; the footprint win is **4.7 %** of peak RSS (1850 → 1763 MB), not 20-30 %. Details below. |
| failure 3, the `ClassCastException` in javac's `Resolve.staticKind` | **Did not reproduce** on 2026-08-06 `dev`, in the Spring run or in `SkProbe` (300k iterations of the exact `candidates.stream().filter(..).map(StaticKind::from).reduce(StaticKind::reduce).orElse(..)` shape, over both an `ArrayList` and a generic-spliterator cons list). Its face — a `MethodSymbol` where an enum belongs — is the JIT dispatch-memo aliasing signature fixed by `383e7f5cf` (2026-08-05), which postdates the binary this page measured it on. Recorded as gone, not as chased down. |

### Compressed oops, re-measured with hole 1 properly closed

This is the measurement the page asked for. Same binary both ways, arms
**interleaved A-B-B-A** in one script per this page's own "Measuring here at
all" rule, on the 1001-definition sibling (the page's designated A/B proxy —
the 10001 case takes 70 minutes on this box and its variance swamps the effect).
`A` = default, `B` = `CRATONVM_COMPRESSED_OOPS=1`. Peak RSS from
`/proc/<pid>/status` `VmHWM`, sampled every 2 s.

| arm | peak RSS | wall |
|---|--:|--:|
| A (wide, slot 1) | 1833 MB | 194 s |
| B (narrow, slot 2) | 1763 MB | 205 s |
| B (narrow, slot 3) | 1762 MB | 157 s |
| A (wide, slot 4) | 1866 MB | 153 s |
| **mean A** | **1850 MB** | |
| **mean B** | **1763 MB** | |

All four arms `succ=1 fail=0`.

**Verdict, three parts:**

1. **The throughput penalty is gone.** The earlier measurement in this page had
   narrow oops *not finishing* inside a 2700 s ceiling while wide finished in
   1677 s. With the emitter's narrow arm in place instead of the blanket
   intrinsic refusal, the two arms are indistinguishable — the A arm's own
   spread across a separate A-B-B-A round (220 / 413 s) is larger than any
   A-vs-B gap. That earlier number was measuring the stopgap, not compression.
2. **The footprint win is ~4.7%, not 20-30%.** 87 MB of 1850. That is a real
   saving and it is not nothing, but it is nowhere near what
   `gc/src/compressed_oops.rs`'s header advertises, and it does not make narrow
   oops "the lever" this page called it. The arithmetic below explains why: the
   dominant term is the **32-byte `ObjectHeader`** (HotSpot's is 12), which
   compression does not touch at all. Halving the reference *fields* of objects
   whose header already costs 32 bytes moves a minority of the bytes.
3. **So the recommendation stands, for a different reason.** Do not enable
   compressed oops for this workload — not because it is unsound on the
   generational backend (it no longer is) and not because it is slow (it is
   not), but because a 4.7 % RSS saving does not justify running the one
   configuration in this VM that no suite exercises by default. The header's
   `ObjectHeader` shrink is the item with the leverage.

The gate is **still off**, now for §1.3's reason in
`arch-2026-07-26/value-repr-and-compressed-oops.md` — G1/ZGC are unmigrated
behind a load-bearing backend check — and not because the generational backend
has a known wrong-width slot access. `enable_for_live_heap`'s stderr warning
was rewritten to say what is actually left.

---

# Historical record (2026-08-02 → 2026-08-05)

Everything from here down is the original page, unchanged except where a claim
is explicitly annotated. It is kept because the eliminations in it are real work
that should not be redone — in particular the two withdrawn measurements in
"Heap accounting" and the object-width table, which are still correct facts
about this VM even though they were not what failed this test.

## Symptom

The class runs to completion on HotSpot in ~10–16 s, 14/14. On CratonVM the
first 13 tests pass; the 14th builds a contribution for **10001** bean
definitions, generates three source files, and compiles them with the
in-process `TestCompiler`. That compile dies:

```
org.springframework.core.test.tools.CompilationException: Unable to compile source
```

with an **empty** `Errors:` section, because `JavacTaskImpl.call()` returned
`false` without reporting a diagnostic. javac's own crash report is in the
run log and names the cause:

```
An exception has occurred in the compiler (25.0.3). ...
java.lang.OutOfMemoryError: Java heap space (alloc_array length 1048320)
```

Read the `CompilationException` message alone and you get "Unable to compile
source" with no errors listed, which reads like a codegen bug. It is not — grep
the log for `OutOfMemoryError`.

## The gap is footprint, not the heap cap

HotSpot does not merely have a bigger default heap here; it needs *far less*
heap than CratonVM can finish in:

| | heap | result |
|---|---|---|
| HotSpot | `-Xmx512m` | **passes, 13 s** |
| HotSpot | `-Xmx1g` / `-Xmx2g` | passes, 13 s / 15 s |
| CratonVM | 4 GiB (its own ergonomic default) | **OOM in javac after ~2324 s** |
| CratonVM | `--Xmx 8g` | still running at the 1800 s cap, no result |

So raising `MAX_ERGONOMIC_HEAP` (`vm-cli/src/main.rs`, capped at 4 GiB because
the generational heap eagerly commits its arenas) would not close this: 8 GiB
does not finish either. This is **not** the "CratonVM's default heap is capped
at 4 GiB while HotSpot's is an uncapped RAM/4 = 8.4 GiB" story.

`-Xlog:gc` on HotSpot at `-Xmx512m` puts its live set after a young collection
at **~180 MB** (`326M->180M(352M)` near the end of the run).

**The corresponding CratonVM number is NOT yet measured — do not quote one.**
An earlier revision of this doc claimed CratonVM's live set was "1.2 GB and
still climbing, roughly 8x". That was wrong, and the way it was wrong is worth
recording:

* `jcmd GC.heap_info`'s young "used" is `young_from_used()` — the from-space
  **bump-allocation cursor**, i.e. everything allocated since the last young
  collection, live or not. It is not a live-set figure.
* `jcmd GC.class_histogram` (`SharedVm::class_histogram`, `vm/src/vm/vm_init.rs`)
  calls `heap.walk_objects()` with **no preceding collection and no liveness
  mark** — it histograms every object in the heap, garbage included. HotSpot's
  `GC.class_histogram` reports live objects.

So the comparison was CratonVM-allocated against HotSpot-live, which proves
nothing. Getting a real number needs a forced collection immediately before the
walk, or an instrument that marks. See
[[verify-what-the-instrument-measures-before-believing-it]] — this is that
lesson, paid for again.


## Heap accounting: nothing is promoted

Two `GC.heap_info` samples ~10 minutes apart on the `--Xmx 8g` run:

```
Young Generation: 1.2 GB / 2.0 GB (58.9% used)      Young: 1.4 GB / 2.0 GB (70.0% used)
Old   Generation: 13.1 MB / 4.0 GB (0.3% used)      Old:   13.1 MB / 4.0 GB (0.3% used)
```

Old gen is **frozen at 13.1 MB** across many collections. That figure comes
from `old_gen_stats()`, which is a real used-bytes accounting rather than a
cursor, so the flatness is meaningful — but "nothing is promoted" is one
reading and "promoted then collected by a major GC" is another, and these two
samples cannot tell them apart. The young column beside it is the allocation
cursor (above) and should not be read as growth of live data.

Eliminations, so the next person does not redo them:

* **Not a general promotion defect.** `PromoProbe` (200 MB of long-lived
  `byte[]` plus heavy short-lived churn, `--Xmx 2g`) behaves correctly:
  `CRATONVM_DBG_YOUNG_TRIGGER=1` shows `live` flat at ~131 MB, `non_moving=false`,
  churn reclaimed. The pathology is specific to this workload.
* **Not the non-moving-sweep livelock** of
  `young-gc-trigger-livelock-under-nonmoving-sweep`: only 8 `[moving-young]
  fallback` events fire in a 2324 s run (the counter is rate-limited to
  `n <= 8` then powers of two, so a 9th–15th are possible but no 16th), i.e.
  almost every young collection took the *moving* path, which does promote.
* **`--nojit` is inconclusive** — RSS was lower (2.8 GB vs 4.4 GB at comparable
  elapsed time, which is suggestive of conservative JIT roots retaining
  garbage) but the run did not finish inside 900 s either, so it neither
  confirms nor rules that out. This is the most promising next thread.


## 2026-08-05 FINAL: three failures deep, each one uncovering the next

This page has now been through three distinct causes for the same test. Each
fix moved the failure later:

| | failure | status |
|---|---|---|
| 1 | `OutOfMemoryError` in javac at a 4 GiB heap | gone on current `dev` (plausibly the 2026-08-04 `defrag-promote` change) |
| 2 | `ArrayIndexOutOfBoundsException` in `CharBuffer.putBuffer` | **FIXED** — heap CharBuffer carried `address = -1`; see below |
| 3 | `ClassCastException` in javac's `Resolve.staticKind` | **OPEN**, and it is the current failure |

> **2026-08-06:** make that five. Failure 3 did not reproduce on `dev` of
> 2026-08-06 (its face is the JIT dispatch-memo aliasing `383e7f5cf` fixed the
> day after this section was written), and failure **4/5** — the one that
> actually closed this page — was `Collections.unmodifiableList(…).get(i)`
> throwing for every index over a non-`ArrayList` backing, 55 minutes earlier
> in the test than anything below. See the resolution section at the top.

Failure 3, measured on the fixed binary (`rc=0`, 3222 s, `aioobe=0`, so the
test now runs to completion rather than dying in `BaseFileManager.decode`):

```
An exception has occurred in the compiler (25.0.3)
java.lang.ClassCastException: com.sun.tools.javac.code.Symbol$MethodSymbol
    cannot be cast to com.sun.tools.javac.comp.Resolve$ReferenceLookupResult$StaticKind
  at com.sun.tools.javac.comp.Resolve$ReferenceLookupResult.staticKind(Resolve.java:3317)
  at com.sun.tools.javac.comp.Resolve$ReferenceLookupResult.<init>(Resolve.java:3301)
  at com.sun.tools.javac.comp.Resolve.resolveMemberReference(Resolve.java:3173)
```

`StaticKind` is a nested **enum**; a `MethodSymbol` reaching a cast to it is
type confusion on CratonVM, not a javac bug (HotSpot compiles the same sources).
That is the next thing to chase, and it is a *correctness* defect, not
throughput or footprint. Whoever picks it up should start by reducing it the
way failure 2 was reduced — a standalone probe around a method reference whose
resolution goes through `ReferenceLookupResult`, rather than the 55-minute
Spring run.

## 2026-08-05 UPDATE: on current `dev` this is no longer a heap failure

Re-run against `dev` of 2026-08-05 (+882 commits), the test **no longer OOMs at
all** — it runs to completion (`rc=0`, 1677 s) and fails with

```
java.lang.RuntimeException: java.lang.ArrayIndexOutOfBoundsException
  at com.sun.tools.javac.api.JavacTaskImpl.invocationHelper
Caused by: java.lang.ArrayIndexOutOfBoundsException
  at java.nio.CharBuffer.putBuffer(CharBuffer.java:1143)
  at java.nio.CharBuffer.put(CharBuffer.java:1050)
  at com.sun.tools.javac.file.BaseFileManager.decode(BaseFileManager.java:366)
```

That is a **VM correctness bug, not a footprint one**: a heap `CharBuffer`
carried `address = -1` instead of `ARRAY_CHAR_BASE_OFFSET` (16), so every
`CharBuffer.put(CharBuffer)` threw. Fixed separately (`cb_write_hb` in
`native-builtins/src/phases_late/charset_buffers.rs`); repro in
`docs/known-issues/repros/charbuffer-address/`. javac's `BaseFileManager.decode`
grows its CharBuffer and copies the old one in, so every source file it reads
hit it.

Something in dev's 882 commits — plausibly the 2026-08-04 `defrag-promote`
change, which the flag table describes as replacing a non-moving young sweep
that "promoted only" a subset — appears to have relieved the heap pressure this
page was written about. **The footprint analysis below still describes real
object widths, but it is no longer the thing failing this test.**

### Compressed oops measured, and it did NOT help

With hole 1's unblock in tree, the same binary was run both ways on the same
box, same test:

| | result |
|---|---|
| `CRATONVM_COMPRESSED_OOPS` unset | completes in 1677 s (fails on the CharBuffer bug) |
| `CRATONVM_COMPRESSED_OOPS=1` | **does not finish** — killed at the 2700 s ceiling |

So narrowing references is not a demonstrated win for this workload. Part of
that is self-inflicted: hole 1's unblock refuses the inlined String intrinsics
under narrow oops, which is a real throughput cost. Do not cite compressed oops
as the fix for this page without re-measuring after hole 1 has a proper narrow
arm in `emit_load_string_value_ptr` rather than the blanket refusal.

## Why CratonVM needs more heap: object width, measured

Per-object *sizes* from `GC.class_histogram` are exact regardless of liveness
(each entry's bytes/instances is that class's real instance size), so unlike
the totals they can be compared directly. Confirmed against the `[layout]`
diagnostic (`CRATONVM_DBG_LAYOUT=1`), which prints each class's compact body:

| class | CratonVM | HotSpot (compressed oops) |
|---|--:|--:|
| `java/lang/String` | 56 (32 hdr + 24 body) | 24 |
| `java/util/HashMap$Node` | 64 (32 + 32) | 32 |
| `com/sun/tools/javac/util/List` | 48 | 24 |
| `java/util/HashMap` | **320** | ~48 |
| `java/util/LinkedHashMap` | **400** | ~48 |
| `java/util/ArrayList` | **112** | ~24 |

Two separate effects:

1. **A broad ~2-2.5x** on everything, from a 32-byte `ObjectHeader`
   (`types/src/heap_types.rs`, vs HotSpot's 12) and 8-byte reference fields
   (vs 4 narrow). This is the dominant term, because it applies to the millions
   of small nodes javac allocates. Note the nodes themselves are *fine* —
   `HashMap$Node` gets the compact layout (`body=32 refs=3 fields=4`).

2. **A ~7x on a few container classes.** `HashMap`, `LinkedHashMap` and
   `ConcurrentHashMap` print `LEGACY, no compact layout` and fall back to the
   uniform 16-byte tagged-`Value` cell (320 = 32 + 18x16). This is
   **deliberate**, not a bug: `build_compact_layout`
   (`classloading/src/class.rs`) refuses any class with a *padded* slot — a slot
   with no field descriptor — because native code stores mixed types into those
   raw slots (`map_alloc_node` writes `Int(hash)` into one and refs into
   others), and guessing they are references makes the GC scan a primitive as a
   pointer and corrupt the heap. The legacy cell self-describes via its tag.
   `IdentityHashMap` (`body=40`) and `WeakHashMap` (`body=64`) have no padded
   slots and are compact, which is the control.

Effect 2 is bounded (one object per map). **Effect 1 is the lever**, and the
fix already exists behind a gate.

## The lever: compressed oops, gated off by two named holes

`gc/src/compressed_oops.rs` narrows reference instance fields and reference
array elements from 8 bytes to 4 under `-XX:+UseCompressedOops` /
`CRATONVM_COMPRESSED_OOPS=1`, **off by default**. Its header is explicit that
the gate is off for correctness, not throughput, and names both holes:

1. `emit_load_string_value_ptr` (`jit/src/x64.rs`) emits an unconditional
   64-bit load of `String.value` and is not gated on
   `narrow_oops_block_inline_fields` the way the getfield/putfield arms are, so
   under narrow oops every inlined `charAt`/`length`/`hashCode`/... is a
   deterministic wild-pointer SIGSEGV.
2. `mark_young_to_old_refs` and `rewrite_stretch_conservatively`
   (`gc/src/gen_heap.rs`) rescan unparseable heap stretches in aligned 8-byte
   words; a pair of adjacent narrow oops never matches, so marks are missed
   (premature reclamation) and refs to moved objects are left dangling.

Hole 1 has a one-line unblock the header itself prescribes — refuse
`try_resolve_string_intrinsic` under `narrow_oops_enabled()`, trading the
intrinsic's throughput for correctness. **That unblock is now in tree** (it is
inert while the gate is off). Hole 2 is untouched, so the gate must stay off;
`enable_for_live_heap` still prints its unsoundness warning.

## What the profile says

`perf record -F 199 -g` against the pre-fix binary during this test put
`cratonvm_gc::old_gen::OldGen::scan_region_filtered` at **73.66% of all CPU
samples** — a per-object linear scan of the dirty-card range list, i.e.
O(old-gen objects x dirty ranges). That is fixed on this branch and the symbol
no longer appears in the profile at all.

After the fix the profile is **flat** — top entry 5.6%, spread across
`NativeMethodRegistry::slot_for_exact` (+ its `memcmp`),
`force_native_over_real_jdk_bytecode`, `jit_method_calls_native_shadowed` and
`resolve_id_with_descriptor_quirks`, i.e. ~15–25% in by-name "is this method
natively shadowed?" lookups. Per
`profile-before-calling-it-the-interpreter-throughput-wall`, a flat profile is
where the general interpreter gap starts and a single-bug hunt stops paying.

Note the CPU fix does not change the outcome: it makes the test reach the same
OOM sooner. Removing 73.7% of the CPU cannot fix a memory-footprint failure.

## Reproduce

Single test, ~2300 s to the OOM on a quiet host:

```bash
cd /data/data/wt-springsuite8b-20260726/apps/spring-framework/spring-beans
CP="<runner-dir>:$(tr -d '\r' < build/cratonvm-testcp.txt)"
printf -- '-cp\n%s\n' "$CP" > /tmp/af.txt
<cratonvm> --java-home /home/victor/jdk25 \
  --add-opens=java.base/java.lang=ALL-UNNAMED \
  --add-opens=java.base/java.util=ALL-UNNAMED \
  -Djava.awt.headless=true @/tmp/af.txt \
  KRunM org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests \
        applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles
```

`KRunM` is `apps/spring-suite-runner/KRunM.java` (one test method, own launcher
pass). The 1001-definition sibling
`applyToWithLargeBeanDefinitionsCreatesSlices` is a ~200 s proxy that *passes*,
useful for A/B work; note its old gen is small enough that the dirty-card
quadratic never bites, so it is **not** a proxy for the GC fix (an alternating
2-binary A/B there is within noise: base 221/221/183 s, fixed 229/204 s).

**2026-08-06 addendum: use that sibling, not this test.** The
`unmodifiableList.get` defect broke the sibling too, in **19 seconds**, with the
identical `IllegalStateException: Unable to parse source file content` /
`ArrayIndexOutOfBoundsException` pair. A/B on the same host, same classpath:

| binary | result |
|---|---|
| `dev` @ `5081aa095` | `found=1 succ=0 fail=1`, 19 s |
| same + the `native_unmod_get` fix | `found=1 succ=1 fail=0`, 144 s |

That is a 19-second reproducer for a defect this page spent three sessions
chasing through a 55-minute one. The general lesson is
[[run-it-alone-before-calling-it-a-missing-feature]]'s sibling: when a page
names a *cheap proxy that passes*, re-run the proxy on current `dev` before
paying for the expensive case — a regression that lands in between will show up
there first and far faster.

For the VM-level defect, skip Spring entirely (3 seconds, no Gradle, no
classpath):

```java
List<String> u = Collections.unmodifiableList(new LinkedList<>(List.of("a","b","c")));
u.size();   // 3
u.get(0);   // "a" on HotSpot; ArrayIndexOutOfBoundsException on CratonVM before the fix
```

## Measuring here at all

This host is shared. Two things invalidated whole measurement rounds:

* **Load.** Other sessions took the 16-core box to load average 36; absolute
  times moved ~2x. Always A/B two binaries *alternately* in one script rather
  than comparing against a number from an earlier round.
* **The shared Gradle cache.** `build/cratonvm-testcp.txt` is a July-27
  snapshot of Gradle's `sourceSets.test.runtimeClasspath`, pinned to exact jar
  paths. On 2026-08-05 another session re-resolved dependencies, evicting the
  pinned versions (e.g. `junit-vintage-engine` 6.1.0 -> 6.1.1/6.1.2): **54 of
  254** spring-test entries vanished. The symptom is *not* a classpath error —
  it is `found=0 status=EMPTY`, or worse, a plausible-looking assertion failure
  (the vintage engine silently missing shifts Spring's AOT `TestContextNNN`
  numbering and `AotIntegrationTests#endToEndTests` fails on the diff). Check
  it before believing any result:

```bash
for p in $(tr -d '\r' < build/cratonvm-testcp.txt | tr ':' '\n'); do [ -e "$p" ] || echo "MISSING: $p"; done
```

  Repair by regenerating the dumps (needs network):
  `./gradlew --no-daemon -I ../spring-suite-runner/dump-testcp.init.gradle :spring-test:dumpTestCp`
