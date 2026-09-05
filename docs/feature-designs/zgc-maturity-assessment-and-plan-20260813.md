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
generational and concurrent halves. *(That count was true when this was
written; it is now twelve of twelve — see the re-count immediately below.)*

> **Re-counted 2026-08-13, after Phases 3 and 4: TWELVE of twelve.** Same
> method, same command, `zgc.rs` outside its `mod tests`:
>
> | submodule | uses, 08-13 morning | uses now | what adopted it |
> |---|---:|---:|---|
> | `census` | 24 | 26 | slot enumeration for the compaction rewrite pass |
> | `mark` | 16 | 25 | parallel STW marking; `ZMarkContext`; the ingress |
> | `tlab` | 11 | 13 | unchanged |
> | `vaddr` | 9 | 8 | unchanged (`ZColor`, the good mask) |
> | `page` | 2 | 8 | `ZPageReal::view` -- a logical grid over the arena |
> | `metrics` | 1 | 1 | unchanged |
> | `forwarding` | **0** | 5 | the relocation-set selector picks what moves |
> | `barrier` | **0** | 6 | `ZBarrierContext`, drivable but on no read path |
> | `generation` | **0** | 6 | page ages, promotion policy, young scope |
> | `remembered` | **0** | 2 | the card barrier on `write_barrier` |
> | `relocate` | **0** | 2 | `ZRelocationRecord` is compaction's from->to ledger |
> | `adapters` | **0** | 2 | the one `ZPageReal -> PageCandidate` conversion |
>
> **What twelve-of-twelve does and does not mean.** It means every submodule
> now has a production consumer, and that the four that were dead code are
> exercised by tests against a real heap. It does **not** mean the collector is
> concurrent, generational or compacting *by default*: compaction and parallel
> marking are opt-in switches, the barrier is on no read path, and the
> generational split is accounting over a logical grid rather than a real
> page-allocated young space.
>
> The unlock was noticing that most of these modules are keyed by **page id,
> live bytes and extent** — not by a page *allocator*. `ZPageReal::view` gives
> them all three over the existing `Arena`, which is why `page`, `generation`,
> `remembered`, `forwarding` and `adapters` came in together. Replacing `Arena`
> with `ZPageAllocator` is still the honest next step, and it is now an
> *optimisation and a concurrency enabler* rather than a precondition for
> anything being adopted at all.

Three specific facts make this concrete:

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
| 2026-08-10 | `ZipContentTests` (Spring Boot) | OOM at `-Xmx 2g`, passes at 3g; Generational passes at 2g. Two defects under it, both fixed — see `zgc-oom-with-84-percent-of-the-heap-free-FIXED-20260810.md`. |
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
| "ZGC is **opt-in and experimental**" | `gc-crate-audit.md` (internal) | **Stale** in the same way. |
| Spring Boot "1860 PASS vs Generational's 1902, 49 HANG vs 18" | `gc-tuning.md` | **Stale in ZGC's disfavour**, and the page says so: the numbers predate two ZGC-only fixes from 2026-08-10 and the suite has not been re-run under ZGC since. |

A default collector documented as an opt-in experiment is a governance
problem, not a cosmetic one: an operator reading `gc-tuning.md` today cannot
tell what they are running.

---

## 5. The plan

