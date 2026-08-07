# Production ZGC: parallel implementation plan

Status: **superseded in part — see [§0 Status as of 2026-08-07](#0-status-as-of-2026-08-07).**
Phase 0 is landed; Phases 1 (except 1c), 2, 3 and 4a landed the same day as
**unadopted modules**; 4b landed as an audit; Phase 5 is not started.
*Original banner, 2026-08-07 (superseded): "plan — Phase 0 partly landed,
Phases 1–5 not started."* Target, unchanged: turn the default-off `zgc` feature
from a stop-the-world non-moving mark-sweep into a concurrent, generational,
compacting collector that is at pass-rate parity with Generational.

*Written 2026-08-07, grounded in a read of `gc/src/{zgc,zgc_concurrent,vm_heap,tlab,g1_concurrent,satb,compressed_oops}.rs`,
`vm/src/config.rs`, `vm/src/vm/vm_init.rs`, `gc/Cargo.toml`, `vm/Cargo.toml` and
`.github/workflows/ci.yml`. The measured baseline is
[`docs/known-issues/springboot/zgc-real-fullsuite-regression-20260807.md`](../known-issues/springboot/zgc-real-fullsuite-regression-20260807.md);
that record is history and is not edited by this plan.*

This doc is the **execution guide** — who can work on what, simultaneously,
without colliding. It follows the lane/ownership convention of
[`jdk-only-wave2/README.md`](jdk-only-wave2/README.md) and the status-banner
convention of [`concurrent-gc-maturation.md`](concurrent-gc-maturation.md),
whose §8 lists production ZGC as explicitly out of its own scope. **This doc is
that follow-up.**

---

## 0. Status as of 2026-08-07

*Added 2026-08-07, later the same day this plan was written, by a recount from
disk. Sections 1–5 below are preserved as the reasoning trail that produced the
work; where a claim was true when written and is no longer, it carries a dated
supersession note rather than being deleted.*

### 0.1 The scope point, first, because it is the one that gets misread

The **eleven** modules under `gc/src/zgc/` are **built, unit-tested, and
compiling green under `--features zgc`. They are NOT ADOPTED BY `ZgcRealHeap`.**
The "Production-ZGC submodules" banner over the `pub mod` block in
`gc/src/zgc.rs` says so in the source:

> NONE of them is wired into `ZgcRealHeap` yet — `ZgcRealHeap` is still the
> stop-the-world non-moving mark-sweep it has always been.

A `grep` for `vaddr::|page::|barrier::|forwarding::|metrics::|remembered::|mark::|generation::|relocate::|tlab::|census::`
in `gc/src/zgc.rs` returns **zero** hits outside the `pub mod` declarations
themselves — the sole match is a `TODO(zgc)` comment about
`metrics::ZgcMetrics::format_cycle_line`, i.e. an intent, not a call.

*Recounted 2026-08-07 (later): the count was **ten** when this section was
first written; `census.rs` was declared shortly afterwards. The
non-adoption verdict is unchanged and was re-verified against the tree.*

**One nuance, because it cuts the other way and should not be hidden:**
`gc/src/zgc_concurrent.rs` **does** consume `zgc::mark` (it imports
`ZMarkCoordinator`, `ZMarkEndResult`, `ZMarkHandle`, `ZNonStrongRefHook`), so
`mark.rs` is not friendless. But that driver has **no `ZMarkContext`
implementation for `ZgcRealHeap`** — its own module docs say the only
implementation is the test-only `TestMarkContext`, and that
`ZgcRealHeap::collect_garbage` remains single-threaded. So `zgc_concurrent.rs`
is not on the execution path either, and the conclusion below stands.

**RUNTIME BEHAVIOUR IS UNCHANGED.** `-XX:+UseZGC` today selects exactly the
collector it selected before these modules landed: stop-the-world, non-moving,
non-generational, no TLABs, no load barrier, no compaction. **ZGC is not now
concurrent and not now generational.** Nothing in the 2026-08-07 baseline
(1860 PASS / 49 HANG / 22 FAIL) has been re-measured or moved, because no code
on the execution path changed.

This distinction is not pedantry. Writing "Phase 2 landed" when what landed is
an unadopted `generation.rs` is precisely how this tree accumulated the stale
ZGC docs that workstream 0c existed to clean up. **"Module landed" and
"collector does this" are two different claims.** Keep them apart.

### 0.2 What actually landed

| WS | Artifact | Lines | Tests | State |
|---|---|---|---|---|
| **0a** | `.github/workflows/ci.yml:490-495` | — | — | **LANDED** — builds `cargo build -p cratonvm-cli --features zgc`, runs `cargo test -p cratonvm-gc --lib --features zgc` |
| **0b** | `gc/src/zgc/metrics.rs` | 1819 | 29 | **LANDED, unadopted** |
| **0c** | `docs/GC.md`, `docs/gc-tuning.md`, `docs/gc/gc-crate-audit.md` | — | — | **LANDED** |
| **1a** | `gc/src/zgc/vaddr.rs` | 1499 | 30 | **LANDED, unadopted** |
| **1b** | `gc/src/zgc/barrier.rs` | 2326 | 39 | **LANDED, unadopted** (interpreter side only) |
| **1c** | JIT emit sites | — | — | **NOT STARTED** — see §0.4 |
| **1d** | `gc/src/zgc/page.rs` | 2111 | 19 | **LANDED, unadopted** |
| **1e** | `gc/src/zgc/forwarding.rs` | 2068 | 25 | **LANDED, unadopted** |
| **2a** | `gc/src/zgc/generation.rs` | 2632 | 20 | **LANDED, unadopted** |
| **2b** | `gc/src/zgc/remembered.rs` (planned as `remset.rs`) | 2137 | 29 | **LANDED, unadopted** — but see the live decision in §0.3 |
| **3a** | `gc/src/zgc/mark.rs` + `gc/src/zgc_concurrent.rs` rewrite | 3256 + 1531 | 20 + 12 | **LANDED, unadopted** (`mark.rs` is used by the driver — see §0.1) |
| **3b** | `gc/src/zgc/relocate.rs` | 3079 | 19 | **LANDED, unadopted** |
| **4a** | `gc/src/zgc/tlab.rs` | 2090 | 19 | **LANDED, unadopted** — an *adapter* over `gc/src/tlab.rs`, not a duplicate |
| **4b** | [`docs/gc/zgc-vmheap-arm-audit.md`](../gc/zgc-vmheap-arm-audit.md) | — | — | **AUDIT LANDED**, arms unchanged — see §0.5 |
| **5a–5d** | — | — | — | **NOT STARTED** |

`gc/src/zgc.rs` is now **3788 lines** and `gc/src/zgc_concurrent.rs` **1531**.
The **eleven declared** modules total **~25,700 lines** and **~275 `#[test]`s**.
Whole-feature unit-test count: **~380 and rising** (91 + 12 + ~275).

*Recounted 2026-08-07 (later). This paragraph first read "ten … ~22,970 lines …
241 … 344"; that was correct before `census.rs` was declared (+2708 lines, +24
tests). **Every number in §0.2 is a moving target — modules were still gaining
tests during this very recount (`barrier.rs` 36→39, `metrics.rs` 27→29,
`remembered.rs` 26→29, and the whole-feature total 368→379, all within the
hour).** Treat the line counts as approximate
and the test totals as "as-of"; the stable facts are the **module count** and
the **unadopted** state.* Recount with:

```bash
grep -c '#\[test\]' gc/src/zgc.rs gc/src/zgc_concurrent.rs \
  $(sed -n 's/^pub mod \(.*\);$/gc\/src\/zgc\/\1.rs/p' gc/src/zgc.rs)
```

There is also `gc/tests/zgc_module_integration.rs` (2021 lines, 25 tests), an
integration target — it is **not** in the `--lib` count above.

> **`gc/src/zgc/census.rs` (2708 lines, 24 tests) — the eleventh module.**
> *Superseded 2026-08-07 (later the same day): this blockquote originally read
> "exists on disk but is **NOT declared** in `gc/src/zgc.rs`. It therefore does
> not compile and none of its tests run." **That is no longer true.*** It is now
> declared (`pub mod census;`, the last entry in the `pub mod` block), so it
> compiles and its 24 tests run under `--features zgc` — worth **+24** on the
> whole-feature count (344 → 368 at that moment; ~376 by the time of this
> recount, as other modules gained tests). Risk **R4** (feature-gate rot) is
> **closed for this file**; the "declare it or delete it" call was resolved by
> declaring it.
>
> It still belongs to
> [`zgc-reference-slot-representation.md`](zgc-reference-slot-representation.md)
> rather than to a workstream in this plan, and like every other module here it
> is **unadopted** — a reference-slot *measurement* instrument, not collector
> code. Declaring it changed what CI compiles; it changed nothing at runtime.

