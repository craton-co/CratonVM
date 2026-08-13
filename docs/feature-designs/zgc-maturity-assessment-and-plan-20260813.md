# ZGC maturity: what "immature" actually names, and the plan to close it

**Written 2026-08-13**, alongside the fix for the `TestNonBlockingAPI`
fragmentation OOM. Every count and flag state below was re-derived from the tree
on that date rather than quoted from an existing page — several of the existing
pages have drifted, and saying which is part of the assessment.

Companion documents, all still accurate on their own subject matter:
[`zgc-production-implementation-plan.md`](zgc-production-implementation-plan.md)
(what is built and what is not),
[`zgc-jit-load-barrier.md`](zgc-jit-load-barrier.md) (barrier emission),
[`zgc-reference-slot-representation.md`](zgc-reference-slot-representation.md)
(slot shape).

---

## 1. Where the "immature" verdict comes from

It is not one agent's opinion; it is the same conclusion recorded independently
in five places, and it survives re-checking:

| source | what it says |
|---|---|
| `docs/feature-designs/zgc-production-implementation-plan.md` | "**Status: Partial** — the collector it selects is a stop-the-world non-moving mark-sweep. The concurrent, generational, compacting machinery is written and unit-tested and **not adopted**." |
| `docs/gc-tuning.md` | "Default, and still **not a real ZGC**." |
| `docs/book/src/contributing/building.md` | "It is a stop-the-world non-moving mark-sweep, **not production ZGC**." |
| `docs/known-issues/tomcat/gc-backend-3way-fullsuite-comparison-20260810.md` | "ZGC's lead here is **a narrower guarantee, not a broader correctness**. It is not evidence that ZGC is more complete — it is evidence that the defect is specific to relocation." |
| `docs/feature-designs/zgc-jit-load-barrier.md` / `-reference-slot-representation.md` | Both open with "**Designed, not built.**" |

The three-way comparison page is the sharpest of them, because it is the one
that had ZGC *winning*. It won the Tomcat suite (fewest hangs, zero crashes)
and the page still refused to read that as maturity: the pathology hurting the
other two backends is a *moving* collector's JIT-frame root-coverage gap, and a
collector that never relocates cannot be exposed to it. Doing less is not the
same as doing it correctly.

So "immature" is a claim about **completeness against what the name promises**,
plus a **measured cost**, plus **documentation drift**. Those are three
different problems and they close differently.

---

## 2. Gap A — the name promises four properties; the shipping collector has none

`ZgcRealHeap` is a **stop-the-world, non-moving, non-generational, whole-heap
mark-sweep** over a single `Arena`. Real ZGC is concurrent, relocating,
generational and region-based. Every one of those four is absent.

The machinery is not missing — it is *unadopted*. Counting symbol uses of each
`gc/src/zgc/` submodule inside `gc/src/zgc.rs` on this commit:

| submodule | uses in `zgc.rs` | adopted? |
|---|---:|---|
| `census` | 24 | yes |
| `mark` | 16 | yes (context impl only — see below) |
| `tlab` | 11 | yes |
| `vaddr` | 9 | yes (`ZColor` only) |
| `page` | 2 | yes |
| `metrics` | 1 | yes |
| `barrier` | **0** | no |
| `forwarding` | **0** | no |
| `generation` | **0** | no |
| `relocate` | **0** | no |
| `remembered` | **0** | no |
| `adapters` | **0** | no |

Six of twelve, and the six that are missing are exactly the moving,
generational and concurrent halves. Three specific facts make this concrete:

* **Relocation is hard-coded off.** `vm/src/vm/vm_init.rs:1713` —
  `const RELOCATION_REQUESTED: bool = false;`
