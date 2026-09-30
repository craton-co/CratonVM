# GC round 2026-09-20 — ZGC backend: proposed work

Scope of the review this came out of: `gc/src/zgc.rs`, `gc/src/zgc_concurrent.rs`,
`gc/src/zgc/{adapters,barrier,census,forwarding,generation,mark,mark_roots,
metrics,page,relocate,remembered,starts,sweep,vaddr}.rs`, and
`gc/tests/zgc_{module_integration,colored_word_degradation}.rs`. The TLAB
chunk-zeroing family (`tlab.rs`, `vm_tlab.rs`, `arena_tlab.rs`) is owned by a
neighbouring session and is excluded.

Each item is sized, names its first step, and says what would make it fail.
Ordered by expected value, highest first.

> **Items 1, 4 and 7 are CLOSED (2026-09-21).** Their pages are retired to
> `docs/internal/fixed-bugs/` and carry the measurements. Read the outcomes
> before acting on the item text below, which is the proposal as written and is
> wrong in two places:
>
> * **Item 1** — the budget **is** binding (`reloc_pages_deferred=104`,
>   `reloc_budget_truncated_cycles=1` at the default, on
>   `tools/probes/ZgcEvacBudgetProbe.java`). The `G1ChurnPauseProbe 50 1800`
>   this item prescribes **cannot reach the budget at all** and reads zero even
>   at a 1 MiB budget, so that run is not the test. The A/B was done and says
>   *leave the default*: ~45 % of the slide is an O(live) reference rewrite
>   independent of how much moved, so the budget decides *when* the copying
>   happens rather than what it costs. **That result is item 3's premise,
>   measured** — and it is the objection to item 2.
> * **Item 4** — the mapping table, the phase spans and the TSV wiring all
>   landed. The claim that "G1 and Generational both feed" the
>   `cratonvm_jfr::phase` reconciliation identity is **false**: no collector fed
>   it, and ZGC is now the only one that does.
> * **Item 7** — done, and the behaviour question is answered *no*: the four
>   bypassed terms stay bypassed, because `SWITCH_OFF`'s bypass is its
>   documented semantics and the rest are obligations about compiled frames.
>   What was wrong was the invisibility, which is fixed.

---

## 1. Decide the relocation budget, with a number rather than a constant

**Why.** `docs/internal/fixed-bugs/zgc-relocation-evacuation-budget-is-a-fixed-64mib-RETIRED-20260921.md`.
The one production `ZRelocationSet::select` call uses
`ZRelocationPolicy::default()`, whose `max_evacuation_bytes` is 64 MiB of live
bytes and is never assigned by anything in the workspace. `select` is a prefix
rule, so on a low region with more than 64 MiB live the slide compacts a prefix
and stops — and on this collector compaction is the only defragmentation there
is. Nothing read `deferred_for_budget`, so "the slide ran" and "the slide
finished" were the same reading.

**Size.** Instrument: done in this round. Decision: one measurement day.

