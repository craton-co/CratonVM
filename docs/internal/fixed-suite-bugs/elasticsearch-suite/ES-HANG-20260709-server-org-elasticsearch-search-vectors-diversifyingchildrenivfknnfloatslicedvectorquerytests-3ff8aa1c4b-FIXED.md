Status: FIXED — archived 2026-07-19. Every hang/corruption/crash bug this
doc tracked across its 2026-07-09 through 2026-07-19 history is now found
and fixed (see the dated sections below for each one's own writeup). The
one remaining open item, `testSlicesDense`'s raw interpreter throughput
(genuinely slow, ~600s, never a hang or a correctness bug — characterized
repeatedly throughout this doc's history), is split out to its own doc:
[`ES-PERF-20260719-testSlicesDense-interpreter-throughput.md`](../../known-issues/elasticsearch-suite/ES-PERF-20260719-testSlicesDense-interpreter-throughput.md).

---

# 2026-07-18/19 continuation: stale-precise-root-mirror bug FOUND and FIXED (raw JIT-to-JIT call RBP-mirror race); JIT-throughput regression from the fix's interim safety defaults FOUND and FIXED; end-to-end `testSlicesDense` re-verification initially BLOCKED by a newly-discovered, separate, pre-existing `Thread.join()` lost-wakeup bug — that bug is now FOUND and FIXED too, and `testSlicesDense` has been re-verified to run to completion

**Status update: took over `C:\craton\CratonVM-es-ivfknn-slicesdense-closure-20260717`
from other agents' in-progress WIP (uncommitted, ~880 lines across
gc/jit/vm) that had independently found and started fixing the same
"Elasticsearch IVFKnn stress test exposed a stale precise-root mirror
after a raw JIT-to-JIT CALL" mechanism referenced in this doc's earlier
sections. Verified, completed, and hardened that work; found and fixed
two additional real bugs surfaced along the way; the doc's own primary
subject (`testSlicesDense`) remains unverified end-to-end only because
of a brand-new, unrelated, pre-existing blocker (see below) — not
because of anything still open in the GC/JIT fix itself.**

## What was fixed (all committed, branch
`codex/fix-es-ivfknn-slicesdense-closure-20260717`, merged with current
`origin/dev`)

1. **The stale-precise-root-mirror bug itself.** A raw JIT-to-JIT CALL
   updates the per-thread active-RBP mirror to the callee while the
   interpreter-owned root-chain entry still describes the caller until
   the callee's own prologue runs; a GC landing in that window can
   select the caller's oop map for the callee's frame and lose live
   roots. Fixed by (a) republishing the caller's RBP after every raw
   JIT-to-JIT CALL returns (`Compiler::emit_post_call_rbp_republish`),
   and (b) `JitEntryGuard::enter_with_compiled` now always retains
   precise frame metadata (even for methods with no real oop maps) so
   `scan_compiled_frame_bands` can bound each stacked raw-call frame
   exactly instead of falling back to one imprecise whole-band
   conservative sweep.
