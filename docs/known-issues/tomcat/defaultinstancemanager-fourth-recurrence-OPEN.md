# `TestDefaultInstanceManager.testClassUnloading` — fourth occurrence (OPEN)

| | |
|---|---|
| **Status** | OPEN — **root source NAMED 2026-08-10: `collection-overlays`.** Re-measured on dev; the mechanism moved. Everything from *What is measured* down to *The open question* describes a tree that no longer behaves that way; start at *RE-MEASURED ON dev*. |
| **Symptom** | `java.lang.AssertionError: expected:<8> but was:<9>` (`TestDefaultInstanceManager.java:66`) |
| **First bad commit** | [`1d2817c75`](#the-bisect) `feat(types,gc,vm,jit): delete identity_hash_code from ObjectHeader` (2026-08-07 08:24) |
| **Reproduces** | default GC and ZGC; **G1 passes**. Deterministic, ~17 s standalone |
| **Fourth in a chain** | 07-14 `fixed-suite-bugs/tomcat/defaultinstancemanager-classunloading-count-mismatch-FIXED.md` · 07-27 `fixed-suite-bugs/tomcat/defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md` · 08-01 `fixed-suite-bugs/tomcat/defaultinstancemanager-third-recurrence-FIXED.md` |

## Reproduction

```powershell
apps\tomcat-suite-runner\run-one.ps1 `
  -Class org.apache.catalina.core.TestDefaultInstanceManager `
  -Exe <cratonvm.exe> -TimeoutSec 900
```

**Before 2026-08-10 this did not reach the assertion at all.** The fixture died
in `LifecycleBase.start` with

```
Unable to create WebResourceSet from [<docBase>\file:\C:\…\WEB-INF\lib\bug69135-lib.jar]
```

which is a **different defect that masks this one**: `URL.toURI()` allocated a
real `java/net/URI` and wrote only the SYNTHETIC positional slots into it, so
the field the real bytecode reads (`string`) stayed null and every
`url.toURI()` stringified to empty. `StandardRoot.processWebInfLib`'s
`new File(uri)` then produced a File whose path was the whole URL text, which
is not absolute, so it was resolved against the doc base. Fixed by populating
the named fields the way `File.toURI()` always has (`url_parse` +
`uri_store_named`); `probes/FileUrlShapeProbe.java` is the differential and is
byte-identical to HotSpot on both platforms now.

Worth knowing before trusting a green run here: **any** webapp test with a
`WEB-INF/lib/*.jar` was failing at startup, so a Tomcat suite result from
before that fix is not evidence about this test's own assertion.

## It is NOT the defect that was fixed three times

Every lever the previous three writeups turned is inert now. This matters more
than any single row: the 08-01 page closed on a differential
(`CRATONVM_NO_MOVING_YOUNG` flipping PASS/FAIL), and that differential is gone.

| lever | 08-01 behaviour | now |
|---|---|---|
| `CRATONVM_NO_MOVING_YOUNG=1` | flipped baseline FAIL → PASS | **still FAIL** |
| `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1` | flipped fixed PASS → FAIL | **still FAIL** |
| `--nojit` | PASS | **still FAIL** |
| `-Xmx` (promotion timing, the 07-14 mechanism) | — | 2g / 512m / 256m all FAIL |

`--nojit` failing removes the entire JIT-root family the 07-27 fix addressed.
The heap-size rows remove the promotion-timing accident 07-14 and 07-27 both
describe. The 08-01 predicate is intact and *working*: measured
`conditional_metadata=true` and the evicted mirror is `DEFERRED`, i.e. left out
of the unconditional root set exactly as intended.

## The bisect

`git bisect run`, 10 steps over 1100 revisions, `638bd1190` (the 08-01 fix) as
the good endpoint — **verified PASS on this fixture** rather than trusted from
the 08-01 page's recorded result. Five commits were SKIPped legitimately (build
failures, and one commit that died with an unrelated `langtagSets` NPE — the
run script's third arm, exit 125, so a broken build could not be miscounted as
BAD and point the bisect at the wrong commit).