* **`mark` is adopted but not used to collect.** `ZgcRealHeap` implements
  `mark::ZMarkContext`, which makes the concurrent marker *reachable*;
  `collect_garbage` still runs its own single-threaded STW loop and the class's
  own doc comment says so in as many words ("This makes the engine POSSIBLE; it
  does not adopt it"). `gc/src/zgc_concurrent.rs` has no caller that points it
  at a real heap.
* **`vaddr` is adopted only as an enum.** The single import is `ZColor`; slots
  hold raw pointers, never a colored word, and `ZMarkContext` returns
  `Z_REMAPPED` unconditionally.

This is the honest version of the maturity gap and it is *not* a criticism of
the code that exists — the submodules are real and unit-tested. The gap is that
nothing joins them to the production path.

---

## 3. Gap B — the measured cost of not compacting, which is what this doc's fix is about

Not compacting is a design choice with a price, and the price is now measured
three times over:

| date | workload | shape |
|---|---|---|
| 2026-08-10 | `ZipContentTests` (Spring Boot) | OOM at `-Xmx 2g`, passes at 3g; Generational passes at 2g. Two defects under it, both fixed — see `fixed-suite-bugs/vm/zgc-oom-with-84-percent-of-the-heap-free-FIXED-20260810.md`. |
| 2026-08-11 | Hibernate `sql.exec.SmokeTests` | OOM on a 65,552-byte `DFAState[8192]` with `free_list_bytes=1211993376 largest_free_block=65528` — short by 24 bytes. Fixed by raising the TLAB chunk 64 KiB -> 512 KiB. |
| 2026-08-13 | Tomcat `TestNonBlockingAPI` | OOM on a 2,101,264-byte `char[]` with 1.99 GB free and `largest_free_block=524192`. **Same mechanism, one chunk size later.** |

The third one is the important data point, and not only because it shows the
second one's fix was a *treadmill*. The fragmentation report added for this
investigation named the walls exactly — **544 live bytes in four runs (four
`AbstractQueuedSynchronizer$ConditionNode`s and two `ExclusiveNode`s) standing
inside 2,621,264 bytes of otherwise contiguous arena** — and chasing *why those
walls were spaced one TLAB chunk apart* found something that is not a
fragmentation problem at all:

> **A TLAB chunk is RESERVED space, not allocated space.** The part of it not
> yet handed to an object belongs to no object and to no free list, and no
> collection can reclaim it while its owning thread is alive. Its size was flat
> at 512 KiB however many threads a workload ran — and `TestNonBlockingAPI`
> runs ~4,000 of them. `4,000 x 512 KiB` is 2 GB: **the whole heap, claimed by
> reservations the GC trigger cannot even see** (`allocated` sat around 150 MB
> while the arena filled to 2.15 GB). Proved with a switch that already
> existed: `CRATONVM_ZGC_TLAB=0` passes the class 44/44 in 244.7 s.

That is worth stating in a maturity assessment because of what it says about
the *shape* of the remaining risk. The collector is not mostly failing in the
places its design documents predict; it is failing in the places where a
supporting mechanism was sized by a constant that nobody re-derived when the
workload changed. Three of the six defects behind this one OOM are of that
kind. The fixes that landed with this document — a reservation budget scaled by
the live thread count, a refill that accepts a recycled chunk, a two-ended
arena with a floor under its large-object end — mitigate Gap A. (A fifth,
arming the headroom trigger earlier, was written, measured at **4.6% on an
ordinary class for no change in outcome once the reservation was bounded**, and
removed. That is the discipline the rest of this plan needs: price every arm
against what it changed.) They do **not**
make the collector compact, so a workload whose large objects are themselves
long-lived and interleaved will still find a ceiling, just a much higher one.

**The general statement stands and should keep standing in `gc-tuning.md`: a
non-compacting collector needs more headroom, and the amount it needs is a
property of the workload's allocation shapes, not a constant.**

---

## 4. Gap C — the documentation describes a collector that is no longer the one shipping

This is the cheapest gap to close and the most damaging to leave, because it is
what makes the maturity question hard to answer at all. As of this commit:

| claim | where | status |
|---|---|---|
| "Why it is **default-off**" | `zgc-production-implementation-plan.md` | **Stale.** ZGC has been the default `GcAlgorithm` since 2026-08-10 (`vm/src/config.rs:769`). |
| ZGC "has **no TLABs** (every allocation takes the arena lock)" | `gc-tuning.md` | **Stale.** The ZGC TLAB is default-ON (`CRATONVM_ZGC_TLAB`, `zgc_tlab_enabled_by_default`) and `alloc_raw_tlab` is the funnel for every object and array. |
| "ZGC is **opt-in and experimental**" | `audits/gc-crate-audit.md` (internal) | **Stale** in the same way. |
| Spring Boot "1860 PASS vs Generational's 1902, 49 HANG vs 18" | `gc-tuning.md` | **Stale in ZGC's disfavour**, and the page says so: the numbers predate two ZGC-only fixes from 2026-08-10 and the suite has not been re-run under ZGC since. |

A default collector documented as an opt-in experiment is a governance
problem, not a cosmetic one: an operator reading `gc-tuning.md` today cannot
tell what they are running.

---

## 5. The plan

Five phases. Each has an exit criterion that is a **measurement**, not a
merge — the standing lesson from this tree is that a landed change with no
re-measurement is indistinguishable from an inert one.

### Phase 0 — Say what is actually shipping *(days; no collector work)* — **DONE 2026-08-13**

Close Gap C. Reconcile `gc-tuning.md`, `zgc-production-implementation-plan.md`
and `gc-crate-audit.md` with the tree: ZGC is the default, it has TLABs, it is
non-moving, and here is the headroom it costs. Add the two-ended arena and its
reserve to the tuning page as an operator-visible property.

**Exit:** no page describes ZGC as default-off or TLAB-less; a reader can
determine the shipping configuration from `gc-tuning.md` alone.

**Done.** `gc-tuning.md` gained a *What is actually shipping, in one place*
table — eight questions, each with the file that decides it — and the book's
`memory-and-gc.md`, `internals/garbage-collector.md` and `contributing/building.md`
were rewritten: all three still said ZGC was absent from a stock build. `GC.md`,
`concurrent-gc-maturation.md`, the production plan's §1 header and the 2026-07-10
gc-audit page were corrected or date-scoped rather than edited away.

**Phase 0 also turned up a live defect, which is the argument for doing it
first.** `zgc-tlab` and `zgc-startbits` are kill switches for default-ON
machinery, and both were declared in `flag_groups::INVENTORY` with
`off_word: None` — the shape reserved for a default-OFF opt-in. `resolve` turns
`-token` into *unsetting* the key for such a row, and both parsers read an unset
key as **on**, so `CRATONVM_GC=-zgc-tlab` was accepted, reported no unknown
token, and left the TLAB running. The same wrong shape is what made the
generated `flag-inventory.md` describe both as `opt-in | off`. Both now carry
`off_word: Some("0")`, with a test that fails if either reverts. This is the
`young-pause-goal-ms` defect (two rows away in the same file) for the second
time: **the documentation drift and the inert switch were the same edit.**

### Phase 1 — Re-establish the empirical baseline *(one suite cycle)* — **DONE 2026-08-13, except the one run that needs a machine**

The default flip rests on one Tomcat run and a Spring Boot number that is five
days stale and known to be measured on a binary without two of its own fixes.
Re-run the **1975-class Spring Boot suite** and the **651-class Tomcat suite**
under ZGC and Generational on the same commit, same host, interleaved.

**Exit:** a current three-way table. If ZGC is not at pass-rate parity with
Generational, that is the finding and the default should be revisited — the
comparison page's own warning that single-day cross-backend gaps are perishable
cuts both ways.

**Delivered as
[`zgc-phase1-empirical-baseline-20260813.md`](zgc-phase1-empirical-baseline-20260813.md)**,
built entirely from data already in the repository — five suites, not two, and
no suite re-run. Three findings, in descending order of how much they change:

1. **The Tomcat margin the shipping docs quote is superseded by its own
   source.** `gc-tuning.md` and `GC.md` cite ZGC 604/29/0 against Generational
   519/115/1 (2026-08-10). The same record re-ran all three arms on 2026-08-11
   and got **629/11/0 against 628/11/0** — a one-class lead. Both pages now
   carry the newer row.
2. **Widening past the two suites the phase asked for makes the case
   stronger, not weaker.** Spring Framework (2848 classes), H2 and Hibernate
   Reactive all already had per-collector sweeps; ZGC is at parity on all
   three, and H2 is the only suite where a collector separated itself — G1,
   with 4 crashes.
3. **The Spring Boot deficit cannot be closed from existing data, and that is
   the honest answer.** No full-suite ZGC run exists after 2026-08-08. It is
   the oldest number in the table, measured without two ZGC-only fixes, one of
   which silently zeroed primitive arrays — a shape that manufactures FAILs
   wherever it occurs. Treat it as **unmeasured**, not as evidence against ZGC.

**The evidence record had been deleted from the tree.** `ad3393f7c` ("new netty
bug docs") removed
`known-issues/tomcat/gc-backend-3way-fullsuite-comparison-20260810.md`,
touching no other GC file and leaving six pages — `gc-tuning.md` and `GC.md`
among them — citing a path that no longer existed. Restored unmodified from
`ad3393f7c^`. **A default flip whose justification can be deleted by an
unrelated commit without anything noticing is a governance problem of the same
family as Gap C**, and it is worth stating that Phase 1 found it only because
the phase was run under a constraint (use existing data) that forced someone to
go and read the source.

### Phase 2 — Make the non-compacting collector defensible on its own terms *(weeks)*

Do not start relocating in order to fix fragmentation. First take the
non-moving design as far as it honestly goes, because every step here is
independently valuable and none of it needs a barrier:

1. **Two-ended arena** — landed with this document.
2. **A fragmentation ratchet.** The `frag_profile` instrument added here
   computes the cheapest window that could serve a failing request. Run it as a
   *periodic* gauge, not only at failure, and fail CI when a suite's steady-state
   `largest_free_block / capacity` falls below a threshold. This turns "ZGC
   fragments" from an anecdote per suite run into a tracked number — the same
   move that closed the bridge-wave population.
3. **Re-derive every allocator constant from what it is actually bounding.**
   `ZGC_TLAB_MAX_CHUNK` was raised once to dodge a specific array shape and was
   still flat in the thread count; `zgc_headroom_margin` was sized against the
   whole arena rather than the end under pressure; the chunk request was fixed
   at a size its own recycled remnants could never satisfy. Each was found by a
   different OOM. Audit the rest the same way — for every constant in the
   allocator, write down what it bounds and check that the thing it bounds is
   itself bounded.
4. **A last-ditch collection before OOM on the native path.** `alloc_raw`
   currently raises `OutOfMemoryError` from a native without ever running a
   collection (the interpreter and JIT array paths both collect and retry
   first; the native path latches `native_alloc_pressure` and gives up). Closing
   that asymmetry needs a safe collection point reachable from inside a native,
   which is a real piece of work — but the asymmetry itself is a defect, not a
   design.

**Exit:** no suite class OOMs at a heap where Generational passes, and the
fragmentation gauge is green over a full Tomcat run.

### Phase 3 — Adopt concurrency before relocation *(weeks-months)*

Concurrent marking is the half of ZGC that does **not** need a load barrier for
correctness of *reads* — it needs a mutator write barrier feeding
`mark::ZMarkIngress`, a mark-start safepoint, and a decision about
`collect_garbage_with_finalizers`'s resurrection pass. All three are named in
`ZgcRealHeap`'s own doc comment. `zgc_concurrent.rs` is a complete driver
waiting for them.

Doing this before relocation is deliberate: it converts the STW pause into a
concurrent phase without ever moving an object, so a bug here loses throughput
rather than corrupting the heap.

**Exit:** `zgc_concurrent`'s coordinator drives a real collection; pause time
falls measurably on a heap with a large live set; no suite regression.

### Phase 4 — Relocation, gated behind the JIT barrier *(months)*

Only now does `RELOCATION_REQUESTED` become a switch worth flipping, and the
gate is already written: `zgc-jit-load-barrier.md`'s verdict is that
**relocation must refuse to run with the JIT enabled** until barrier emission
lands, and the refusal machinery exists (`zgc_relocation_permitted`). The order
is forced:

1. Colored reference slots (`zgc-reference-slot-representation.md`), including
   the **seven value-degrading sites** that silently return 0 for a word with
   bit 63 set — those turn a colored pointer into a null, which is a
   correctness bug that no test currently catches.
2. Interpreter/native-side barrier, x64 JIT barrier emission (~6-7 instructions
   on the fast path, not the 3-4 originally assumed, because `vaddr` is
   single-mapped and `Z_NULL` is all-zero). aarch64 needs nothing — it refuses
   every method containing a reference load.
3. `forwarding` + `relocate` adoption, then `generation` + `remembered`.

**Exit:** relocation on, JIT on, both suites at parity, and the ~1.5x heap
premium gone — which is the only outcome that actually retires Gap B rather
than mitigating it.

---

## 6. What this document deliberately does not recommend

**Reverting the default.** The evidence for the flip (63 Tomcat classes
non-PASS under Generational and passing under both other backends, 62 of them
logging `[moving-young] fallback`) is strong and was re-verified on current
`dev` at the time. Phase 1 exists to re-test it honestly, not to pre-judge it.

**Renaming the backend.** "ZGC" names the destination, and the plan above is
the road to it. The docs must not claim the destination has been reached, which
is Phase 0 — but a name change would lose the link between this collector and
the twelve submodules built for the real one.
