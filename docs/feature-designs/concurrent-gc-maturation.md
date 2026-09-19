# Concurrent garbage collection

**Status:** Partial — concurrent old-gen marking ships and runs by default;
G1 is opt-in and concurrent; ZGC is neither concurrent nor generational.

## What is built

Three collector backends exist (`GcBackend` in `gc/src/vm_heap.rs`,
`GcAlgorithm` in `vm/src/config.rs`). **Generational is the default**; the
others are selected at startup and are not tried automatically.

| Backend | How it is selected | Concurrency it actually has |
|---|---|---|
| Generational | default | STW initial mark → concurrent mark → STW remark → concurrent sweep, for the **old gen**. No flag; triggered by `old_gen_needs_gc()`. The young gen is a Cheney copy under STW. |
| G1 | `-XX:+UseG1GC` | A real background concurrent-mark thread (`ConcurrentMarkController::spawn`, `gc/src/g1_concurrent.rs`), IHOP-triggered. |
| ZGC | `--features zgc` build **and** `-XX:+UseZGC` | None. `ZgcRealHeap` is a stop-the-world, non-moving mark-sweep. |

Concurrency on the default collector is not gated by any environment variable.
There is no `CRATONVM_*CONCURRENT*` or `CRATONVM_SATB*` flag; the phases are
unconditional.

- **The default cycle** is driven from `maybe_concurrent_gc`
  (`vm/src/runtime/interpreter/gc_and_alloc.rs`) over `ConcurrentMarker`
  (`gc/src/concurrent_mark.rs`).
- **SATB is wired by default.** `enable_concurrent_gc(satb, state)` runs at VM
  init (`vm/src/vm/vm_init.rs`) and `GenerationalHeap::satb_barrier`
  (`gc/src/gen_heap.rs`) gates on `is_marking_active()`.
- Other `-XX:+Use*GC` selectors (Serial/Parallel/Shenandoah/Epsilon) warn and
  fall back to Generational rather than failing (`parse_gc_algorithm`,
  `vm/src/config.rs`).

## What is not built yet

- **The default collector has no marker thread.** The "concurrent" mark and
  sweep phases run inline on the mutator that initiated the cycle, holding
  `old_gen_lock()`. Other mutators keep running, so the pause is short, but the
  initiating thread pays the whole phase.
- ~~**ZGC is not concurrent.**~~ **Built 2026-08-16, opt-in.** A
  `ZMarkCoordinator` over `Arc<ZgcRealHeap>` traces the strong closure while
  every mutator runs; the cycle opens at a brief STW and closes inside the next
  collection's pause. `CRATONVM_ZGC_CONC_START=60` opts in; the default is `0`
  because it trades throughput for pause and the measurement says the trade is
  not one to make for everybody. See
  [`zgc-concurrent-and-generational-plan-20260813.md`](zgc-concurrent-and-generational-plan-20260813.md)
  §2, whose C1 records why the mark-end safepoint is taken by a **mutator**
  rather than by `zgc_concurrent.rs`'s driver thread — no background thread in
  this VM can take a safepoint, and G1 answers it the same way.
- **G1 parallel evacuation has no gauntlet-scale soak.** It is on by default since 2026-08-13 on
  the strength of unit coverage and a differential checksum probe; the large-heap soak and the
  throughput measurement that would justify the flip on performance grounds have not been run.
- **G1 is not the default** and is not proposed as one here. Flipping it would
  change behaviour for every application and every test that assumes
  Generational, so it must be its own change with a full suite re-run.

## 1. Problem & motivation

CratonVM ships a single production collector: the **generational** semi-space young gen +
old-gen mark-sweep (`GcBackend::Generational`, the default). The project's headline goal is the
"app gauntlet" — WildFly, Elasticsearch, Kafka, Spring, Quarkus and friends must boot and pass
e2e on real Java bytecode. Those are **long-lived server workloads** with multi-GB live sets,
where HotSpot's default is G1 and the operationally relevant metrics are *pause time* and
*throughput under sustained allocation*, not just correctness on a short program.

Two collectors beyond Generational now exist in-tree with different maturity profiles:

- **G1** (`gc/src/g1.rs`, 5362 LOC) is substantially built and selectable: region-based heap,
  young/mixed/full collection drivers, SATB concurrent marking with a real background worker
  thread (`g1_concurrent.rs`), IHOP triggering, humongous allocation, remembered sets, region
  pinning, and string dedup config. It is wired into `VmHeap`, VM safepoints, and
  `-XX:+UseG1GC`; remaining work is gauntlet validation and pause/throughput hardening.