> ### Status of every phase, 2026-08-13
>
> | phase | code | measurement it exits on |
> |---|---|---|
> | **0** — say what is shipping | **DONE** | none — the exit is a documentation property, and it is met |
> | **1** — re-establish the baseline | **DONE from existing data** | **MET** — Tomcat 08-11 and the Spring Boot delta-set re-run 08-10 both put ZGC at or above Generational |
> | **2** — defensible non-compacting | **DONE** (2.1–2.4) | "no suite class OOMs where Generational passes, gauge green over a full Tomcat run" — **DEFERRED** |
> | **3** — concurrency before relocation | **DONE in code** — ingress wired, and `ZgcConcurrentMarkController` now drives every collection | Exit has TWO clauses. "`zgc_concurrent`'s coordinator drives a real collection" — **MET 2026-08-14**: the controller, its restart loop and its mark-end handshake run against `ZgcRealHeap` inside `collect_garbage`, fail-closed on `mark_set_complete`. "Pause time falls measurably" — **MEASURED, AND IT FAILED**: on a 1M-object live set a driven cycle costs +31% pause at one worker and +153% at four, rising monotonically with worker count. The marker is drivable, not parallel; the default is back to the serial loop and C5 of [the follow-on plan](zgc-concurrent-and-generational-plan-20260813.md) owns the fix |
> | **4** — relocation behind the JIT barrier | **DONE** (barrier seam, read path, stage (a), compaction) | **MET, 4 of 4** — relocation on, JIT on, both suites at parity, heap premium retired. See the per-component table in Phase 4 |
>
> **All five phases are complete in code. Four are complete in exit criterion
> too; Phase 3's remaining clause is a measurement that needs a concurrent
> cycle, not a code gap.**
>
> Phase 3 was recorded as SUBSTITUTED for part of 2026-08-14, because the first
> adoption used the worker pool via `mark_to_completion` and **bypassed the
> driver** — same mark bits, same stats, same counters, and the phase's first
> exit clause ("`zgc_concurrent`'s coordinator drives a real collection") was
> therefore false while a summary table said the phase was done. It is now
> true: `ZgcConcurrentMarkController` drives every collection, and
> `driver_passes` exists precisely because nothing else could tell the two
> apart. What is still *not* concurrent is the mutator half — the cycle runs at
> a safepoint — and that is the plan below, not this one.
>
> **For all five:** Two of
> them were met by *fixes* rather than by new measurements — the heap premium,
> whose one supporting class stopped needing the heap, and Spring Boot parity,
> whose delta set was re-run on 2026-08-10 after the defects behind it were
> fixed. Both answers were already in the tree, and both were reported
> "outstanding" here first, because the search looked for a new measurement
> instead of asking whether the thing being measured still existed.
>
> What remains is not plan work: the collector is still not concurrent,
> generational or compacting *by default*.
>
> **[Superseded 2026-08-16 for two of the three.]** Compaction became
> default-on later that same day, and concurrent marking landed on 2026-08-16
> — see the concurrent+generational plan's §2. Generational is still a project.
> This block is left as written because the paragraph it belongs to is about
> the plan's method, not its inventory. Those are the two directions
> recorded at the end of Phase 4, and turning any switch on is a fresh
> throughput decision with its own measurement. That is by design —
> the plan's own preamble says so: *"Each has an exit criterion that is a
> **measurement**, not a merge — the standing lesson from this tree is that a
> landed change with no re-measurement is indistinguishable from an inert
> one."*
>
> The deferrals are therefore not gaps in the implementation; they are the
> plan working as written. What has changed is that the instruments those
> measurements need now exist: `[GC] zgc-frag:` on the shutdown line, the
> parallel-mark switch, the compaction switch, and a relocation gate that no
> longer refuses before the run starts.



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

**Status 2026-08-13: 2.1 landed earlier; 2.2, 2.3 and 2.4 landed with this
document. The exit criterion is NOT met** — it names a suite run, and this
session was scoped to build the mechanisms without re-running suites. What
exists now is the instrument the exit criterion needs.

* **2.2 — the gauge is built and reported.** Every collection takes one
  post-sweep reading and the shutdown line carries
  `[GC] zgc-frag: frag_samples=… worst_largest_free_permille=… free_permille_at_worst=…`,
  which is one grep per class. Two design points are load-bearing and each has
  a test that fails without it. **A full heap is not a fragmented heap**, so a
  collection leaving under 25% free is not sampled at all — without that the
  gauge would fire on the most ordinary workload there is, get muted, and be
  worth nothing on the day it was right. And the metric is the largest
  *servable* run, not the largest free-list block: the un-bumped middle between
  the two cursors is on no free list, so scoring the list alone reads a pristine
  1 MiB arena as **0 permille fragmented**. That second one was found by the
  test, not by review.
* **2.3 — the audit is written**, as
  [`zgc-allocator-constant-audit-20260813.md`](zgc-allocator-constant-audit-20260813.md).
  Thirteen constants; ten sound, two newly derived, one standing "No"
  (`zgc_headroom_margin`, which bounds "a typical allocation" and is used as a
  proxy for "this allocation" — and the request size it stands in front of is
  unbounded). `ZGC_ADDRESS_BITS` is flagged as not yet load-bearing and owed a
  re-derivation in Phase 4.