```
good  af2105aa6  feat(gc,vm): the identity hash now lives in the mark word, end to end
BAD   1d2817c75  feat(types,gc,vm,jit): delete identity_hash_code from ObjectHeader
```

The switchover to the mark word is **good**; deleting the header field is bad.
That is worth stating explicitly because the opposite is the natural guess.

## STALE (pre-2026-08-10) — What is measured, and what it rules out

All from the failing run, via `CRATONVM_DBG_MIRRORPIN=1 CRATONVM_DBG_MIRRORPIN_WHY=1`
(this branch wires callers for `VmHeap::root_held_paths` / `find_referrers`,
which had existed since the doc-26 round with **no call sites at all**):

| question | answer |
|---|---|
| Is the mirror in the unconditional root set? | No — `deferrable=true => DEFERRED` |
| Does any heap field reference the mirror? | No — `root_held_paths=1`, the target itself |
| Does any heap field reference its loader? | No — same |
| Does a live activation root the loader (frame / JIT)? | No — no `root via FRAME`/`JIT_ACTIVATION` for any JSP class |
| Does `loader_pin` pin it from a live instance? | No — `live_instances_of_this_loader=0` |
| How many non-moving young sweeps run? | Exactly **1**, the `System.gc()` — the intended path |
| Which liveness arm calls the mirror live? | `region=young old_gen_allocated=false young_survivor=true` |

Two traps in reading those rows, both of which cost a round of investigation:

* **"Zero heap referrers" is not evidence of a direct root.** instance→class is
  the header's `class_id` and instance→loader is the `loader_pin` side table;
  neither is a ref slot, so neither appears in a referrer walk. The mirror
  having no referrer is the EXPECTED state, not a finding.
* **`young_survivor=true` looked like a faithful report and is not one.**
  `is_live_young_survivor` is `word0 != 0` — it reports whether the sweep
  zeroed the span, never a mark bit. I first read that as "so it faithfully
  reports what the MARKER decided, and the marking is what needs explaining",
  and wrote that down. It is wrong, and the section below measures why: the
  marker never marked this object, and the sweep never *visited* it either, so
  the byte pattern reports the liveness of neither. Inferring a verdict from a
  side effect is sound only while every path that skips the side effect is
  also a path that cannot leave a dead object behind — and the sweep has at
  least three such paths.

Also eliminated:

* **Overlay over-rooting (`roots.rs` step 17).** The long comment at
  `gc_and_alloc.rs:2062` documents this as an unfixed deeper bug that retains
  exactly this JSP cluster ("21 direct hits"). It has since been gated:
  `native_roots::scan_collection_overlays` skips the unconditional scan when
  `major_gc_requested()` under Generational, which is precisely this test's
  path. Still unconditional for **ZGC** and G1, which may be why ZGC fails —
  but it cannot be the default-collector mechanism. **The comment is stale and
  should be corrected when this closes.**
* **The plausibility-screen hole `1d2817c75` shipped red.** Its own message
  predicted Stage 3 (HEADER_SIZE 16) would close it, and it did:
  `mark_and_push_rescues_a_walked_base_the_plausibility_screen_rejects` passes
  on dev.

## 2026-08-10 RE-MEASURED ON dev: none of the chain below still holds

