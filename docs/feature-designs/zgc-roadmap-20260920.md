# ZGC roadmap, 2026-09-20 — one ordered programme for four proposals

**Written** 2026-09-20, at the close of the ZGC design & performance round on
`claude/zgc-design-perf-improvement-db5e99`. It sequences the four direction
documents that round produced, which were written independently by four lanes
that could not see each other's drafts.

This page is **the order and the dependencies**. It does not restate the
designs; each proposal is the authority on its own subject and is linked at
every point where the detail matters.

| proposal | subject | where |
|---|---|---|
| **A** | load barrier and colored pointers | [`../internal/zgc-round-20260920/proposal-a-load-barrier-and-colored-pointers.md`](../internal/zgc-round-20260920/proposal-a-load-barrier-and-colored-pointers.md) |
| **B** | concurrent relocation | [`../internal/zgc-round-20260920/proposal-b-concurrent-relocation.md`](../internal/zgc-round-20260920/proposal-b-concurrent-relocation.md) |
| **C** | a page-based heap | [`../internal/zgc-round-20260920/proposal-c-page-based-heap.md`](../internal/zgc-round-20260920/proposal-c-page-based-heap.md) |
| **D** | a real young space | [`../internal/zgc-round-20260920/proposal-d-real-young-space.md`](../internal/zgc-round-20260920/proposal-d-real-young-space.md) |

Smaller, self-contained proposals from the same round —
`proposal-a-concurrent-mark-by-default.md`,
`proposal-e-memory-pool-mxbean-and-jfr.md`, `proposal-e-split-zgc-rs.md` — are
not sequenced here. They have no dependency on these four and can be scheduled
whenever there is room.

> **Two corrections, applied 2026-09-21 (`ROUND-SUMMARY.md` §7 raised them and
> could not edit this page).**
>
> 1. **`proposal-b-page-based-evacuation.md` and
>    `proposal-e-allocation-rate-driven-trigger.md` were listed above as "not
>    sequenced … can be scheduled whenever there is room". Both had a stage
>    land in this round**, so a reader of this page alone would rebuild work
>    that exists. `CRATONVM_ZGC_TRIGGER_SHADOW` (wave 2) is the rate trigger's
>    stage 1 — the measurement the rest is conditional on, and it has **not been
>    read**. Page-based evacuation's stage 1 is half-landed
>    (`impl ZRelocateContext for ZgcRealHeap`, `CRATONVM_ZGC_PAGE_EVAC`) and is
>    **the prerequisite for B**, which this page *does* sequence, at phase 4.
>    It is therefore not a "whenever there is room" item at all.
> 2. **The dependency graph draws no edge from C to B. It should.** Wave 3
>    showed that adopting the page relocator is blocked on having a to-space
>    that is not the free list — precisely what C0/C1 provide. (Wave 4 then
>    built one directly in `gc/src/arena.rs`, `ToSpaceWindow`, which is a
>    narrower answer to the same requirement.) That makes **C0 unblock two
>    workstreams rather than one**, and strengthens this page's own case for
>    scheduling it first.

---

## 1. The baseline this starts from

Stated once, because two of the four proposals and both older plans open from a
baseline that has moved. As of this commit:

- **ZGC is the default collector**, and the `zgc` Cargo feature is default-on.
- **It compacts by default** — a stop-the-world slide after the sweep, since
  2026-08-13, returning a non-empty `PointerMap`. A cycle may still decline
  (the cost gate, the per-cycle JIT coverage proof); `moved=` on
  `[GC] zgc-relocate:` is the only honest answer for a given run.
- **Concurrent marking works and is opt-in** (`CRATONVM_ZGC_CONC_START`, also
  `=auto`). **Generational mode works and is opt-in**
  (`CRATONVM_ZGC_GENERATIONAL=1`), and on the workload shape it is for it is
  measured *neutral*, not a win.
- **`impl barrier::ZBarrierContext for ZgcRealHeap` exists in production**, as
  does a `zgc_concurrent::ZgcConcurrentMarkController` call site. What does
  *not* exist is a slot that ever holds a colored word: the heap stores plain
  machine pointers, liveness is the header's `GC_FLAG_MARKED` bit, and no
  `ZGoodMask` is constructed by a running heap.