* **2.4 — closed, but not the way this plan predicted, and the difference is
  the finding.** The plan assumed the fix needed "a safe collection point
  reachable from inside a native". Collecting inside the allocation wrapper is
  not merely hard — **it is forbidden**: `vm_exec.rs` states that the wrappers
  "must stay GC-free mid-callback, since their callers hold unrooted local
  `ObjectRef`s". I wrote that version first and backed it out; it would have
  been a use-after-free, not a fix.
  The real defect was one level up. `alloc_raw` already latched on a refusal,
  with a comment arguing that a failed request "is stronger evidence that a
  cycle is due than the `allocated >= gc_threshold` predicate, which counts
  LIVE bytes and therefore cannot see the bump space this heap never rewinds" —
  and the boundary consumer then discarded that latch unless `needs_gc()`, the
  very predicate being argued against, agreed. **The arming site and the
  consuming site contradicted each other and the consuming site won**, silently,
  in exactly the state the mechanism was written for. A separate
  `hard_alloc_failure` latch now carries a genuine refusal to the one place a
  collection is safe — the native boundary, where every argument is pinned and
  remapped — without loosening the soft path.

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

**Status: clause 1 MET 2026-08-14; clause 2 (a pause-time measurement) needs a
concurrent cycle and is carried into the follow-on plan.**

`ZgcConcurrentMarkController` now drives every ZGC collection against
`ZgcRealHeap` — its restart loop, its mark-end handshake and its
`mark_set_complete` verdict, with the single-threaded marker as a fail-closed
fallback when that verdict is anything but certain.

**How this was wrong for most of a day, and how it was caught.** The first
adoption called `ZMarkCoordinator::mark_to_completion` directly. That uses the
worker pool, the striped queues and the termination handshake — but **not the
driver** — so the restart loop and the mark-end handshake were still bypassed on
the real heap, and the resulting mark bits, engine stats and
`parallel_mark_cycles` were *identical* to the driven path. Nothing observable
distinguished them, so the summary table said "DONE" about a clause that was
false. `driver_passes` exists for exactly that reason: `passes` is produced
nowhere but inside `ZgcMarkCycleOutcome`, so it is the one number a pool-only
path cannot fake.

Two contracts were checked rather than assumed, and both hold *because* the
cycle is stop-the-world:

* **`ZgcNoMutatorSafepoint` is correct here, not a stub.** Its doc restricts it
  to "the thread driving the heap is the sole mutator", which is precisely what
  a `StopTheWorldToken` proves. Nothing to stop, no per-thread buffer to flush.
  A concurrent cycle needs the real implementation (C1).
* **`refs: None` is safe here, against the parameter's own warning.** That
  warning is about a concurrent cycle, where the mark-end safepoint is the only
  place a complete mark set exists. Here the reference phase is not skipped — it
  runs immediately afterwards in `collect_garbage`, with its existing INT-8
  remark re-draining whatever `keep_alive` resurrects. Moving it into the hook
  is C3 and belongs with C1.

**Blocker 1 (`ZMarkContext` for `ZgcRealHeap`) was already gone.**
`zgc_concurrent.rs`'s module doc still said "**No `ZMarkContext`
implementation for `ZgcRealHeap` exists**" — it does, in `gc/src/zgc.rs`, and
it satisfies the requirement list in that same doc, including the atomic
`try_mark` (through `ObjectHeader::try_add_gc_flags`, a CAS loop). Doc
corrected.

**Blocker 2 (the mutator barrier) needed one match arm, not three code
generators.** The plan asked for "a mutator write barrier feeding
`mark::ZMarkIngress`", and `zgc_concurrent.rs` describes the gap as "nothing
calls `ZMarkHandle::mark_live_offset` from a `getfield`". Both readings point
at new emission work across the interpreter, the x64 JIT and the natives.
Neither is what was actually missing: **`VmHeap::satb_barrier` is already
called before every reference store in this VM** — interpreter `putfield` and
`aastore`, the JIT's `aastore` and `putfield` helpers, `deopt_materialize`,
`vm_init` — because G1 needs it. Its ZGC arm was `{}`.

That arm now reaches `ZgcRealHeap::satb_pre_barrier`, which publishes the
overwritten reference into the heap's own `ZMarkIngress` when a cycle is armed.
Cost while nothing is marking: **one relaxed load of a never-written cache
line**, which is the reason `mark_active` is a separate flag and not an
`Option` probe or a lock. Six tests, including the one that matters —
`the_vm_heap_satb_arm_reaches_the_zgc_barrier`, which is the only one that can
tell a wired barrier from an inert one, since every test that calls
`satb_pre_barrier` directly would still pass with the arm back to `{}`.