2. **The young-GC exact-walk hardening** this doc's 2026-07-16 section
   already covers (GAP_FILLER_CLASS_ID special-casing) was generalized:
   the non-moving young sweep now uses `origin/dev`'s own
   `oracle_trusted_abs` truncated-walk fallback (a slightly later,
   independently-developed, more refined fix for the same underlying
   "walk breaks early → silently drops live candidates" hazard, found
   during the `origin/dev` merge below — see the bt18-family reference in
   that fix's own comment). A companion allocator/walker alignment
   mismatch was also fixed: compact object bodies can be non-8-aligned,
   but the arena/TLAB bump allocators weren't reserving the full aligned
   footprint, leaving a phantom gap between adjacent objects that the
   linear collector walks could desync on; `Tlab::alloc(_initialized)`
   and `Arena::alloc`/`OldGen::alloc` now round up to the full footprint,
   matching `gen_object_total_size`'s own rounding.
3. **The MIC (monomorphic inline cache) install protocol** is now atomic
   (CAS-based reservation, monomorphic for the slot's lifetime) instead
   of a read-then-store retarget race that could pair one receiver's
   class guard with a different receiver's compiled-entry pointer.
4. **A severe (~15-30x) general JIT throughput regression**, introduced
   by the interim safety default the original stale-mirror fix shipped
   with (`direct_jit_callee_calls_enabled` / the dispatch-cache direct-
   entry paths defaulting OFF to avoid the very race #1 fixes). Verified
   with `bench/BenchSuite.java`: `bintrees16` went 44s (flag off) → 2.9s
   (flag on) with the RBP-republish + precise-metadata fixes in place;
   `fib44` (a classic non-tail self-recursive numeric benchmark that
   routes through the dispatch-cache path, not the direct-callee-compile
   path) was the worst-hit case. Root-caused and re-enabled default-ON —
   see `direct_jit_callee_calls_enabled`'s doc comment in `jit/src/lib.rs`
   for the full reasoning, the narrower residual left opt-in-only (the
   virtual-dispatch-cache counterpart, and a separate pre-existing
   invokespecial/static-bridge target-resolution gap — see the follow-up
   task filed for that), and why this is safe: re-verified against
   `testSlicesSparseWithFilter` with zero corruption warnings, plus the
   full `cratonvm-gc`/`cratonvm-jit`/`cratonvm-vm` `--lib` suites and the
   `ir_vs_singlepass`/`differential` JIT suites, all green.
5. **Merged 280 commits of `origin/dev`** into this branch (it had 0
   commits of its own — all its real work was uncommitted) — this alone
   fixed most of an apparent ~18x `fib44` regression that turned out to
   be simple staleness against unrelated `dev` perf work, not anything
   this session's fixes did. The merge itself surfaced one genuine new
   bug: a sweep-phase lockstep cross-check this session had added
   (comparing the mark-phase's `young_object_ranges` against a fresh
   sweep-phase walk, 1:1 in sequence) assumed `young_object_ranges` was a
   dense list of every live object; `origin/dev` had independently turned
   it into a sparse, candidate-filtered list as its own optimization,
   which made the two views "diverge" (spuriously) on almost every GC —
   the sweep then permanently under-reclaimed, OOM-crashing `bintrees18`.
   Removed the now-invalid check (item 2's `oracle_trusted_abs` fallback,
   preserved from `origin/dev`, already covers the same safety property
   soundly).

**Verification performed:** full `cratonvm-gc`/`cratonvm-jit`/`cratonvm-vm`
`--lib` suites (885/915/2218 passed; the same 19 pre-existing/
environmental failures as `origin/dev` itself — debug-only lock-order
assertions, JIT skip-list feature tests, JNI table-size tests, real-JDK-
detection tests); `cratonvm-jit`'s `ir_vs_singlepass`/`differential`
suites (89/89); `bench/BenchSuite.java` bt10/12/14/16/18 (all correct
checksums matching the documented HotSpot-verified golden values,
including `bt18=68332206`, and fast: bt16 1.1s, bt18 5.5s), `arith1500M`,
`matrix800`; `testSlicesSparseWithFilter` (`DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests`,
direct JUnitCore invocation, zero corruption/OOB warnings across
multiple runs — see the caveat below for why it could not be observed to
completion).

## Blocker for full end-to-end confirmation — NOW FIXED:
[`ES-HANG-20260719-threadjoin-randomizedrunner-worker-windows.md`](ES-HANG-20260719-threadjoin-randomizedrunner-worker-windows.md)

While confirming `testSlicesSparseWithFilter`/`testRandomWithFilter`/
`testSlicesDense` complete cleanly end-to-end (not just "no corruption
warnings before an external timeout"), found that **every** direct
`JUnitCore` invocation of a `RandomizedRunner`-based ES test class hung
on this Windows host — `Thread.join()` never returned for a worker thread
that had already finished (`alive=false`). **Confirmed unrelated to
everything in this doc**: reproduced identically on a clean, from-scratch
`origin/dev` build with none of this session's changes; reproduced with
`--nojit`; the identical repro passed cleanly under real HotSpot in
~2.2s. See that doc for the full root-cause writeup (a `release_monitors_held_by`
call inside the `Thread.join()`-termination-notify sequence was
force-releasing the terminating thread's own termination monitor before
`notify_all()`, silently discarding the resulting `NotOwner` error — a
lost wakeup, present regardless of OS/JIT/ES).

**2026-07-19 re-verification, post-fix:** re-ran this doc's own original
`testSlicesDense` repro recipe (below) against the fixed binary
(worktree `serene-lamarr-01d83a`, fix merged to `dev` at `bd83c42fa`). The
process now exits cleanly — `Time: 602.662`, `System.exit(1)` called
normally, a proper JUnit failure report (2 failures, both
`Test abandoned because suite timeout was reached` /
`Suite timeout exceeded (>= 580000 msec)`) — instead of hanging
indefinitely and requiring an external watchdog to abort it. This is
exactly the previously-documented genuine-slow-progress shape (see the
2026-07-13 section below: an earlier unbounded run also completed in
~602s, stopped by the test framework's own `-Dtests.timeoutSuite=580000!`,
not a VM-level hang) — `testSlicesDense`'s underlying performance
characteristic is unchanged and NOT re-litigated here; this update only
confirms the join-hang blocker no longer prevents the process from
running to completion (or, in this test's case, to its own framework
timeout) end-to-end.

---

# 2026-07-16 continuation: root cause of the underlying corruption/hang FOUND and FIXED (`GAP_FILLER_CLASS_ID` young-GC exact-walk regression); `testSlicesDense` perf remains a separate, unchanged, OPEN issue

**Status update: the correctness/hang mechanism behind this cluster's
non-deterministic corruption and watchdog aborts is FIXED. `testSlicesDense`
itself remains open, but ONLY as the pre-existing performance issue this
doc already characterized (2026-07-13 section below) — not as a hang or a
correctness bug.**

Investigated in isolated worktree `/data/wt/wt-es-ivfknn-20260716` on the
Azure host (`victor@20.83.144.174`), forked from `origin/dev` at `04e70345`
(which already included the `fe76025e7` array-ctor-reference fix from the
2026-07-13 section below, plus everything since — the STW-barrier redesign,
`SynchronizedMethodGuard` stale-monitor fix, CHM pin fixes, and same-day
sibling commit `1c4aaa06` "close stream ArrayList pressure corruption").

## Re-baseline: `testSlicesSparseWithFilter` (the 2026-07-13 fix's own
verification target) had REGRESSED back to a deterministic hang

Re-running this doc's own 2026-07-13 confirmation recipe
(`testSlicesSparseWithFilter`, same seed, direct invocation, JIT on)
against fresh `origin/dev` hit the 90s watchdog **3/3**, preceded by a mass
burst of `gen_heap::get_field` out-of-bounds / "Stale pointer detected...
all-zero header" warnings across dozens of unrelated live objects
(`java/lang/Thread`, `ArrayList`, `IndexWriter`, `IndexReader`, `List`, …) —
reproduced identically under `--nojit`, so not JIT-specific. This is a
**regression**, not the same bug this doc already tracks: the 2026-07-13 fix
itself was not reverted, but a same-day sibling commit broke a different,
adjacent mechanism.

## Root cause: `1c4aaa06` ("fix(gc): close stream ArrayList pressure
corruption", landed on `dev` hours before this session, fixing an unrelated,
already-tracked bug) added two new "exact young-object walk" loops in
`gc/src/gen_heap.rs` — one for the moving-young path
(`young_object_starts`), one for the non-moving sweep
(`young_object_ranges`) — to build a precise object-start set so
conservative root candidates can't corrupt the wrong slot. **Neither new
loop special-cased the `GAP_FILLER_CLASS_ID` TLAB-tail sentinel** (Bug-D,
2026-06-12), an established pattern already handled correctly at 7 other
call sites in the same file (e.g. the selective-promotion walk, `~line
5243`). A `GAP_FILLER_CLASS_ID` sentinel's on-disk layout is NOT a real
`ObjectHeader` — offset 4 holds the raw gap length, not header fields — so
feeding it straight into `gen_object_total_size` (as both new loops did)
misparses it as either a size that overshoots the arena or an implausible
`total < HEADER_SIZE`, and the loop **`break`s early**, permanently. Every
object allocated *after* that point in address order then falls outside the
exact set: `mark_young`'s binary-search lookup returns `None` for genuinely
live objects located past the break, so the non-moving sweep reclaims them
as garbage — mass corruption of live, in-use objects across every class
that happened to be allocated after whichever thread's retired TLAB tail
the walk stumbled on first. This exactly explains the doc's
long-standing, previously-"believed-benign" observation of
`gen_heap::get_field` OOB WARN bursts firing in a tight cluster and then
stopping (the corruption event itself is a single, brief burst at GC time —
what happens *afterward*, when other threads dereference the now-garbage
addresses, is the mass "stale pointer / all-zero header" cascade and the
eventual deadlock/timeout).

**Fixed**: both new exact-walk loops now check
`header.class_id.as_u32() == GAP_FILLER_CLASS_ID.as_u32()` before calling
`gen_object_total_size`, read the sentinel's raw gap length from offset 4,
and skip over it — mirroring the established pattern used elsewhere in this
file. Branch `fix/es-ivfknn-hang-20260716`, worktree
`/data/wt/wt-es-ivfknn-20260716`, binary
`/data/wt/target-es-ivfknn-20260716/release/cratonvm-es-ivfknn-20260716-fix1`.

## Verification

- `testSlicesSparseWithFilter`: 0/3 corruption bursts post-fix (was 3/3
  pre-fix); passes cleanly (`OK (1 test)`, ~147s under host contention, 300s
  watchdog, zero `implausible extent`/`GAP-filler sentinel`/`Stale pointer`
  warnings).
- **`testRandomWithFilter`** (the sibling
  [IVFKnnFloatVectorQueryTests doc](ES-HANG-20260709-server-org-elasticsearch-search-vectors-ivfknnfloatvectorquerytests-565afb965e-FIXED.md)'s
  own hang target): now passes cleanly **4/4** (~20-22s each, zero
  corruption warnings) — see that doc for the full write-up; it is being
  marked FIXED and moved to `docs/internal/fixed-suite-bugs/`.
- `testSlicesDense` (this doc's original 2026-07-09 subject): **still hits
  a long watchdog** (700s and 1800s both tried) with **zero** GAP-filler/
  implausible-extent warnings — this fix does not touch it. The only WARN
  burst observed is the same `class_id=ClassId(0) num_slots=0
  java/lang/Object` shape the 2026-07-09 original report already flagged
  and already characterized as believed-benign speculative probing (fires
  once, early, then the process keeps running); consistent with the
  2026-07-13 section below's conclusion that `testSlicesDense`'s slowness is
  a genuine CratonVM interpreter/JIT throughput characteristic on this
  reflection/exception-handling-heavy path, not a hang or a data-corruption
  bug. **Unchanged status: still a performance investigation, not a
  correctness one** — see that section for the still-valid guidance for
  whoever picks this up next.

## Not the same mechanism as the GC audit's finding 1(b)

Before finding the actual cause above, this looked like a strong candidate
match for
[`docs/internal/gc-audit-2026-07-10-open-findings.md`](../../gc-audit-2026-07-10-open-findings.md)
finding 1(b) (the monitor-vs-evacuation / missed-marking-root residual that
doc's own text cross-references this exact cluster against). It is not:
finding 1(b)'s own forensics (the `[MONEXIT-IMSE]` capture, the
`InetAddressRandomBinaryDocValuesRangeQueryTests` repro) point at a
different, still-unresolved missed-root mechanism on a **different** test
class, unaffected by this fix (not re-verified here — out of scope for this
doc). This cluster's hang had a much simpler, freshly-introduced cause (a
same-day regression, not a long-standing race) that happened to produce a
superficially similar symptom (the same `gen_heap::get_field` OOB WARN
family). Do not treat this fix as closing finding 1(b) generally.

## New, separate finding while re-verifying (not this doc's concern)

Running the sibling doc's full test class (not just the single named
method) surfaced an unrelated, previously-undocumented failure,
`testMergeAwayAllValues` (a real `posix_madvise` `EINVAL` via the Panama FFI
downcall path — confirmed absent on real HotSpot, so a genuine CratonVM
bug, but in a completely different subsystem). Filed separately:
[`ES-FAIL-20260716-testMergeAwayAllValues-posix-madvise-einval.md`](ES-FAIL-20260716-testMergeAwayAllValues-posix-madvise-einval.md).

---

# 2026-07-13 follow-up: array-constructor-reference lambda bug found + fixed
# (real, verified, but NOT yet confirmed as this doc's residual root cause)

While independently re-investigating this cluster (parallel to, and without
visibility into, the "2026-07-13 current-dev residual update" section
immediately below until after landing this fix), found and fixed a real,
previously-unknown interpreter correctness bug: **array-constructor-reference
lambdas (`SomeType[]::new`, used as an `IntFunction<SomeType[]>` — the
mechanism behind `Collection.toArray(SomeType[]::new)`, ubiquitous since
Java 11, and any direct user code) allocated a corrupted zero-field pseudo-
object instead of a real array.**

Root cause: `MethodHandleKind::NewInvokeSpecial` (the constructor-reference
lambda dispatch, `vm/src/runtime/interpreter.rs` and its duplicate in
`vm/src/vm/vm_exec.rs`) unconditionally treated the impl handle's target as a
regular class — allocate `num_total_fields` object slots, dispatch `<init>`.
For an array-shaped impl class name (e.g. `"[Ljava/nio/ByteBuffer;"`),
`load_class` correctly resolves it to the synthesized array `ClassId` (JVMS
5.3.3), but arrays have neither fields nor a constructor: the old path
allocated a zero-field object wearing the array's `ClassId`, silently
discarded the requested length (the `IntFunction`'s sole `int` argument), and
produced a value that fails a later `checkcast` to the real array type.

This exact mechanism reproduces from real Lucene: `ByteBuffersDataInput`'s
constructor does `this.blocks = list.toArray(ByteBuffer[]::new)`, immediately
followed by `checkcast [Ljava/nio/ByteBuffer;`. A direct, isolated repro
(`new ByteBuffersDataInput(list)` against the real `lucene-core-10.4.0.jar`,
bypassing the whole ES/suite-runner harness) threw
`ClassCastException: java.lang.Object cannot be cast to [Ljava.nio.ByteBuffer;`
on unfixed `dev`, and one *direct* (non-suite-runner) repro of this doc's own
`testSlicesDense` — bypassing the outer watchdog entirely — surfaced this
identical exception in ~85s instead of the usual multi-hundred-second
non-progress, i.e. this bug is a real, independent cause of SOME of this
cluster's non-deterministic behavior (fast completion with a wrong-data
exception, vs. the more common very-slow/non-progress runs), not necessarily
of every manifestation.

**Fixed** (commit on `fix/gc-stw-monitor-race-20260711-local`, merged to
`dev`): detect the array case via the resolved `ClassId`'s `array_info`
before the object-allocation path, and allocate a real array — primitive or
reference, correct component type, correct length — the same way
`anewarray`/`newarray` do for the same class metadata. Verified: the real
`ByteBuffersDataInput` construct + `readByte()` + `slice()` repro now matches
real JDK 25 output exactly. `cratonvm-vm --lib` lambda-proxy/lambda-dispatch
suite 8/8 pass; full `cratonvm-vm --lib` 2185 passed / 23 failed, all 23
pre-existing/environmental (debug-only lock-order assertions that cannot fire
in a release build, JIT skip-list feature tests, JNI table-size tests,
real-JDK-detection tests) and unrelated by name/code-path to this change.

**CONFIRMED 2026-07-13 (same session, host came back):** re-ran the
"2026-07-13 current-dev residual update" repro directly (`testSlicesSparseWithFilter`,
direct-invocation form bypassing the suite runner, `--stack-dump-on-timeout 90`)
against a fresh build of `origin/dev` including this fix (`fe76025e7`,
worktree `/data/data/wt-gc-stw-monitor-race-20260711`, binary
`/data/data/cratonvm-arrayctorfix-verify-20260713`). **4/4 runs now pass
cleanly (`OK (1 test)`), consistently ~80-85s, no watchdog abort, no
exception.** Before this fix every run hit the 90s watchdog with the process
genuinely non-progressing inside `ByteBuffersIndexInput.slice`/
`MockIndexInputWrapper.slice` (per the residual-update section above). This
fix IS the root cause of that residual — the STW-monitor-race historical
attribution is confirmed NOT applicable to `testSlicesSparseWithFilter` and
can be retired for that test.

**`testSlicesDense` (this doc's ORIGINAL 2026-07-09 subject) was ALSO
re-tested against the same fixed build and is NOT resolved by this fix**:
3/3 runs still hit the 90s watchdog. This is consistent with the earlier
(pre-this-fix) finding elsewhere in this doc's history that `testSlicesDense`
is genuinely, if very slowly, making forward progress rather than
deadlocked — an unbounded run (no watchdog) earlier in this same
investigation completed in ~602s, terminated by the test framework's own
`-Dtests.timeoutSuite=580000!`, not by a VM-level hang. `testSlicesDense`'s
slowness (not correctness) is therefore a SEPARATE, still-open issue from
`testSlicesSparseWithFilter`'s (now-fixed) correctness bug — likely
CratonVM interpreter overhead on a reflection/exception-handling-heavy path
(deep `Method.invoke()` chains + `local_liveness::analyze` cache misses were
observed dominating a live gdb snapshot of a `testSlicesDense` run), not a
lost-wakeup or a data-corruption bug. Whoever picks this doc back up next
should treat `testSlicesDense` as a performance investigation, not a hang/
correctness investigation, and should NOT expect the STW-monitor-race /
GC-audit finding 1 attribution to apply here either — finding 1(a) itself
was independently fixed and merged (`371347920`) before this fix landed, and
`testSlicesDense`'s slowness persists on top of that fix too.

---

# 2026-07-13 current-dev residual update

**Status: OPEN.** This remains a real CratonVM-only non-progress failure, but
the current evidence does **not** establish the historical GC/STW-monitor-race
attribution as its root cause. Keep the cross-reference as historical context;
do not use it to close or otherwise classify this current residual.

Validated on `origin/dev` at `acf9aaf7`, in isolated worktree
`/data/victor-worktrees/cratonvm-es-ivfknn-complete-20260712-1540`, using
binary
`/data/data/cratonvm-targets/es-ivfknn-complete-20260712-1540/release/cratonvm-es-ivfknn-complete-20260712-1540`
and the existing Elasticsearch fixture
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`.
The current `others.tsv` selection is runner start `2538` (not the historical
start `549`):

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/victor-worktrees/cratonvm-es-ivfknn-complete-20260712-1540/apps/elasticsearch-suite-runner/.suite-es-ivfknn-complete-20260712-1540" -Exe /data/data/cratonvm-targets/es-ivfknn-complete-20260712-1540/release/cratonvm-es-ivfknn-complete-20260712-1540 -JdkHome /home/victor/jdk25 -TimeoutSec 90 -RunName ivfknn-current -ModeName craton-current -Start 2538 -Count 1
```

Results:

- The same selection passes on HotSpot/JDK 25 in 13.5 seconds.
- With Craton JIT enabled, the runner aborts after roughly 5--8 seconds with
  `Test abandoned because suite timeout was reached`; the in-test message says
  `>=580000 msec`. This is not the outer 90-second runner timeout. Narrow
  `System.nanoTime` and `TimeUnit.MILLISECONDS.toNanos(580000)` probes return
  correct values, so a generic timeout-clock or `TimeUnit` conversion fault is
  not an adequate explanation.
- With `--nojit`, the process consumes about one CPU continuously and fails to
  produce a test result before a 90-second outer timeout. A 150-second run
  behaved the same. This is genuine non-progress, not merely a slow test.

A Craton watchdog dump during the no-JIT run places the active worker in the
vector-query path:

```
testSlicesSparseWithFilter -> doTestSlicesSparse -> doTestSlices
-> IndexSearcher.search/rewrite -> IVFKnnFloatVectorQuery.rewrite
-> AbstractIVFKnnVectorQuery.rewrite -> TaskExecutor.invokeAll
-> searchLeaf -> IVFKnnFloatSlicedVectorQuery.getLeafResults
-> CodecReader.getSortedDocValues -> Lucene90DocValuesProducer.getSorted
-> IndexInput.randomAccessSlice -> MockIndexInputWrapper.slice
-> ByteBuffersIndexInput.slice
```

The suite coordinator is waiting in `ThreadLeakControl.join` while that worker
does not advance. A local, uncommitted experiment which decoded raw compact
`long` arguments for `ByteBuffersDataInput.seek(long)` and `slice(long,long)`
also still timed out in no-JIT mode at 90 seconds; it was deliberately not
merged because it did not fix the residual.

The host disallows non-parent `gdb -p` attachment through Yama ptrace policy,
so native thread-state confirmation remains unavailable without a permitted
parent/debug launch. The next investigation should obtain that capture (or
equivalent VM instrumentation) around the `TaskExecutor`/`ByteBuffersIndexInput`
path, rather than treating the older GC-monitor finding as proven for this
current run.

---
# ES HANG - server org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests

Status: OPEN

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard4`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.126`
- tests parsed: `0`
- failed parsed: `0`
- note: ``

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun`
- Branch used for collection: `codex/es-nonpassed-rerun-20260708-191002`
- Collection binary: `/data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002`
- Binary base dev SHA: `3d61003bbfdf9c6b045d29afefd45519dc558881`
- Docs generated after isolated worktree fast-forwarded to dev SHA: `8736a20b6e269bae3ec89d44e22117e2d4eba9a0`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-3ff8aa1c4b -ModeName repro-3ff8aa1c4b -Start 549 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/logs/server.org.elasticsearch.search.vectors.DiversifyingChildrenIVFK.5df450c3f4cb.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/logs/server.org.elasticsearch.search.vectors.DiversifyingChildrenIVFK.5df450c3f4cb.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/results.tsv`

Extracted stderr signals:
- `2026-07-09T05:10:44.709577Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:10:44.709598Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `==== jstack at approximately timeout time ====`
- `2026-07-09T05:16:18.150553Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:16:18.150573Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:16:18.150578Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:16:18.150582Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
Current classification:
- 600 second class watchdog timeout in the completed four-shard collection run.
- Treat as an open hang until reproduced or disproved on current `dev`.


---

## 2026-07-10 investigation (fix/es-vectors-ivfknn-hang-20260710)

**Status: OPEN** (unchanged) — the underlying interpreter-level deadlock this
doc describes is real and NOT fixed. What changed: a JIT regression that had
started masking the hang behind a much-faster SIGSEGV is now fixed, so the
suite runner will show the original 600s HANG again (not a crash) until the
deadlock itself is root-caused.

### Regression found and fixed: guarded-inline-getfield SIGSEGV

Reproducing on current `dev` (tip `f4ee4065` at investigation time, worktree
`/data/data/wt-es-vectors-ivfknn-hang-20260710`, binary
`cratonvm-es-vectors-ivfknn-hang-20260710`) with the doc's own repro command
(`-Dtests.method=testSlicesDense`, same seed) no longer hangs for 600s under
JIT — it **SIGSEGVs almost immediately** (~1-2s in):

```
dmesg: SUITE-Diversify[97470]: segfault at 4c ip 0000760e80332071 sp ... error 4
```

gdb (`handle SIGUSR1/SIGUSR2 nostop noprint pass` first, per
[[reference_linux_gdb_signal_passthrough]]) shows the fault inside JIT-compiled
code, an array bounds-check/load sequence (`mov 0xc(%rax),%r10d` = array
length read) with `rax=0x40` — a small int-shaped value, not a real object
pointer, being dereferenced as one; `0x40 + 0xc = 0x4c` matches the dmesg
fault address exactly. The corrupted value traces back to a local variable
slot fed by a preceding `getfield`.

**Bisection:**
- `CRATONVM_JIT_GETFIELD_HELPER=1` (forces the checked-helper-only getfield
  path) makes the SIGSEGV disappear — the run reverts to the ORIGINAL
  interpreter hang this doc describes (confirms the SIGSEGV is a NEW
  regression sitting on top of the pre-existing hang, not a different bug).
- `CRATONVM_OSR_NEWARRAY=0` (the sibling `perf/throughput-20260710` change)
  does NOT avoid the crash — rules out OSR-newarray re-enable as the cause.
- Root cause isolated to commit `07dfa5e0` ("Guard inline JIT getfield with
  heap-region bounds check (default on)", merged to dev via `756c4b84`,
  see [[project_perf_throughput_20260710]]). Static review of the guard's
  region-containment codegen and the per-object `GC_FLAG_COMPACT` routing did
  not surface a specific wrong-instruction bug with full confidence — this is
  hot JIT x64 codegen and a wrong guess risks a worse regression, so rather
  than patch the codegen blind, `guarded_inline_getfield_enabled()`
  (`jit/src/x64.rs`) was flipped from default-ON to opt-in
  (`CRATONVM_JIT_GUARDED_GETFIELD=1`), matching this codebase's own established
  pattern for an unproven fast path. This is a real, measurable performance
  give-back (bt18 was 7080ms/8.2x with the guard on, vs ~13s helper-only per
  the original commit's own A/B) until someone re-derives the exact
  corrupting instruction and re-enables it default-on.
- Fixed on branch `fix/es-vectors-ivfknn-hang-20260710`, JIT test suite
  (970 tests: 888 lib + 82 `ir_vs_singlepass` differential) green after the
  flip (one new test and one whole differential-test file —
  `ir_vs_singlepass.rs`, purpose-built around the guarded-inline path — needed
  updating to explicitly opt in via the same env var, since they'd relied on
  the old default-on behavior).

### Underlying interpreter hang — now characterized in detail, still OPEN

Reproduced directly (bypassing the suite runner) with
`CRATONVM_JIT_GETFIELD_HELPER=1` (or `--nojit`) and
`--stack-dump-on-timeout=90`. The process genuinely spins (82-112% CPU, not
blocked/idle) with **zero forward progress** across repeated watchdog dumps
during Lucene's `IndexWriter` flush/merge machinery for `testSlicesDense`.
Exact contention point **varies run to run** (seed is fixed but real-thread
scheduling isn't) — seen stuck in, across different runs:
- `org/apache/lucene/util/FileDeleter.decRef`/`getRefCountInternal`
  (confirmed via `javap` these are NOT `synchronized` in Lucene 10.4's
  `FileDeleter`, so this is not a lock — the interpreter frame table's `pc`
  simply stops advancing here across the 3s grace period)
- `IndexFileDeleter.logInfo` (also not synchronized)
- A live `Lucene Merge Thread` blocked entering
  `MockDirectoryWrapper.maybeThrowDeterministicException()` — confirmed
  `synchronized` via `javap` on the real `lucene-test-framework` jar — while
  another `Lucene Merge Thread` (`IndexWriter.mergeMiddle`) sits in a
  legitimate `Object.wait()` (wait-site frame captured via
  `CRATONVM_DBG_MONENTER=1`)

A live gdb attach (`gdb -p <pid>`, no signal needed since it's spinning, not
crashed) during one run caught a genuine two-thread situation: the worker
thread and a `Lucene Merge Thread` each blocked in
`vm/src/threading/monitor.rs::Monitor::enter` (`monitor_enter_synchronized_method`,
`vm/src/vm/vm_exec.rs:1104`) at the same moment, on different Lucene
`synchronized` methods.

**Ruled out:**
- `Monitor::enter`/`wait`/`notify` (`vm/src/threading/monitor.rs`) were
  read in full — the reentrant-owner fast path and the wait/notify condvar
  loops are already hardened with explicit "LOST-WAKEUP FIX" handling for the
  interrupt-races-notify case. No obvious bug found by inspection.
- A GC/monitor-registry desync matching the historical
  [[reference_bugv_nonmoving_sweep_monitor_remap]] (BUG-V) shape was
  considered (this workload allocates heavily) — a differential test with
  `--Xmx 8g` (much less GC pressure) still hung, weakening but not
  conclusively ruling this out (8g can still GC under this workload's
  allocation volume).
- The `gen_heap.rs`/`g1.rs` OOB-field-read WARN bursts this doc originally
  flagged (`class_id=ClassId(0) num_slots=0 java/lang/Object`) fire in a tight
  ~1.6ms burst and then stop — they are NOT the hang itself (the thread keeps
  running for tens of seconds afterward before getting stuck elsewhere); per
  the guard's own log message these are believed-benign speculative
  collection-layout probes, not corruption.

**Not yet found:** the actual mechanism that leaves a thread spinning forever
with no forward progress. Given the contention point moves between runs, this
smells like a genuine timing-dependent VM-core bug (lock-order or a
notify/wakeup gap under specific interleavings) rather than one fixed bad
instruction — needs dedicated concurrency-debugging time, ideally with
`RUST_BACKTRACE`+`CARGO_PROFILE_RELEASE_DEBUG=2` symbols and a tighter, more
deterministic repro (e.g. forcing single-threaded merging) than this full ES
suite test.

**Repro recipe** (direct invocation, bypasses the suite runner's own
possibly-broken jstack — see below):
```bash
ES=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch
CP=$(tr -d '\r' < "$ES/server/build/craton-testcp.txt" | tr '\n' ':' | sed 's/:$//')
CRATONVM_ENABLE_NATIVE_RING=1 CRATONVM_DBG_MONENTER=1 "$EXE" \
  --java-home /home/victor/jdk25 --stack-dump-on-timeout 90 --Xmx 2g \
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home="$ES" -Djava.awt.headless=true \
  -Dtests.method=testSlicesDense \
  <standard ES test JVM args - see run-elasticsearch-suite.ps1 Get-EsJavaArgs> \
  -cp "$CP" org.junit.runner.JUnitCore \
  org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests
```
Note: `apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1`'s own
`craton-testcp.txt` classpath files have **CRLF line endings** — a naive
`tr '\n' ':'` leaves a trailing `\r` on every path component and breaks
classloading (`Could not find or load main class org.junit.runner.JUnitCore`)
before you even get to the hang; strip with `tr -d '\r'` first.

Also: the suite runner's own `"==== jstack at approximately timeout time ===="`
section (visible in this doc's original evidence) is **empty** — no thread
dump content follows it, on this build. Use CratonVM's own
`--stack-dump-on-timeout=N` watchdog (dumps real interpreter frame chains +
a thread summary) instead of relying on that marker for future repros of this
cluster.

**Verified NOT the same bug as `afa4a6fd`** (String compact-layout
field-offset dual-dispatch, TaskInfoTests SIGSEGV, landed on `dev` while this
investigation was in progress): merged `origin/dev` into this branch and
re-tested with `CRATONVM_JIT_GUARDED_GETFIELD=1` (forcing the guard back on)
— still SIGSEGVs. The two are separate, if superficially similar
(compact/legacy-layout dispatch), bugs in different code paths (generic
getfield inline arms vs. hand-rolled String-intrinsic field loads).


---

## 2026-07-10 follow-up: confirmed as the GC audit's Finding 1 (STW/monitor race)

Deep-dived the underlying interpreter hang with live gdb + a full-debug-info
rebuild (`CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_PROFILE_RELEASE_STRIP=none` —
the normal release profile is line-tables-only and can't resolve locals/args
for a `self`-based inspection). Found this is the SAME bug as
`docs/known-issues/gc-audit-2026-07-10-open-findings.md` finding 1
("STW/monitor race family: GC and monitors vs excluded threads"), an
actively-investigated VM-core defect discovered independently via a
synthetic MTChurn stress harness. Full cross-reference and new evidence
(a genuine Lucene NPE manifestation, not just the deadlock) recorded in
that doc. Summary:

- Direct live-gdb inspection (multiple attempts) confirms the worker thread
  AND a `Lucene Merge Thread` genuinely parked in
  `parking_lot::Condvar::wait` inside `Monitor::block_enter`/`enter`
  (`vm/src/threading/monitor.rs:457/500`) simultaneously — with only 6
  threads total in the process and the other 4 idle/legitimately elsewhere,
  no live thread is positioned to ever release/notify what these two are
  waiting on.
- The manifestation is **non-deterministic run to run, same seed**: most
  runs hang forever; one 153s run instead completed with 2 real failures,
  including `NullPointerException` on `ReadersAndUpdates.dropMergingUpdates()`
  (`rld` null — should never happen) thrown from a Lucene merge thread,
  immediately preceded by a 544+ occurrence burst of the
  `gen_heap::get_field` OOB-field WARNs this doc originally flagged. This
  is the same "corruption escaping as a downstream exception" shape the GC
  audit doc describes for finding 1(b), via a different (real, non-synthetic)
  trigger workload.
- Did NOT attempt a fix. `vm/src/threading/monitor.rs`/the STW barrier is
  already being investigated by a dedicated effort with its own probe
  tooling; a WIP attempt at part of the fix
  (`wip/gc-stw-quota-race-20260710`) is explicitly parked as unsafe
  ("DO NOT MERGE... hangs completely under load... still SEGVs"). This ES
  repro is left as an additional, real-world validation case for whoever
  picks that investigation back up — see the GC audit doc for the repro
  recipe cross-reference and status.

**Status stays OPEN** — root cause is now well-characterized and tied to a
known, tracked, actively-investigated VM-core defect rather than an
ES/Lucene-specific bug. Not expected to be independently fixable without
the GC-audit team's monitor/STW-barrier work landing first.


---

## 2026-07-10 follow-up: guarded-inline-getfield SIGSEGV root-caused and FIXED; flag re-enabled default-ON

The unproven fast path this doc flipped to opt-in has been root-caused, in a different
investigation the same day: the WildFly Host Controller invoke-inline-cache SIGSEGV
(`docs/internal/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md`).
Root cause: the vm-side JIT field resolvers (`vm/src/runtime/interpreter.rs`) fabricated a
`(0, false)` "compact slot" for any field with NO genuine registered `CompactLayout` entry
(`compact_field_slot(...).unwrap_or((0, false))`), and the compact-offset inline getfield arm
(`jit/src/x64.rs`) trusted it — a REFERENCE field with the fabricated `is_ref=false` fell into
the int-category match arm and got a 32-bit `MOVSXD` (sign-extended) load of half a 16-byte
`Value` cell, producing exactly this doc's "receiver `0x40`, a small int value used as an array
pointer" shape (the loaded bytes are the cell's uninitialized tag/payload boundary padding —
non-null, 8-aligned, and totally bogus). Fixed: the resolver now returns `Option<(u32, bool)>`
for the compact slot (never fabricates), and `'L'`/`'['` type tags get an explicit 64-bit
payload load in the compact arm as defense-in-depth.

Re-verified clean on branch `fix/ivfknn-guarded-getfield-reverify-20260710`: ran this doc's
exact repro (`testSlicesDense`, same seed) with `CRATONVM_JIT_GUARDED_GETFIELD=1` — **no
SIGSEGV, no dmesg segfault entry**. Both this run and a baseline run (flag unset) hit the
IDENTICAL pre-existing watchdog-timeout abort (the STW/monitor-race hang described below,
unaffected by this flag) at the 90s `--stack-dump-on-timeout` mark — confirming the SIGSEGV is
gone and the only remaining blocker for this class is the already-tracked, separate hang.
`guarded_inline_getfield_enabled()` is re-enabled default-ON (opt out with
`CRATONVM_JIT_GETFIELD_HELPER=1`); `cargo test --release -p cratonvm-jit` — `--lib` 889/889,
`ir_vs_singlepass` 82/82 (matching this doc's own prior baseline), all other integration test
binaries green, except two PRE-EXISTING, unrelated SIGSEGVs in `intrinsic_string_access`/
`intrinsic_string_search` (confirmed independent of this flag — crash identically with
`CRATONVM_JIT_GETFIELD_HELPER=1` forcing the old default too; spun off as a separate follow-up,
not investigated further here).

---

## 2026-07-11 addendum: independent JIT-cache invalidation gap found (unrelated to this SIGSEGV's actual cause)

While independently re-investigating this cluster (in parallel with, and
without visibility into, the `fix/ivfknn-guarded-getfield-reverify-20260710`
work documented immediately above), found and fixed a real but **separate**
defect: `ClassManager::upgrade_synthetic_class` / `recompute_subclass_layouts`
(`classloading/src/class_manager.rs`) can change a class's field layout
mid-run (synthetic JDK stub -> real `.class` bytecode found on the
classpath), shifting the byte offsets already-JIT-compiled `getfield`/
`putfield` code may have baked in as x86 immediates — and nothing evicted
that stale-compiled code. `install_jit_invalidate_hook` already existed
(already called from `redefine_class`'s Step 8) but had **zero installers
anywhere in the VM**, so that call was always a silent no-op.

This is NOT the cause of this doc's SIGSEGV — that is conclusively the
fabricated-`(0, false)`-compact-slot bug described in the follow-up section
above, verified against this doc's own repro with a clean run (no SIGSEGV,
no dmesg entry). This is a different, independently-real gap surfaced while
looking at the same code paths: nothing else in the VM evicted JIT code
after a layout-changing synthetic-stub upgrade, so a similar "stale baked
offset" corruption could occur through that mechanism even with the
fabricated-slot bug fixed. Fixed by wiring up `install_jit_invalidate_hook`
in `vm/src/vm/vm_init.rs` (mirroring the existing
`resolution_invalidate_adapter` pattern) and calling it from both
layout-changing paths. Regression test added in
`classloading/src/class_manager.rs`
(`recompute_subclass_layouts_fires_jit_invalidate_hook_for_changed_descendants`,
verified to fail without the fix). Full `cratonvm-classloading` (549 tests),
`cratonvm-jit --lib` (889 tests), and targeted `cratonvm-vm` redefine tests
pass with the fix.