- **Marking is snapshot-at-the-beginning under a pre-write barrier**, not
  ZGC's load barrier. That is the single property whose absence makes the name
  wrong, and proposal A is the only path to it.

`docs/gc-tuning.md`'s "What is actually shipping, in one place" table is the
current authority; `scripts/zgc-submodule-adoption.sh` is the machine-checked
answer to "is module X adopted".

---

## 2. The dependency graph

```
                  A0  slot resolver  ─────────────┐
                   │  (no flag, no behaviour)     │
                   ▼                              │
                  A1  read_prim_element ZGC arm   │
                   │                              │
                   ▼                              ▼
   C0 ──► C1 ──► A2 colored slots           B0 quarantine measurement
   one    page       │                              │
   reserv chunks     ▼                              │
   two              A3 barrier armed: MARK ◄────────┘
   views             │
     │               ▼
     ▼              A4 relocation under the barrier ──► B2 lazy remap
   C2 ──► C3         │                                     │
   page   reclaim    ▼                                     ▼
   object by page   A5 retire SATB              B3 thread handshake  ◄── JIT lane
   source   │        │                                     │
            │        ▼                                     ▼
            └──► D = C4  real young space       B4 concurrent relocation
                 (ONE work item, see §3)
```

Read as: an arrow is *"cannot start until"*, not *"should follow"*.

### The edges that are load-bearing