- **ZGC** (`gc/src/zgc.rs`) is split into an honest **simulation** (`ZgcCollector`/`ZgcHeap`,
  metadata-only, no backing storage, emits a one-time `warn_zgc_simulation_selected()`) and a
  real STW mark-sweep (`ZgcRealHeap`). `ZgcRealHeap` is now wired as `GcBackend::Zgc` /
  `VmHeap::Zgc` and is selectable with `-XX:+UseZGC`. It is deliberately non-moving and
  stop-the-world; the low-latency colored-pointer/load-barrier implementation remains future work.

The command-line/backend truth now matches the code; the remaining gap is validation depth and how
honestly each collector is described.

**This design picks G1 as the maturation target** and lays out the path to make
`-XX:+UseG1GC` a real, gauntlet-validated, selectable collector with HotSpot-comparable pause and
throughput behaviour on server apps.

### Why G1 over production low-latency ZGC first

- G1 remains the gauntlet-maturation target because it is HotSpot's default and is already the
  collector most app-server workloads expect operationally.
- ZGC-real is now selectable, which closes the backend reachability gap, but it is intentionally a
  stop-the-world non-moving collector. Maturing production ZGC still means real colored pointers,
  an atomic load barrier, and concurrent relocation/compaction.
- G1's barriers (SATB pre-barrier + RSet post-barrier) already exist and are exercised; the work
  is hardening + validation, not green-field.

ZGC-real now reuses the collector-agnostic CLI/config/backend plumbing from this design. Production
low-latency ZGC remains tracked as follow-up work in section 8.

---

## 2. Current state in the codebase (what actually exists)

### 2.1 Backend selection plumbing

- `vm/src/config.rs` - `enum GcAlgorithm { Generational, G1, Zgc }` when the `zgc`
  feature is enabled. `parse_gc_algorithm` accepts `g1`, `generational`, `z`, and `zgc`.
- `vm/src/vm/vm_init.rs` - `GcAlgorithm` maps to `GcBackend::{Generational,G1,Zgc}`;
  `PrintFlagsFinal` reports `UseGenerationalGC`, `UseG1GC`, or `UseZGC` truthfully.
- `gc/src/vm_heap.rs` - `GcBackend::Zgc` and `VmHeap::Zgc(ZgcRealHeap)` are wired under
  the `zgc` feature. Core allocation, field/array access, GC, address validation, stats, and heap
  walking dispatch to `ZgcRealHeap`; G1/generational-only helpers return neutral no-op values on
  ZGC.

### 2.2 CLI selection status

`-XX:+UseG1GC` and `-XX:+UseZGC` now survive the launcher normalization pipeline and land in
`Args::gc_selector`; config-apply validates them through `parse_gc_algorithm` and selects the
matching backend. Unsupported `Use*GC` selectors still warn and fall back to Generational.

### 2.3 G1 collector internals (`gc/src/g1.rs`)

- `G1CollectorConfig` (line 43): `heap_size`, `region_size` (default 1 MB), `max_gc_pause_ms`
  (200), `ihop_percent` (raised to **70**, comment T19.3.G1, because 45% on a 256 MB default heap
  fires too eagerly), `promotion_age` (15), `gc_worker_threads` (**4, but unused** — evacuation is
  single-threaded), `string_dedup_enabled` (false), mixed-GC targets.
- Collection drivers: `young_collection` (line 617), `mixed_collection` (line 907), `full` path,
  Cheney-style evacuation (`evacuate_object` line 1113, `scan_and_evacuate_refs` line 1225).
  Remembered-set source scanning (`scan_source_region_for_cset_refs`) closes a documented UAF.
- **Single-threaded STW evacuation under one big lock.** `young_collection`/`mixed_collection`
  hold `self.regions.lock()` for the entire pause. The `scan_and_evacuate_refs` doc comment
  (lines 1204-1224) is explicit that the `pointer_map` dedup is a **TOCTOU the moment evacuation
  goes parallel** — `gc_worker_threads` is config-only scaffolding for a future parallel evacuator
  and would need `pointer_map` → `DashMap`/sharding first.
- Concurrent mark worklist (`mark_worklist`, line 296) with an overflow cap
  (`MARK_WORKLIST_CAP = 1<<20`, line 223) that degrades to a conservative full re-walk
  (`mark_worklist_overflowed`, line 305) rather than panicking.