> **R4 is NOT closed in general — it has already recurred.** As of this recount
> `gc/src/zgc/adapters.rs` (59 KB, 11 `#[test]`s) sits in the module directory
> and is **declared nowhere** (`grep -rn 'mod adapters' gc/src/` returns
> nothing). It does not compile and none of its 11 tests run. This is the exact
> situation `census.rs` was in a few hours earlier, so treat the pattern, not
> the file, as the finding: **a file appearing under `gc/src/zgc/` is not
> evidence it builds.** Either declare it or delete it.
>
> **Consequence for anyone counting:** do **not** recount with a
> `gc/src/zgc/*.rs` glob — it counts undeclared files and silently inflates the
> total (by 11 right now). Use the `pub mod`-driven command above, which counts
> only what the compiler sees.

### 0.3 Live decision: `remembered.rs` vs the existing card table

`gc/src/zgc/remembered.rs` builds a per-page bitmap pair, faithful to OpenJDK
Generational ZGC. **Its own author recommends NOT using it for the first
generational landing.** From its module doc (`remembered.rs:25-163`): the
bitmap pair costs **16x** the byte-map of the existing
[`crate::card_table::CardTable`], and the three properties that justify that
cost — O(1) per-page lookup, page-relative indexing, a non-contiguous address
space — only become load-bearing once ZGC actually allocates from real
`page.rs` pages. Until then the recommendation is to **reuse
`gc/src/card_table.rs`** over a contiguous range.

**This is an open decision, not a settled one.** Whoever adopts Phase 2 picks
one; picking the bitmap by default because it is the newer file would be the
wrong reason.

### 0.4 1c (JIT load barrier) is the outstanding correctness gap

1b shipped the **interpreter** barrier only. Risk **R3** in §4 is therefore
live and unmitigated: a barrier the interpreter honours and compiled code does
not is not a partial barrier, it is a broken one. This does not bite today only
because 1b is unadopted — the moment `ZgcRealHeap` takes the barrier, 1c is a
hard blocker, not a follow-up.

> **Superseded 2026-08-07 (later the same day).** This blockquote originally
> read: *"a companion doc `docs/feature-designs/zgc-jit-load-barrier.md` has
> been described as costing this work out. **No such file exists** — `grep -r
> zgc-jit-load-barrier` returns nothing anywhere in the tree as of this
> writing."* **That was true when written and is now false:**
> [`zgc-jit-load-barrier.md`](zgc-jit-load-barrier.md) landed the same day and
> is referenced from `vm/src/vm/vm_init.rs`. The ZGC companion docs are now
> that one, [`zgc-reference-slot-representation.md`](zgc-reference-slot-representation.md)
> and [`../gc/zgc-vmheap-arm-audit.md`](../gc/zgc-vmheap-arm-audit.md).
>
> **What has NOT changed is the status of 1c itself: no JIT emit site was
> modified, so the correctness gap above is fully live.** A design doc is not
> an implementation — treat that file as a costing, and re-read it before
> quoting it, rather than as evidence 1c has moved.

### 0.5 4b landed as an audit, and it found live bugs

[`docs/gc/zgc-vmheap-arm-audit.md`](../gc/zgc-vmheap-arm-audit.md) classifies
every `VmHeap::Zgc` arm. Headline: **16 NON-MOVING-ONLY** (correct today, wrong
under compaction or a generational split) and **5 ALREADY-WRONG** — wrong for
today's non-moving collector, i.e. **five live defects that do not need Phase 3
to bite**. No code was changed by that audit; the arms are as they were.

Ranked 1–5 there: the empty `pointer_map`; the `is_addr_live` /
`watched_pre_gc_addr_survived` predicates that must land *with* it;
`supports_jit_tlab_skip` plus the two skip-region no-ops; `pin_critical_region`
returning `Vec::new()`; and `metadata_pin_deferrable` / `mirror_pin_deferrable`
answering `true`.

### 0.6 Metrics: the concurrent column is zero by construction