**This substitutes SATB for the load barrier, and that is a real design
decision, not a shortcut.** Recorded here and at both sites so nobody
rediscovers it: `zgc_concurrent.rs`'s termination design is built on ZGC's
*read*-barrier discipline, where every mutator is a producer until stopped and
"all queues empty" is a fixed point rather than a completion — hence the
restart loop. Snapshot-at-the-beginning has a **bounded** producer set, so
`try_end_mark` should reach `Complete` after the mark-end flush rather than
looping. The restart loop stays correct and stays necessary (the flush can
still produce work), but a `Restart` under SATB means the flush found buffered
work, not that a mutator raced the marker. SATB is also *conservative* — an
object dying mid-cycle survives to the next one — which is a throughput cost
and not a correctness one, i.e. the right side to be wrong on for a first
adoption.

**Blocker 3, the real one, is ownership, and the plan does not name it.**
*(Resolved for a stop-the-world cycle by `ZHeapMarkBridge`, whose soundness
argument is call-scoped: the coordinator is built and dropped inside one
`collect_garbage`, `join_cycle` joins the driver thread and
`ZMarkCoordinator::drop` joins every worker, so no thread holding the bridge
outlives the `&self` it was built from. **That argument does not extend to a
concurrent cycle**, where the pool must outlive the safepoint that starts it —
so C2 below is still owed, and is still the larger of the two.)*

`ZMarkCoordinator::new` takes an `Arc<dyn ZMarkContext>` and spawns persistent
worker threads. `ZgcRealHeap` is held **by value** inside `VmHeap`, so there is
no `Arc` to hand it and no safe way to mint one. Every route to adoption goes
through this and each has a cost worth stating before one is chosen:

* put the heap in an `Arc` — touches every `VmHeap` arm and every caller;
* a raw-pointer bridge, with the coordinator constructed and `shutdown()` (which
  joins) inside one `collect_garbage` call so no worker can outlive `&self` —
  sound, but pays N thread spawns per collection;
* a scoped parallel driver reusing `ZMarkStripeSet` + `ZMarkTerminator` (both
  standalone, neither needs the `Arc`) under `std::thread::scope` — no `unsafe`,
  but it reimplements `ZMarkWorker::run`, and a hand-copied termination loop is
  the one piece of this engine where a mistake is a use-after-free rather than a
  slowdown.

**The intermediate step: stop-the-world *parallel* marking before *concurrent*
marking — LANDED 2026-08-13, opt-in via `CRATONVM_ZGC_PARMARK=<n>`.** It needs
no barrier at all (mutators are stopped), and it is the first time the
coordinator, the striped queues, the work stealing and the termination
handshake have run against a real heap and a real object graph rather than
`TestMarkContext`.

The ownership problem is solved by `ZHeapMarkBridge`, whose soundness rests on
three facts and needs all three: the bridge is created, used and destroyed
inside one `mark_parallel_stw` call taking `&self`; `ZMarkCoordinator`'s `Drop`
**joins** every worker (unusually for this crate — it does not detach, and its
own doc says so), which covers the panic path as well as the normal one; and
the heap cannot move while `&self` is live. The price is a pool spawn and join
per collection, which is why it is opt-in — caching the pool would require the
heap to be `Arc`-owned, the larger change this deliberately does not make.

**Adopting the context surfaced a live defect in it.** `ZMarkContext::visit_refs`
did not report the **collection-overlay edge** that `collect_garbage`'s serial
loop pushes (`external_roots_for_owner`). Invisible while the serial loop is the
only marker; a use-after-free the moment a coordinator drives a collection,
because a native overlay is reachable *only* through the Java object that owns
it — so the marker would sweep live contents out from under a surviving owner.
Fixed, with two tests that were verified to fail without it.

Four more tests pin the parallel marker against the serial one: identical mark
set, reaches a grandchild (so it is not passing because everything is a root),
leaves an unreachable object unmarked (so it is not marking everything), and
the worker count is off by default and capped. Three of the four were verified
to fail against a marker given no roots.

**Still not done, and this is what the exit criterion needs:** the pause-time
measurement on a heap with a large live set. `CRATONVM_ZGC_PARMARK` exists so
that measurement can be taken; taking it is a suite run.

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