- RSet write-barrier fast path is **epoch-gated** (`rset_cache_epoch`, line 327; SECURITY FIX V7a)
  so a cached `*const G1Region` cannot write into a recycled region.
- `region_lookup` (line 343): sorted `(base_addr, region_idx)` table, O(log R) `region_for_ptr`,
  built once and never mutated (region `Vec` never reallocates).
- Humongous objects use a per-region `HEADER_SIZE` prefix + `HumongousFiller` sentinel so walkers
  stay correct (lines 467-581). **Humongous garbage is only reclaimed on full GC** (young-time
  humongous reclaim is a documented TODO, lines 742-776).

### 2.4 Concurrent mark controller (`gc/src/g1_concurrent.rs`)

- `ConcurrentMarkController::spawn` launches a background worker
  (`WORKER_STEP_BUDGET = 256` gray ptrs/step, `WORKER_POLL_MS = 5`) that drains the gray queue
  while mutators run; SATB keeps it refilled. Lifecycle: STW initial mark → concurrent mark →
  STW remark → cleanup → Idle.
- Wired into `VmHeap` via `G1State` (`vm_heap.rs:30`): `g1_start_concurrent_mark`,
  `g1_concurrent_mark_finished`, `g1_signal_marking_complete` (lines 959-1045), with
  re-entry/double-start/no-op edge cases handled and unit-tested
  (`concurrent_mark_controller_tests`).

### 2.5 Interpreter integration (`vm/src/runtime/interpreter.rs`)

- `maybe_concurrent_gc` (line 1877): for G1, calls `g1_concurrent_mark_cycle` when
  `g1_should_start_marking()` (IHOP crossed) and not already marking.
- `g1_concurrent_mark_cycle` (line 1966): Phase 1 initial mark under `brief_stw`
  (flushes initiator SATB, `g1_start_concurrent_mark`, `g1_mark_roots`); Phase 2 the spawned
  worker runs concurrently; Phases 3+4 a `G1-MarkComplete` watcher thread polls
  `g1_concurrent_mark_finished` then calls `g1_signal_marking_complete`.
- This means G1's **concurrent marking path is genuinely exercised** when G1 is the backend — it
  is not dead code. What is missing is reachability + validation, not wiring.

### 2.6 SATB (`gc/src/satb.rs`) and the known correctness contract

- `SatbQueue::activate` / `is_active` / `deactivate_and_drain` (lines 274, 296, 329). Per-thread
  `THREAD_SATB_BUFFER` flushes to the global sharded queue at ~256 entries or on explicit
  `flush_thread_satb_buffer`.
- **Full-review finding #18 (HIGH, `gc-collectors`)**: `deactivate_and_drain`/`drain` only drain
  the *global* shards; they have **no visibility into other mutator threads' thread-local
  buffers**. SATB completeness depends on an *external, unenforced* safepoint contract that every
  parked mutator flushes its own buffer before remark. `g1_concurrent.rs:392-397` delegates this
  to "the safepoint protocol" with no in-module enforcement. If unmet: up to 255 live old refs per
  thread go unmarked → swept while reachable → **use-after-free**.
  - Today the VM does flush at safepoint arrival (`safepoint_check` calls `flush_thread_satb`, and
    the initiator flushes in `g1_concurrent_mark_cycle`), but this is **not verified** and not
    covered by a multi-mutator test. Maturing G1 must close this.

### 2.7 ZGC

- `gc/src/zgc.rs` module docs remain explicit that `ColoredPointer`/`LoadBarrier`/`ZPage`/
  `ZgcCollector`/`GenerationalZgc` are simulations: synthetic addresses, no object backing storage,
  no byte-copy relocation, and no production load barrier.
- `ZgcRealHeap` is the selectable backend behind `-XX:+UseZGC`. It is memory-backed and runs a
  real stop-the-world mark-sweep with shared reference processing. It does not compact and does
  not provide ZGC's low-latency concurrent relocation guarantees.
- `zgc_concurrent.rs` remains scaffold for future convergence once a production colored-pointer
  implementation exists.

### 2.8 Other full-review GC findings relevant to maturation

- Non-atomic 16-byte `Value` reads during concurrent marking are formally a data race on slots a
  mutator may write (`concurrent_mark.rs:667`; `g1.rs:1289,1858,3542`). Mitigated by bounds checks
  + coarse locking in G1, but must be made an atomic 8-byte reference-word load (or asserted
  STW-only) before G1 is a credible concurrent default.
