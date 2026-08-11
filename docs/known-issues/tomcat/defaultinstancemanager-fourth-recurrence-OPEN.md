# `TestDefaultInstanceManager.testClassUnloading` — fourth occurrence (OPEN)

| | |
|---|---|
| **Status** | OPEN — **root-caused**, fix not yet written |
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

## What is measured, and what it rules out

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

## The marker is innocent. The SWEEP never visits the span.

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

The premise involved is the one the bisected commit is documented as having
killed — "a fresh header is never all-zero" died when the hash went lazy. The
section after next PROVES the connection rather than leaving it plausible.

## Which skip arm? NONE

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

So the mirror is not retained by a rooting decision, a side-table edge, or a
skip list. The next section shows what does it: the walk identifies the span as
dead and then UNWINDS that decision, and re-anchors past it.

## ROOT CAUSE: the sweep's all-zero-span anomaly screen fires on live objects

`dead_regions` is not empty because the walk finds nothing dead. It finds
27,252 dead objects and then throws almost all of them away. Per-unwind-site
census on the failing run:

```
side_marked=118647  late_pinned=0  forwarded=0  header_marked=0
dead_pushed=27252   unwinds=83  sites=[0, 83, 0, 0]
unwound_entries=27285  dead_regions_final=51  side_sorted=132548
```

**All 83 unwinds come from one site**, and its trigger is:

```rust
let word0 = unsafe { *(obj_ptr as *const u64) };
if word0 == 0 {
    // "unlisted all-zero span" -> anomaly evidence
    dead_regions.truncate(dead_watermark);   // drop every reclaim decision
    // ... then re-anchor at the next FREE BLOCK, abandoning everything between
```

That screen exists because an all-zero header used to be proof of reclaimed
memory. It rests on the premise the bisected commit is documented as killing,
in its own words:

> The stale-pointer detector and the zeroed-region screens keyed on "a fresh
> header is never all-zero, because a hash is stamped at allocation". That
> premise is dead: the hash is lazy now, so a live never-hashed never-locked
> bare `new Object()` reads all-zero exactly like reclaimed memory.

With `HEADER_SIZE` 16, `word0` is `class_id` (0..4) + `shape` (4..8). A live,
never-hashed, never-locked object with `ClassId(0)` and no shape bits reads
all-zero — indistinguishable from a reclaimed span by header bytes alone. So
ordinary live objects now trip an anomaly screen **83 times per sweep**, and
each hit discards every reclaim decision taken since the last anchor.

That is the whole defect, and it produces the symptom twice over:

1. **The unwind** throws away the evicted JSP loader's own reclaim decision
   along with ~27k others, so its span is never zeroed and
   `is_live_young_survivor` (`word0 != 0` — the loader has a real class id)
   answers "live".
2. **The resume** re-anchors at the next free block, "abandoning everything in
   between" — which is why the earlier probe never saw the walk stop at that
   base at all. The site's own comment already calls this resume behaviour
   wrong and records it costing 233 MB of a 256 MB young generation on the H2
   UPDATE path.

## Scope

This is not a Tomcat bug. The non-moving young sweep publishes **51 reclaimed
regions out of 27,252 identified** on the `System.gc()` path.
`TestDefaultInstanceManager` is simply the test that happens to assert on a
consequence; everywhere else it costs throughput and footprint while failing
nothing, which is why it went unnoticed from 08-07.

(Correcting an earlier reading on this page: I first reported `objects_swept=0`
as "the sweep reclaims nothing". That counter is incremented in the publication
loop, and my probe printed before it. The accurate statement is the ratio
above.)

## The fix, and the constraint on it

The screen must stop treating "all-zero header" as evidence of anything. The
commit that broke it also closed the obvious repair: minting a hash eagerly to
restore the invariant is not available, because a non-zero mark word loses the
thin-lock CAS and every `synchronized` block in the program would inflate.

The discriminator that IS available is the one the sweep already has:
`side_sorted`, the complete live set (132,548 entries here). A span that is
all-zero and NOT side-marked is legitimately reclaimable; the anomaly is
all-zero AND on a stretch the grid cannot vouch for. Any fix must keep the
double-free protection the site was written for — the comment above it records
a real UAF from parsing zero spans as 40-byte phantom objects — so the
conservative arm has to survive, just stop firing on live never-hashed objects.

## Do not close this without a regression pin

Three prior fixes, three recurrences, and the 08-01 page's own lesson was that
the predicate it repaired **had no test**, which is how a one-line default flip
disarmed it in silence. Whatever closes this needs a vector that runs in
`regression-suite` — not a `probes/` reproducer that nothing schedules.