`gc/src/zgc/metrics.rs:544` defaults `phases_run_concurrently` to **`false`**,
deliberately, because the collector is stop-the-world for everything. **Any
report showing 0% concurrent time is reproducing that default, not measuring
the collector.** It becomes a measurement only once a phase genuinely runs off
the safepoint and something calls `set_phases_run_concurrently(true)`. Do not
quote it as evidence either way before then.

---

## 1. Where ZGC actually is

> **Superseded in its FIGURES, not its verdict — dated 2026-08-07 (later the
> same day).** This section was written before the modules in §0.2 landed, and
> every line number and size in it has drifted. Known-stale as of a recount from
> disk: `zgc.rs` is **3788** lines, not 3454; `ZgcRealHeap` begins at **:1448**,
> not 1396; `ZPage` **:378**, `ZgcHeap` **:499**, `ZgcCollector` **:711**,
> `GenerationalZgc` **:1075**; the one-time `tracing::warn!` is at **:155**, not
> :103. The `gc/Cargo.toml` / `vm/Cargo.toml` line cites at the end of this
> section are stale too — find the feature by name (`^zgc = `), not by line.
>
> **One substantive claim, not just a number, has changed:** the paragraph below
> saying `gc/src/zgc_concurrent.rs` "(505 lines)" drives `Arc<Mutex<ZgcCollector>>`,
> "i.e. **the simulation**" describes the *pre-rewrite* file. It is now **1531**
> lines and drives the real `zgc::mark` engine (`ZMarkCoordinator`) instead. The
> sentence that follows it — *"It has never been pointed at `ZgcRealHeap`"* —
> **remains true**: no `ZMarkContext` for `ZgcRealHeap` exists, so the driver is
> still off the execution path (§0.1).
>
> Everything else here — the two-halves-in-one-file shape, the simulation's
> character, `ZgcRealHeap` being real but non-moving/non-concurrent/non-generational
> with no TLABs, and the measured suite cost — was re-verified and **stands**.

`gc/src/zgc.rs` (3454 lines) is two different things sharing a file. Lines
1–1395 are a **metadata-only simulation** (`ColoredPointer`, `LoadBarrier`,
`ZPage` at `:326`, `ZgcHeap` at `:447`, `ZgcCollector` at `:659`,
`GenerationalZgc` at `:1023`): `ZPage::virtual_start`/`physical_start` are
synthetic `u64` counters with no `*mut u8` behind them, `concurrent_relocate`
records a forwarding entry but copies no bytes because there are no bytes,
"concurrent" phases run synchronously inside `trigger_gc`, and
`LoadBarrier::slow_path` takes `&mut self` and mutates a plain `ColoredPointer`
value rather than performing an atomic self-healing CAS on an in-memory oop.
Selecting it emits a one-time `tracing::warn!` (`zgc.rs:103`) so it can never
masquerade as production ZGC. **This half is scaffolding, not a collector.**

Line 1396 onward is `ZgcRealHeap`, and it is genuinely real: `Arena`-backed
storage, real `ObjectHeader`s, real `java.lang.ref` reference processing, real
bounds-checked field/array semantics, and a real stop-the-world **non-moving
whole-heap mark-sweep**. It implements `GarbageCollector` in full and is wired
end to end — `GcAlgorithm::Zgc` (`vm/src/config.rs:51`) → `GcBackend::Zgc`
(`vm/src/vm/vm_init.rs:1357`) → `VmHeap::Zgc(ZgcRealHeap)`
(`gc/src/vm_heap.rs:214`, `:278`) — and is selectable with `-XX:+UseZGC` /
`--XX:UseGc ZGC`. It has no TLABs (`vm_heap.rs:2114` returns `None` from
`refill_tlab`), no compaction, no concurrency, and no generational split.

`gc/src/zgc_concurrent.rs` (505 lines) has a `ZgcConcurrentMarkController`
(`:174`) with a real background worker thread — but it drives
`Arc<Mutex<ZgcCollector>>` (`:198`), i.e. **the simulation**. It has never been
pointed at `ZgcRealHeap`.

All of the above is behind the default-off `zgc` Cargo feature
(`gc/Cargo.toml:90`, `vm/Cargo.toml:106`). The gate hides an enum variant and a
`parse_gc_algorithm` arm, so in a default build `-XX:+UseZGC` warns and silently
falls back to Generational.