- G1 `concurrent_mark_step` holds the global `regions` lock per step (`g1.rs:1654`) — a
  **scalability ceiling** that serializes mutators against the marker.
- `CompactHeader` forwarding-pointer truncates to 8 GB (latent; compact headers unwired). Not on
  the G1 path today but blocks any future "G1 + compact headers".

---

## 3. Proposed design

Goal: **`-XX:+UseG1GC` selects a validated G1 backend** whose pause and throughput on the server
gauntlet are within an agreed band of HotSpot, with no GC-correctness regressions, while
`Generational` remains the default and safety net.

### 3.1 Reachability — make the flag real

1. Parse `-XX:+UseG1GC` / `-XX:-UseG1GC` (and the explicit `-XX:+UseSerialGC`-style mutually
   exclusive set, even if only G1/Generational are honoured) in the launcher argument handler,
   setting `config.gc_algorithm`. Unknown/unsupported `-XX:+Use*GC` flags warn-and-fall-back to
   Generational (HotSpot errors; we choose lenient-with-warning to keep the gauntlet booting).
2. Keep `Generational` the default. `-XX:+UseG1GC` is **opt-in** during maturation; flipping the
   default is a separate, late step gated on green gauntlet (§4 / §5).
3. Surface the active collector truthfully in `-XX:+PrintFlagsFinal`-style output and JMX
   `GarbageCollectorMXBean` names (G1 reports "G1 Young Generation" / "G1 Old Generation").

### 3.2 Barrier & safepoint correctness (the part that must be bullet-proof)

This is where a concurrent collector earns trust. Concrete work:

1. **Enforce the SATB drain contract (finding #18).** Add a process-global registry of per-thread
   `SatbBuffer`s so the collector can drain *all* mutator buffers during the remark safepoint,
   instead of trusting each thread to self-flush. Add a `debug_assert!` at remark that fires if any
   registered buffer is non-empty after the safepoint drain. Add a multi-mutator integration test
   that allocates + overwrites references on N threads across a mark cycle and asserts no live ref
   is lost.
2. **Atomic reference-slot reads during concurrent mark.** Replace the 16-byte `Value` reads on
   concurrently-mutable slots with an 8-byte atomic load of the reference word (the only field the
   marker needs), or prove + assert the read site is STW-only. Align G1 and the standalone
   `ConcurrentMarker` on one barrier discipline.
3. **Post-write (RSet) barrier coverage audit.** Confirm every interpreter and JIT reference store
   that can create an old→young or cross-region edge routes through the RSet post-barrier. The JIT
   path (`jit_putfield_object`/`jit_aastore`) already does SATB-pre + card post per the
   full-review jit findings; verify the G1 RSet variant is the one being fed and that the
   epoch-gated fast path (`rset_cache_epoch`) is invalidated on every region recycle (it is, at
   `young_collection`/`mixed_collection`/`cleanup`).
4. **Root completeness vs the moving evacuator.** G1 young/mixed evacuation *moves* objects. The
   full-review flags multiple side-tables historically missed by the moving collector
   (OscCache, IoFuture refs, JNI-critical pinned arrays, flat-API handles). Maturing G1 must run
   the existing `update_all_roots` remap set against G1's evacuation pointer map and add a
   moving-GC stress test that exercises each side-table. (Most are already handled for the
   generational moving young gen; the task is to confirm parity under G1.)
5. **Region pinning under JNI critical sections.** `G1Region.pinned` + the JEP-423 pin path must be
   driven by `GetPrimitiveArrayCritical`/`GetStringCritical` so a held native pointer's region is
   excluded from the CSet. The full-review jni finding notes the direct (no-copy) critical path
   does not currently pin; G1 makes this exploitable, so wire pinning (or force-copy under G1).

### 3.3 Pause-time engineering

- **IHOP / adaptive marking.** Keep the adaptive IHOP (`update_ihop`) so marking starts early
  enough to finish before old-gen exhaustion (avoiding the to-space-exhaustion full GC), tuned for
  the gauntlet's larger heaps (the 70% default was chosen for the 256 MB *test* default; server
  runs use `-Xmx` in the GB range where 45% is closer to HotSpot).
- **Pause-target-driven CSet sizing.** `max_gc_pause_ms` (200) currently does not bound CSet size.
  Use the rolling per-region copy cost to cap how many old regions enter a mixed CSet so a pause
  stays near target — the `select_old_regions_for_mixed_gc` worst-first selection (line 882) is the
  hook; add a time-budget cap on top of the percentage cap.