Every row in *What is measured* and in the two sections after it was re-taken on
today's `dev` with the same instrument (`CRATONVM_DBG_MIRRORPIN=1
CRATONVM_DBG_MIRRORPIN_WHY=1`) and the same fixture, and **the measurements
invert**:

| question | this page | dev, 2026-08-10 |
|---|---|---|
| young-marker edges reaching the evicted loader | **0** | `is_marked=true`, `loader_marked=true` |
| `young_survivor` arm for the mirror | **true** | **false** |
| `live_instances_of_this_loader` | **0** | **1** |
| `root_held_paths` for the loader | 1 (the target itself) | **8** |
| non-moving young sweeps | "exactly 1, the `System.gc()`" | **zero — the cycle never runs** |

The last row is the one that dismantles the chain. `[MARKWHY] sweep enter` is
new and prints whenever a watch is ARMED, with no from-space condition; the
arming line prints and the sweep line never does, on the default collector and
under `CRATONVM_NO_MOVING_YOUNG=1` alike. `CRATONVM_DBG=heap-trace` agrees
independently — `run_non_moving_young_cycle`'s own trace line never fires.
**There is no non-moving young sweep in this test on dev**, so "the walk covers
the address and strides over it" is not a live statement about anything.

Read *The marker is innocent* and *Which skip arm* below as a record of a tree
that no longer exists, not as findings to build on.

## What dev shows instead: an ordinary reachability chain, and Jasper's JDT compiler is holding it

The mirror is retained because the class has a **live instance**, whose loader
`loader_pin` therefore keeps alive, whose mirror `mirror_pin` therefore keeps
alive. That is the pin machinery working exactly as designed. The question is
why an instance of an EVICTED JSP is still reachable, and the instrument
renders the paths:

```
StackMapFrame -> VerificationTypeInfo -> VerificationTypeInfo
  -> SourceTypeBinding -> LookupEnvironment
  -> JDTCompiler$1 -> JDTCompiler
  -> JspCompilationContext   -> <the evicted loader>
  -> JspServletWrapper       -> <the evicted annotations_jsp instance>
```

Seven of the loader's eight paths are that chain; the eighth is
`java/lang/Class -> loader`, i.e. the mirror pin itself. The LIVE control
(`bug36923_jsp`) has the same JDT chain **plus** a legitimate one:

```
ArrayList -> Object -> StandardWrapper -> JspServlet -> JspRuntimeContext
  -> FastRemovalDequeue -> FastRemovalDequeue$Entry -> JspServletWrapper -> instance
```

`FastRemovalDequeue` is Jasper's `maxLoadedJsps` LRU. The evicted JSP is
correctly **absent** from it — eviction worked. What did not go away is the JDT
compiler's own graph.

So the open question is now the opposite of this page's premise, and it is a
question about **roots**, the family this page declared innocent:

> Every rendered path is headed by an `org.eclipse.jdt.internal.compiler`
> object — `StackMapFrame`, or an `ArrayList` — with no parent. Those are
> transient objects the JSP compiler produces while emitting a class file and
> should be garbage the moment compilation ends. **What roots them?**

### Answered, by measurement: `collection-overlays`

`CRATONVM_DBG_ROOT_SOURCE=1` is new and records which NAMED entry of
`VM_ROOT_SOURCES` contributed each root; `[MIRRORWHY]` now prints
`[root=<source>]` on every rendered path. For the evicted JSP, over two runs:

```
  33 [root=collection-overlays]        11 [root=collection-overlays]
   6 [root=<not-a-direct-root>]         2 [root=<not-a-direct-root>]