**The measured cost of the gap** (2026-08-07, 1975-class Spring Boot suite,
same binary, `-Xmx 2g`, only `-XX:+UseZGC` varied): PASS 1902 → 1860, HANG
18 → 49, FAIL 11 → 22. **50 classes changed status, 46 of them regressions**;
35 were PASS → HANG at the 300 s ceiling, overwhelmingly `*AutoConfigurationTests`
— the build-and-tear-down-many-`ApplicationContext`s shape, i.e. allocation
churn. That is the signature a whole-heap STW mark-sweep with no young-gen
fast path produces. **It is a working hypothesis, not a root cause** — no
GC-count or pause-time instrumentation was pulled from any of those 35 logs.
Phase 0 exists to fix that, and Phase 2 is the phase most likely to move the
number.

---

## 2. Phases and their workstreams

Each workstream names its **owned files**. Two workstreams that share no owned
file can run at the same time by different agents.

### Phase 0 — unblock and instrument *(no dependencies)*

| WS | What | Owned files | Gated on | Status |
|---|---|---|---|---|
| **0a** | CI must build the *binary*, not just the libs, under `--features zgc` | `.github/workflows/ci.yml` | — | **DONE 2026-08-07** |
| **0b** | Pause/phase/GC-count instrumentation for `ZgcRealHeap` | `gc/src/zgc/metrics.rs` (new) | — | in progress |
| **0c** | De-stale the ZGC docs | `docs/GC.md`, `docs/gc-tuning.md`, `docs/gc/gc-crate-audit.md` | — | **DONE 2026-08-07** |

**0a is already landed and is the precedent that motivates the rest.**
`ZgcRealHeap::with_capacity()` was left out of the change that gave the
compact-layout registry a `layout_domain` — its three sibling collectors
(`Heap`, `G1Collector`, `GenHeap`) all got the field, it did not — and the
feature stopped compiling with `E0063` on 2026-08-05. It was found on
2026-08-07 by a human running a full suite, **not by CI**, and fixed in
`18ae88a21`. Two gaps let it through, and `ci.yml:491`/`:495` now close both:
nothing built `cratonvm-cli` with the feature (every `zgc` step was `-p
cratonvm-vm -p cratonvm-native-builtins`, i.e. libraries, and `zgc` gates whole
dispatch paths a default build resolves to nothing), and the collector's own
**89 `#[test]`s** (86 in `zgc.rs`, 3 in `zgc_concurrent.rs`) executed nowhere.
Every module this plan adds must land inside a build CI actually runs — see the
feature-gate-rot risk in §4.

**0b is the gate on believing anything in Phase 2.** The 35-class HANG
hypothesis is unverified. Until `ZgcRealHeap` reports GC count, per-phase
(mark/sweep) time and bytes reclaimed per cycle, a Phase 2 improvement cannot be
distinguished from host noise. Precedent for the shape: `gc/src/gc_metrics.rs`
and the young-pause phase-share work — note that on a shared host the *shares*
survive a sizing change but the absolute figures do not, so report shares.

### Phase 1 — core moving-GC mechanics

These four are **deliberately decoupled and are being written in parallel right
now** as self-contained submodules under a new `gc/src/zgc/` directory. The
decoupling is the design: the barrier depends on a **trait**, not on the other
modules' concrete types, so all four can land independently and in any order.
None of them is wired into `ZgcRealHeap` as it lands — wiring is Phase 3/4 work.

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **1a** | Real colored pointers + multi-mapped virtual address space | `gc/src/zgc/vaddr.rs` (new) | — |
| **1b** | Atomic self-healing load barrier, **interpreter** side | `gc/src/zgc/barrier.rs` (new) | — (trait-only) |
| **1c** | The same barrier, **JIT** side | JIT emit sites (see §4) | 1b's trait |
| **1d** | Real region/page management with backing storage | `gc/src/zgc/page.rs` (new) | — |
| **1e** | Lock-free forwarding table + relocation-set selection | `gc/src/zgc/forwarding.rs` (new) | — |

**1b and 1c are separate tracks on purpose.** An interpreter barrier is a Rust
function on the load path; a JIT barrier is emitted machine code at every
reference load in compiled code. They fail differently, they are tested
differently, and a barrier that covers only the interpreter is a correctness
hole the moment a method tiers up — see the JIT-coverage risk in §4.