- **Concurrent-mark lock granularity.** The per-step global `regions` lock (scalability ceiling) is
  the throughput risk. Phase the fix: first measure its impact on the gauntlet; if it dominates,
  move to per-region locking or a lock-free gray stack. Do not pre-optimize.

### 3.4 Parallel evacuation (throughput, later phase)

`gc_worker_threads` (default 4) exists but evacuation is single-threaded. Parallelizing is **out of
scope for initial selectability** but is the main throughput lever for large heaps. Sequenced as a
distinct phase so the collector is *correct and selectable* before it is *fast*, gated behind
`CRATONVM_G1_PARALLEL_EVAC` (default off) and differential-validated before any default flip.

**Sequencing caveat (hard):** parallel evacuation must NOT land — even gated — on a base with an
open moving-GC memory-safety bug. The single-threaded evacuator must first be proven memory-safe
across the gauntlet; the **gpu-bench-cpu G1 SIGSEGV** (Step 8 finding, `task_b53503fd`) was named
here as the current blocker.

**Status of that blocker, 2026-08-13 — NOT reproduced, and NOT confirmed fixed.** Two things are
now true and neither is "it is gone". First, the internal record tree's `README.md` records `gpu-bench-cpu`
as PASS/PASS in three separate suite runs, which is inconsistent with the line above being current.
Second, an attempt to reproduce it directly failed for a different reason: `GpuDotBench` under
`-XX:+UseG1GC -Xmx512m` panics in `jit_thread_mut: aliasing &mut JvmThread borrow detected`
(`vm/src/jit/helpers.rs`) — and it panics identically under the DEFAULT generational collector, so
whatever that is, it is not a moving-GC memory-safety bug and not this blocker. The class the
finding names, `CpuOnlyBench`, does not exist in the tree, so the original harness could not be
re-run. Re-establishing this blocker's status needs that harness; until then it should not be cited
as gating anything, and the JIT aliasing panic is its own defect.

**Foundation already landed (Step 9, behaviour-identical):** `evacuate_object` now returns
`(new_ptr, fresh)`, where `fresh` is the dedup signal — `true` iff this call performed the copy.
The ref-scan sites (`scan_and_evacuate_refs`) take their work_list-push decision from `fresh`
instead of a separate `pointer_map.contains_key(...)` pre-check, removing the explicit TOCTOU the
old code documented. In the parallel evacuator `fresh` becomes the outcome of the atomic forwarding
install (below) — the call sites don't change again. The `CRATONVM_G1_PARALLEL_EVAC` opt-out flag
is declared (read-once `OnceLock`, default off) per §7.

**The remaining parallel protocol (the follow-up), four pieces in dependency order:**

1. **Atomic forwarding (the core).** Replace the `pointer_map.get`/`insert` dedup with a CAS on the
   object header's `forwarding_ptr` (already a field): a worker reads `forwarding_ptr`; if non-null
   the object is already evacuated (not fresh) — use it; else it allocates a destination, copies,
   and `CAS(forwarding_ptr, null → new)`. CAS win = this worker owns the copy (`fresh = true`); CAS
   loss = abandon the dest allocation and use the winner's pointer (`fresh = false`). The mark-word
   transfer must re-read after the CAS. CSet objects live in from-space and are not mutated by
   mutators during STW, so only worker-vs-worker races exist.
2. **Concurrent `pointer_map`.** Still needed to remap roots / refs in non-CSet regions. Convert
   `HashMap<usize,usize>` → a `DashMap` (add the dep) or per-worker shards merged at end-of-pause;
   inserts become idempotent `entry().or_insert(new)` keyed by the winning forward.
3. **Per-worker allocation.** Today `young/mixed_collection` hold `self.regions.lock()` for the
   *entire* pause and bump-allocate via `alloc_in_type_locked`. N workers can't share that. Give
   each worker a GC-TLAB: a short critical section to claim/retire a Survivor/Old region, then
   lock-free bump within it (`cursor` → `AtomicUsize` at the claim/retire boundary). The single big
   lock is replaced by claim/retire + per-worker bump.
4. **Sharded / work-stealing work_list.** Replace the single `Vec<*mut u8>` with per-worker deques
   + work-stealing (Chase-Lev, or a shared `Mutex<Vec>` with per-worker local batches as a first
   cut). Termination = all deques empty + an atomic active-worker count at zero. The RSet-source
   scan (`scan_source_region_for_cset_refs`) and `verify_no_dangling_into_cset` must also gate their
   pushes on `fresh` (today they push unconditionally — fine single-threaded, redundant in
   parallel).

