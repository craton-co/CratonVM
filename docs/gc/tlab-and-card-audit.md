# TLAB retirement, card costs, and what the collector actually decides

*Written 2026-07-31 against `feat/c2-review-remediation`. Closes three C2
review items: the P0 "audit TLAB retirement and publication", the P1 "make
remembered-set/card costs observable", and the P1 "detect documentation/runtime
GC drift".*

Companion to [`docs/threading/objectref-concurrency-contract.md`](../threading/objectref-concurrency-contract.md),
which maps the four-way STW mutator census, the `StopTheWorldToken` relocation
witness, and the two incompatible "pin" vocabularies. This document assumes that
one. Where the two overlap, that one owns the threading model and this one owns
the allocation buffer.

Line numbers are as of the commit this landed on. Where a claim rests on a
convention rather than on a check, that is stated and — where it was possible
inside `gc/` — a `debug_assert!` now names it.

---

## 0. Summary of findings

| # | Finding | Severity | Status |
|---|---|---|---|
| T-1 | `jit_tlab_skip_offsets` returned the published reserved tails un-deduplicated and un-coalesced, while both consumers' comments asserted "ascending & disjoint" and `skip_free_blocks` requires it. Two partially-overlapping spans make the walk resync twice and silently skip every object between them. | **High** (missed objects → premature reclamation) | **Fixed** — the heap now sorts, coalesces and `debug_assert!`s the invariant. |
| T-2 | `Tlab::reserved_tail()` could publish a non-8-aligned start, which `jit_tlab_skip_offsets` silently *drops*, after which the sweep walks the un-retired tail as objects. Held only because every production allocation uses `align = 8`. | **Medium** (unreachable today, one `Tlab::alloc(_, 1)` away) | **Fixed** — `debug_assert!` names it; release rounds the published start up, which is the fail-safe direction. |
| T-3 | A moving young collection can run while reserved TLAB tails are published, which by construction means an alive mutator left a TLAB un-retired across its exclusion point. The walk is still correct, but after the semispace swap the owner's cursor points into recycled memory. | **High if reachable**; no reproducer found | **Tripwire added** in the moving path (warn, rate-limited). The producing transition, if any, is in `vm/`. |
| T-4 | `install_tail_filler` had one exit (`< 8` bytes of unaligned slack) that returned with `cursor < end`, leaving a publishable span the filler had not covered — breaking its callers' "after this, the TLAB is fully consumed" assumption. | Low | **Fixed** — the function is now total. |
| T-5 | "Size the outgoing TLAB *before* retiring it" is load-bearing (`retire` zeroes the `cursor - start` span the sizer reads, and a zero reads as *idle*, reviving the JIT-blind one-way shrink ratchet) and was enforced by nothing. | Low (correct today) | **Tripwire added** — `debug_assert!` in `Tlab::next_refill_size`. |
| T-6 | `VmNativeThreadBlocker::enter_blocked` (`vm/src/vm/vm_exec.rs:4615`) excludes a thread from the STW census **without** retiring its TLAB, unlike every other blocking-region entry — and threads reached through it publish no `tlab_addr`, so the collector's reserved-tail backstop cannot see them either. Both defences absent. | Open | **Not fixed** — `vm/`-side, outside this change's ownership. No evidence found that such a thread ever holds a TLAB. |
| T-7 | The reserved-tail backstop (`collect_reserved_tlab_tails` → `set_jit_tlab_skip_regions`) only runs on the multi-threaded initiator path and only when `xt::enabled() && supports_jit_tlab_skip()` (`vm/src/runtime/interpreter.rs:598`). With cross-thread takeover off, an un-retired tail on any peer is neither skipped nor detected. | Open | **Not fixed** — `vm/`-side. Documented in §1.4. |
| D-1 | `docs/GC.md` and `ARCHITECTURE.md` give contradictory accounts of when a young collection moves. | — | **Resolved** in §3: `ARCHITECTURE.md` is correct, `docs/GC.md:14` and `docs/GC.md:157-159` are stale. A runtime report now states the answer for the running process. |

No case was found where a TLAB tail is **scanned twice** in the sense of being
*marked* twice — the skip machinery only ever removes bytes from a walk. The
double-skip hazard (T-1) is the mirror image: skipping too much.

---

## 1. The TLAB transition table

### 1.1 What "retired" means

A TLAB is *retired* when `Tlab::retire()` (`gc/src/tlab.rs`) has:

1. written a walkable filler over the unused `[cursor, end)` tail — a synthetic
   `int[]` with `TLAB_FILLER_CLASS_ID` when 40 or more bytes remain, or the
   two-word `GAP_FILLER_CLASS_ID` sentinel when 8–32 remain; and
2. nulled `start`, `cursor` and `end`, so `is_retired()` is true,
   `reserved_tail()` is `None`, and the next allocation must refill.

Both halves matter to a different consumer. (1) is what lets a *linear heap
walk* stride the tail in O(1) instead of decoding zeroed bytes as a run of
40-byte objects and desyncing off the object grid. (2) is what makes the
cross-thread STW protocol's central assumption true — `ThreadRegistry`'s
`tlab_addr` safety note (`vm/src/threading/thread_registry.rs:198-205`) reads
"every *alive* peer is either parked at a safepoint or blocked (both having
already retired their TLAB → `cursor`/`end` null)".

`retire()` is **idempotent**, and it has to be: a thread that parks at a
safepoint, is then chosen as the next GC initiator, and finally terminates runs
three retires with no refill between them. This is now asserted rather than
assumed (`Tlab::retire`'s post-condition `debug_assert!`).

### 1.2 The table

"Worst-case interleaving" = the collector's view if a stop-the-world lands at
the least convenient instant of the transition.

| Transition | Who retires | Synchronisation | Worst-case interleaving | Verdict |
|---|---|---|---|---|
| **Cooperative safepoint park** | the thread itself, `vm/src/runtime/interpreter.rs:4312` (`safepoint_check`) | before `arrive_and_wait_auto`, so the retire happens-before the initiator observes the arrival | STW lands before the retire → the thread has not arrived, the barrier still waits for it | **Sound** |
| **GC initiator, allocation-triggered** | itself, `interpreter.rs:1189` (`maybe_gc`) | before `request_stw`; the initiator never passes through `safepoint_check`'s arrive | none — single-threaded prologue | **Sound** |
| **GC initiator, forced / OOM ladder** | itself, `interpreter.rs:1600` (`maybe_gc_forced`), plus the pre-retires at `:1573`, `:1839`, `:3000`, `:3337`, `:3527` and `vm/src/runtime/exceptions.rs:1192` | as above | this path was historically the *one* initiator that did not retire; the comment at `interpreter.rs:1591-1599` records the resulting SIGSEGV | **Sound** (fixed earlier) |
| **TLAB exhausted → refill** | itself, `interpreter.rs:3226`, retire-before-replace | single-threaded on the owning thread; the sizer runs at `:3154`, *before* the retire | a STW between retire and `Tlab::new` sees an empty TLAB — the strictly safest state | **Sound**. The size-then-retire ordering is now a `debug_assert!` (T-5). |
| **JIT allocation slow path / guarded refill** | itself, `interpreter.rs:3137`, `vm/src/jit/helpers.rs:2740`, `:3180`, `:3242`, `:3537` | before each forced GC | helpers retire before kicking the collector, so the tail is filled while the arena is still mapped | **Sound** |
| **Enter blocking native** | itself, `vm/src/vm/vm_exec.rs:4626` (`begin_blocking_region_with_state`) | **before** `deposit_root_snapshot` and `enter_blocked`, i.e. before the thread leaves the counted set | this is the case the retire exists for: while blocked, a moving collection can `grow()` (realloc) the young arena and free the backing buffer; a retained `[cursor, end)` would dangle and the first post-block bump would write a header into freed memory | **Sound** |
| **Leave blocking native** | n/a — refills on next allocation | `mark_blocked_region_leave` then `check_post_block_gc` | the TLAB is empty, so there is nothing to fix up | **Sound** |
| **Foreign native-thread blocker** | **nobody** — `VmNativeThreadBlocker::enter_blocked`, `vm_exec.rs:4615` | `mark_native_thread_blocked` + `mark_blocked_region_enter`, no retire | thread is excluded from the census while (hypothetically) holding a TLAB, *and* publishes no `tlab_addr`, so `collect_reserved_tlab_tails` cannot see it either | **T-6, open.** Asymmetric with every other blocking entry. Not reproducible: no `set_tlab_addr` call covers threads reached this way, which also suggests they hold no TLAB. |
| **Platform thread termination** | itself, `vm_exec.rs:10464` | retires *before* the terminal `deposit_root_snapshot` / `enter_blocked` / `clear_tlab_addr` / `mark_dead` sequence at `:10585-10599` | clearing `tlab_addr` first would leave a raw zeroed tail with no owner from which to recover a skip span | **Sound** — the ordering comment at `:10458-10463` states exactly this |
| **Virtual thread termination** | itself, `vm_exec.rs:2947` | same shape as the platform path (`clear_tlab_addr` + `mark_dead` inside `finish_after`) | as above | **Sound** |
| **Virtual thread unmount** | the unmounting carrier, `vm/src/threading/virtual_threads.rs:1265` (`suspend_runtime`) | under the manager lock, after `vt.unmount()`, before the boxed `JvmThread` is parked in `vt.runtime` | the registry entry stays alive with `tlab_addr` still published — but pointing at a *retired* TLAB, so `reserved_tail()` is `None` and it contributes no skip region for the whole parked window | **Sound** |
| **Virtual thread remount / carrier migration** | nothing to retire | `set_os_tid_current` → `set_jvm_thread_addr` → `set_tlab_addr` (`vm_exec.rs:2744-2752`) all happen *before* `check_post_block_gc` makes the thread a counted mutator again | publishing after becoming counted would point a takeover at the previous carrier's OS thread; publishing before is the fix | **Sound**. The address is stable across migration because the `JvmThread` is boxed and only the `Box` moves. |
| **JNI `AttachCurrentThread` / `DetachCurrentThread`** | itself, `vm/src/native/jni.rs:562` (detach), `:788`, `:1012`, `:6754` | `tlab.retire()` **then** `clear_tlab_addr` **then** `mark_dead` (`jni.rs:558-566`) | reversing the first two would let a later sweep see raw zeroed tail bytes with no owner | **Sound** — the ordering comment at `jni.rs:556-561` states it |
| **Forced OS takeover of an in-JIT peer** | **nobody, by design** — the peer never reached a safepoint | the peer is `SuspendThread`-ed, so the collector reads its `Tlab` race-free through the published `tlab_addr`; `Tlab::reserved_tail()` (`gc/src/tlab.rs`) → `ThreadRegistry::collect_reserved_tlab_tails` (`thread_registry.rs:520`) → `VmHeap::set_jit_tlab_skip_regions` (`interpreter.rs:856`) | the JIT commits the bump cursor only *after* writing the full object header (`jit/src/x64.rs`, the BinTrees-18 fix comment at ~`:9053`), so `cursor` is the linearization point and `[cursor, end)` wholesale-covers any in-flight object | **Sound**, and this is the one intentional un-retired case |
| **VM shutdown** | main thread via the normal termination path; `vm_init.rs:6708` publishes its `tlab_addr` and it is address-stable inside the `Vm`'s own `Box` | no collection runs after shutdown begins | — | **Sound** |
| **Forced / low-memory collection** | the initiator, as above; peers via their own transition | the escalation ladder (`maybe_gc_forced` → `g1_force_full_cycle` → OOM) retires once at the top | a retry that allocated between retires would refill first | **Sound** |

### 1.3 Defects found in the publication path (and what was done)

**T-1 — reserved tails were published un-normalised.** `GenerationalHeap::jit_tlab_skip_offsets`
(`gc/src/gen_heap.rs`) filtered the published `(cursor, end)` pairs to the walked
window and sorted them, and stopped there. Both consumers then merged the result
with `Arena::free_blocks_sorted()` and fed it to `skip_free_blocks`
(`gen_heap.rs:11352`), whose contract is *ascending, disjoint* spans: it advances
the walk cursor to `off + sz` and drops the entry.

* Two identical entries are harmless (the second is consumed with the cursor
  already past it).
* Two **partially overlapping** entries `[a, c)` and `[b, d)` with `a < b < c < d`
  resync the cursor to `c`, then immediately to `d` — silently swallowing every
  object in `[c, d)`. On the object-start walk that is a missing grid entry; on
  the non-moving sweep it is a live object never visited, i.e. premature
  reclamation.

The producer is a per-registry-entry read of a *published raw address*, and
guarantees neither property. Two entries can name the same `Tlab`: a virtual
thread's boxed `JvmThread` is published under its thread id at `install_runtime`
(`vm_exec.rs:10081`) and re-published at every remount (`:2752`), and a foreign
thread that re-attaches before its previous entry is reaped publishes again.
Duplicates are also exactly what a stale publication looks like.

`jit_tlab_skip_offsets` now sorts, **coalesces overlapping and touching spans**,
and `debug_assert!`s the disjointness the callers rely on. Coalescing is the
fail-safe direction: over-skipping only over-retains, while the desync it
prevents frees live memory. Both prose claims ("both inputs are ascending &
disjoint", `gen_heap.rs:4444`; "a TLAB tail is reserved, never on the free
list", `:5586`) are now backed by the function that produces the list.

**T-2 — a misaligned published start vanishes.** `jit_tlab_skip_offsets` drops
any region whose start fails `(c & 0x7) == 0`. Dropping is *not* a conservative
degrade: the sweep then walks the un-retired tail as if it held objects. Every
production allocation goes through `alloc_initialized(_, 8, _)`
(`interpreter.rs:2753`, `:3088`, `:3254`), so cursors are 8-aligned — but
`Tlab::alloc` is public with a caller-supplied alignment and nothing checked.
`reserved_tail()` now `debug_assert!`s the alignment and, in release, rounds the
published start **up** to the next 8-byte boundary so the region survives the
filter instead of vanishing from it. The at-most-7 bytes given up are
inter-object padding after the last allocation.

**T-3 — a moving cycle with published tails.** `stw_take_over_and_wait`
(`interpreter.rs:849-870`) publishes skip regions when `taken.count() > 0 ||
helper_windows > 0 || !regions.is_empty()`, but calls
`mark_moving_young_coverage_incomplete()` only when `taken.count() > 0 ||
helper_windows > 0`. The comment justifies the asymmetry — "reserved TLAB tails
alone freeze nobody, and gating on them would starve promotion on every
cooperative multi-threaded cycle" — and that reasoning is sound *if* the
regions list is empty whenever no peer was frozen, which is what the transition
table above says should hold.

If it does not hold, a moving Cheney cycle runs while some alive thread owns a
TLAB in the arena about to be evacuated, swapped and reset. The walk itself is
fine (the tail is skipped, not parsed); the hazard is afterwards, when the owner
resumes and bump-allocates from a cursor into recycled memory. Since the
condition is precisely detectable from inside the collector, the moving path now
carries a rate-limited `warn` tripwire naming it. Diverting to the sweep on a
heuristic would be a worse trade than naming the offending transition, so this
is a diagnostic, not a policy change.

**T-4 — `install_tail_filler` was not total.** Its `aligned >= end_addr` exit
(fewer than 8 bytes of *unaligned* slack) returned with `cursor < end`, so a
subsequent `reserved_tail()` still handed the collector a sub-8-byte span the
filler had not covered. Every other exit sets `cursor = end`. It now does too,
making "the filler was installed" equivalent to "this TLAB publishes no reserved
tail" on every path — which is what `retire`'s post-condition assertion checks.

**T-5 — size before retire.** `Tlab::next_refill_size` reads
`consumed_bytes() = cursor - start`, and that is the *only* signal that sees the
JIT's inline bump (which never touches `TlabPressureTracker`). `retire()` nulls
both pointers, so sizing after retiring reads `consumed == 0` — indistinguishable
from an idle thread, and exactly the input that revives the one-way shrink
ratchet described in `Tlab::next_refill_size`'s own doc comment (every refill
halves, down to `MIN_TLAB_SIZE`, forever). The single caller
(`interpreter.rs:3154`, then `:3226`) has the order right; nothing enforced it.
A cold `retired_since_refill` flag on the tracker, set by `retire` and cleared by
`begin_refill`, now backs a `debug_assert!`. The flag lives on the *tracker*,
not on `Tlab` directly, so the JIT's `cursor@0` / `end@8` layout contract is
untouched.

### 1.4 Residual gaps (not fixed here)

* **T-6**, above: `VmNativeThreadBlocker::enter_blocked` is the one
  census-exclusion point with no retire. `vm/`-side.
* **T-7**: the reserved-tail backstop runs only inside `stw_take_over_and_wait`,
  which returns early to a plain `wait_for_all()` when
  `!xt::enabled() || !shared.mem.heap.supports_jit_tlab_skip()`
  (`interpreter.rs:598-601`). With cross-thread takeover disabled, the "collect
  every alive thread's tail, not just the frozen peers'" hardening at `:849-857`
  does not run at all, so an un-retired tail is neither skipped nor reported.
  `vm/`-side.
* The single-threaded collection arms never publish skip regions, which is
  correct (the only thread is the initiator, and it retired) but means the
  tripwire in §1.3 T-3 only fires on multi-threaded cycles.
* G1's remembered set is not yet fed into the counters (§2.3).

---

## 2. Card / remembered-set counters

Module: [`gc/src/gc_metrics.rs`](../../gc/src/gc_metrics.rs). Entry point:
`cratonvm_gc::gc_metrics_report()`.

### 2.1 Inventory

| Counter | Where it is recorded | Gated? | What it means |
|---|---|---|---|
| `barrier_ref_stores` | `GenerationalHeap::write_barrier`, after the reference-tag test | **yes** | Reference stores that reached the cross-generation check. The denominator of the barrier hit rate. |
| `card_marks_executed` | `write_barrier`, after `thread_local_dirty_addr` | **yes** | Card marks the *Rust* barrier performed. |
| `cards_found_dirty` | `GenerationalHeap::scan_dirty_cards_inner`, from `take_dirty_cards()` | no | Distinct cards the collector had to scan, summed over every refinement pass. |
| `duplicate_card_marks` | `CardTable::drain_pending` | no | Buffered offsets that landed on a card already dirty (`pending.len() - newly_dirtied`). Pure waste in the buffered path. |
| `remembered_set_bytes` | `CardTable::retained_bytes()`, published per refinement pass (**gauge**) | no | Card byte-map (one byte per 512 bytes of old gen) + dirty-index tracking list + undrained pending queue. |
| `old_to_young_edges` | growth of `extra_roots` across `scan_dirty_cards` | no | Cross-generational reference slots the scan actually found. |
| `refinement_nanos` / `refinement_passes` | wall clock around `scan_dirty_cards` | no | Time spent refining, and how many passes it is spread over. |
| `allocated_objects` / `allocated_bytes` / `live_bytes` | `GenerationalHeap::publish_gc_metrics_occupancy`, at the top of every `collect_garbage_inner` and again at the end-of-run summary (**gauges**) | no | The normalization denominators. |

### 2.2 Hot-path cost, and what is gated and why

`write_barrier` runs on **every reference store in the VM**. An unconditional
`fetch_add(1, Relaxed)` there is a locked read-modify-write on a single
process-global cache line: uncontended that is roughly 5–20 cycles, but with N
mutators storing references concurrently it is a guaranteed cache-line
ping-pong on a line that otherwise has no reason to be shared between cores.
That is a genuine multi-threaded throughput regression paid by every workload in
exchange for a number almost no run wants. So the two barrier counters are
gated on `CRATONVM_GC_CARD_METRICS`, **default off**.

Cost when off: one `Relaxed` load of an already-resolved `AtomicU8` — a plain
`mov` from a line that is read-only-shared and therefore replicated in every
core's cache — plus a perfectly-predicted not-taken branch. Sub-cycle in steady
state, zero coherence traffic. Cost when on: one relaxed increment per
reference store, i.e. the regression described above, which is the point of
opting in.

Everything else is bumped **once per collection or once per drain**, never per
store, so it is unmeasurable against the pause it measures. Those are ungated
deliberately: gating them would make the default report empty, and "the metric
existed but nobody set the flag" is the exact failure the sibling
`moving_young_fallbacks` flag already produced once
(`gc_quiescence::record_moving_young_coverage_fallback`'s doc comment).

**What the barrier counter cannot see.** The JIT emits its own inline post-write
barrier (`GenerationalHeap::jit_card_table_info` → a direct release byte-store
into the card bitmap) which never enters `write_barrier`. So
`card_marks_executed` is *interpreter and native marks only*, and
`barrier_hit_rate` describes those paths. The collector-side counters are
complete regardless: every card, however it was dirtied, funnels through
`take_dirty_cards`.

### 2.3 Reading the report

`print_gc_summary` (`--verbose:gc` / `CRATONVM_GC_STATS`) emits four lines:

```
[GC] cards: dirty_scanned=… duplicate_marks=… old_to_young_edges=… rset_bytes=… refinement_ms=… passes=…
[GC] cards: barrier counters NOT ARMED (set CRATONVM_GC_CARD_METRICS=1) — …
[GC] cards/alloc: dirty_cards_per_obj=… edges_per_obj=… card_marks_per_obj=… (allocated_objects=…)
[GC] cards/live: rset_bytes_per_live_byte=… refine_ns_per_live_byte=… edges_per_live_byte=… edge_density=… duplicate_ratio=… (live_bytes=…)
```

* **`edge_density`** = old→young edges per dirty card scanned. This is the number
  that says whether the card table is doing useful work. Near zero means the
  scan is walking cards that hold no cross-generational reference at all —
  either the 512-byte granularity is too coarse for the workload's mutation
  pattern, or cards are staying dirty across cycles.
* **`duplicate_ratio`** = duplicates / (duplicates + distinct cards). High values
  argue for a per-thread last-card filter in front of the buffered path: the
  mutator is repeatedly re-buffering a card that is already dirty.
* **`rset_bytes_per_live_byte`** is the card table's space overhead, measured
  rather than assumed. The floor is 1/512 of the old generation; anything much
  above that is the pending queue or the dirty index.
* **`dirty_cards_per_obj`** distinguishes "the card table tracks this workload's
  mutation rate" from "it tracks its allocation rate". The latter means the
  barrier is firing on freshly-allocated old-gen objects' initializing stores.
* Every ratio is `0.0` when its denominator is zero. A report taken before the
  first collection reads as "no cost observed", never `NaN` — a `NaN` in a
  summary line is indistinguishable from a parse bug for whoever reads the log.
* When the barrier counters are not armed the report says **NOT ARMED** rather
  than printing a measured-looking zero. `card_marks_executed = 0` under a
  disarmed gate means *unmeasured*, not *no marks*.

**Not yet covered.** G1's `RememberedSet` (`gc/src/region.rs`) has a
`source_count()` but is not fed into `remembered_set_bytes`; the gauge currently
describes the generational card table only. The hook to call is
`gc_metrics::record_remembered_set_bytes` from G1's per-cycle path.

---

## 3. The collector-decision report, and the documentation drift

### 3.1 The two claims

| Source | Claim |
|---|---|
| `docs/GC.md:14` (backend table) | "Young collections run **non-moving** whenever any JIT frame is active." |
| `docs/GC.md:157-159` (Generational body) | "while any thread holds a JIT frame the young collection is a non-moving sweep with selective promotion (this is the load-bearing reason conservative JIT roots are safe here)" |
| `docs/GC.md:191-200` (same section, 30 lines later) | "…it is now the **default** … every cycle must carry its own root-coverage proof, and one that cannot prove complete coverage diverts to the non-moving sweep" |
| `ARCHITECTURE.md:255-286` | `moving_young` defaults **true**; requesting a moving cycle is not running one; each cycle carries a per-cycle coverage proof; diversions are counted and logged with the specific unproven obligation. |

`docs/GC.md` contradicts *itself* between its backend table and its own
Generational section.

### 3.2 What the code says

The decision is one expression, `GenerationalHeap::collect_garbage_inner`
(`gc/src/gen_heap.rs`, around the `divert_non_moving` binding):

```rust
let has_conservative_roots = gc_quiescence::is_active()
    || gc_quiescence::unregistered_jit_frame_on_stack();
let moving_young_requested   = gc_quiescence::moving_young_enabled();
let coverage_incomplete      = gc_quiescence::moving_young_coverage_incomplete()
    || gc_quiescence::unregistered_jit_frame_on_stack();
let divert_for_incomplete_moving_coverage =
    moving_young_requested && (force_non_moving_jit_roots || coverage_incomplete);
let moving_young = moving_young_requested && !divert_for_incomplete_moving_coverage;
let divert_non_moving = (has_conservative_roots && !moving_young)
    || honor_promotion_oom_risk
    || divert_for_incomplete_moving_coverage
    || explicit_full_gc;
```

Three facts settle it:

1. **`moving_young` defaults to `true`.** `types/src/flags.rs:495`
   `DEFAULT_MOVING_YOUNG = true`, shaped as an opt-**out**
   (`CRATONVM_NO_MOVING_YOUNG` turns it off; `CRATONVM_MOVING_YOUNG` is a
   retained no-op opt-in), pinned by
   `moving_young_is_an_opt_out_with_a_compatibility_opt_in`
   (`flags.rs:2418`).
2. **The `is_active()` blanket is gone.** The term
   `fail_closed_non_moving = is_active() && !allow_moving_young`, which is
   *precisely* the `docs/GC.md` claim, was deleted along with its flag — see the
   comment block at `gen_heap.rs:4030-4046`. What remains is
   `has_conservative_roots && !moving_young`, which reduces to
   "a live JIT frame forces the sweep" **only when moving-young is off**.
3. **The remaining process-wide blanket is opt-in.**
   `conservative_roots::refresh_moving_young_coverage_for_current_thread`
   (`vm/src/jit/conservative_roots.rs:1789`) still has a branch where the mere
   existence of compiled code forces the sweep — but it is now gated on
   `moving_young_no_jit()` (`CRATONVM_MOVING_YOUNG_NO_JIT=1`), and the comment
   above it records why: with the blanket armed, "66 of 66 young cycles fell
   back, all of them attributed to this branch and none to a real obligation".

**Verdict: `ARCHITECTURE.md` is correct and `docs/GC.md:14` / `:157-159` are
stale.** They describe the pre-2026-07-26 behaviour, which today is reachable
only under `CRATONVM_NO_MOVING_YOUNG` or `CRATONVM_MOVING_YOUNG_NO_JIT=1`. The
correct statement is:

> A live JIT frame does not by itself force the non-moving sweep. Moving-young
> is on by default; each cycle diverts to the sweep only if its per-cycle
> coverage proof fails, if `promotion_oom_risk` is honoured, or if
> `System.gc()` requested a full cycle. A JIT-warm workload may legitimately
> spend most cycles non-moving — but that is a *measured* fallback rate, not a
> rule, and the reason is recorded.

Neither `docs/GC.md` nor `ARCHITECTURE.md` is edited by this change (both are
outside its ownership). Both should be reconciled against this section; the
concrete edit is to replace `docs/GC.md:14`'s clause and the `:157-159`
sentence with the quoted paragraph above, or with a pointer here.

### 3.3 Why prose could not settle it, and what replaces it

The disagreement is not resolvable by reading, because both claims are true
under some configuration and the effective answer depends on a three-layer
interlock that no single document owns: the **codegen** decides whether a
rewritable shadow map is emitted at all (`jit/src/x64/licm.rs:848`), the **root
gatherer** ANDs that with the typed config and *publishes* the result
(`vm/src/jit/conservative_roots.rs:427-441` →
`gc_quiescence::publish_moving_young_enabled`), and only then does the
**collector** read it. A document can describe any one layer correctly and still
mislead about the outcome.

So the outcome is now recorded where it is decided.
`gc_metrics::record_collector_decision` is called from both arms of the branch
above, and `gc_metrics::collector_decision_report()` renders the last one:

```
[GC] decision #37: backend=generational young=NON-MOVING reason=nonmoving-coverage-incomplete unproven_obligation=compiled-frame-oop-not-published (moving_young_requested=true jit_active=true unregistered_jit_frame=false)
[GC] decision history: moving_cycles_under_live_jit=12 coverage_fallbacks=25
```

`print_gc_summary` emits it on every run that asks for GC stats, immediately
above the card-cost lines.

The reasons are stable codes with labels (`gc_metrics::decision_reason`), and
the moving/non-moving verdict is derived from the reason rather than stored
alongside it, so the two can never disagree:

| Reason | Meaning |
|---|---|
| `moving-no-jit-frames-live` | Nothing compiled was on any stack. Relocation is trivially safe — **this is not evidence that the coverage proof works.** |
| `moving-jit-coverage-proven` | A Cheney cycle ran *while a JIT frame was live*, because every live compiled frame certified a complete rewritable root map. This is the case `ARCHITECTURE.md` describes and `docs/GC.md` denies. |
| `moving-forced-by-debug-flag` | `CRATONVM_DBG_FORCE_MOVING` overrode a diversion. Evidence of nothing. |
| `nonmoving-conservative-jit-roots` | Moving-young is switched off and a live JIT frame's roots are unrewritable. **This is the `docs/GC.md` claim** — seeing it means the process is running with moving-young disabled. |
| `nonmoving-coverage-incomplete` | Moving-young was requested and this cycle's proof failed. Carries the `gc_quiescence::incomplete_reason` code naming the root source that reported incomplete coverage — `xt-takeover-conservative-scan`, `compiled-frame-oop-not-published`, `unregistered-jit-frame-on-stack`, `jit-relocation-contract-unproven`, and the rest. |
| `nonmoving-promotion-oom-risk` | Both generations near full; the moving path could `abort()` on promotion failure. |
| `nonmoving-explicit-full-gc` | `System.gc()` asked for an old-gen-inclusive cycle, routed through the non-moving marker. |

The non-moving arms are tested in the order the terms appear in
`divert_non_moving`, so the reported reason is the *first* one that forced the
diversion — the one an operator has to fix to get a moving cycle back.

Together with the pre-existing `moving_young: cycles=N coverage_fallbacks=M`
line and its per-reason histogram, this makes "is the young generation actually
a copying collector in this process, and if not, what is stopping it?" a
question answered by the runtime rather than by a document.

---

## 4. Tests

| Test | File | What it pins |
|---|---|---|
| `retire_is_idempotent_and_publishes_no_tail` | `gc/src/tlab.rs` | Three consecutive retires with no refill leave identical observable state — the real transition graph does this. |
| `retire_on_an_already_empty_tlab_is_a_noop` | `gc/src/tlab.rs` | Every abrupt-transition path can reach an already-retired TLAB. |
| `abrupt_transition_at_any_cursor_leaves_no_unretired_tail` | `gc/src/tlab.rs` | Sweeps the cursor across a whole TLAB in 8-byte steps; after `retire` there is no reserved tail at any of them, and the tail is covered exactly by an `int[]` filler or the gap sentinel. |
| `reserved_tail_rejects_a_misaligned_cursor_in_debug_builds` | `gc/src/tlab.rs` | T-2's tripwire fires. |
| `reserved_tail_is_exactly_the_unallocated_span` | `gc/src/tlab.rs` | The published span is neither short (walks into raw memory) nor long (hides live objects). |
| `install_tail_filler_always_consumes_the_tlab` | `gc/src/tlab.rs` | T-4: the filler is total on every exit. |
| `sizing_a_retired_tlab_trips_the_ordering_assertion` / `sizing_before_retiring_is_accepted` | `gc/src/tlab.rs` | T-5 both ways. |
| `jit_tlab_skip_offsets_coalesces_overlaps_and_duplicates` | `gc/src/gen_heap.rs` | T-1: duplicates and partial overlaps arrive as disjoint ascending spans. |
| `jit_tlab_skip_offsets_drops_regions_outside_the_walked_window` | `gc/src/gen_heap.rs` | Out-of-window / empty / misaligned regions are dropped, not clamped. |
| `drain_pending_attributes_duplicate_card_marks` | `gc/src/card_table.rs` | Counter arithmetic in the real path: duplicates = offsets consumed − clean→dirty transitions, and a re-dirty after a consume is not a duplicate. |
| `retained_bytes_accounts_for_the_byte_map_and_the_queues` | `gc/src/card_table.rs` | The rset-bytes gauge is the byte-map plus both queues. |
| `normalization_is_a_pure_function_of_the_raw_counters` | `gc/src/gc_metrics.rs` | Every normalized ratio, by hand. |
| `every_ratio_is_zero_when_its_denominator_is_zero` | `gc/src/gc_metrics.rs` | No `NaN` reaches a log line. |
| `collector_side_counters_accumulate_without_the_hot_path_gate` | `gc/src/gc_metrics.rs` | The default report is not empty, and it says "NOT ARMED" rather than reporting an unmeasured zero. |
| `barrier_counters_record_only_while_armed` | `gc/src/gc_metrics.rs` | The gate genuinely keeps the increment off the barrier. |
| `every_decision_reason_has_a_label_and_a_moving_verdict` | `gc/src/gc_metrics.rs` | A new reason cannot be added without a label, and `is_moving` cannot drift from it. |
| `decision_report_names_the_fallback_reason` | `gc/src/gc_metrics.rs` | The report names the *root source* that reported incomplete coverage, not merely that coverage was incomplete. |
| `decision_report_distinguishes_a_proven_moving_cycle` | `gc/src/gc_metrics.rs` | A proven moving cycle does not print a fallback obligation. |

The `gc_metrics` counters and decision slot are process-global in production and
thread-local under `cfg(test)`, mirroring `gc_quiescence`: the gc unit tests run
in parallel threads of one process, and a global tally would let one test's
deliberate increments break another's arithmetic assertion.

---

## 5. Reconciliation list

1. `docs/GC.md:14` and `docs/GC.md:157-159` state the pre-2026-07-26 rule as
   current. Replace with §3.2's paragraph or a pointer here. (Outside this
   change's ownership.)
2. `ARCHITECTURE.md:255-286` is correct and should gain a pointer to
   `gc_metrics::collector_decision_report()` as the runtime check.
3. **T-6** — `VmNativeThreadBlocker::enter_blocked` (`vm/src/vm/vm_exec.rs:4615`)
   should retire the TLAB like every other census-exclusion point, or document
   why the threads reaching it provably hold none.
4. **T-7** — the reserved-tail backstop should not be conditional on
   cross-thread takeover being enabled (`vm/src/runtime/interpreter.rs:598`).
5. G1's remembered set should be fed into `gc_metrics::record_remembered_set_bytes`
   so `rset_bytes_per_live_byte` means something under `-XX:+UseG1GC`.
6. If the §1.3 T-3 tripwire ever fires, the transition table in §1.2 has a row
   that is wrong, and the warn line's occurrence count is the reproducer budget.