```

Every JDT-headed path is rooted by **`collection-overlays`** —
`native_roots::scan_collection_overlays` → `external_roots::scan_external_roots`.
The `<not-a-direct-root>` remainder is the mirror itself and the
`java/lang/Class -> loader` pin edge, i.e. the pin machinery working as
designed on top of a root that should not be there.

**So this page's own *Also eliminated* entry was wrong.** It reads:

> **Overlay over-rooting (`roots.rs` step 17).** … It has since been gated:
> `native_roots::scan_collection_overlays` skips the unconditional scan when
> `major_gc_requested()` under Generational, which is precisely this test's
> path. … it cannot be the default-collector mechanism.

The gate exists and its condition is right, but it does not cover the
collection whose root set retains this cluster — the roots are contributed
anyway, on the default collector, in the failing run. The entry was eliminated
by reading the gate rather than by asking which source rooted the object, which
is the question nothing could answer until now. Its own earlier evidence
("21 direct hits" on exactly this JSP cluster, `gc_and_alloc.rs:2062`) was
pointing at the answer the whole time.

### What to do next, and the trap in doing it

The obvious move — widen the gate so the overlay scan skips here too — is the
one to be careful with. `scan_external_roots` exists because an overlay's
backing array can be the ONLY reference to a live object; skipping it wholesale
trades this over-retention for under-retention, which is a use-after-free rather
than a failing assertion. The gate's shape (`is_active() ||
unregistered_jit_frame_on_stack() || major_gc_requested()`) is a
safety-conditional, not a feature flag.

Two things to establish first, both one run each now that the source is
nameable:

1. **Which collection contributes them.** If it is a young/old cycle outside
   the `System.gc()` window, the gate is simply not being consulted then, and
   the question is whether that cycle can afford to skip.
2. **Whether the overlay entries are stale.** A JDT `StackMapFrame` reachable
   only from an overlay after compilation has finished suggests the overlay is
   retaining entries whose Java-side owner is gone — in which case the fix is
   pruning the overlay, not skipping the scan, and it costs nothing in safety.

## STALE (pre-2026-08-10) — The marker is innocent. The SWEEP never visits the span.

`CRATONVM_DBG_MARK_WHY_CLASS=<internal/class/Name>` arms a watch on that class's
loader (the address is not knowable before the class is defined) and labels
every young-marker edge that reaches it, plus the sweep walk's decision at that
base. Measured, dead loader vs live loader in the same run shape:

| | `annotations_jsp` (evicted) | `bug36923_jsp` (live) |
|---|---|---|
| young-marker edges reaching the loader | **0** | 4 — `3x field`, `1x loader_pin` |
| sweep walk stops at its base | **never** | yes, `side_marked=true` |

The live loader is the control that proves the instrument can fire. So:

1. The **marker is correct** — it never marks the evicted loader. Every root
   source eliminated across four investigations was eliminated correctly.
2. The **sweep walk never stops at that base.** I expected a skip list here — a
   free block, a TLAB reservation, or an abandoned stretch — and the next
   section measures that it is **none of them**. The walk covers the address
   and strides over it.
3. A span the walk never visits is **never zeroed**.
4. `is_live_young_survivor` is literally `word0 != 0`, resting on "the young
   sweep writes an ALL-ZERO header over every span it reclaims". An unvisited
   span never got that write, so it reports **live**.
5. `reconcile_class_mirrors` consults exactly that verdict, so it retains the
   mirror; `rebuild_mirror_pins` re-registers it; the `WeakReference<Class>`
   never clears; the annotation cache stays at 9.

That is a complete chain from a false premise to the assertion, and it explains
why every rooting lever was inert: nothing about rooting is wrong.

Note the premise is the one the bisected commit is documented as having
weakened — "a fresh header is never all-zero" died when the hash went lazy, and
these predicates were rewritten onto the mark word "WEAKER, not equivalent".
The connection is consistent but **not yet proven**; the open question is now
narrow and mechanical.

## RETRACTED 2026-08-10 — `objects_swept=0` was a counter read before it is written

The section below concluded, from `objects_swept=0`, that this sweep reclaimed
nothing, that no young object is reclaimed at all, and that class unloading
"cannot work by construction". **All three are withdrawn.** `objects_swept` is
declared at `gen_heap.rs:9439`, printed by the `[MARKWHY] walk ended` line, and
incremented in exactly one place — the publication loop **345 lines below that
print**. The number is a structural zero in every run, on every workload,
whether the sweep reclaimed a million objects or none. It measured the distance
between two lines of code, not the heap.

Two things follow, and the second is why this matters more than a typo:

* **Class unloading works on this path.**
  `regression-suite/src/RClassUnloadSweep.java` (new, scheduled) defines a class
  in a throwaway loader, uses it, drops every strong reference and collects.
  `payload.class.unloaded=true` on HotSpot and on CratonVM under the default
  collector, `CRATONVM_NO_MOVING_YOUNG=1`, `-XX:+UseG1GC` and `-XX:+UseZGC`. If
  the young sweep reclaimed nothing, that vector could not print `true` under
  `CRATONVM_NO_MOVING_YOUNG=1` — the exact path the section below called inert.
* **The scope claim went with it.** "This is a young-generation reclamation
  failure on the `System.gc()` path and should be expected to cost throughput
  and footprint everywhere else" was the most actionable sentence on the page,
  and it pointed at a defect that is not there.

The instruments were fixed rather than the line deleted, because the question
under it — *did this sweep free anything* — is a good one that nothing answered:

* `[MARKWHY] walk ended` now reports the walk-time facts it can actually see:
  `dead_regions`, `dead_watermark`, and `unwinds`/`unwound_entries`. The five
  `dead_regions.truncate(dead_watermark)` sites were **silent**, so "found
  nothing" and "found plenty and unwound all of it on a grid anomaly" printed
  identically — which is shape 2 of *The open question* below, previously
  undecidable from any output the VM produced.
* `[MARKWHY] sweep done` reports `objects_swept`/`bytes_swept` after the
  publication loop, the only thing that writes them.
* `[MARKWHY] STRADDLE` names the object whose computed extent swallows the
  watched base. The pre-existing sweep hook tests `cursor == watch`, so it is
  silent in precisely the case this page measured — the walk covering the
  address without stopping at it — and could never name the culprit stride.

**What survives is the whole finding:** the walk covers the evicted loader's
base and never stops at it. That is measured, it has a live control, and the
sections above stand. What was wrong was the inference from there to the heap.

One structural point the retraction turns up, because it explains why this
defect surfaces as a class-unloading failure and never as corruption: the
phantom-extent guard (`gen_heap.rs`, "a header that SUBSUMES a live object")
fires only when the swallowed object is in `side_sorted` — i.e. **marked**. An
over-sized extent that swallows only DEAD objects is invisible to it. A dead
object inside a retained LIVE extent is never visited, never zeroed, and
`is_live_young_survivor` (`word0 != 0`) then answers "live" for it forever.
That is a one-sided guard, and this is the failure it cannot see.

## Which skip arm? NONE (read the retraction above first)

`CRATONVM_DBG_MARK_WHY_CLASS` now also classifies the watched address against
every stretch the walk can stride over, and reports where the walk ended.
Measured on the failing run:

```
[MARKWHY] skip-arm probe: watch=0x28b16267140 off=0x4267140 used=0x603e118
                          in_free_block=None in_jit_tlab_skip=None