**All four landed, and the flag is now default-ON with a `=0` opt-out (2026-08-13).** The default
flip is a separate decision from the four pieces and rests on G1-9 being root-caused rather than
worked around: it was a compact-layout scan divergence in the parallel evacuator's own object walk,
reproducing identically at one worker, not a race. Piece 2 was never owed a `DashMap` — per-worker
shards merged after the barrier is the other half of the same advice.

**Fifth piece, not in the original list: a persistent worker pool** (`gc/src/evac_pool.rs`,
2026-08-13). The four pieces above left thread creation inside every pause, which is overhead
charged against `max_gc_pause_ms` for threads that are identical from one pause to the next. The
pool creates them once per collector and parks them on a condvar; `EvacPool::scope` keeps the
dispatch/participate/barrier shape so the module SAFETY MODEL note is unchanged. What a persistent
pool cannot inherit from `std::thread::scope` is the *type-level* proof that no worker outlives the
borrowed job, so that argument is written out and turns on one property — `scope` does not return
until every worker has decremented, and a worker decrements after its call returns INCLUDING on an
unwind. A panicking worker that skipped its decrement would hang the driver on the condvar while it
holds the regions lock, with the panic never surfacing; both panic directions are tested.

**Validation (§4 step 9 / §5):** differential — the deterministic benches (e.g. `bintrees18@8g`)
under parallel evacuation must produce **byte-identical checksums** vs serial G1 vs HotSpot, across
worker counts; plus a parallel-evac soak with no leak/corruption. The checksum half is met at the
probe scale used for G1-9 (byte-identical to a real JDK run of the same class, 0/10 corrupt after
the fix, serial arm clean throughout). **The gauntlet-scale soak and the large-heap throughput
number are still owed** — the default flip was taken on correctness evidence, not on a measured
win, and this document should not be read as claiming one.

---

## 4. Incremental delivery plan (small, independently-mergeable, each build-green)

Each step compiles and passes existing tests on its own; no step depends on a later step to be
sound.

1. **CLI flag wiring + warning fallback.** Parse `-XX:+UseG1GC`/`-XX:-UseG1GC` → `gc_algorithm`.
   Unknown `Use*GC` → warn + Generational. Unit-test the parser. (No GC behaviour change; G1 only
   runs if explicitly asked.)
2. **Doc truth-up.** Reconcile `ARCHITECTURE.md` vs `README.md`/`CONTRIBUTING.md`: describe
   Generational as default, G1 as opt-in/experimental-but-real, and ZGC-real as built and
   dispatched but compiled in only behind the `zgc` Cargo feature — which was
   default-off when this was written and has been **default-ON since
   2026-08-10**, because the default `GcAlgorithm` is now the variant it gates.
   (Docs-only; full-review docs-governance row.) This is the only step that may touch files outside
   the design doc, and is pure documentation.

   > *Corrected 2026-08-07.* This step used to end "…ZGC-real as built-but-undispatched",
   > which contradicted this document's own §1 progress notes, §2.1, §2.7 and §7.2.
   > `ZgcRealHeap` **is** dispatched: `GcAlgorithm::Zgc` (`vm/src/config.rs:24`, `:51`) →
   > `GcBackend::Zgc` (`vm/src/vm/vm_init.rs`) → `VmHeap::Zgc` (`gc/src/vm_heap.rs:20`), so
   > `-XX:+UseZGC` really selects it and `PrintFlagsFinal` reports `UseZGC` truthfully. The
   > half that survives is the gate, not the dispatch: the `zgc` feature is **default-off**,
   > so a stock build has no `GcAlgorithm::Zgc` variant and no `parse_gc_algorithm` arm for
   > it, and `-XX:+UseZGC` there warns and falls back to Generational. A ZGC-capable
   > launcher is `cargo build --release -p cratonvm-cli --features zgc` (the `cratonvm-cli`
   > pass-through landed 2026-08-07; before it, only `--features cratonvm-vm/zgc` reached
   > the gated code).