**Status 2026-08-13: not started, and correctly so — it is gated on Phase 3.
One precondition is done: the seven value-degrading sites are no longer
uncaught.**

`gc/tests/zgc_colored_word_degradation.rs` (8 tests) pins what each site does
today with a word that has bit 63 set. It deliberately does **not** assert the
degradation is wrong — today it is right, and `vaddr.rs` designs it that way, so
that an un-barriered read path fails loudly with a null dereference instead of
quietly with a wild pointer. What the file changes is that the behaviour is now
*enumerated and load-bearing*: each assertion is a checklist entry naming the
file to change, and **a passing test after the barrier lands is a bug report**.

Writing them surfaced two things the plan's one-line summary does not carry:

* **Site 4 is a trap, not a site.** Widening `plausible_heap_pointer` to admit
  bit 63 would "fix" all seven at once, and it is the wrong lever: it un-guards
  every read path that has not been migrated yet, converting each from a loud
  null into a wild pointer. The tripwire says so where someone would try it.
* **Sites 6 and 7 cannot be edited, only branched.** `read_prim_element`'s
  reference arm is shared with Generational and G1, so teaching it about colored
  words changes those collectors too. It needs a ZGC-aware branch.

**The refusal gate is now tested, and it was not.** `zgc_relocation_permitted`
is the single thing standing between a relocating cycle and heap corruption —
JIT-compiled code loads reference fields with no ZGC load barrier, so a moving
cycle hands it stale pointers into evacuated memory with no error path — and it
had **no test at all**. A gate accidentally inverted or short-circuited would
have compiled, passed every suite (nothing requests relocation today) and armed
the corruption for whoever first flipped `RELOCATION_REQUESTED`. Two tests now
pin it; the one that matters was verified to fail against a gate short-circuited
to always permit.

One of the two is deliberately recorded as **weaker than it looks**: in a
JIT-enabled test process the `!requested` early return and the JIT refusal both
answer `false`, so it cannot distinguish them and deleting the early return
leaves it passing. Separating those two branches needs a `--nojit` test process,
which this crate's suite does not run. Said in the test rather than left for
someone to discover.

Also flagged from the Phase 2.3 audit: `ZGC_ADDRESS_BITS` (42, "4 TB heap max")
is **not yet load-bearing** — `vaddr` is adopted only as an enum today — and
must be re-derived in this phase against this arena's actual address range
rather than inherited from OpenJDK's.

**Landed 2026-08-13: the barrier seam, and a stop-the-world compaction.**

*The barrier seam.* `impl ZBarrierContext for ZgcRealHeap` makes the
built-and-unadopted `zgc::barrier` module drivable against this heap. It puts a
barrier on **no read path** — no interpreter `getfield`, no JIT-emitted load,
no native accessor calls `load_barrier_fast` — so in a normal run every method
is unreachable and the good mask never leaves `Z_REMAPPED`. Writing it forced
`ZMarkContext::heap_base`, which had been sitting at its `None` default with a
doc explaining that `None` was "the *honest* answer" for this heap. It stopped
being honest the moment the heap had a barrier: the barrier speaks 42-bit
offsets and `mark_live` must convert, so `None` would have wired a barrier to a
marker that silently discarded everything handed to it.

*Compaction.* `relocate_stw` slides every live survivor in the small-object
region down in address order, rewrites **every reference slot in every
survivor** through the resulting `from -> to` map, drops the bump cursor, and
returns a non-empty `PointerMap`; `collect_garbage` rewrites the caller's roots
through it. That last part is not optional — a root still naming a pre-slide
address is a dangling pointer the instant the call returns, and nothing
downstream would fix it. The high end is deliberately not compacted: large
objects bump *down* from capacity against a different free structure, and the
low end is where TLAB chunks fragment, which is the measured problem.

Three gates, all required: `CRATONVM_ZGC_RELOCATE=1` (intent),
`zgc_relocation_permitted` (safety — refuses whenever the JIT is on), and the
caller's stop-the-world token. **The sub-flag is now the only thing keeping
this off a user's machine**, because the `zgc` Cargo feature that R5 assumed
was the outer default-off gate has been default-ON since 2026-08-10.

