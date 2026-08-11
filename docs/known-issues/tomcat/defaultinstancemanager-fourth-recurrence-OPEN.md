# `TestDefaultInstanceManager.testClassUnloading` — fourth occurrence (OPEN)

| | |
|---|---|
| **Status** | OPEN — **two root causes now confirmed, only the first is fixed**; see 2026-08-11 update at the bottom |
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

## Two independent additions, 2026-08-10 (separate session)

Both were found while working the same page from the sweep side, and neither
changes the root cause above. They are recorded because one of them contradicts
a row this page still carries, and the other names a second retention path.

### `objects_swept` on the `walk ended` line is a structural zero

`objects_swept` is declared at `gen_heap.rs:9439`, printed by
`[MARKWHY] walk ended`, and incremented in exactly one place — the publication
loop several hundred lines below that print. It reads 0 in every run on every
workload, whatever the sweep reclaimed.

An earlier revision of this page took that 0 as "this sweep reclaimed NOTHING",
and from there to "no young object is reclaimed at all" and "class unloading
cannot work by construction". The disposition census above settles it the other
way — `dead_pushed=27252` — and so does
`regression-suite/src/RClassUnloadSweep.java` (new, scheduled): a class defined
in a throwaway loader, used, dropped and collected is unloaded on HotSpot and on
CratonVM under the default collector, `CRATONVM_NO_MOVING_YOUNG=1`,
`-XX:+UseG1GC` and `-XX:+UseZGC`.

The line now reports the walk-time facts it can see and a new
`[MARKWHY] sweep done` reports `objects_swept`/`bytes_swept` after the loop that
writes them. Two further instruments came from the same reading:

* `[MARKWHY] sweep enter` — prints whenever a watch is ARMED, with **no**
  from-space condition. Every other MARKWHY line is gated on the watch being in
  from-space, so they go silent together, and that silence has three causes
  wanting three different next steps ("the sweep never ran", "it ran and the
  object is elsewhere", "it ran and the walk never reached the object"). Only
  the third is about the object.
* `[MARKWHY] STRADDLE` — names the object whose computed extent swallows the
  watched base. The pre-existing sweep hook tests `cursor == watch`, so it is
  silent in exactly the case *The marker is innocent* measures, and cannot name
  the culprit stride. `par_prefix_end` / `watch_in_par_prefix` are on the
  `walk ended` line for the same reason: every sweep hook lives in the
  SEQUENTIAL walk, so a watch inside the accepted parallel prefix is one the
  sequential walk never visits by construction.

### A second retention path: `collection-overlays` roots the JDT compiler graph

`CRATONVM_DBG_ROOT_SOURCE=1` is new and records which named entry of
`VM_ROOT_SOURCES` contributed each root; `[MIRRORWHY]` now prints
`[root=<source>]` on each rendered path. Measured twice on the failing run, for
the evicted `annotations_jsp`:

```
  33 [root=collection-overlays]        11 [root=collection-overlays]
   6 [root=<not-a-direct-root>]         2 [root=<not-a-direct-root>]
```

with every attributed path of the shape

```
StackMapFrame -> VerificationTypeInfo -> SourceTypeBinding -> LookupEnvironment
  -> JDTCompiler$1 -> JDTCompiler
  -> JspCompilationContext / JspServletWrapper -> loader / instance
```

**This contradicts two rows this page still carries.** *What is measured* records
`live_instances_of_this_loader=0` and `root_held_paths=1`; today's dev reports
**1** and **8**, with `is_marked=true` on the loader. And *Also eliminated*
dismisses overlay over-rooting because the gate skips under
`major_gc_requested()` — but the roots are contributed anyway, on the default
collector, in the failing run. That entry was eliminated by reading the gate
rather than by asking which source rooted the object, which nothing could do
until now; its own "21 direct hits on exactly this JSP cluster" was the answer.

Whether this is a *second* defect or the same one seen from the other end is
not settled here: an object the marker marked is reachable, which is a different
statement from "dead but unreclaimed". Worth resolving before the fix above
lands, because if the mirror is genuinely rooted then repairing the all-zero
screen will not clear this assertion on its own.

The trap in the obvious fix, if it turns out to be needed:
`scan_external_roots` exists because an overlay backing array can be the ONLY
reference to a live object, so widening the gate trades over-retention for a
use-after-free. A JDT `StackMapFrame` reachable only from an overlay after
compilation finished suggests stale overlay ENTRIES, in which case pruning is
the fix and costs nothing in safety.

## 2026-08-11: root cause #1 fixed; root cause #2 confirmed real and unfixed — this test needs both

Picked this page up to write the fix `## The fix, and the constraint on it`
describes. Both halves below were measured on `dev` merged fresh into
`fix/definstmgr-fourth-recurrence-20260810` (271 commits ahead of where this
page left off) — worth restating because the second finding directly
contradicts a still-standing claim in `## Also eliminated` above.

### Root cause #1 (the all-zero-span screen): fixed

`gc/src/gen_heap.rs`, the site the disposition census names (`mw_site[1]`):
added the discriminator the page's own "The fix, and the constraint on it"
section specified. `side_sorted` — this cycle's independently-computed live
set, object bases ascending — is checked with a binary search before the
anomaly/unwind branch fires:

```rust
let vouched_live = side_sorted.binary_search(&(from_base + cursor)).is_ok();
if run_end - cursor >= HEADER_SIZE && !vouched_live {
    // unwind-and-resync, unchanged
}
// else: fall through and parse the header normally, exactly like the
// existing short-zero-run case just below it
```

An all-zero span the marker vouches for is a live never-hashed object, not
anomaly evidence — it now falls through to ordinary header parsing instead of
discarding every reclaim decision since the last anchor and abandoning the
walk to the next free block. The conservative arm is untouched for spans
`side_sorted` does NOT vouch for, so the double-free protection the site was
written for survives.

Verified: `cargo test --release -p cratonvm-gc` — 109 `gen_heap` tests + the
full crate suite (131 total) pass, no regressions.

**This fix alone does not clear `TestDefaultInstanceManager` — confirmed by
running it 3× after the fix, still `FAILURES!!! Tests run: 1, Failures: 1`
every time.** That is expected, not a refutation: see below.

### Root cause #2 (`collection-overlays` unconditional rooting): confirmed real on the default collector, NOT gated as this page assumed

`## Also eliminated` above dismisses overlay over-rooting because
`scan_collection_overlays`'s `major_gc_requested()` gate "skips the
unconditional scan when [...] under Generational, which is precisely this
test's path." **That reasoning was checked by reading the gate, not by asking
which source actually rooted the object — the same trap the page's own
`## Two independent additions` section calls out one level up.** Re-running
`CRATONVM_DBG_ROOT_SOURCE=1` with the sweep fix applied (so any signal here
cannot be root cause #1 in disguise):

```
[MIRRORWHY]   [root=collection-overlays] org/eclipse/jdt/internal/compiler/codegen/StackMapFrame@...
  -> VerificationTypeInfo -> VerificationTypeInfo -> SourceTypeBinding -> LookupEnvironment
  -> JDTCompiler$1 -> JDTCompiler -> JspCompilationContext -> cid<evicted mirror>
```

`collection-overlays` is contributing roots on the default collector, in this
exact failing run, after the sweep fix — the gate is not preventing it here.
This is the SAME mechanism `vm/src/runtime/interpreter/gc_and_alloc.rs:2061-2079`
already documents in detail from an earlier (pre-dating this page's chain)
session: `gc_scan_collection_overlay_roots` roots every element of every
overlay-backed collection unconditionally, with no gate on whether the
backing collection itself is reachable; a scratch `List<StackMapFrame>` the
JDT compiler uses transiently during JSP compilation gets force-rooted this
way, and forward-tracing from that illegitimate root walks back through the
compiler's real field references into the evicted JSP's `JspServletWrapper`
and its `ClassLoader` — keeping the whole cluster permanently, artificially
reachable. That comment already scoped the real fix (the same
conditional-rooting + mark-time-propagation treatment `class_mirrors` got,
generalized to every overlay table) and already flagged it as "a materially
larger, higher-risk change... left for a dedicated follow-up." It still is —
not attempted here.

**Correction to `## Also eliminated` above:** its claim that the
`major_gc_requested()` gate makes this "not the default-collector mechanism"
is wrong; the gate does not fire (or fires too narrowly) for this test's
actual `System.gc()` timing, and this IS live on the default collector.

### Net effect

Both root causes are independently necessary conditions for this symptom.
Fixing #1 removes one way the evicted mirror's span could be
mis-reported-live; fixing #2 (not done) would remove the other way it is
*genuinely* kept alive via an illegitimate root. Closing this test requires
both. Root cause #1's fix is real, tested, and worth keeping regardless of
when #2 lands — it is a correctness and throughput bug in its own right (see
`## Scope` above: "everywhere else it costs throughput and footprint while
failing nothing").

## 2026-08-11, continued: root cause #2 fixed too — and two bigger findings underneath

### The default collector is no longer Generational — it is ZGC, as of 2026-08-10

This page's own header claims "reproduces default GC and ZGC; G1 passes,"
written as if those were two different things. **They are the same thing.**
`vm-cli/src/main.rs:3369`: *"Absent → keep the default, which is **ZGC** as of
2026-08-10."* Confirmed empirically — a build with neither fix, run with no
`-XX` flag at all, reports `gc_algo=Zgc` via a diagnostic added this session
(`CRATONVM_DBG_OVERLAY_GATE=1`, `vm/src/memory/native_roots.rs`). **Every
measurement on this page prior to this section, including the bisect, ran
under whatever "default" meant at capture time — check the date against the
flip before trusting any "reproduces on default" claim on an older page.**
This one's bisect (`git bisect run`, 08-10) predates the flip, so it is
unaffected, but the "Also eliminated" section's `major_gc_requested()` gate
reasoning was evaluated as prose, not measured, and both fixes below target
Generational specifically — neither one touches the actual default path.

### Root cause #2 fixed: `scan_collection_overlays`'s gate used the wrong predicate

`vm/src/memory/native_roots.rs`'s `scan_collection_overlays` skipped the
unconditional external-root scan only when `is_active() ||
unregistered_jit_frame_on_stack() || major_gc_requested()` — three JIT-safety
diversion signals, not "will this cycle's precise marker handle overlay
propagation itself." `gc_quiescence::young_marker_follows_side_tables()` is
that exact predicate (same one `VmHeap::mirror_pin_deferrable` already uses
for the analogous class-mirror case) and already folds in
`major_gc_requested()` as its certain term. Swapped the three-condition OR
for a direct call to it.

**Verified working, precisely, under explicit `-XX:+UseGenerationalGC`:**
`CRATONVM_DBG_OVERLAY_GATE=1` now reports `conditional=true` for this test's
`System.gc()` (it reported `conditional=false` before, on both the pre-fix
build and, unhelpfully, on a first re-check that forgot the companion
`CRATONVM_DBG_MIRRORPIN*` flags and appeared to show zero overlay roots for
the wrong reason). With the gate fixed, `CRATONVM_DBG_ROOT_SOURCE=1` +
`CRATONVM_DBG_MIRRORPIN_WHY=1` shows **zero** `[root=collection-overlays]`
attributions this run, down from 33 before either fix. That mechanism is
closed under Generational.

**The test still fails under explicit Generational with both fixes applied
— confirmed by direct rerun, `FAILURES!!! Tests run: 1, Failures: 1`, still
`expected:<8> but was:<9>`.** A third factor is now the blocker there:
`CRATONVM_DBG_MIRRORPIN_WHY=1` shows the evicted loader as `is_marked=true`
(a real MARK-phase result, not a sweep-time liveness misread) attributed to
`[root=<not-a-direct-root>]` — the exact same generic attribution the two
*genuinely live* control loaders (`bug36923_jsp`, `bug5nnnn/bug51544_jsp`)
also get, so it does not discriminate a real root from this diagnostic's
catch-all bucket for "marked, but not via a named external-root source."
Tracing which actual heap edge earns that mark needs a finer instrument than
exists today — not attempted here. `sites=[0, 73, 0, 0]` in the sweep census
on this same run shows root cause #1's conservative arm still firing 73
times (expected: it fires for spans that genuinely aren't side-marked, which
this run still has plenty of), but `dead_regions_final=55` — up from the
original page's measured 51 — so more genuine reclaims are surviving the
sweep than before either fix.

### The bigger problem: ZGC has no equivalent mechanism, and it is the default

Both fixes on this page are Generational-only. `gc/src/zgc.rs` has:

- No `external_roots_for_owner` / `external_roots_for_matching_owners` call
  anywhere — no per-owner overlay propagation exists for ZGC at all, so the
  "conditional" branch this page's fix enables can never apply there; ZGC
  permanently takes the unconditional (safe, over-retentive) overlay scan.
- Its own class-mirror/loader/metadata pin handling
  (`collect_garbage`'s hand-pushed pin edges, `zgc.rs:4979-4984`), calling
  `cratonvm_types::mirror_pin::mirrors_for_loader` **directly and
  unconditionally** — never `mirror_pin_deferrable`, the conditional variant
  `roots.rs` uses for Generational. Confirmed via `-XX:+UseZGC` (the actual
  default): still fails, 3/3 runs.

So under the current default, this test's mirror is pinned by design, not by
a bug with a small patch — ZGC has not yet grown the precise,
mark-time-propagated pin mechanism Generational has for either overlays or
class mirrors. Building that is at minimum the same scope as root cause #2's
fix, generalized to an architecture with no equivalent infrastructure yet to
extend, and is not attempted here.

### Where this leaves the test

- **Fixed and verified, keep regardless:** root cause #1 (sweep anomaly
  screen) and root cause #2 (overlay-rooting gate), both Generational-only,
  both real defects independent of this specific test.
- **Open, Generational-specific:** a third, not-yet-traced mark-phase edge
  still keeps the evicted loader reachable even with both fixes. Needs a
  finer root-attribution instrument than exists today.
- **Open, and now the more consequential gap:** ZGC (the actual default
  since 2026-08-10) has neither fix's underlying mechanism at all. This is
  the one worth prioritizing next, precisely because it's what "default GC"
  now means to anyone who runs this suite without flags.