3. **SATB drain enforcement.** Thread-buffer registry + remark-time global drain + debug-assert +
   multi-mutator integration test (closes finding #18). Build-green regardless of which backend
   runs, since the registry is inert unless SATB is active.
4. **Atomic concurrent-mark slot reads.** 8-byte atomic reference-word load on concurrently-mutable
   read sites; assert STW-only elsewhere. Add a loom/TSan-style or targeted concurrency test.
5. **Moving-GC root-parity audit + stress test under G1.** Run `update_all_roots`' full side-table
   set against G1 evacuation; add a heavy-alloc stress test that allocates between storing each
   side-table ref and the next GC. Fix any G1-specific gaps (pin/remap).
6. **JNI critical pinning under G1.** Wire `GetPrimitiveArrayCritical`/`GetStringCritical` to
   `G1Region.pinned` (or force-copy under G1). Test: hold a critical pointer across a forced G1
   young GC, assert the region is excluded and the pointer stays valid.
7. **Pause-target CSet sizing.** Time-budget cap on mixed CSet on top of the percentage cap; verify
   pauses track `max_gc_pause_ms` on a synthetic churn benchmark.
8. **Gauntlet validation pass (opt-in G1).** Run WildFly/ES/Kafka/Spring suites with
   `-XX:+UseG1GC` and record boot + e2e green vs the Generational baseline (§5). File/triage any
   divergence as its own bug.
9. **(Throughput, separate effort) Parallel evacuation.** `pointer_map` → concurrent map +
   per-worker work_list; enable `gc_worker_threads`. Gated behind its own opt-out flag until
   differential-validated.
10. **(Late, gated) Consider G1 as default** only after steps 3–8 are green across the gauntlet for
    a sustained soak, and pause/throughput are within band (§5). Until then Generational stays
    default as the safety net.

---

## 5. Validation / acceptance (how to prove it works)

Acceptance is **differential against HotSpot** plus **no GC-correctness regression**.

- **Correctness — gauntlet boot + e2e.** With `-XX:+UseG1GC`, the gauntlet pool (WildFly, ES,
  Kafka, Spring; per `apps/TARGET_APPS.md`) must boot and pass the same e2e suites that pass under
  Generational. Zero new SIGSEGV / UAF / ThreadLeak relative to the Generational baseline.
- **Correctness — unit/integration.** All existing `g1.rs` / `g1_concurrent.rs` / `satb.rs` /
  `vm_heap.rs` tests stay green; add the new multi-mutator SATB test (step 3), the atomic-read
  concurrency test (step 4), the moving-GC side-table stress test (step 5), and the JNI-critical
  pin test (step 6).
- **Correctness — checksum parity.** Run the deterministic-output benches (e.g. bintrees18:
  HotSpot checksum **68332206** per project memory) under `-XX:+UseG1GC` and confirm byte-identical
  results vs `java -cp bench BenchSuite` and vs CratonVM Generational. A moving collector that
  loses/duplicates objects shows up here.
- **Pause time.** Under `-Xlog:gc`-equivalent (`verbose_gc` / `enable_gc_logging`), record p50/p99
  young + mixed pauses on a sustained-allocation server workload (e.g. ES indexing, Kafka
  produce/consume). Target: p99 pause within an agreed multiple (e.g. ≤ 2×) of HotSpot G1 at the
  same `-Xmx`/`-XX:MaxGCPauseMillis`. We are not claiming to beat HotSpot; we are claiming to be in
  the same regime and to honour the pause target's *direction*.
- **Throughput.** Application-level throughput (req/s, msgs/s) under `-XX:+UseG1GC` must not
  regress more than an agreed margin vs Generational on the gauntlet, and should be reported
  alongside HotSpot G1 as a reference. Parallel evacuation (step 9) is the lever if throughput is
  short.
- **Soak / leak.** A multi-hour soak (the existing `gc/tests/leak_soak.rs` harness scaled up)
  under G1 with no unbounded heap growth and no thread leak across many mark cycles
  (`repeated_cycles_do_not_leak_threads` already guards Arc/thread leakage at unit scale).

Sign-off to move G1 from opt-in to default (step 10) requires: gauntlet green, checksum parity,
pause within band, throughput within margin, soak clean.

---

## 6. Risks & open questions

- **SATB drain race (finding #18) is a real UAF if the safepoint contract is ever violated.** The
  registry-based enforcement (step 3) is the gating safety item; without it, a concurrent default
  is not defensible.
- **Moving-GC side-table parity.** G1 moves objects on every young/mixed GC. The generational
  moving young gen already exercises `update_all_roots`, but the full-review lists side-tables
  (OscCache, IoFuture, JNI-critical, flat-API handles) that are latent under *any* moving
  collector. Need to confirm each is covered under G1 specifically — a missed side-table is a
  stale-pointer SIGSEGV exactly matching the project's GC-safety history.
- **Per-step global lock throughput ceiling.** May make G1 throughput unacceptable on highly
  concurrent gauntlet apps before parallel evacuation lands. Open question: measure first; decide
  whether per-region locking is needed for *selectability* or only for *default*.
- **Pause-target honesty.** `max_gc_pause_ms` is currently advisory. If we cannot keep pauses near
  target on large heaps without parallel evacuation, the honest position is to document G1 as
  "selectable, pause-target best-effort" rather than overclaim.
- **Humongous reclaim only on full GC** (`g1.rs:742`). Humongous-churning workloads grow the heap
  between full GCs. Open: is any gauntlet app humongous-heavy enough to force this fix before
  selectability?
- **Default-flip blast radius.** Flipping the default to G1 changes behaviour for every app and
  every existing test that assumes Generational. Step 10 must be its own change with a full suite
  re-run, not bundled.
- **ZGC scope.** `ZgcRealHeap` is selectable now, but production low-latency ZGC remains a separate, larger project: real colored pointers, an atomic load barrier, concurrent relocation, and compaction. This doc deliberately treats that as future work.

---

## 7. Scaffolding to land first (minimal compiling stubs / flags / knobs)

These are the smallest, build-green pieces that unblock everything else. Described here; landed in
their own steps (§4). None require risky GC-internal surgery.

1. **`-XX:+UseG1GC` / `-XX:-UseG1GC` / `-XX:+UseZGC` argument parsing** -> `VmConfig.gc_algorithm`.
   This is landed: `parse_gc_algorithm(&str)` accepts G1, ZGC, and Generational selectors; unknown
   `Use*GC` flags warn and fall back to Generational.
2. **`GcBackend::Zgc` is now real plumbing, not a placeholder.** The `zgc` feature adds
   `VmHeap::Zgc(ZgcRealHeap)` plus dispatch arms across `vm_heap.rs`, and the CLI selects it with
   `-XX:+UseZGC`. The remaining ZGC work is production low-latency semantics, not backend reachability.
3. **SATB thread-buffer registry type** (inert stub first): a `pub struct SatbBufferRegistry` with
   `register(thread_id)` / `drain_all() -> Vec<usize>` that is a no-op/empty until step 3 populates
   it. Lets the remark path call `drain_all()` unconditionally and compile, with behaviour added
   incrementally.
4. **Config knobs (parse + plumb, default-preserving):**
   - `-XX:MaxGCPauseMillis=<n>` → `G1CollectorConfig.max_gc_pause_ms` (already a field; just parse).
   - `-XX:InitiatingHeapOccupancyPercent=<n>` → `ihop_percent`.
   - `-XX:G1HeapRegionSize=<n>` → `region_size`.
   - `-XX:+UseStringDeduplication` → `string_dedup_enabled`.
   Each maps to an existing `G1CollectorConfig` field — pure plumbing, no new GC logic.
5. **Opt-out env safety net for risky sub-features**, matching the project's "real path default-on,
   opt-out flag is the safety net" convention: e.g. `CRATONVM_G1_PARALLEL_EVAC=0` (when step 9
   lands), `CRATONVM_G1_FORCE_COPY_CRITICAL=1` (step 6 conservative mode). Declared as recognized
   env knobs early (read-once behind `OnceLock`), defaulting to the safe behaviour, so later steps
   can flip the default without re-plumbing.
6. **Validation harness hook:** a small `--gc-stats`/`verbose_gc` JSON-or-line dump of
   per-collection pause + bytes (the data already flows through `GcStats`/`log_gc_event`); the
   gauntlet runner consumes it to produce the pause/throughput tables in §5. No collector change —
   just a structured sink on the existing logging path.

---

## 8. Follow-ups (explicitly out of scope here)

- **Production low-latency ZGC:** `ZgcRealHeap` is selectable and reference-processing aware.
  Remaining work is the real ZGC design: multi-mapped colored pointers, atomic CAS load barrier,
  concurrent relocation/compaction, and validation under app workloads.
- **G1 + compact object headers:** blocked on the `CompactHeader` 8 GB forwarding-pointer
  truncation fix (full-review `gc-heap`/`gc-collectors`).
- **Parallel evacuation throughput** (step 9) graduating to default-on.
- **Generational ZGC (JEP 439)** — far future.