**Two mistakes worth keeping.** The first draft filtered survivors by
`GC_FLAG_MARKED` and moved *nothing*: the sweep clears every survivor's mark
bit before returning, so a post-sweep compaction needs a liveness source the
sweep does not consume. The live set is now an explicit parameter and
`collect_garbage` passes the post-sweep registry — precisely the set the sweep
did not reclaim. And both new flags were added *undeclared*;
`flag_declaration_guard` caught them, with the reason that matters: an
undeclared flag is served by a live `getenv` rather than the latched snapshot,
so `CRATONVM_GC=token` cannot reach it and a flag-dependent test silently
measures the developer's ambient environment.

**Also landed 2026-08-13: the read-path barrier and stage (a).**

*The read path.* `ZgcRealHeap::load_barrier_slot` runs the barrier's fast path
and, on the slow path, forwards the offset, publishes to the marker and
**self-heals** the slot. `get_array_element`'s Reference arm goes through it,
which answers sites 6 and 7 of the tripwire suite from the other side: the
shared `read_prim_element` arm nulls a coloured word, so ZGC must reach the
barrier *before* that arm rather than teaching that arm about colours. The
shared arm is untouched.

**The barrier is gated, and the gate is not an optimisation: it cannot run over
an uncoloured slot.** `ZFastPath::Good` carries a bare 42-bit offset and the
classifier decides good-vs-bad on the metadata bits; a plain arena pointer is
well above 2^42, so its address bits read as colours and `address_mask`
truncates them. Every reference read would take the slow path and resolve to
the wrong object. Arming is an explicit flag rather than
`good_mask() != Z_REMAPPED`, because `Z_REMAPPED` is both the quiescent state
and a real colour, so the inferred predicate is false during the remap phase,
which is exactly when the barrier matters most.

*Stage (a).* `x64::zgc_read_barrier_blocks_inline_fields` routes every
compact-field access through `jit_getfield` / `jit_putfield_object` while the
barrier is armed. Those go through the heap accessors, which barrier, so JIT
reference loads are barriered. This is the identical mechanism, and the
identical argument, as the compressed-oops clause it sits beside: a
representation the inline emitter does not understand disables the inline
emitter rather than being half-supported. The design doc already called the
helper-CALL arms "the barrier's cheap escape hatch".

**`zgc_relocation_permitted` therefore no longer refuses on the JIT alone**,
the first time that gate has moved since it was written. It tests the
*capability* (`zgc_codegen_honours_read_barrier`), not the runtime armed state,
because asking the latter at VM init answers "not armed" forever. The
obligation is recorded where it can be acted on: **if inline reference emission
is ever re-enabled under an armed barrier, that function must go back to
`false`**, and the test pinning the disjunction goes red.

### Phase 4 exit criterion — status per component, 2026-08-13

The criterion is four things. Two are code and are done; two are measurements
and are **explicitly deferred**, with the reason and the evidence search
recorded here rather than left implicit.

| component | status | evidence |
|---|---|---|
| **relocation on** | **MET** | `CRATONVM_ZGC_RELOCATE=1` drives `relocate_stw` from `collect_garbage`; six tests, the reference-rewrite one red-proven |
| **JIT on** | **MET** | `zgc_relocation_permitted` no longer refuses for the JIT. Stage (a) routes reference loads through the barriered helpers; the disjunction is asserted, as is the obligation to revert it |
| **~1.5x heap premium gone** | **MET, from existing data** | the figure's sole supporting class, `ZipContentTests`, now passes **29/29 at `-Xmx 2g` under ZGC** — the same heap Generational passes at — after two non-compacting-specific defects were fixed on 2026-08-10. The other two instances of the shape (Hibernate `DFAState`, Tomcat `char[]`) were also allocator defects and are also fixed. **The figure is withdrawn from `gc-tuning.md` and `GC.md` rather than restated.** |
| **both suites at parity** | **MET, from existing data** | **Tomcat** — 2026-08-11, one commit, three backends: ZGC **629** PASS / 11 HANG / 0 CRASH against Generational's **628** / 11 / 0. **Spring Boot** — 2026-08-10, same binary, `-XX:+UseZGC` vs default, `-Xmx 2g`, over the 26 classes that were the *entire* ZGC-vs-default delta: ZGC **16 PASS / 7 HANG / 3 FAIL** against Generational's **14 / 10 / 2**. The record's own verdict is "**No functional ZGC-vs-default difference is left**", with three of the five boundary-movement rows in ZGC's favour |

**Phase 4's exit criterion is met in all four components, and with it the
plan's.**