`ZPage` in the simulation is the shape 1d should inherit, but not the code:
it has no backing storage. `1d` is `ZPage`'s API over a real `Arena`.

### Phase 2 — generational split *(highest value for the measured regression)*

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **2a** | Real young/old storage under the existing policy | `gc/src/zgc/generation.rs` (new) | 1d |
| **2b** | Remembered set / cross-generational edge tracking | `gc/src/zgc/remset.rs` (new) | 1d |

**There is a real head start here, and it is a policy head start, not a code
head start.** `GenerationalZgc` (`zgc.rs:1023`) already implements the
JEP-439-shaped policy: minor/major scheduling, per-page promotion aging
(`page_ages`, `promotion_age`, `promotion_count`), occupancy thresholds, and a
`remembered_set` as a card-table substitute. It needs **real storage under it,
not a new policy design.** The `with_total_size` / `with_default_split`
constructors and `DEFAULT_YOUNG_FRACTION` are reusable as-is.

This is the phase that should move the 35 HANG classes, because the hypothesis
those 35 classes support is specifically "every collection scans the entire live
set instead of a small young generation". Phase 0b's instrumentation is what
turns that from a hypothesis into a measurement.

### Phase 3 — concurrency

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **3a** | Rewire the concurrent-mark controller from the simulation onto `ZgcRealHeap` | `gc/src/zgc_concurrent.rs` | 0b, 1b |
| **3b** | Concurrent relocation / compaction | `gc/src/zgc/relocate.rs` (new) | 1a, 1b, 1c, 1d, 1e, 3a |

**3a has a good in-tree precedent.** `gc/src/g1_concurrent.rs` (675 lines) plus
`gc/src/satb.rs` (1234 lines) is a working background-worker concurrent marker
against a real collector, including the drain contract that
`concurrent-gc-maturation.md` Step 3 fixed (G1's `remark` must call
`flush_all_thread_satb_buffers` before the shard drain, or a mutator's unspilled
thread-local buffer is excluded from the remark snapshot → UAF). ZGC's
concurrent mark will need the equivalent, and should reuse `satb.rs` rather than
grow a second implementation.

**3b is the highest-risk item in this plan and has NO in-tree precedent.**
G1's evacuation is stop-the-world and single-threaded under one big lock
(`g1.rs` `young_collection`/`mixed_collection` hold `self.regions.lock()` for
the entire pause); the generational young gen's Cheney copy is also STW. Nothing
in this tree has ever moved an object while mutators were running. That means
3b is simultaneously the piece with the most novel correctness surface and the
piece with no working example to copy. Sequence it last, land it behind its own
sub-flag, and expect it to be the phase that needs the most validation budget.

### Phase 4 — integration debt *(correctness landmine, not a nicety)*

| WS | What | Owned files | Gated on |
|---|---|---|---|
| **4a** | TLAB support for the ZGC backend | `gc/src/tlab.rs`, `gc/src/vm_heap.rs` | 1d |
| **4b** | Audit and populate every neutral `VmHeap::Zgc` arm | `gc/src/vm_heap.rs` | 1e |

`gc/src/vm_heap.rs` has **58 literal `VmHeap::Zgc` arms** plus the `dispatch!`
macro's own arm (`vm_heap.rs:228`), which expands across ~40 more call sites.
Many of the literal arms are neutral placeholders chosen because
`ZgcRealHeap` is non-moving, and **they stop being correct the instant ZGC moves
an object.** The specific landmine:

* **`ZgcRealHeap::collect_garbage` returns an always-empty pointer map**
  (`gc/src/zgc.rs:2464`: `let pointer_map: HashMap<usize, usize> = HashMap::new();`,
  returned in `GcResult` at `:2478`). Its own doc comment (`zgc.rs:1394`) says
  this is "exactly correct for a non-compacting collector" — which it is,
  today. A moving ZGC that still returns an empty map means every consumer of
  the map (`monitors.remap_after_gc`, and the `pointer_map`-keyed liveness
  checks at `vm_heap.rs:2189` and `:2375`) silently sees "nothing moved" while
  objects moved. That is a use-after-free with no error path.
