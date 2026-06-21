# Concurrent GC Maturation: G1 from gated scaffolding to a gauntlet-validated, selectable collector

Status: design + scaffold plan. Target branch: `design-concurrent-gc`.
Author note: this doc is grounded in a read of `gc/src/{g1,g1_concurrent,zgc,zgc_concurrent,concurrent_mark,satb,vm_heap}.rs`,
`vm/src/config.rs`, `vm/src/vm/vm_init.rs`, `vm/src/runtime/interpreter.rs`, and
`docs/internal/reviews/full-review-2026-06-20.md` (findings #18, the `gc-collectors` section, and the docs-governance row).

### Implementation progress

- **Step 1 (CLI flag wiring) — DONE** (branch `feat/g1-cli-flag`). `-XX:+UseG1GC`
  now selects the G1 backend; `-XX:-UseG1GC` reverts to Generational; any other
  `-XX:+Use*GC` (Serial/Parallel/Z/Shenandoah/Epsilon) warns and falls back to
  Generational (lenient-with-warning, §3.1). Generational remains the default —
  G1 is opt-in. Implementation: `parse_gc_algorithm()` in `vm/src/config.rs`;
  `-XX:+Use<name>GC` → `--XX:UseGc <name>` rewrite + `gc_selector` clap field +
  config-apply in `vm-cli/src/main.rs`. Unit-tested (parser + normalize +
  full-pipeline + last-wins). The existing `vm_init.rs` `GcAlgorithm`→`GcBackend`
  map and `-XX:+PrintFlagsFinal` collector string already reflect the selection
  truthfully. Still open within §3.1.3: JMX `GarbageCollectorMXBean` names
  ("G1 Young/Old Generation").
- **Step 2 (doc truth-up) — DONE** (branch `feat/g1-cli-flag`). Reconciled the
  docs-governance gap now that G1 is selectable: `README.md` (feature bullet +
  crate-tree line), `CONTRIBUTING.md` (the `gc` crate row dropped "no G1"), and
  `ARCHITECTURE.md` (Generational = default safety net, G1 = opt-in via
  `-XX:+UseG1GC`, ZGC = simulation + a built-but-undispatched `ZgcRealHeap`).
- **Step 3 (SATB drain enforcement, finding #18) — G1 path DONE** (branch
  `feat/g1-cli-flag`). The per-thread SATB buffer registry + collector-side
  `flush_all_thread_satb_buffers` already existed (and are wired into
  `SatbQueue::deactivate_and_drain`), but G1's `remark` drains with `drain()`,
  not `deactivate_and_drain` — the latter runs only at end-of-cycle `cleanup`,
  which *discards* stragglers. So G1 remark never drained the registry: a
  reference a mutator overwrote since its last ~256-entry spill sat in that
  thread's local buffer, excluded from the remark snapshot → the still-live
  target is swept while reachable (UAF). Fix: `G1Collector::remark` now calls
  `flush_all_thread_satb_buffers(&self.satb_queue)` before the shard `drain()`,
  at the STW safepoint — removing the dependence on every mutator self-flushing
  (the external, unenforced contract finding #18 flagged). Regression test:
  `g1_concurrent::tests::remark_drains_thread_local_satb_buffer` — a ref logged
  into a thread-local buffer (never spilled to a shard) is marked only because
  `remark` drains the registry. The existing
  `satb::tests::{flush_all_thread_satb_buffers_reaches_global_queue,
  deactivate_and_drain_includes_thread_local_buffer}` already cover the drain
  mechanism itself.
  - The design's hot-path `debug_assert!`(registry all-empty after drain) was
    **intentionally omitted**: the registry is process-global and the gc crate
    cannot observe the VM's STW state, so the check races with any concurrent
    SATB user (the parallel test harness itself) and would flake.
  - Test isolation: because `remark` now drains the *process-global* registry,
    a parallel test that holds a non-empty thread-local buffer across a wide
    window can have its entries stolen into a sibling's queue. This is a
    shared-registry test artifact, not a production issue (one heap, true STW);
    the suite is green single-threaded. `satb_captures_mutator_writes_during_
    concurrent_mark` was made robust by spilling its buffer to its own queue
    shards immediately after the barrier (per-queue shards are isolated).
  - Generational `ConcurrentMarker::remark` already drains the registry (its
    remark calls `deactivate_and_drain` → `flush_all`), so it is unaffected.
    Applying the same discipline uniformly is a small follow-up.
- **Step 4 (atomic concurrent-mark slot reads) — DONE via atomic-per-word
  read/write** (branch `feat/g1-atomic-mark-reads`). The design's literal "8-byte
  reference-word load" was infeasible; the implemented fix reads/writes the whole
  16-byte slot as two `AtomicU64` words instead. Findings that shaped it:
  - The *standalone* `ConcurrentMarker::scan_object` (`concurrent_mark.rs`) is
    already mitigated: 8-byte ref-array elements use a single-word `u64` read;
    16-byte object slots read under `collector::volatile_stripe_lock` + SeqCst
    fences. G1's own concurrent scan `scan_object_refs` (`g1.rs`) is **not** —
    it does a plain non-atomic `ptr::read::<Value>` (the §2.8 g1.rs sites).
  - The design's primary fix — *"8-byte atomic load of the reference word"* — is
    **not portable**: `Value` is `repr(Rust)` (line ~22; `repr(C)` would grow it
    to 24 bytes and break JIT slot layout), so the discriminant/payload offsets
    are compiler-private — there is no stable "reference word" to load. And the
    mutator write (`set_field`) is a non-atomic `ptr::write::<Value>`, so even an
    atomic read would be **mixed-atomicity UB** unless every writer (interpreter,
    JIT raw stores, natives) also becomes atomic — a cross-cutting, perf-critical
    change far beyond a marker tweak.
  - The design's alternative — *"prove + assert STW-only"* — fits the **other
    three** g1.rs Value reads (`scan_and_evacuate_refs`,
    `scan_source_region_for_cset_refs`, `verify_no_dangling_into_cset`: all in
    the STW evacuation path) but **not** `scan_object_refs`, which is genuinely
    concurrent.
  - The `concurrent_mark.rs` **stripe-lock** approach is **ruled out for G1**:
    `scan_object_refs` runs holding `self.regions.lock()`, while the volatile
    write path is `set_field_volatile` → stripe → `set_field` → `regions.lock()`.
    Adding a stripe lock under the regions lock inverts the order (regions→stripe
    vs stripe→regions) → **deadlock**.
  - **Key narrowing:** the *interpreter* write path (`set_field`) takes
    `regions.lock()` — the same lock the marker holds for the whole
    `concurrent_mark_step` — so interpreter writes are already serialized against
    the marker read (no race). The **only** genuinely concurrent writer is the
    JIT: `jit_putfield_*` write the 16-byte slot **directly** (bypassing
    `set_field`/the regions lock). So the race is JIT-write ↔ marker-read, and it
    is benign in practice (typed-field tag invariance → no spliced garbage
    pointer) — formal UB, not a reachable memory-safety hole.
  - **Implemented fix (atomic-per-word):** `types::{read_value_atomic,
    write_value_atomic}` read/write a slot as two relaxed `AtomicU64` words —
    copying all 16 bytes, so **no `repr(Rust)` layout assumption**, and
    perf-neutral on x86 (a 16-byte `ptr::write` was already two stores). Applied
    to the marker reads (`g1::scan_object_refs`, `concurrent_mark::scan_object` —
    the latter keeps its stripe lock for volatile-write serialization) and to all
    five `jit_putfield_*` writes plus `jit_putfield_object`'s SATB old-value read.
    The regions lock is left intact (no deadlock; §3.3 says don't pre-optimize it
    — defer to gauntlet measurement). `Relaxed` suffices: marking *correctness* is
    carried by the SATB pre-barrier, not this access's ordering.
  - **Validation:** build + `cratonvm-gc` (709, parallel) + `cratonvm-types` (283)
    green; a JIT field-stress program (`scratch/step4/FieldStress.java`,
    exercising all five putfields + GC churn) yields a **byte-identical checksum
    `4495525842000` across HotSpot, cratonvm-generational, and cratonvm
    `-XX:+UseG1GC`** (the G1 run drives Step 1 flag → G1 marking → atomic
    read/write end-to-end). Note: the 8-byte reference-*array* read/`jit_aastore`
    path is single-word (cannot tear) and left as-is; the three STW evacuation
    Value reads are non-concurrent and unchanged.
- **Step 5 (moving-GC root-parity audit under G1) — AUDIT DONE; 1 of 4 gaps
  fixed, 3 documented** (branch `feat/g1-root-parity`). A 25-agent fan-out audited
  every root/side-table category for G1-evacuation parity (each adversarially
  verified). Core finding: the interpreter GC spine is collector-agnostic
  (`collect_roots` → `collect_garbage` → `update_all_roots` with G1's
  `GcResult.pointer_map`), so the large majority of sources (interpreter frames,
  statics/mirrors/interns, monitors, native_pin_roots, native-root registry incl.
  OscCache, weak/soft/phantom references, XNIO futures, JNI globals) have automatic
  parity. Four genuine gaps surfaced (each verified against the code by hand):
  - **GAP B — non-initiator JIT precise-map remap (HIGH, G1-specific) — FIXED.**
    `apply_pointer_map_to_thread` (the path a thread parked at the STW barrier runs
    on *itself* when it resumes) remapped frames/monitors/shadow-stack/native-pins
    but NOT `remap_active_jit_frames` (the precise JIT oop-map RBP-chain remap the
    initiator does at `gc.rs:73`). With `CRATONVM_PRECISE_JIT_MAPS` on, a
    non-initiator's JIT-frame oops stayed stale after a move → UAF. G1-specific:
    G1 moves unconditionally, while the generational collector falls back to a
    non-moving sweep whenever any thread is in JIT (`gc_quiescence`). Fix: add the
    thread-local `remap_active_jit_frames(pointer_map)` to
    `apply_pointer_map_to_thread` (inert when no precise-map frame is live).
  - **GAP A — smuggled-jobject remap skipped after CSet free (HIGH, G1-specific)
    — DOCUMENTED, fix is non-trivial.** `value_stack::update_object_refs`'s
    *ambiguous Long/Double smuggle arm* gates the rewrite on a POST-GC
    `heap.is_heap_addr(old_ptr)` (value_stack.rs:1365). Under G1 the CSet region is
    reset to `Free` during evacuation, and `is_heap_addr` skips Free regions
    (g1.rs:2828), so a genuinely-moved jobject's old address now reads "not in
    heap" → the rewrite is SKIPPED → stale (UAF; the JNI long-as-jobject path,
    e.g. WildFly jboss-modules). The generational collector is safe because its
    `young_from` is swapped, never freed, so `is_heap_addr(old_ptr)` still returns
    `Some`. NOTE the *object-tagged* arm (value_stack.rs:1304-1325) was already
    fixed for this (it gates on `pointer_map` membership, not `is_heap_addr` — the
    H2 stale-stack crash). The Long/Double arm can't simply drop the guard: it
    disambiguates a real smuggled jobject from a coincidental primitive long whose
    bits collide with a `pointer_map` key (tested by
    `frame.rs::update_local_refs_preserves_collision_long_matching_pointer_map_key`).
    A correct fix keeps that disambiguation while surviving CSet-free — cleanest:
    have G1 keep just-collected CSet address ranges queryable during the remap
    window (a `was_in_collection_set(old_ptr)` accepted alongside `is_heap_addr`),
    mirroring gen's "from-space still resolvable during remap". Needs its own
    focused change + the frame.rs collision tests re-run.
  - **GAP C — Panama/FFM upcall targets never rooted (MEDIUM, affects BOTH
    collectors) — DOCUMENTED.** `native-builtins/src/panama.rs` UPCALL_REGISTRY
    holds an upcall stub's target `ObjectRef` (also leaked into the libffi closure)
    with ZERO `register_native_root_source` calls, so it is neither scanned nor
    remapped — a moving-GC UAF on the next upcall. Not G1-specific (any moving
    collector); manifests under G1 young because objects actually move. Fix:
    register a native-root source (scan+remap) for the upcall registry, like
    `oscache.rs:175`.
  - **GAP D — ec_watch table not remapped on multi-thread GC paths (LOW,
    diagnostic-only) — DOCUMENTED.** `ec_watch::remap` is missing from the two
    multi-threaded initiator paths (`maybe_gc_forced`, `force_gc_with_finalizers`);
    degrades only the debug corruption-watch, no production UAF.
  - The synthesizer also DOWNGRADED a `monitor_on_exit` over-claim: it is reachable
    via local-0 / class-mirror in the normal case, so it is in `pointer_map`; the
    residual (a method overwriting local 0) is pre-existing and not G1-specific.
  - Remaining for Step 5: implement the GAP A G1-lifecycle fix + GAP C registration,
    and a moving-GC stress test (smuggled jobject + multi-thread precise-JIT frame
    relocated under `-XX:+UseG1GC`) asserting no staleness.
- Steps 6–10 — not started. Next highest-value: Step 8 (opt-in G1 gauntlet
  validation).

---

## 1. Problem & motivation

CratonVM ships a single production collector: the **generational** semi-space young gen +
old-gen mark-sweep (`GcBackend::Generational`, the default). The project's headline goal is the
"app gauntlet" — WildFly, Elasticsearch, Kafka, Spring, Quarkus and friends must boot and pass
e2e on real Java bytecode. Those are **long-lived server workloads** with multi-GB live sets,
where HotSpot's default is G1 and the operationally relevant metrics are *pause time* and
*throughput under sustained allocation*, not just correctness on a short program.

Two collectors beyond Generational already exist in-tree but neither is a selectable, validated
backend:

- **G1** (`gc/src/g1.rs`, 5362 LOC) is substantially built: region-based heap, young/mixed/full
  collection drivers, SATB concurrent marking with a real background worker thread
  (`g1_concurrent.rs`), IHOP triggering, humongous allocation, remembered sets, region pinning,
  and string dedup config. It **is** wired into the `VmHeap` dispatch enum and into the
  interpreter safepoint path. But it is **not reachable from the command line** and has not been
  run against the gauntlet.
- **ZGC** (`gc/src/zgc.rs`) is split into an honest **simulation** (`ZgcCollector`/`ZgcHeap`,
  metadata-only, no backing storage, emits a one-time `warn_zgc_simulation_selected()`) and a
  real STW mark-sweep (`ZgcRealHeap`). The simulation cannot hold Java objects; `ZgcRealHeap` is
  real but non-moving, does no reference processing, and — critically — **is not in the
  `GcBackend` dispatch enum at all** (`vm_heap.rs` only has `Generational` and `G1`).

The result is a **maturity/perception gap**: `ARCHITECTURE.md` claims "full G1+ZGC" while
`README.md:236` and `CONTRIBUTING.md:66` say "no G1, experimental zgc stub" (full-review
docs-governance row). Neither is right: G1 is ~80% built but unselectable; ZGC-real is built but
undispatched.

**This design picks G1 as the maturation target** and lays out the path to make
`-XX:+UseG1GC` a real, gauntlet-validated, selectable collector with HotSpot-comparable pause and
throughput behaviour on server apps.

### Why G1 over ZGC first

- G1 is already integrated end-to-end (dispatch + safepoint driver); ZGC-real is not even in the
  enum and the colored-pointer/load-barrier machinery that makes ZGC *low-latency* is an explicit
  simulation (`zgc.rs` module docs). Maturing ZGC means building real multi-mapped colored-pointer
  address space + an atomic CAS load barrier — a much larger, riskier effort.
- G1 is HotSpot's default; matching the *default* collector behaviour is the highest-value target
  for gauntlet credibility and for differential comparison against `java`.
- G1's barriers (SATB pre-barrier + RSet post-barrier) already exist and are exercised; the work
  is hardening + validation, not green-field.

ZGC-real maturation is tracked as a **follow-up** (see §8). The scaffolding in this doc
(`GcBackend` plumbing, CLI flag parsing, validation harness) is deliberately collector-agnostic so
ZGC reuses it.

---

## 2. Current state in the codebase (what actually exists)

### 2.1 Backend selection plumbing

- `vm/src/config.rs:8` — `enum GcAlgorithm { Generational, G1 }`. **No ZGC, Parallel, Serial, or
  Shenandoah variants.** `VmConfig::gc_algorithm` field at `config.rs:82`, defaulting to
  `GcAlgorithm::Generational` at `config.rs:330`.
- `vm/src/vm/vm_init.rs:989-993` — the *only* place `GcAlgorithm` is mapped to a `GcBackend`:
  ```
  let gc_backend = match config.gc_algorithm {
      GcAlgorithm::Generational => GcBackend::Generational,
      GcAlgorithm::G1          => GcBackend::G1,
  };
  let heap = VmHeap::new(gc_backend, config.max_heap_size);
  ```
- `gc/src/vm_heap.rs:62` — `enum GcBackend { Generational, G1 }`. `VmHeap` (line 178) is
  `enum { Generational(GenerationalHeap), G1(G1State) }`. **`ZgcRealHeap` is NOT a `VmHeap`
  variant and NOT a `GcBackend` variant** — the key gap for ZGC. `VmHeap::new` (line 199) scales
  G1 region size to 2 MB for heaps > 4 GB.

### 2.2 The CLI gap (G1 is unselectable today)

A repo-wide grep for `UseG1GC` / `gc_algorithm =` / `GcAlgorithm::G1` shows **no argument-parse
path that sets `gc_algorithm` to `G1`**:

- `vm_init.rs:5024` and `serviceability.rs:332,418` only *emit* the string `"-XX:+UseG1GC"` (in
  diagnostic / `-XX:+PrintFlagsFinal`-style output and a sample command line).
- `config.rs:330` hard-defaults to `Generational`.
- There is no `-XX:+UseG1GC` → `config.gc_algorithm = GcAlgorithm::G1` assignment in the launcher
  argument handling.

So G1 is implemented and dispatched but **only reachable via constructing `VmConfig`
programmatically** (e.g. the test at `interpreter.rs:23547` builds `GcBackend::G1` directly). This
is the single most important "scaffolding to land first" item (§7): wire the flag.

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

### 2.7 ZGC (for completeness / why it's deferred)

- `gc/src/zgc.rs` module docs are unusually honest: the `ColoredPointer`/`LoadBarrier`/`ZPage`/
  `ZgcCollector`/`GenerationalZgc` machinery is an **explicit simulation** (synthetic `u64`
  addresses, no `*mut u8`, no byte-copy relocation, non-atomic `&mut self` "barrier", no pause
  benefit). `ZgcRealHeap` (line 1563 `impl GarbageCollector for ZgcRealHeap`) is a **real,
  memory-backed STW mark-sweep** but: non-moving (no compaction), and **no weak/soft/phantom
  reference processing** (full-review `gc-collectors` row, `zgc.rs:1744-1828`).
- `g1_concurrent.rs:57-65` and `zgc_concurrent.rs` note the controller API is intentionally narrow
  so ZGC can reuse it once it moves off the simulation.

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

`gc_worker_threads` exists but evacuation is single-threaded. Parallelizing is **out of scope for
initial selectability** but is the main throughput lever for large heaps. Prerequisite (documented
in `g1.rs:1204-1224`): convert `pointer_map` to a concurrent map with `entry().or_insert_with`
dedup, and shard the work_list per worker. Sequenced as a distinct phase (§4 step 7) so the
collector is *correct and selectable* before it is *fast*.

---

## 4. Incremental delivery plan (small, independently-mergeable, each build-green)

Each step compiles and passes existing tests on its own; no step depends on a later step to be
sound.

1. **CLI flag wiring + warning fallback.** Parse `-XX:+UseG1GC`/`-XX:-UseG1GC` → `gc_algorithm`.
   Unknown `Use*GC` → warn + Generational. Unit-test the parser. (No GC behaviour change; G1 only
   runs if explicitly asked.)
2. **Doc truth-up.** Reconcile `ARCHITECTURE.md` vs `README.md`/`CONTRIBUTING.md`: describe
   Generational as default, G1 as opt-in/experimental-but-real, ZGC-real as built-but-undispatched.
   (Docs-only; full-review docs-governance row.) This is the only step that may touch files outside
   the design doc, and is pure documentation.
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
- **ZGC scope.** Deferred — but the simulation vs `ZgcRealHeap` split, and `ZgcRealHeap`'s absence
  from `GcBackend`, mean "ship ZGC" is a separate, larger project (real colored pointers + load
  barrier). This doc deliberately does not attempt it.

---

## 7. Scaffolding to land first (minimal compiling stubs / flags / knobs)

These are the smallest, build-green pieces that unblock everything else. Described here; landed in
their own steps (§4). None require risky GC-internal surgery.

1. **`-XX:+UseG1GC` / `-XX:-UseG1GC` argument parsing** → `VmConfig.gc_algorithm`. The `GcAlgorithm`
   enum and the `vm_init.rs:989` mapping already exist; this is purely the missing *parse* edge.
   Add a small `parse_gc_algorithm(&str) -> Option<GcAlgorithm>` helper + a warn-and-fallback for
   unrecognized `Use*GC` flags. (Unit-testable with zero GC behaviour change.)
2. **A `GcBackend::Zgc` placeholder is intentionally NOT added yet** — adding it would require a
   `VmHeap::Zgc(ZgcRealHeap)` variant and dispatch arms across `vm_heap.rs`, which is the ZGC
   follow-up's first scaffold, not G1's. Documented here so the omission is deliberate, not an
   oversight: the `GcBackend` enum is the seam where ZGC plugs in later.
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

- **ZGC-real as a selectable backend:** add `GcBackend::Zgc` + `VmHeap::Zgc(ZgcRealHeap)` dispatch,
  wire `ReferenceProcessor` into `ZgcRealHeap::collect_garbage` (currently absent,
  `zgc.rs:1744-1828`), then the real low-latency work (multi-mapped colored pointers, atomic CAS
  load barrier, concurrent relocation/compaction) — a major separate feature.
- **G1 + compact object headers:** blocked on the `CompactHeader` 8 GB forwarding-pointer
  truncation fix (full-review `gc-heap`/`gc-collectors`).
- **Parallel evacuation throughput** (step 9) graduating to default-on.
- **Generational ZGC (JEP 439)** — far future.