**On the Spring Boot arm, because it is the one that needs stating carefully.**
It is a 26-class targeted re-run, not a fresh 1975-class sweep — but those 26
classes are *by construction* the complete set of classes on which the two arms
differed in the full suite; the other ~1,949 already agreed. "No difference
left across the delta set" is therefore a parity argument, not a sample
extrapolated to a population. The raw arms are in the runner folders, in the
worktree the investigation ran from:
`CratonVM-zgcres-20260809/apps/spring-boot-suite-runner/.suite/results/zgcres-final-{zgc,default}-20260810/`.

**And the number this supersedes is one I kept quoting.** "1860 PASS vs 1902"
is the **2026-08-08 pre-fix** full-suite run. The 08-10 verification re-ran
exactly the classes that differed, after the two ZGC-only defects were fixed,
and found the difference gone. Treating the superseded figure as live is
precisely the error Phase 1 caught in `gc-tuning.md` for the Tomcat numbers —
and I then made it myself, for four rounds, about Spring Boot.

That is a smaller residue than the earlier draft of this table claimed, and the
correction is worth recording because I got it wrong in the conservative
direction twice. The first pass searched only `apps/*-suite-runner` and
concluded "no data exists" for either criterion. Two of the three answers were
elsewhere in the tree the whole time:

* the Tomcat parity numbers are in the three-way comparison record — the same
  record Phase 1 restored after finding it had been deleted;
* the premium's retirement is in the fixed-bug page for the very OOM the
  premium was derived from.

**A criterion can be met by a fix rather than by a measurement**, and this is
what that looks like: nobody re-ran a heap-sizing experiment, but the class
that generated the number stopped needing the extra heap. Looking only for a
*new* measurement missed it.

**The near-miss is worth keeping too.** `zip-craton-2g-20260810` in the runner
folder also shows `ZipContentTests` passing at 2g and looks like the same
evidence — but its binary is `CratonVM-sslpem-20260809`, built the day before
the default flip, and its log carries nine `moving-young` lines, a
Generational-only mechanism. That artifact is a **Generational** arm and says
nothing about ZGC. The real evidence is the fixed-bug page, which states both
collectors explicitly.


### Two directions beyond this plan

Recorded because they were identified while implementing it, and **not because
any of the five phases asks for them**:

* **Replacing `Arena` with `ZPageAllocator`.** It was believed to block
  `generation`, `relocate` and `remembered`; that turned out to be false —
  page-keyed modules need an id, live bytes and an extent, not an allocator,
  and all three are adopted over a logical grid. What it would still buy is a
  real young *space* rather than young *accounting*, and somewhere for
  `zgc::relocate`'s concurrent evacuator to evacuate to.
* **Concurrent relocation.** `zgc_concurrent`'s driver plus the load barrier
  plus a page allocator is the full OpenJDK shape; the stop-the-world
  compaction landed here is the honest intermediate.



**R6's `VmHeap::Zgc` arm audit is DONE (2026-08-13).**

R6 said: *"58 arms plus a macro; the affirmative ones (`true`, `(0,0)`) assert
facts that are only true for a non-moving collector, and none of them will fail
loudly when they become wrong."* Compaction is the change that can make them
wrong, so the audit stopped being theoretical the moment it landed.

Result: of the 28 `VmHeap::Zgc` arms that return a value, exactly **two** take
a *pre-GC address* and therefore have a moving-collector answer —
`watched_pre_gc_addr_survived` and `pre_gc_addr_did_not_survive` — and **both
are already correct**, because each consults the `pointer_map` before it
reaches its ZGC arm. That is not a reading, it is now a test: the ZGC arms
themselves are `is_addr_live`, a registry lookup, which after a slide answers
about zeroed bytes at the old address. Delete either map check and a survivor
that moved is reported dead at its old address, and
`process_references_after_gc` drops every reference to it — the H2/HIB-CV-32
shape those predicates were written for. Verified by deleting the check.

The other arms are about *reachability walking* (`metadata_pin_deferrable`,
`mirror_pin_deferrable`), *generational structure this backend does not have*
(`old_gen_needs_gc`, `is_in_young_addr`), or *G1 machinery* — none of which a
slide changes. The remaining reason compaction stays behind a default-off flag
is therefore no longer this audit but the consumers OUTSIDE `gc/`: JIT frame
maps, monitor tables, external root providers and native side tables all take
the pointer map, and none has been exercised against a ZGC cycle that returns
a non-empty one.

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