* The **affirmative** arms are worse than the `None` ones, because a `None`
  routes the caller to a slow generic path while a `true` asserts a fact:
  `vm_heap.rs:1809`, `:2304`, `:2344` answer `true` for ZGC, and `:1694`/`:1711`
  answer `!self.needs_gc()`. Each needs re-deriving under a moving collector.
* `conservative_addr_span` (`vm_heap.rs:527`) returns `None` because ZGC keeps
  live bases in a registry rather than a contiguous arena — with real pages
  (1d) this becomes answerable, and answering it is a stack-scan speedup.
* `jit_card_table_info` (`:1381`), `old_gen_info` (`:1594`), `old_gen_lock`
  (`:1605`), `collect_young_to_old_roots` (`:1619`) and `enable_concurrent_gc`
  (`:1633`) are all "generational-only, ZGC returns nothing" — every one of them
  becomes live work in Phase 2/3.

**4b is not a cleanup task. It is the phase that decides whether Phase 3b
corrupts the heap.** Do the audit before 3b lands, not after.

`4a`: `ZgcRealHeap` takes the arena lock on every allocation. `docs/GC.md`
already calls this out ("it is a correctness-first reference backend, not a
throughput one"). Since the measured regression is an *allocation-churn*
workload, TLABs may account for part of the 35-class HANG group independently of
the generational split — 0b's instrumentation should be able to separate the two.

### Phase 5 — validation and rollout

| WS | What | Gated on |
|---|---|---|
| **5a** | Re-run the 1975-class Spring Boot suite per phase; track PASS/HANG/FAIL against the 2026-08-07 baseline | any of 2–4 |
| **5b** | Triage the 11 PASS → FAIL classes, which are **probably not all GC** | 0b |
| **5c** | First-ever Hibernate ORM run against ZGC | 2 |
| **5d** | Default-on decision | 5a parity |

**5b first, because it is cheap and it may shrink the problem.** The 11 FAIL
classes fail fast (1–24 s, not timeouts), so they are a different mechanism from
the HANG group. The one class inspected closely — `VirtualZipDataBlockTests` —
reads like a fixture/working-directory or zip-content bug, not memory
corruption. Re-run those 11 under Generational with the same binary back to back
before assuming they share a cause with each other or with the HANG group.

**5c: the Hibernate ORM suite has never been run against ZGC.** The only
full-suite ZGC data that exists is the Spring Boot autoconfig set in the
2026-08-07 record. Hibernate is a different allocation profile and is the more
likely place a moving collector's remaining root-coverage gaps surface.

**5d gates on pass-rate parity with Generational, not on a date.** The bar is
the Generational arm of the same suite on the same host (1902/1975 = 96.3%),
and ZGC must reach it with no new HANG class. Until then `zgc` stays default-off
and the docs keep saying so.

---

## 3. Dependency graph

```
Phase 0 ──────────────────────────────────────────────────────────
  0a CI binary build            [DONE]
  0b metrics.rs                 [no blockers — START NOW]
  0c doc de-staling             [DONE]

Phase 1 ── all four START NOW, no blockers, no shared files ──────
  1a vaddr.rs      ─┐
  1b barrier.rs    ─┼─ (1b exposes a trait; 1c consumes only the trait)
  1d page.rs       ─┤
  1e forwarding.rs ─┘
  1c JIT barrier    ← 1b (trait only)

Phase 2 ── needs real pages ──────────────────────────────────────
  2a generation.rs  ← 1d
  2b remset.rs      ← 1d

Phase 3 ─────────────────────────────────────────────────────────
  3a concurrent mark on real heap ← 0b, 1b
  3b concurrent relocate          ← 1a,1b,1c,1d,1e,3a   [HIGHEST RISK]

Phase 4 ─────────────────────────────────────────────────────────
  4a TLABs                        ← 1d
  4b vm_heap arm audit            ← 1e   [MUST precede 3b]

Phase 5 ─────────────────────────────────────────────────────────
  5b FAIL triage   ← 0b
  5a suite re-runs ← any of 2,3,4
  5c Hibernate     ← 2
  5d default-on    ← 5a parity
```

**Startable immediately with zero blockers: 0b, 1a, 1b, 1d, 1e** — five
workstreams, five agents, no shared owned file. 1c unblocks as soon as 1b's
trait exists (not its implementation). Everything in Phase 2 and 4a unblocks on
1d alone, which makes **1d the critical path** — it is the single highest-leverage
module in the plan and should be staffed first if staffing is scarce.

`gc/src/vm_heap.rs` is contended: **4a and 4b both touch it**, and so does
anything that adds a `VmHeap` method. Serialize them or hold the file.

---

## 4. Risk register

| # | Risk | Why it matters | Mitigation |
|---|---|---|---|
| **R1** | **The young-gen hypothesis is unverified.** | The entire justification for prioritizing Phase 2 is one unmeasured inference from 35 timeouts. If the 35 HANG classes are actually the arena-lock/no-TLAB path, or a livelock relative of the `gc_rearm` mode already fixed in `zgc.rs`, then Phase 2 is a large investment against the wrong cause. | 0b before 2a. Pull GC count + phase times from a sample of the 35 before committing. |
| **R2** | **Compressed oops are structurally incompatible with colored pointers.** | `gc/src/compressed_oops.rs` narrows a pointer to 32 bits with a shift; ZGC colored pointers spend high bits on mark/remap/finalizable colors and rely on multi-mapping the same physical page at several virtual addresses. Both want the same bits. HotSpot resolves this by making ZGC and compressed oops mutually exclusive. | Decide explicitly in 1a. Most likely outcome: `-XX:+UseZGC` forces compressed oops off, and that must be enforced at config-parse time with a clear message, not discovered as corruption. |
| **R3** | **JIT barrier coverage is all-or-nothing.** | A load barrier that the interpreter honors and compiled code does not is not a partial barrier — it is a broken one, because the first tier-up silently drops the invariant. This is why 1c is its own workstream. | 1c ships with a coverage assertion over JIT reference-load emit sites, and a debug mode that refuses to compile a method containing an unbarriered reference load. |
| **R4** | **Feature-gate rot.** | Already happened once: `zgc` was uncompilable from 2026-08-05 to 2026-08-07 and CI did not notice, because every `zgc` job was library-scoped. `synthetic-jdk` lost 1,522 tests the same way. Every new `gc/src/zgc/*.rs` module is behind the same default-off gate. | 0a's two `ci.yml` steps must stay. Any new module needs its tests inside `cargo test -p cratonvm-gc --lib --features zgc` (`ci.yml:495`), and any new *flag* needs the binary build (`ci.yml:491`). |
| **R5** | **Concurrent relocation has no in-tree precedent.** | Every existing CratonVM collector moves objects only at a stop-the-world. 3b is novel correctness surface with no working example, and it depends on five other new modules being simultaneously correct. | Sequence 3b last. Land it behind its own sub-flag, default-off within the already-default-off `zgc` feature, so a bad 3b cannot regress a good Phase 2. |
| **R6** | **The `VmHeap::Zgc` neutral arms are silent.** | 58 arms plus a macro; the affirmative ones (`true`, `(0,0)`) assert facts that are only true for a non-moving collector, and none of them will fail loudly when they become wrong. | 4b before 3b. Prefer `unimplemented!()` over a neutral value for any arm whose correct moving-collector answer is not yet known — a panic in a default-off feature is cheaper than a UAF. |

---

## 5. What this plan does not cover

* **The simulation half of `zgc.rs` (lines 1–1395) is not deleted by this plan.**
  It is the API shape Phase 1 is re-implementing against real storage, and
  `GenerationalZgc`'s policy is directly reused by 2a. It can be retired once
  Phase 2 lands, and not before. Its `warn_zgc_simulation_selected()` (`zgc.rs:103`)
  must survive as long as any type in it is constructible.
* **G1 maturation** — owned by [`concurrent-gc-maturation.md`](concurrent-gc-maturation.md),
  which picks G1 as its target and explicitly defers production ZGC to its §8.
* **The 2026-08-07 regression record** —
  [`docs/known-issues/springboot/zgc-real-fullsuite-regression-20260807.md`](../known-issues/springboot/zgc-real-fullsuite-regression-20260807.md)
  is a run record. It is history. Phase 5 adds new records beside it rather than
  editing it.
* **`gc/src/zgc.rs`'s address-keyed state was never audited.**
  [`docs/gc/gc-crate-audit.md`](../gc/gc-crate-audit.md) §5.4 explicitly
  excludes it ("a distinct model that deserves its own pass"). That audit is
  still owed, and Phase 1 makes it larger, not smaller.