**Shape.**
- `ZgcRealHeap::relocation_budget_engagement()` now exists and is bumped at the
  call site. **First step: print it.** One field on the `[GC] zgc-features:`
  line in `gc/src/vm_heap.rs` (the exact edit is in this round's hand-off).
- Then `G1ChurnPauseProbe 50 1800` at `-Xmx2048m` and `-Xmx4096m`, three
  interleaved reps, one binary. If `cycles_truncated == 0` at both sizes, the
  constant is not binding on anything this tree measures and the page closes
  with the counter as the proof.
- If it is non-zero: `CRATONVM_ZGC_RELOCATE_BUDGET_MB` as a knob first, A/B'd
  against pause p50/max **and** `frag_gauge`'s worst free permille — the two
  axes this trade actually moves. Default change only after that, for the reason
  the `CRATONVM_ZGC_ALLOC_TRIGGER` default's one-day life is recorded in
  `docs/GC.md`.

**What makes it fail.** A larger budget lengthens the pause, and
`refresh_pause_target_budget` is a *second* feedback loop over the same cycle.
Two controllers over one quantity, tuned apart, is how the `ALLOC_TRIGGER`
default was withdrawn. If the A/B shows the pause-target controller simply
eating the budget increase by collecting less often, the answer is to give the
budget to the *controller* rather than to a constant — which is item 2.

---

## 2. Make the pause-target controller own the evacuation budget too

**Why.** The controller (`refresh_pause_target_budget`, default 200 ms) already
scales *one* quantity from measured pause: the allocation span a cycle may run
to. The slide's copy cost is the other half of the same pause and is governed by
an unrelated constant. `docs/GC.md` already records the consequence in a
different register — "it holds the MEDIAN; the tail stays high" — and attributes
the floor to the arena high-water mark. The copy prefix is a second term in that
floor that nobody has priced.

**Size.** Medium. One new term in an existing control law, plus its counter.

**Shape.** `max_evacuation_bytes = f(target_ms, measured_relocate_us_per_byte)`,
multiplicative in exactly the way the existing law is (it never has to model the
cost curve). `relocate_us` is already measured per cycle and already on the
`zgc-pause:` line; survivors walked is already in
`slide_verification_stats()`. So the two inputs exist.

**First step.** Add `bytes_copied` to what `refresh_pause_target_budget` is
handed, and log the derived bytes/µs for three runs **without acting on it** —
the "probe below the pin" rule. If the ratio is stable across cycles the law is
buildable; if it swings an order of magnitude, it is not and item 1's knob is
the honest answer.

**What makes it fail.** If the slide's cost is dominated by the whole-live-set
*rewrite* pass rather than by the copy — the profile in
`ZGC_PAR_RELOCATE_DEFAULT_WORKERS`' doc says ~45 % is reference rewrite against
a copy of ~60 K objects out of 10 M live — then budgeting the copy controls the
wrong term and the law will not track.

---

## 3. Bound the reference-rewrite pass with a real remembered set

**Why.** This is the single largest known cost in the slide and it is already
written down: `ZGC_PAR_RELOCATE_DEFAULT_WORKERS`' doc measures the collector
thread's samples under `relocate_stw_admitted` as ~45 % reference rewrite, ~20 %
`logical_pages`, ~20 % the `live_ceiling` screen, ~12 % the partition — each one
*per live object*, with 60 K moved out of 10 M live. Round 9 wave 7 answered it
with threads (8 workers, ~2.9× less STW). Threads divide the constant; they do
not change that the pass is O(live) to rewrite O(moved).

The doc also names the blocker: "bounding the rewrite ... needs a remembered set
fed by a barrier on EVERY reference store, and several store paths here bypass
the accessor barrier".

**Size.** Large. This is the round's biggest lever and its biggest commitment.

**Shape.** The machinery is mostly built and mostly unreached. `zgc::remembered`
is a complete page-keyed table with precise bitmaps, a two-buffer swap
protocol, coarsening and its own tests; `card_object` already feeds it from
`set_field_no_satb`, i.e. from the **one** accessor every interpreted store
funnels through. What is missing is (a) the store paths that bypass the
accessor, and (b) using the table to answer "which pages can hold a pointer into
a moved page" instead of walking everything.

**First step, and it is a census, not code.** Enumerate the reference-store
paths that do *not* reach `set_field_no_satb`: the JIT's inline ref-store fast
path, `set_array_element`, the native-collection overlays, `set_static_shared`,
and `Unsafe`. Each one either gets the card or is proved unable to create an
old→young edge. Until that list is closed and written down, a bounded rewrite is
a correctness regression waiting for the first missed path — and
`set_field_no_satb`'s own comment records that this exact family ("the card
barrier was on no store path at all until 2026-08-17") has already bitten once.

**What makes it fail.** If the census finds a path that genuinely cannot be
carded (a raw JIT store with no helper), the pass stays unbounded and item 3
becomes "make the *other* three per-live-object passes cheaper" — which is a
real but much smaller win.

---

## 4. Adopt `zgc::metrics`, or delete it

**Why.** `docs/internal/fixed-bugs/zgc-metrics-module-has-no-production-consumer-RETIRED-20260921.md`.
1,984 lines of OpenJDK-shaped pause accounting whose only consumer is a test,
beside a hand-rolled `eprintln!` in `collect_garbage` that the collector's own
TODO says should be replaced by it wholesale. Two renderers of one line, already
drifted. ZGC contributes nothing to the `cratonvm_jfr::phase` reconciliation
identity that G1 and Generational both feed.

**Size.** Medium, and mostly a decision.

**First step.** Write the mapping table: each of the 11 `ZgcPhase` variants
against the `ZPhaseClock` lap that feeds it, with an explicit `0` and a reason
for every phase this collector does not have. `ConcurrentRemap` is the one that
is a fold rather than a zero — the STW slide's rewrite pass *is* the remap.

**What makes it fail.** If the mapping cannot be written without inventing
phases, the module is measuring a collector that does not exist and the honest
outcome is deletion plus a `format_cycle_line`-shaped upgrade of the existing
line. That is a legitimate result of the first step, not a failure of it.

---

## 5. Retire the `page` / `generation` / `adapters` stack, or wire one seam of it

**Why.** `zgc::page` (2,368 lines), `zgc::generation` (2,668) and
`zgc::adapters` (1,310) are reachable only from `adapters.rs`, whose own
consumers are tests. `docs/GC.md` says plainly that the page allocator "is not
adopted"; what it does not say is that `ZGenerationalHeap`, `ZYoungGeneration`,
`ZOldGeneration`, `ZPromotionPolicy::minor_cycle` and the whole
`ZPageAllocator` free/recommit path go with it. Meanwhile the *shipped*
generational mode (`CRATONVM_ZGC_GENERATIONAL=1`) is a per-object `gc_age`
scheme over one arena that shares none of it — `age_pages_and_split` uses
`ZPromotionPolicy` only to age a logical grid whose young/old split the
collector then does not use for the split.

That is two generational designs in one module tree, one of which is described
in the present tense and is tested, and neither of which is the other's
reference.

**Size.** Small if the answer is a paragraph; large if the answer is adoption.

**First step.** One header comment in `gc/src/zgc/page.rs` and one in
`gc/src/zgc/generation.rs`, on the model `gc/src/lib.rs` already uses for
`class_unloading` and `metaspace`: *unreachable, honestly labelled, kept on
purpose, and this is the one place that says it.* That comment is what stops the
next reviewer re-deriving it — this round is the third time it has been
re-derived.

**What makes it fail.** Nothing; the first step is free and correct either way.
It is item 5 rather than item 1 only because it buys clarity, not pauses.

---

## 6. Give the parallel STW marker a reason to exist, or say it does not have one

**Why.** `Z_PARMARK_DEFAULT_WORKERS` is `0` and carries the interleaved
measurement that made it `0`: serial 95.2 ms, one driven worker 124.9 ms, four
240.6 ms, eight ~304 ms. The diagnosis was revised on 2026-09-02 (the cost now
*falls* with worker count, so it is a fixed per-cycle charge, not contention)
and the persistent pool was built in response — measured at 270–966 µs per
cycle, "10–18 %" of a 1.6–4.0 ms gap. The remaining 82–90 % has no owner.

Until this round the function's own comment claimed the opposite of its
constant ("DEFAULT-ON since 2026-08-13 ... half the cores capped at 4"), which
is fixed here.

**Size.** Medium, and it is a measurement before it is a change.

**First step.** Profile one four-worker cycle at the line level, the way round 9
wave 7 profiled `relocate_stw_admitted`. The hypothesis worth testing first is
that the fixed charge is the **root push**: `push_roots` hands the whole root
array to the coordinator, which stripes it, and the driver then waits on a
condvar per pass. A serial marker pays none of that.

**What makes it fail.** If the charge turns out to be the termination protocol
itself, the answer is C5 of the concurrent plan (a scalable marker) and not a
tuning change — in which case this item closes by saying so in
`Z_PARMARK_DEFAULT_WORKERS`' doc, which is already most of the way there.

---

## 7. Name which collector `CRATONVM_NO_MOVING_YOUNG` binds

**Why.** `docs/internal/fixed-bugs/zgc-relocation-refusal-is-bypassed-with-no-compiled-frame-RETIRED-20260921.md`
§2. ZGC's relocation refusal reads
`gc_quiescence::moving_young_enabled()` — the **Generational** collector's
policy — as its `MOVING_YOUNG_DISABLED` term, and then discards that verdict on
any cycle with no compiled frame live. So the token half-binds a collector it
does not name, and the skip-reason census cannot show it, because the counting
lives inside the same `if` as the decision.

**Size.** Tiny for the documentation; small for the counter move.

**First step.** A `docs/flag-tokens.md` row for `CRATONVM_NO_MOVING_YOUNG` and
`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` stating which backends each reaches.
Then move the two `fetch_add`s out of the decision `if` so the census answers
"which obligations failed" independently of "did that stop the slide" — with a
before/after soak, because it changes what every existing
`zgc-relocation-skip-reason:` number means.

---

## 8. A test for the coarsened-page root path

**Why.** This round found that `ZRememberedSetTable::take_coarse_pages()` had no
caller anywhere in the tree, while both its own doc and
`ZRememberOutcome::Coarsened`'s promise that a coarsened page "must be scanned
in full" by "the next young collection" and that the mechanism exists "so a
barrier can never *silently drop* an old→young edge". It silently dropped them.
`young_extra_roots` now consumes the set and roots every registered base inside
a coarsened page.

It is currently unreachable in production — `card_object` registers the page
before setting the bit, so `remember` always takes the precise arm — which is
exactly why it needs a test rather than a soak.

**Size.** Small.

**First step.** A unit test that calls `heap.remembered.remember(page, offset)`
directly for an **unregistered** page holding a live object, then asserts
`young_extra_roots` returns that object. The control arm — the same test with
the page registered — must return it too, via the precise path, or the test
proves nothing about which arm ran.