[MARKWHY] walk ended: cursor=0x603e118 used=0x603e118 watch_off=0x4267140
                      objects_live=119985 objects_swept=0
```

All three candidates are eliminated:

* **not a free block** — `in_free_block=None`;
* **not a reserved TLAB tail** — `in_jit_tlab_skip=None` (consistent with
  `--nojit` failing too);
* **not an abandoned stretch** — the walk ran to completion, `cursor == used`,
  and the watched offset is well inside the walked range.

So the walk *covers* the address and still never stops at it. Its object grid
strides over that base: some earlier object's computed size swallows the span.

~~And the headline number: `objects_swept=0`.~~ **WITHDRAWN — see the
retraction above.** The counter is incremented in the publication loop, which
runs 345 lines AFTER the line that printed it, so the zero was structural. The
paragraph that followed it — "no young object is reclaimed at all … class
unloading cannot work by construction" — is withdrawn with it, and
`RClassUnloadSweep` is the vector that refutes it.

`objects_live=119985` is real, and so is everything above it: the walk covers
the watched address, no skip arm claims it, and the walk still never stops
there.

## The open question

Why is `dead_regions` empty? Two shapes, distinguishable:

1. the walk classifies every object LIVE (its grid is desynced, so it never
   lands on a dead object's base — which is exactly what the watched address
   shows), or
2. the walk collects dead spans and then UNWINDS them: `dead_regions.truncate`
   back to `dead_watermark` on an anomaly, which is silent without a debug gate.

Both are consistent with the bisected commit, whose layout changes moved
`ARRAY_LENGTH_OFFSET`/`NUM_SLOTS_OFFSET` 12 -> 8 and whose own message records
the walk's plausibility screen getting weaker. The probe to write next reports
`dead_regions.len()` at the watermark and after each truncate, plus the walk's
per-object stride around the watched offset.

~~Note the scope this changes …~~ **WITHDRAWN with the `objects_swept=0`
reading.** The sweep reclaims; there is no whole-heap reclamation failure to
expect elsewhere. The scope is what the measured half says it is: one dead
object whose base the walk strides over.

## Do not close this without a regression pin

Three prior fixes, three recurrences, and the 08-01 page's own lesson was that
the predicate it repaired **had no test**, which is how a one-line default flip
disarmed it in silence. Whatever closes this needs a vector that runs in
`regression-suite` — not a `probes/` reproducer that nothing schedules.
