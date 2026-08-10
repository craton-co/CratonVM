# `TestDefaultInstanceManager.testClassUnloading` — fourth occurrence (OPEN)

| | |
|---|---|
| **Status** | OPEN — **bisected to a named commit**, root cause not yet isolated within it |
| **Symptom** | `java.lang.AssertionError: expected:<8> but was:<9>` (`TestDefaultInstanceManager.java:66`) |
| **First bad commit** | [`1d2817c75`](#the-bisect) `feat(types,gc,vm,jit): delete identity_hash_code from ObjectHeader` (2026-08-07 08:24) |
| **Reproduces** | default GC and ZGC; **G1 passes**. Deterministic, ~17 s standalone |
| **Fourth in a chain** | [07-14](../../internal/fixed-suite-bugs/tomcat/defaultinstancemanager-classunloading-count-mismatch-FIXED.md) · [07-27](../../internal/fixed-suite-bugs/tomcat/defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md) · [08-01](../../internal/fixed-suite-bugs/tomcat/defaultinstancemanager-third-recurrence-FIXED.md) |

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
2. The **sweep walk never reaches that span at all.** It is inside a stretch the
   walk skips — a free block, a TLAB reservation, or an abandoned stretch. No
   desync/abandon markers appear in the run, so the skip list is the leading
   candidate.
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

## The open question

Why does the sweep walk skip that span? `existing_free` (free blocks from prior
sweeps), `jit_skips` (reserved TLAB tails — unlikely, `--nojit` fails too), or a
walk that abandoned early. The probe to write next reports which skip arm covers
the watched address, at the top of the walk loop.

The second question, independent of the first and arguably the more important
one: **`is_live_young_survivor` infers liveness from a side effect.** "The span
was not zeroed" is not the same proposition as "the object is live", and the
sweep has at least three legitimate ways to leave a dead span unzeroed. A
verdict that reads a byte pattern where a mark bit exists will keep producing
this class of bug regardless of which skip arm is responsible here.

## Do not close this without a regression pin

Three prior fixes, three recurrences, and the 08-01 page's own lesson was that
the predicate it repaired **had no test**, which is how a one-line default flip
disarmed it in silence. Whatever closes this needs a vector that runs in
`regression-suite` — not a `probes/` reproducer that nothing schedules.