1. **A0 → everything in A, and → B.** The slot resolver ("given an object and
   a field index, where is the reference word, and may it be healed?") is the
   only piece both A and B need, and it has no ZGC dependency of its own. It is
   the first thing to build in the whole programme and it changes no behaviour.
2. **A1 → A2.** `read_prim_element`'s reference arm runs
   `plausible_heap_pointer` and turns an implausible word into `Object(None)`.
   A colored word is deliberately implausible. Until that path has a ZGC-aware
   branch, **colouring any array slot silently nulls references**. Proposal A
   is right to call this the critical path; it is also a live defect the moment
   anything colors a slot, which is why it cannot be deferred into A2.
3. **A3/A4 → B1..B4.** Concurrent relocation is *not* blocked on the
   relocator, which exists and works. It is blocked on an armed load barrier
   (A) plus a precise-oop thread-stack handshake owned by the JIT lane. Any
   plan that schedules B before A is scheduling the same barrier work twice.
   Proposal B's "C1 — arm the load barrier under STW relocation" **is** A's
   stage 4; they are one stage with two names.
4. **C0/C1 → C2 → D.** A young space that reclaims by releasing pages needs
   pages to be the object source first. D's own stage 2 says this ("hard
   dependency on lane C"); C's stage 4 says the same thing from the other end.
5. **No edge from C to A.** The page heap and colored pointers are
   independent, and that is the most useful fact in this graph: they can be
   built in parallel by different people, and C's stage 0 is the only item in
   the entire programme that is pure win with no flag and no behaviour change.
6. **C0/C1 → page-based evacuation (`proposal-b-page-based-evacuation.md`) →
   B.** *Added 2026-09-21; the graph above does not draw it.* Adopting
   `relocate.rs`'s page evacuator needs a to-space that is provably outside the
   relocation set, which the free list can never be — the sweep rebuilds it out
   of the complement of the live set immediately before relocation runs, so the
   most likely destination `Arena::alloc` returns for an evacuated object is a
   hole inside the page it is being evacuated *out of*. C0/C1 supply that
   to-space as a property of the allocator. This edge is why C0 is worth
   scheduling first regardless of how phase 0 resolves for D.

---

## 3. Proposal C stage 4 and proposal D are the same work item

C's stage 4 is titled *"a real young generation, flag
`CRATONVM_ZGC_PAGE_YOUNG`"*. D is titled *"a real young space — reclaim by
releasing pages"*. They were written by different lanes in the same afternoon
and they are one piece of work described twice, from the allocator side and
from the generational side.

**Treat D as the design and C4 as the slot it occupies.** D carries what C4
lacks: the promotion obligation (stage 1), the survivor-evacuation staging
(stage 3), and — most importantly — the refutation criteria. C4 carries the
allocator-side detail D assumes. Neither should be scheduled without the other
open on the desk.

The practical consequence for planning: the programme has **three** large
workstreams (A, B, C+D), not four.

---

## 4. The order

### Phase 0 — the measurements that can refute the expensive half. *Days.*

Two counters and one flag flip, all of which already exist or are one counter
away, and between them they decide whether the C+D workstream is worth starting
at all.

| measure | how | what it decides |
|---|---|---|
| **Per-page survivor rate of the nursery** | **DONE — run, challenged, re-run, settled (2026-09-21)** | **D is REFUTED, on admissible evidence.** This cell has held three verdicts and the history is the point. The aggregate read 98/100/98% against a `>= 90` threshold fixed in advance — but that aggregate is **monotone within a major epoch** (`gen_young_floor` moves only at a whole-heap cycle, `gc/src/zgc.rs:17613`; promotion is a header label not a copy, `:5713`; `finish_cycle` sums across cycles, `gc/src/zgc/generation.rs:2124`), so a constant 1-in-5 marginal survival rate — which would **support** D — scores identically. The admissibility field `survivor_pct_first` was then built, wired and read: **92 / 100 / 100**, all at or above the threshold, ratchet margin at most 8 points. The bias was real; it was not what produced the verdict. **Do not build D.** Cheap experiment that could still overturn it: the `note_survivor` age filter plus a generational-shaped workload. [`../internal/zgc-round-20260920/measurement-nursery-survivor-census-20260921.md`](../internal/zgc-round-20260920/measurement-nursery-survivor-census-20260921.md) (addendum). **Still does not touch C0-C2**, whose case is the O(registry) sweep, not page release. |
| **Legacy reference-slot share** | `CRATONVM_ZGC_CENSUS=1` on a representative suite; read `legacy_share` off the **`last_walk`** block | Sizes A0. `zgc-reference-slot-representation.md` §6 calls this *"the only number in this document that is a guess, and it is the one that sizes the work"*. The instrument was unreachable until 2026-09-20 and now runs. |
| **Quarantine cost under STW relocation** | proposal B's C0 | Decides whether B's later stages have a budget at all. Cheap, independently useful. |

Nothing in phase 0 changes behaviour. Everything after it should be able to
point at one of these numbers.

### Phase 1 — the free structural work. *Weeks, parallelisable.*

- **C0 — one reservation, two views. HALF-LANDED, 2026-09-21 (wave 4).** No
  flag, no behaviour change, and proposal C is explicit that *"stage 0 alone is
  most of the value"*. It also removes C's own stated blocker.
  **What exists:** `ZPageAllocator::over_reservation` builds a **survey** — it
  indexes and describes a window it does not own and refuses to carve a byte
  (`ZPageError::SurveyOnly`) — plus `adopt_views` for the per-cycle grid and
  `survey_stats` to read it. That makes `sweep.rs`'s
  `add_free_block(base - arena_base, size)` correct by construction and turns
  `ZPageTable` into an O(1) address → page filter.
  **What does not:** the construction site is in `gc/src/zgc.rs`, which the
  lane that built it does not own, so nothing calls it — `survey_stats` reads
  all zeros, which is the honest answer and the reason it reads zero.
  The switch, `CRATONVM_ZGC_PAGE_SURVEY` (default off, because the survey costs
  a page-table write per collection and nothing reads the grid back yet), is
  **not declared** in `types/src/flag_groups.rs`; see
  [`../internal/zgc-round-20260920/gap-t-page-survey-flag-is-not-declared.md`](../internal/zgc-round-20260920/gap-t-page-survey-flag-is-not-declared.md).
  Note that C0 stays worth doing on its own terms whatever phase 0 says about
  D: its case is the O(registry) sweep, not page release.
- **A0 — the slot resolver.** No flag, no behaviour change, needed by A and B
  both.
- **A1 — `read_prim_element`'s ZGC arm.** Small, and closes a latent defect.

These three have no dependency on each other and no dependency on phase 0's
answers. They are what to do while phase 0 is measuring.

### Phase 2 — the branch point.

Phase 0's first row decides this.

- **If nursery pages mostly hold survivors** — *read `survivor_pct_first`, not
  the cumulative `survivor_page_pct`: the aggregate is monotone within a major
  epoch and cannot tell this case from its opposite, see phase 0's first row* —
  then **the D half** of the C+D workstream is refuted. **C0–C2 are not, and
  never were**: their case is the O(registry) sweep, not page release.
  Record the number, stop, and put the effort into A. Say so in
  `docs/gc-tuning.md`'s generational row, which currently says "probably not
  yet" on a measurement rather than a guess and would then be able to say why
  permanently.
- **Otherwise:** run C1 → C2 → C3 → (C4 = D) and A2 → A3 in parallel. C's
  stages are each behind their own default-off flag; A2 is the schedule risk in
  the whole programme, because *"every read path in the VM"* is not a bounded
  list and the failure mode is a wrong answer rather than a crash.

### Phase 3 — the barrier earns the name.

A3 (marking under the load barrier), then A4 (relocation under it), then A5
(retire the SATB store barrier for ZGC), then A6 (default on) as a separate
decision with its own suite evidence. B2's lazy remap becomes available at A4
and is worth taking then, because it removes the reference-rewrite pass that
profiling puts at ~45% of compaction.

### Phase 4 — concurrent relocation.

B3 (the thread handshake) and B4. **Gated on the JIT lane**, whose handshake
proxy currently fails 64 cycles in 68. That is a blocker, not a refutation, and
it is not on the GC team's critical path to fix — which is the reason B sits
last even though its relocator is already built.

---

## 5. What would refute each proposal

Stated in advance, which is the discipline this tree's collector work has
repeatedly needed — parallel STW marking shipped default-on in 2026-08-14 on an
argument and was reverted at +153%.

| proposal | refuted by |
|---|---|
| **A — load barrier** | Stage 3 arming the barrier for marking and **not** reducing floating garbage against SATB at an equal live set. A's payoff is *precision* and *the ability to relocate concurrently*, not throughput; if stage 3 shows no precision gain, stages 4-6 are paying a per-load cost forever for nothing. Secondary: stage 2 failing to reach suite parity with colored slots and the barrier disarmed — that would say the read-path inventory is not closeable, which refutes the programme rather than the design. |
| **B — concurrent relocation** | C0's quarantine measurement coming back large enough that relocation cannot be hidden under the mutators regardless of the handshake. Also refuted, practically, if the JIT handshake proxy cannot be brought to pass — but that is someone else's measurement. |
| **C — page heap** | Stages 0-1 landing and **`headroom_low` events plus the large-object reserve not improving**. C's case is fragmentation and zeroing cost; if the two-view reservation and page-sourced chunks do not move those two, the remaining stages are a rewrite with no number behind them. |
| **D — real young space** | The phase-0 per-page survivor census showing nearly every nursery page holds a survivor (**abandon** — no page-granular reclamation helps a heap like that, and the honest answer is that the workload is not generational); or, at stage 2, total pause improving by less than 5% (**record and stop** — not worth a second allocator). |

---

## 6. What is now out of date in the older plans

Both remain worth reading for their history and their per-phase evidence. These
specific claims are not true of the current tree, and each is the kind of claim
a reader acts on.

### [`zgc-production-implementation-plan.md`](zgc-production-implementation-plan.md)

| claim | status |
|---|---|
| *"Nothing defaults to it."* (under "What is built", of the `zgc` feature) | **Wrong.** `default = ["zgc"]` in `gc/Cargo.toml`, `vm/Cargo.toml` and `vm-cli/Cargo.toml`, and `VmConfig::default` sets `gc_algorithm: GcAlgorithm::Zgc`. ZGC is *the* default collector. The page's own header says "Partial, **and the default collector**", so it contradicts itself. |
| *"It is **non-moving**, so it reclaims but does not compact, and it returns an empty pointer map."* | **Wrong**, and contradicted by the same page's header ("it **compacts** as of 2026-08-13"). A default cycle returns a non-empty `PointerMap`. |
| *"**Nothing concurrent runs.** `gc/src/zgc_concurrent.rs` has zero non-doc callers."* | **Wrong.** `ZgcConcurrentMarkController` has a production call site in `gc/src/zgc.rs`. Concurrent marking is opt-in, which is a different fact from unreachable. |
| *"**Nothing relocates.** `barrier`, `forwarding`, `relocate`, `page`, `generation`, and `vaddr`-as-a-slot-encoding are unadopted."* | **Wrong for five of the six.** The recorded census has `forwarding` 6, `relocate` 7, `page` 9, `generation` 11, `vaddr` 19 production references. Only *`vaddr`-as-a-slot-encoding* is still accurate: the heap stores plain pointers. |
| *"There is no production `impl ZBarrierContext` anywhere — only test implementations."* | **Wrong.** `impl barrier::ZBarrierContext for ZgcRealHeap` is in `gc/src/zgc.rs`. What is true, and is the point worth keeping, is that no slot ever holds a colored word, so the implementation is exercised on a degenerate mask. |
| Phase 4's per-component exit table | **Still accurate**, and is the best short record of what relocation actually required. |

### [`zgc-maturity-assessment-and-plan-20260813.md`](zgc-maturity-assessment-and-plan-20260813.md)

| claim | status |
|---|---|
| §2: *"`ZgcRealHeap` is a **stop-the-world, non-moving, non-generational, whole-heap mark-sweep**… Real ZGC is concurrent, relocating, generational and region-based. **Every one of those four is absent.**"* | **Wrong on three of four.** Relocating is on by default; concurrent marking and generational mode are implemented and opt-in. Region-based is the one still absent, and it is proposal C. |
| §2's submodule adoption table (`census` 24, `mark` 16, `vaddr` 9, `page` 2, `barrier` 0 …) | **Superseded and materially wrong.** This is the hand-maintained census that was wrong twice and that `scripts/zgc-submodule-adoption.sh` + `types/tests/zgc-submodule-adoption.txt` replaced. Current: `census` 34, `mark` 52, `vaddr` 19, `page` 9, `barrier` 14. Do not quote the table; run the script. |
| §3: *"Gap B — the measured cost of not compacting"* | **Premise moved.** The cost it measures is now the cost of a cycle that *declines* to compact rather than of a collector that cannot. The measurement is still the right one; the framing is three months old. |
| §1's source table, quoting four other pages as agreeing that ZGC is a "non-moving mark-sweep" | **Two of the four have since been corrected** (`docs/gc-tuning.md` and `docs/GC.md`, 2026-09-20); `docs/book/src/contributing/building.md` no longer carries the sentence at all. The table is a snapshot of a consensus that has partly dissolved, which is the point of §1 and also the reason it should not be quoted forward. |
| Phase 0-4 status records | **Still accurate as history**, including Phase 4's "MET, 4 of 4". |

### [`zgc-jit-load-barrier.md`](zgc-jit-load-barrier.md)

Corrected in this round. Its header said *"Designed, not built. No barrier
emission exists in the JIT"* for three months after **stage (a) landed on
2026-08-13** — the sentence a reader reaches before deciding whether a
compacting default run is possible at all. Stage (b), the inline fast path, is
still designed and not built, and the cost model for it is current.

### Not corrected, and outside this round's file ownership

Two pages still carry the pre-2026-08-13 baseline and should be fixed by
whoever owns them:

- **`docs/book/src/user-guide/memory-and-gc.md`** (user-facing) — *"a
  memory-backed, **non-moving** mark-sweep over one arena"* and *"Budget ~1.5x
  the heap a compacting collector needs."* Both were true of a collector that
  does not compact; the second is a sizing instruction an operator will act on.
- **`docs/feature-designs/concurrent-gc-maturation.md`** — *"`ZgcRealHeap` is a
  stop-the-world, non-moving mark-sweep"* and, of concurrency, *"None."*
  Concurrent marking has been opt-in-and-working since 2026-08-16.

None of these pages should be deleted. The two ZGC plans above should each gain
a pointer to this one at the top, which is a change for whoever next touches
them.

---

## 7. Where the detail lives

- Current shipping behaviour and every operator-visible flag:
  [`../gc-tuning.md`](../gc-tuning.md) — the "What is actually shipping" table
  and the flags section under it.
- Collector internals, the trigger clauses, the TLAB reservation arithmetic and
  the two-ended arena: [`../GC.md`](../GC.md).
- The round's gap pages, one per finding, including the four that were
  silent-corruption bugs: `docs/internal/zgc-round-20260920/`.
- Adoption per submodule, machine-checked:
  `scripts/zgc-submodule-adoption.sh` and
  `types/tests/zgc-submodule-adoption.txt`.
